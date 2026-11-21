//! The tier C helper server: MCP Streamable HTTP on the Unix socket.
//!
//! Contract (spec B2, same shape as hoocode's `echo_helper`):
//! - Serves rmcp Streamable HTTP on `$HOOCODE_HELPER_SOCKET`, path `/mcp`.
//! - Every request needs `Authorization: Bearer <known token>` (HTTP 401).
//! - `token -> {agentId, parentId, depth}` map; `hoocode/agentStarted`
//!   registers only a known sender's own children; `hoocode/agentEnded`
//!   closes that agent's sessions; `hoocode/sessionEnding` exits.

use std::convert::Infallible;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, CustomNotification,
    ErrorData, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{RoleServer, ServerHandler};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::error::HooError;
use crate::exec::{self, ExecOutput};
use crate::state::{AgentRecord, State};

/// The 8 declared tools (served by `tools/list` and in the manifest).
/// `browser_cli` is the hidden 9th: callable, never listed.
pub const DECLARED_TOOLS: &[(&str, &str)] = &[
    (
        "browser_open",
        "Open a URL in the caller's tab (agent-browser `open`).",
    ),
    (
        "browser_snapshot",
        "Accessibility snapshot of the caller's tab: refs for `browser_act`, text for reading.",
    ),
    (
        "browser_act",
        "Act on a ref from the snapshot: click, fill, select, check, uncheck, hover, press.",
    ),
    (
        "browser_screenshot",
        "Screenshot of the caller's tab, returned as an image.",
    ),
    (
        "browser_tabs",
        "List this agent's tabs, close one, or (root only) adopt another agent's tabs.",
    ),
    (
        "browser_wait",
        "Wait in the caller's tab: milliseconds, text, ref, or load state.",
    ),
    (
        "browser_nav",
        "Navigate the caller's tab: back, forward, reload.",
    ),
    (
        "browser_eval",
        "Run JavaScript in the caller's tab and return the result.",
    ),
];

/// Shared helper state behind the MCP server.
#[derive(Clone)]
pub struct Helper {
    inner: Arc<Inner>,
}

pub struct Inner {
    pub state: Mutex<State>,
    pub agent_browser: PathBuf,
    pub cdp: String,
    pub data_dir: PathBuf,
    pub log_path: PathBuf,
    /// agent-browser daemon isolation (see ).
    pub sock_dir: PathBuf,
    /// The shared Chromium. Killed explicitly on shutdown: `process::exit`
    /// skips destructors, so `kill_on_drop` alone would orphan it (and the
    /// profile lock) on the sessionEnding/parent-death paths.
    pub chromium: Mutex<Option<tokio::process::Child>>,
    /// Profile lock file this helper holds, removed on shutdown (C3).
    /// `None` when the data profile was busy and we used the runtime one.
    pub profile_lock: Mutex<Option<crate::backend::ProfileLock>>,
    /// Socket this helper serves on, set by `serve`. Every fast-shutdown
    /// path (sessionEnding, parent death, SIGTERM/SIGINT) removes it so no
    /// stale socket shadows the next helper.
    pub socket_path: Mutex<Option<PathBuf>>,
}

impl Helper {
    pub fn new(
        agent_browser: PathBuf,
        cdp: String,
        data_dir: PathBuf,
        sock_dir: PathBuf,
        root_token: String,
        root_agent: String,
    ) -> Self {
        let mut state = State::default();
        state.register_root(root_token, root_agent);
        let log_path = data_dir.join("hoo-browser.log");
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                agent_browser,
                cdp,
                data_dir,
                log_path,
                sock_dir,
                chromium: Mutex::new(None),
                profile_lock: Mutex::new(None),
                socket_path: Mutex::new(None),
            }),
        }
    }

    /// Hand the shared Chromium to the helper so shutdown kills it.
    pub async fn set_chromium(
        &self,
        child: tokio::process::Child,
        profile_lock: Option<crate::backend::ProfileLock>,
    ) {
        *self.inner.chromium.lock().await = Some(child);
        *self.inner.profile_lock.lock().await = profile_lock;
    }

    /// Kill the shared Chromium, best effort. Runs before every
    /// `process::exit`: destructors do not run there.
    pub async fn shutdown(&self) {
        if let Some(mut child) = self.inner.chromium.lock().await.take() {
            // The main process first, then its whole group: Chromium's
            // windowed subprocesses would otherwise survive (M9), keeping
            // the profile locked for the next helper.
            let _ = child.kill().await;
            kill_tree(&child);
            let _ = child.wait().await;
        }
        // Dropping the guard closes the fd and releases the flock (N4).
        let _ = self.inner.profile_lock.lock().await.take();
    }

    /// Fast shutdown for sessionEnding, parent death and signals (N1):
    /// kill the browser tree FIRST, then close leases with a hard ~1 s
    /// cap, then reap, then remove the socket. hoocode waits ~2 s for us
    /// to exit; the 60 s close_all could strand us past it. With the
    /// browser already dead, the lease closes fail fast instead of hanging
    /// on a wedged daemon.
    pub async fn shutdown_fast(&self) {
        if let Some(child) = self.inner.chromium.lock().await.as_ref() {
            kill_tree(child);
        }
        self.close_all_capped(std::time::Duration::from_secs(1))
            .await;
        self.shutdown().await;
        if let Some(path) = self.inner.socket_path.lock().await.clone() {
            let _ = std::fs::remove_file(&path);
        }
    }

    /// The agent record for a bearer token, if known.
    pub async fn agent_for(&self, token: &str) -> Option<AgentRecord> {
        self.inner.state.lock().await.tokens.get(token).cloned()
    }
}

/// Input schemas served by `tools/list`. The `manifest_matches_served_schemas`
/// test pins them to `plugin/plugin.json`.
pub fn tool_schemas() -> Vec<(String, String, Value)> {
    fn schema(properties: &[(&str, Value)], required: &[&str]) -> Value {
        let mut props = Map::new();
        for (name, prop) in properties {
            props.insert((*name).to_owned(), prop.clone());
        }
        Value::Object(
            [
                ("type".to_owned(), Value::String("object".into())),
                ("properties".to_owned(), Value::Object(props)),
                (
                    "required".to_owned(),
                    Value::Array(
                        required
                            .iter()
                            .map(|n| Value::String((*n).into()))
                            .collect(),
                    ),
                ),
            ]
            .into_iter()
            .collect(),
        )
    }
    fn string() -> Value {
        Value::Object(
            [("type".to_owned(), Value::String("string".into()))]
                .into_iter()
                .collect(),
        )
    }
    fn described(mut value: Value, text: &str) -> Value {
        value
            .as_object_mut()
            .expect("schema object")
            .insert("description".into(), Value::String(text.into()));
        value
    }
    fn string_enum(values: &[&str]) -> Value {
        Value::Object(
            [
                ("type".to_owned(), Value::String("string".into())),
                (
                    "enum".to_owned(),
                    Value::Array(values.iter().map(|v| Value::String((*v).into())).collect()),
                ),
            ]
            .into_iter()
            .collect(),
        )
    }
    fn tab() -> Value {
        described(
            Value::Object(
                [("type".to_owned(), Value::String("integer".into()))]
                    .into_iter()
                    .collect(),
            ),
            "Tab number, 1-based (default 1); a new number opens a tab.",
        )
    }
    vec![
        (
            "browser_open".into(),
            DECLARED_TOOLS[0].1.into(),
            schema(&[("url", string()), ("tab", tab())], &["url"]),
        ),
        (
            "browser_snapshot".into(),
            DECLARED_TOOLS[1].1.into(),
            schema(
                &[
                    ("tab", tab()),
                    (
                        "interactive",
                        described(
                            Value::Object(
                                [("type".to_owned(), Value::String("boolean".into()))]
                                    .into_iter()
                                    .collect(),
                            ),
                            "Cursor position and interactive-only refs.",
                        ),
                    ),
                ],
                &[],
            ),
        ),
        (
            "browser_act".into(),
            DECLARED_TOOLS[2].1.into(),
            schema(
                &[
                    (
                        "action",
                        string_enum(&[
                            "click", "fill", "select", "check", "uncheck", "hover", "press",
                        ]),
                    ),
                    (
                        "ref",
                        described(string(), "Ref from the snapshot (e.g. \"e3\")."),
                    ),
                    (
                        "value",
                        described(string(), "Text for fill, option for select, key for press."),
                    ),
                    ("tab", tab()),
                ],
                &["action", "ref"],
            ),
        ),
        (
            "browser_screenshot".into(),
            DECLARED_TOOLS[3].1.into(),
            schema(
                &[
                    ("tab", tab()),
                    (
                        "full_page",
                        described(
                            Value::Object(
                                [("type".to_owned(), Value::String("boolean".into()))]
                                    .into_iter()
                                    .collect(),
                            ),
                            "Capture the full page, not just the viewport.",
                        ),
                    ),
                ],
                &[],
            ),
        ),
        (
            "browser_tabs".into(),
            DECLARED_TOOLS[4].1.into(),
            schema(
                &[
                    ("action", string_enum(&["list", "close", "adopt"])),
                    ("tab", tab()),
                    (
                        "from",
                        described(
                            string(),
                            "Agent id to adopt tabs from (adopt only, root only).",
                        ),
                    ),
                ],
                &["action"],
            ),
        ),
        (
            "browser_wait".into(),
            DECLARED_TOOLS[5].1.into(),
            schema(
                &[
                    ("tab", tab()),
                    (
                        "ms",
                        described(
                            Value::Object(
                                [("type".to_owned(), Value::String("integer".into()))]
                                    .into_iter()
                                    .collect(),
                            ),
                            "Milliseconds to wait.",
                        ),
                    ),
                    ("text", described(string(), "Text to wait for.")),
                    (
                        "ref",
                        described(string(), "Ref to wait for (from the snapshot)."),
                    ),
                    (
                        "load",
                        described(string(), "Load state to wait for (e.g. networkidle)."),
                    ),
                ],
                &[],
            ),
        ),
        (
            "browser_nav".into(),
            DECLARED_TOOLS[6].1.into(),
            schema(
                &[
                    ("action", string_enum(&["back", "forward", "reload"])),
                    ("tab", tab()),
                ],
                &["action"],
            ),
        ),
        (
            "browser_eval".into(),
            DECLARED_TOOLS[7].1.into(),
            schema(&[("js", string()), ("tab", tab())], &["js"]),
        ),
    ]
}

/// Bearer token of a request, from its `Authorization` header.
fn bearer_of(parts: &http::request::Parts) -> Option<String> {
    let value = parts.headers.get(http::header::AUTHORIZATION)?;
    let value = value.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::to_owned)
}

/// True when the request carries a known bearer token.
async fn is_authorized<T>(request: &Request<T>, helper: &Helper) -> bool {
    let Some(value) = request.headers().get(http::header::AUTHORIZATION) else {
        return false;
    };
    let Some(token) = value.to_str().ok().and_then(|v| v.strip_prefix("Bearer ")) else {
        return false;
    };
    helper.agent_for(token).await.is_some()
}

fn unauthorized_response() -> Response<BoxBody<Bytes, Infallible>> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .body(
            Full::new(Bytes::from_static(b"unknown token"))
                .map_err(|never| match never {})
                .boxed(),
        )
        .expect("401 response")
}

/// The rmcp service behind an HTTP-401 bearer check. rmcp has no auth layer
/// of its own, so unknown tokens are rejected here, before rmcp sees them.
///
/// Request-entry stamp (M5): recorded here so the overhead window covers
/// auth + decode, not just the tool body.
#[derive(Clone, Copy)]
struct RequestStarted(std::time::Instant);

#[derive(Clone)]
struct AuthService<S> {
    helper: Helper,
    inner: S,
}

impl<S> hyper::service::Service<Request<Incoming>> for AuthService<S>
where
    S: hyper::service::Service<
            Request<Incoming>,
            Response = Response<BoxBody<Bytes, Infallible>>,
            Error = Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<BoxBody<Bytes, Infallible>>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn call(&self, request: Request<Incoming>) -> Self::Future {
        let inner = self.inner.clone();
        let helper = self.helper.clone();
        Box::pin(async move {
            // M5: stamp at request entry, before auth and the rmcp decode.
            let (mut parts, body) = request.into_parts();
            parts
                .extensions
                .insert(RequestStarted(std::time::Instant::now()));
            let request = Request::from_parts(parts, body);
            if !is_authorized(&request, &helper).await {
                return Ok(unauthorized_response());
            }
            inner.call(request).await
        })
    }
}

/// Serve the helper on the Unix socket; returns when the listener fails.
pub async fn serve(helper: Helper, socket: PathBuf) -> Result<(), HooError> {
    let _ = std::fs::remove_file(&socket);
    *helper.inner.socket_path.lock().await = Some(socket.clone());
    let listener = tokio::net::UnixListener::bind(&socket)
        .map_err(|e| HooError::Transport(format!("cannot bind {}: {e}", socket.display())))?;
    eprintln!("hoo-browser: listening on {}", socket.display());
    let factory_helper = helper.clone();
    let service = StreamableHttpService::new(
        move || Ok(factory_helper.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let base = TowerToHyperService::new(service);
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let service = AuthService {
            helper: helper.clone(),
            inner: base.clone(),
        };
        tokio::spawn(async move {
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// MCP error for a refused call.
fn refused(message: &str) -> ErrorData {
    ErrorData::new(
        rmcp::model::ErrorCode::INVALID_REQUEST,
        message.to_string(),
        None,
    )
}

fn internal(message: String) -> ErrorData {
    ErrorData::new(rmcp::model::ErrorCode::INTERNAL_ERROR, message, None)
}

/// Tool arg `tab`: 1-based, default 1. Non-integers, floats, zero and
/// negatives are refused instead of silently defaulting (L2): resolving the
/// wrong tab would address another lease's session.
fn tab_of(args: &Value) -> Result<usize, HooError> {
    match args.get("tab") {
        None => Ok(1),
        Some(Value::Number(n)) if n.as_u64().is_some_and(|v| v >= 1) => {
            Ok(n.as_u64().expect("checked") as usize)
        }
        _ => Err(HooError::Failed("tab must be an integer >= 1".into())),
    }
}

impl Helper {
    /// Argv for a lease-scoped agent-browser call (H2: always pinned).
    fn session_argv(&self, session: &str, command: &[&str]) -> Vec<String> {
        let mut argv = vec![
            self.inner.agent_browser.to_string_lossy().into_owned(),
            "--cdp".into(),
            self.inner.cdp.clone(),
            "--session".into(),
            session.into(),
            "--json".into(),
            "--pin-tab".into(),
        ];
        argv.extend(command.iter().map(|s| (*s).to_owned()));
        argv
    }

    /// Run one lease-scoped CLI call for a tool call, retrying once when
    /// the failure proves the command never reached the daemon
    /// (connect-phase cold-start race, N7): the second attempt's CLI
    /// finds the started daemon. Never retries outcome_unknown, timeouts
    /// or post-send failures — the command may have run, and re-sending a
    /// mutating command could double-execute. Close/shutdown paths must
    /// not use this: they stay single-shot and bounded.
    async fn run_session_command(
        &self,
        argv: &[String],
        deadline: std::time::Instant,
    ) -> Result<Value, HooError> {
        match exec::run_command(argv, exec::spawn_timeout(deadline)?, &self.inner.sock_dir).await {
            Err(e) if exec::not_sent_to_daemon(&e.to_string()) => {
                exec::run_command(argv, exec::spawn_timeout(deadline)?, &self.inner.sock_dir).await
            }
            other => other,
        }
    }

    /// Same connect-phase-only retry for raw `browser_cli` output.
    async fn run_session_raw(
        &self,
        scoped: &[String],
        deadline: std::time::Instant,
    ) -> Result<String, HooError> {
        match self.run_raw(scoped, exec::spawn_timeout(deadline)?).await {
            Err(e) if exec::not_sent_to_daemon(&e.to_string()) => {
                self.run_raw(scoped, exec::spawn_timeout(deadline)?).await
            }
            other => other,
        }
    }

    /// Resolve the caller's tab to a session with its own window (H2): the
    /// first call on a new tab runs `window new`; if that fails the lease is
    /// rolled back so no phantom tab survives; concurrent first calls on a
    /// new tab wait for the creator instead of opening duplicate windows.
    async fn acquire(
        &self,
        agent: &AgentRecord,
        token: &str,
        tab: usize,
        deadline: std::time::Instant,
    ) -> Result<(String, std::time::Duration), HooError> {
        let agent_id = agent.agent_id.as_str();
        loop {
            // N5: agentEnded may have removed the caller's token while a
            // concurrent initializer worked. Re-check registration before
            // (re)creating anything: a rolled-back lease (`None` below)
            // must not come back for a dead agent.
            if !self.inner.state.lock().await.tokens.contains_key(token) {
                return Err(HooError::Refused(format!(
                    "agent {agent_id} is no longer registered (agentEnded?)"
                )));
            }
            let (session, fresh) = self.inner.state.lock().await.ensure(agent_id, tab)?;
            // Touch at call start (M7): the sweeper skips anything touched
            // within the idle window, so a call in flight is never reaped.
            let closing = {
                let mut state = self.inner.state.lock().await;
                state.touch(&session);
                state.is_closing(&session)
            };
            if closing {
                return Err(HooError::Failed(format!("tab {tab} is closing; try again")));
            }
            if fresh {
                // The window-new wall time is reported separately (M5).
                let window_started = std::time::Instant::now();
                match self.open_window(&session, deadline).await {
                    Ok(()) => {
                        let window_new = window_started.elapsed();
                        self.inner.state.lock().await.mark_ready(&session);
                        return Ok((session, window_new));
                    }
                    Err(e) => {
                        self.inner.state.lock().await.abandon(&session);
                        return Err(e);
                    }
                }
            }
            match self.inner.state.lock().await.is_initializing(&session) {
                // Another call is opening this tab right now: wait for it.
                Some(true) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(HooError::Failed(format!(
                            "tab {tab} is still opening; try again"
                        )));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                // Ready: run on it (no window ran on this call). Gone
                // (rolled back under us): loop and re-ensure, which
                // recreates the lease (after the N5 token re-check).
                Some(false) => return Ok((session, std::time::Duration::ZERO)),
                None => continue,
            }
        }
    }

    /// Run one tool call for `agent`: resolve the lease, run agent-browser,
    /// log overhead. agent-browser failures come back as tool errors (isError),
    /// so the model sees them; refusals are protocol errors.
    async fn run_tool(
        &self,
        agent: &AgentRecord,
        token: &str,
        started: std::time::Instant,
        tool: &str,
        args: &Value,
    ) -> Result<Vec<ContentBlock>, HooError> {
        if tool == "browser_cli" {
            return self.run_cli(agent, token, started, args).await;
        }
        if tool == "browser_tabs" {
            return self.run_tabs(agent, started, args).await;
        }
        // One budget for the whole call (M1): acquire-wait, window new and
        // the command share it instead of stacking timeouts. `started` is
        // the auth-layer entry stamp (M5): the total covers the whole call
        // while `child` covers only agent-browser spawns.
        let deadline = started + exec::CALL_BUDGET;
        let tab = tab_of(args)?;
        let (session, window_new) = self.acquire(agent, token, tab, deadline).await?;
        let (child, output) = if tool == "browser_screenshot" {
            self.run_screenshot(&session, args, deadline).await
        } else {
            let argv = exec::tool_argv(
                &self.inner.agent_browser,
                &self.inner.cdp,
                &session,
                tool,
                args,
                None,
            )?;
            let child_started = std::time::Instant::now();
            let data = self.run_session_command(&argv, deadline).await;
            (child_started.elapsed(), data.map(ExecOutput::Json))
        };
        let total = started.elapsed();
        self.inner.state.lock().await.touch(&session);
        let error = match &output {
            Ok(_) => None,
            Err(e) => Some(e.to_string()),
        };
        exec::log_call(
            &self.inner.log_path,
            exec::CallRecord {
                tool,
                agent: &agent.agent_id,
                session: &session,
                total,
                child,
                window_new,
                ok: error.is_none(),
                error: error.as_deref(),
            },
        )
        .await;
        match output? {
            ExecOutput::Json(data) => Ok(vec![ContentBlock::text(
                serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string()),
            )]),
            ExecOutput::Png(bytes) => Ok(vec![ContentBlock::image(
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
                "image/png",
            )]),
        }
    }

    /// Hidden `browser_cli {args}`: raw passthrough scoped to the caller's
    /// own sessions. `--json` is not forced; stdout returns as text.
    async fn run_cli(
        &self,
        agent: &AgentRecord,
        token: &str,
        started: std::time::Instant,
        args: &Value,
    ) -> Result<Vec<ContentBlock>, HooError> {
        let raw = args
            .get("args")
            .and_then(Value::as_array)
            .ok_or_else(|| HooError::Refused("browser_cli needs \"args\"".into()))?;
        let raw: Vec<String> = raw
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| HooError::Refused("browser_cli args must be strings".into()))
            })
            .collect::<Result<_, _>>()?;
        let deadline = started + exec::CALL_BUDGET;
        let (default_session, window_new) = self.acquire(agent, token, 1, deadline).await?;
        let owned = self.inner.state.lock().await.owned(&agent.agent_id);
        let scoped = exec::scope_cli_args(&raw, &self.inner.cdp, &default_session, &owned)?;
        let mut full = vec![self.inner.agent_browser.to_string_lossy().into_owned()];
        full.extend(scoped);
        let child_started = std::time::Instant::now();
        let text = self.run_session_raw(&full, deadline).await;
        let child = child_started.elapsed();
        let total = started.elapsed();
        let error = match &text {
            Ok(_) => None,
            Err(e) => Some(e.to_string()),
        };
        exec::log_call(
            &self.inner.log_path,
            exec::CallRecord {
                tool: "browser_cli",
                agent: &agent.agent_id,
                session: &default_session,
                total,
                child,
                window_new,
                ok: error.is_none(),
                error: error.as_deref(),
            },
        )
        .await;
        text.map(|body| vec![ContentBlock::text(body)])
    }

    /// Give a fresh lease its own window on the shared browser: one
    /// agent-browser `window new`. The retry budget comes from the
    /// caller's deadline: a connect-phase failure (N7) is retried once
    /// with the remaining budget, so the retry never pushes a window
    /// open past CALL_BUDGET.
    async fn open_window(
        &self,
        session: &str,
        deadline: std::time::Instant,
    ) -> Result<(), HooError> {
        let argv = self.session_argv(session, &["window", "new"]);
        self.run_session_command(&argv, deadline).await?;
        Ok(())
    }

    async fn run_screenshot(
        &self,
        session: &str,
        args: &Value,
        deadline: std::time::Instant,
    ) -> (std::time::Duration, Result<ExecOutput, HooError>) {
        let _ = crate::backend::ensure_dir_0700(&self.inner.data_dir.join("shots"));
        let path = exec::shot_path(&self.inner.data_dir);
        let argv = match exec::tool_argv(
            &self.inner.agent_browser,
            &self.inner.cdp,
            session,
            "browser_screenshot",
            args,
            Some(&path),
        ) {
            Ok(argv) => argv,
            Err(e) => return (std::time::Duration::ZERO, Err(e)),
        };
        let child_started = std::time::Instant::now();
        let result = self.run_session_command(&argv, deadline).await;
        let child = child_started.elapsed();
        let output = result.and_then(|_| {
            std::fs::read(&path).map(ExecOutput::Png).map_err(|e| {
                HooError::Failed(format!(
                    "screenshot saved but cannot be read ({}): {e}",
                    path.display()
                ))
            })
        });
        let _ = std::fs::remove_file(&path);
        (child, output)
    }

    /// Raw passthrough for `browser_cli`: stdout is returned as text.
    async fn run_raw(
        &self,
        scoped: &[String],
        timeout: std::time::Duration,
    ) -> Result<String, HooError> {
        use tokio::process::Command;
        let (program, args) = scoped
            .split_first()
            .ok_or_else(|| HooError::Spawn("empty argv".into()))?;
        let mut child = Command::new(program);
        child
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        exec::child_env(&mut child, Some(&self.inner.sock_dir));
        let output = tokio::time::timeout(timeout, child.output())
            .await
            .map_err(|_| {
                HooError::Spawn(format!(
                    "agent-browser timed out after {}s",
                    timeout.as_secs()
                ))
            })?
            .map_err(|e| HooError::Spawn(format!("cannot spawn agent-browser: {e}")))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if output.status.success() {
            Ok(stdout)
        } else if stdout.trim().is_empty() {
            Err(HooError::Failed(crate::error::short_stderr(&stderr)))
        } else {
            Err(HooError::Failed(crate::error::short_stderr(&stdout)))
        }
    }

    async fn run_tabs(
        &self,
        agent: &AgentRecord,
        started: std::time::Instant,
        args: &Value,
    ) -> Result<Vec<ContentBlock>, HooError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        let deadline = started + exec::CALL_BUDGET;
        // Child (spawn+connect) time is measured per call, never equated
        // with the total: the old `child: total` logged a permanent zero
        // overhead (M5).
        let mut child = std::time::Duration::ZERO;
        let output: Result<Value, HooError> = match action {
            "list" => {
                let owned = self.inner.state.lock().await.owned(&agent.agent_id);
                // A listing in flight touches its leases (M7).
                {
                    let mut state = self.inner.state.lock().await;
                    for session in &owned {
                        state.touch(session);
                    }
                }
                let mut tabs = Vec::new();
                for session in owned {
                    let argv = exec::tool_argv(
                        &self.inner.agent_browser,
                        &self.inner.cdp,
                        &session,
                        "browser_tabs",
                        &serde_json::json!({"action": "list"}),
                        None,
                    )?;
                    if exec::spawn_timeout(deadline).is_err() {
                        tabs.push(
                            serde_json::json!({"session": session, "error": "call budget exceeded"}),
                        );
                        break;
                    }
                    let call_started = std::time::Instant::now();
                    let result = self.run_session_command(&argv, deadline).await;
                    child += call_started.elapsed();
                    match result {
                        Ok(data) => tabs.push(
                            serde_json::json!({"session": session, "tabs": data.get("tabs")}),
                        ),
                        Err(e) => {
                            tabs.push(
                                serde_json::json!({"session": session, "error": e.to_string()}),
                            );
                        }
                    }
                }
                Ok(Value::Array(tabs))
            }
            "close" => {
                let tab = tab_of(args)?;
                let session = self
                    .inner
                    .state
                    .lock()
                    .await
                    .resolve(&agent.agent_id, tab)?;
                // H4: `tab close` refuses the last tab over `--cdp` and
                // `close` only disconnects, so close the lease's bound tab
                // via the DevTools HTTP endpoint first, then `close` the
                // session. Only the `active` (bound) tab is closed: the
                // session's list shows the whole shared browser, so closing
                // every listed target would take other leases' pages with
                // it. The DevTools close is the helper's own localhost
                // HTTP, not child time. The list phase is capped well under
                // the call budget so a wedged daemon cannot eat the `close`
                // itself: a failed list just skips the DevTools step.
                if let Ok(list_timeout) = exec::spawn_timeout(deadline) {
                    let list_timeout = list_timeout.min(std::time::Duration::from_secs(15));
                    let list_started = std::time::Instant::now();
                    let list = self.session_argv(&session, &["tab", "list"]);
                    if let Ok(data) =
                        exec::run_command(&list, list_timeout, &self.inner.sock_dir).await
                    {
                        if let Some(target) = lease_bound_target(&data) {
                            if let Some(authority) = cdp_authority(&self.inner.cdp) {
                                devtools_close_target(&authority, &target).await;
                            }
                        }
                    }
                    child += list_started.elapsed();
                }
                let argv = exec::tool_argv(
                    &self.inner.agent_browser,
                    &self.inner.cdp,
                    &session,
                    "browser_tabs",
                    &serde_json::json!({"action": "close"}),
                    None,
                )?;
                let call_started = std::time::Instant::now();
                self.run_session_command(&argv, deadline).await?;
                child += call_started.elapsed();
                self.inner
                    .state
                    .lock()
                    .await
                    .close(&agent.agent_id, &session)?;
                Ok(serde_json::json!({"closed": session}))
            }
            "adopt" => {
                if agent.agent_id != "root" {
                    return Err(HooError::Refused("adopt is root-only".into()));
                }
                let from = args
                    .get("from")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| HooError::Refused("adopt needs \"from\" (agent id)".into()))?;
                let count = self.inner.state.lock().await.adopt(from, "root")?;
                Ok(serde_json::json!({"adopted": from, "tabs": count}))
            }
            _ => Err(HooError::Refused(format!(
                "browser_tabs action must be one of list|close|adopt, got {action:?}"
            ))),
        };
        let total = started.elapsed();
        let error = match &output {
            Ok(_) => None,
            Err(e) => Some(e.to_string()),
        };
        exec::log_call(
            &self.inner.log_path,
            exec::CallRecord {
                tool: "browser_tabs",
                agent: &agent.agent_id,
                session: "-",
                total,
                child,
                // Listing and closing never acquire: no window can run.
                window_new: std::time::Duration::ZERO,
                ok: error.is_none(),
                error: error.as_deref(),
            },
        )
        .await;
        output.map(|data| {
            vec![ContentBlock::text(
                serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string()),
            )]
        })
    }

    /// Close one agent-browser session, best effort (sweeper/agentEnded).
    ///
    /// Over `--cdp` `tab close` refuses the last tab and `close` only
    /// disconnects the session, so neither removes the lease's page from
    /// the shared browser (H4). Close the lease's bound tab through the
    /// DevTools HTTP endpoint on the helper's own Chromium, then `close`
    /// the session itself.
    async fn close_session(&self, session: &str) {
        // H4: over `--cdp` agent-browser's `tab close` refuses the last
        // tab and `close` only disconnects the session, so neither removes
        // the lease's pages from the shared browser. List the lease's own
        // targets and close each through the DevTools HTTP endpoint on the
        // helper's own Chromium (`GET /json/close/<targetId>`), then
        // `close` the session itself. All best effort: the process exits.
        let list = self.session_argv(session, &["tab", "list"]);
        if let Ok(data) = exec::run_command(&list, exec::call_timeout(), &self.inner.sock_dir).await
        {
            if let Some(target) = lease_bound_target(&data) {
                if let Some(authority) = cdp_authority(&self.inner.cdp) {
                    devtools_close_target(&authority, &target).await;
                }
            }
        }
        let close = self.session_argv(session, &["close"]);
        let _ = exec::run_command(&close, exec::call_timeout(), &self.inner.sock_dir).await;
    }
}

/// `ws://127.0.0.1:9222/devtools/...` -> `127.0.0.1:9222` for the DevTools
/// HTTP endpoint (H4). The helper launched Chromium itself, so the port in
/// its own `--cdp` value is the browser's port.
fn cdp_authority(cdp: &str) -> Option<String> {
    let bare = cdp
        .strip_prefix("ws://")
        .or_else(|| cdp.strip_prefix("wss://"))?;
    let authority = bare.split('/').next()?;
    if authority.is_empty() {
        return None;
    }
    Some(authority.to_owned())
}

/// The lease's bound tab from a `tab list --json` envelope: the entry the
/// session marks `active` is the tab agent-browser runs the session on
/// (probed: each session's list shows the whole shared browser, but exactly
/// its own tab is active). Only that target is provably the lease's, so
/// only it is closed here — never another lease's pages.
fn lease_bound_target(data: &Value) -> Option<String> {
    data.get("tabs").and_then(Value::as_array).and_then(|tabs| {
        tabs.iter()
            .filter(|tab| tab.get("active").and_then(Value::as_bool).unwrap_or(false))
            .filter_map(|tab| tab.get("targetId").and_then(Value::as_str))
            .map(str::to_owned)
            .next()
    })
}

/// Close one page target through the DevTools HTTP endpoint (H4):
/// `GET /json/close/<targetId>`. Frame the reply by Content-Length: the
/// DevTools server keep-alives HTTP/1.1 connections despite
/// `Connection: close`, so read-until-EOF would stall to the timeout on
/// every target. Best effort with a short cap — a wedged browser must not
/// stall lease closing; the `close` after it still drops the session.
async fn devtools_close_target(authority: &str, target_id: &str) {
    let work = async {
        let mut stream = tokio::net::TcpStream::connect(authority)
            .await
            .map_err(|_| ())?;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let request = format!(
            "GET /json/close/{target_id} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.map_err(|_| ())?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return Err(()),
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > 65536 {
                        return Err(());
                    }
                    if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break at + 4;
                    }
                }
            }
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let len: usize = headers
            .lines()
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
            })
            .next()
            .flatten()
            .unwrap_or(0);
        let mut have = buf.len() - header_end;
        while have < len {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    have += n;
                    if have > 1_000_000 {
                        break;
                    }
                }
            }
        }
        Ok::<(), ()>(())
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), work).await;
}

impl Helper {
    /// Close every lease (bounded concurrency) before killing Chromium (H3):
    /// sessionEnding and parent death must not strand windows on the shared
    /// browser. Best effort with an overall cap; the process exits next.
    pub async fn close_all_sessions(&self) {
        self.close_all_capped(std::time::Duration::from_secs(60))
            .await;
    }

    async fn close_all_capped(&self, cap: std::time::Duration) {
        let sessions: Vec<String> = self
            .inner
            .state
            .lock()
            .await
            .leases
            .keys()
            .cloned()
            .collect();
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(8));
        let mut set = tokio::task::JoinSet::new();
        for session in sessions {
            let this = self.clone();
            let semaphore = semaphore.clone();
            set.spawn(async move {
                let _permit = semaphore.acquire_owned().await;
                this.close_session(&session).await;
            });
        }
        let _ = tokio::time::timeout(cap, set.join_all()).await;
    }
}

/// SIGKILL a child and its tree, but only via `killpg` when the child leads
/// a group that is not ours (M9 sets that up for Chromium). A child that
/// shares our group (tests, plain spawns) gets a single `kill`: killpg on
/// a foreign pgid could hit an unrelated group.
fn kill_tree(child: &tokio::process::Child) {
    let Some(pid) = child.id() else {
        return;
    };
    let pid = pid as libc::pid_t;
    // SAFETY: SIGKILL to our own child only.
    let group_killed = unsafe {
        let pgid = libc::getpgid(pid);
        pgid == pid && pgid != libc::getpgrp() && libc::killpg(pid, libc::SIGKILL) == 0
    };
    if group_killed {
        return;
    }
    // SAFETY: SIGKILL to our own child only.
    unsafe {
        let _ = libc::kill(pid, libc::SIGKILL);
    }
}

impl ServerHandler for Helper {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools = Vec::new();
        for (name, description, schema) in tool_schemas() {
            let map: Map<String, Value> =
                serde_json::from_value(schema).map_err(|e| internal(e.to_string()))?;
            tools.push(Tool::new(name, description, std::sync::Arc::new(map)));
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let parts = context
            .extensions
            .get::<http::request::Parts>()
            .cloned()
            .ok_or_else(|| internal("no request parts".to_string()))?;
        let token = bearer_of(&parts).ok_or_else(|| refused("unknown token"))?;
        let agent = self
            .agent_for(&token)
            .await
            .ok_or_else(|| refused("unknown token"))?;
        // M5: the auth layer stamped request entry; fall back to now only
        // when the stamp is missing (direct in-process callers in tests).
        let started = context
            .extensions
            .get::<RequestStarted>()
            .map(|stamped| stamped.0)
            .unwrap_or_else(std::time::Instant::now);
        let name = request.name.as_ref();
        if !DECLARED_TOOLS.iter().any(|(n, _)| *n == name) && name != "browser_cli" {
            return Err(ErrorData::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                format!("unknown tool {name}"),
                None,
            ));
        }
        let args = request
            .arguments
            .as_ref()
            .map(|map| Value::Object(map.clone()))
            .unwrap_or(Value::Null);
        match self.run_tool(&agent, &token, started, name, &args).await {
            Ok(blocks) => Ok(CallToolResult::success(blocks).into()),
            Err(HooError::Refused(message)) => Err(refused(&message)),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())]).into()),
        }
    }

    async fn on_custom_notification(
        &self,
        notification: CustomNotification,
        context: NotificationContext<RoleServer>,
    ) {
        let parts = context.extensions.get::<http::request::Parts>().cloned();
        let Some(parts) = parts else { return };
        let Some(token) = bearer_of(&parts) else {
            return;
        };
        let Some(sender) = self.agent_for(&token).await else {
            return;
        };
        match notification.method.as_str() {
            "hoocode/agentStarted" => {
                let params = notification.params.unwrap_or(Value::Null);
                let agent_id = params
                    .get("agentId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let parent_id = params
                    .get("parentId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let depth = params.get("depth").and_then(Value::as_u64).unwrap_or(0);
                let child_token = params
                    .get("token")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let mut state = self.inner.state.lock().await;
                let _ = state.register_agent(&sender, agent_id, parent_id, depth, child_token);
            }
            "hoocode/agentEnded" => {
                let params = notification.params.unwrap_or(Value::Null);
                let Some(agent_id) = params.get("agentId").and_then(Value::as_str) else {
                    return;
                };
                // Only an agent strictly below the sender (or root) may be
                // ended: a sibling ending another agent would steal its
                // leases (H1). Anything else is ignored.
                if sender.agent_id != "root"
                    && !agent_id.starts_with(&format!("{}/", sender.agent_id))
                {
                    return;
                }
                let sessions = self.inner.state.lock().await.remove_agent(agent_id);
                for session in sessions {
                    self.close_session(&session).await;
                }
            }
            "hoocode/sessionEnding" => {
                // Only root ends the session: anyone else's notice is noise.
                if sender.agent_id != "root" {
                    return;
                }
                self.shutdown_fast().await;
                std::process::exit(0);
            }
            _ => {}
        }
    }
}

/// Idle sweeper: every minute, close leases idle over the limit.
/// Idle sweeper: every minute, close leases idle over the limit (M7).
/// The check-then-claim protocol closes nothing a call touched: the sweep
/// notes the time, claims only leases untouched since then, closes them
/// browser-side, and re-checks before removal. A call starting mid-close
/// touches its lease, so the re-check fails and the lease survives (the
/// claim is released).
pub async fn sweep_loop(helper: Helper) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        interval.tick().await;
        let check = std::time::Instant::now();
        let idle = {
            let state = helper.inner.state.lock().await;
            state.idle(check)
        };
        for session in idle {
            let claimed = {
                let mut state = helper.inner.state.lock().await;
                state.try_claim_close(&session, check)
            };
            if !claimed {
                continue;
            }
            helper.close_session(&session).await;
            let mut state = helper.inner.state.lock().await;
            if state.still_idle(&session, check) {
                state.remove_session(&session);
            } else {
                state.unclaim(&session);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_helper(dir: &std::path::Path) -> Helper {
        Helper::new(
            PathBuf::from("/bin/true"),
            "ws://fake".into(),
            dir.to_owned(),
            crate::backend::socket_dir(dir),
            "root-token".into(),
            "root".into(),
        )
    }

    #[test]
    fn cdp_authority_takes_the_browser_port() {
        assert_eq!(
            cdp_authority("ws://127.0.0.1:9222/devtools/browser/abc").as_deref(),
            Some("127.0.0.1:9222")
        );
        assert_eq!(
            cdp_authority("wss://127.0.0.1:1/x").as_deref(),
            Some("127.0.0.1:1")
        );
        assert_eq!(cdp_authority("http://127.0.0.1:9222/x"), None);
        assert_eq!(cdp_authority("ws://"), None);
        assert_eq!(cdp_authority(""), None);
    }

    #[test]
    fn lease_bound_target_is_the_active_tab() {
        // The session's list shows the whole shared browser; exactly its
        // own (bound) tab is active, and only that target may be closed.
        let data = serde_json::json!({"tabs": [
            {"tabId": "t1", "targetId": "AAA", "active": false},
            {"tabId": "t2", "targetId": "BBB", "active": true},
            {"tabId": "t3", "targetId": "CCC"},
        ]});
        assert_eq!(lease_bound_target(&data).as_deref(), Some("BBB"));
        assert_eq!(lease_bound_target(&serde_json::json!({"tabs": []})), None);
        assert_eq!(lease_bound_target(&serde_json::json!({})), None);
        // No active tab (a gone binding): nothing to close.
        let data = serde_json::json!({"tabs": [
            {"tabId": "t1", "targetId": "AAA", "active": false},
        ]});
        assert_eq!(lease_bound_target(&data), None);
    }

    #[tokio::test]
    async fn devtools_close_hits_json_close_for_the_target() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let authority = listener.local_addr().expect("addr").to_string();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 1024];
            let n = stream.read(&mut buf).await.expect("read");
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"closed\":1}")
                .await;
            // Keep the connection open like the real DevTools server
            // (keep-alive despite `Connection: close`): the client must
            // return from the framed body, not from EOF.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            request
        });
        let started = std::time::Instant::now();
        devtools_close_target(&authority, "TARGET-9").await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "the close must return from the framed body, not EOF/timeout"
        );
        let request = server.await.expect("server");
        assert!(
            request.starts_with("GET /json/close/TARGET-9 "),
            "must close the target over DevTools HTTP, got: {request}"
        );
        // An unreachable browser never stalls the close (5 s cap, best effort).
        devtools_close_target("127.0.0.1:1", "TARGET-9").await;
    }

    #[tokio::test]
    async fn acquire_refuses_an_ended_agent_instead_of_recreating() {
        // N5: agentEnded removes the token and the leases. A concurrent
        // initializer finishing under the waiter must not bring the lease
        // back: the re-ensure re-checks the token first.
        let dir = std::env::temp_dir().join(format!("hoo-acquire-ended-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let helper = Helper::new(
            std::path::PathBuf::from("/nonexistent/agent-browser-for-tests"),
            "ws://fake".into(),
            dir.clone(),
            crate::backend::socket_dir(&dir),
            "root-token".into(),
            "root".into(),
        );
        let sender = helper
            .inner
            .state
            .lock()
            .await
            .tokens
            .get("root-token")
            .cloned()
            .expect("root token");
        helper
            .inner
            .state
            .lock()
            .await
            .register_agent(
                &sender,
                "root/task-1".into(),
                Some("root".into()),
                1,
                "child-token".into(),
            )
            .expect("register");
        // A lease created before the end (still initializing, as if its
        // `window new` were in flight) goes away with agentEnded.
        let (session, _) = helper
            .inner
            .state
            .lock()
            .await
            .ensure("root/task-1", 1)
            .expect("ensure");
        assert!(helper
            .inner
            .state
            .lock()
            .await
            .is_initializing(&session)
            .unwrap());
        helper.inner.state.lock().await.remove_agent("root/task-1");
        let agent = AgentRecord {
            agent_id: "root/task-1".into(),
            parent_id: Some("root".into()),
            depth: 1,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(50);
        let error = helper
            .acquire(&agent, "child-token", 1, deadline)
            .await
            .expect_err("an ended agent must be refused");
        assert!(
            matches!(error, HooError::Refused(_)),
            "ended acquire must be a refusal, got {error:?}"
        );
        assert!(
            helper.inner.state.lock().await.leases.is_empty(),
            "no lease may be recreated for a dead agent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn shutdown_fast_kills_and_reaps_a_plain_child() {
        let dir = std::env::temp_dir().join(format!("hoo-shutdown-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let helper = test_helper(&dir);
        // A plain `sleep` shares our process group, so kill_tree must take
        // the single-kill path (never killpg on our own group).
        let child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep");
        let pid = child.id().expect("child pid") as libc::pid_t;
        helper.set_chromium(child, None).await;
        // Must return promptly even though the child would sleep 30 s.
        tokio::time::timeout(std::time::Duration::from_secs(10), helper.shutdown_fast())
            .await
            .expect("shutdown_fast is bounded");
        // Reaped: signalling the pid fails.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
