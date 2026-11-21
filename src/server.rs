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
use crate::exec::{self, ExecOutput, CALL_TIMEOUT};
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
    /// skips destructors, so `kill_on_drop` alone would orphan it (and its
    /// profile lock) on the sessionEnding/parent-death paths.
    pub chromium: Mutex<Option<tokio::process::Child>>,
}

impl Helper {
    pub fn new(
        agent_browser: PathBuf,
        cdp: String,
        data_dir: PathBuf,
        root_token: String,
        root_agent: String,
    ) -> Self {
        let mut state = State::default();
        state.register_root(root_token, root_agent);
        let log_path = data_dir.join("hoo-browser.log");
        let sock_dir = data_dir.join("ab-sockets");
        let _ = std::fs::create_dir_all(&sock_dir);
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                agent_browser,
                cdp,
                data_dir,
                log_path,
                sock_dir,
                chromium: Mutex::new(None),
            }),
        }
    }

    /// Hand the shared Chromium to the helper so shutdown kills it.
    pub async fn set_chromium(&self, child: tokio::process::Child) {
        *self.inner.chromium.lock().await = Some(child);
    }

    /// Kill the shared Chromium, best effort. Runs before every
    /// `process::exit`: destructors do not run there.
    pub async fn shutdown(&self) {
        if let Some(mut child) = self.inner.chromium.lock().await.take() {
            let _ = child.kill().await;
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

impl Helper {
    /// Run one tool call for `agent`: resolve the lease, run agent-browser,
    /// log overhead. agent-browser failures come back as tool errors (isError),
    /// so the model sees them; refusals are protocol errors.
    async fn run_tool(
        &self,
        agent: &AgentRecord,
        tool: &str,
        args: &Value,
    ) -> Result<Vec<ContentBlock>, HooError> {
        if tool == "browser_cli" {
            return self.run_cli(agent, args).await;
        }
        if tool == "browser_tabs" {
            return self.run_tabs(agent, args).await;
        }
        let tab = args.get("tab").and_then(Value::as_u64).unwrap_or(1) as usize;
        let (session, fresh) = self.inner.state.lock().await.ensure(&agent.agent_id, tab)?;
        if fresh {
            self.open_window(&session).await?;
        }
        let started = std::time::Instant::now();
        let (child, output) = if tool == "browser_screenshot" {
            self.run_screenshot(&session, args).await
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
            let data = exec::run_command(&argv, CALL_TIMEOUT, &self.inner.sock_dir).await;
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
        args: &Value,
    ) -> Result<Vec<ContentBlock>, HooError> {
        let started = std::time::Instant::now();
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
        let (default_session, owned, fresh) = {
            let mut state = self.inner.state.lock().await;
            let (session, fresh) = state.ensure(&agent.agent_id, 1)?;
            (session, state.owned(&agent.agent_id), fresh)
        };
        if fresh {
            self.open_window(&default_session).await?;
        }
        let scoped = exec::scope_cli_args(&raw, &self.inner.cdp, &default_session, &owned)?;
        let mut full = vec![self.inner.agent_browser.to_string_lossy().into_owned()];
        full.extend(scoped);
        let child_started = std::time::Instant::now();
        let text = self.run_raw(&full).await;
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
                ok: error.is_none(),
                error: error.as_deref(),
            },
        )
        .await;
        text.map(|body| vec![ContentBlock::text(body)])
    }

    /// Give a fresh lease its own window on the shared browser.
    async fn open_window(&self, session: &str) -> Result<(), HooError> {
        let argv = vec![
            self.inner.agent_browser.to_string_lossy().into_owned(),
            "--cdp".into(),
            self.inner.cdp.clone(),
            "--session".into(),
            session.into(),
            "--json".into(),
            "window".into(),
            "new".into(),
        ];
        exec::run_command(&argv, CALL_TIMEOUT, &self.inner.sock_dir).await?;
        Ok(())
    }

    async fn run_screenshot(
        &self,
        session: &str,
        args: &Value,
    ) -> (std::time::Duration, Result<ExecOutput, HooError>) {
        let _ = std::fs::create_dir_all(self.inner.data_dir.join("shots"));
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
        let result = exec::run_command(&argv, CALL_TIMEOUT, &self.inner.sock_dir).await;
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
    async fn run_raw(&self, scoped: &[String]) -> Result<String, HooError> {
        use tokio::process::Command;
        let (program, args) = scoped
            .split_first()
            .ok_or_else(|| HooError::Spawn("empty argv".into()))?;
        let output = tokio::time::timeout(
            CALL_TIMEOUT,
            Command::new(program)
                .args(args)
                .env("AGENT_BROWSER_SOCKET_DIR", &self.inner.sock_dir)
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| {
            HooError::Spawn(format!(
                "agent-browser timed out after {}s",
                CALL_TIMEOUT.as_secs()
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
        args: &Value,
    ) -> Result<Vec<ContentBlock>, HooError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        let started = std::time::Instant::now();
        let output: Result<Value, HooError> = match action {
            "list" => {
                let owned = self.inner.state.lock().await.owned(&agent.agent_id);
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
                    match exec::run_command(&argv, CALL_TIMEOUT, &self.inner.sock_dir).await {
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
                let tab = args.get("tab").and_then(Value::as_u64).unwrap_or(1) as usize;
                let session = self
                    .inner
                    .state
                    .lock()
                    .await
                    .resolve(&agent.agent_id, tab)?;
                let argv = exec::tool_argv(
                    &self.inner.agent_browser,
                    &self.inner.cdp,
                    &session,
                    "browser_tabs",
                    &serde_json::json!({"action": "close"}),
                    None,
                )?;
                exec::run_command(&argv, CALL_TIMEOUT, &self.inner.sock_dir).await?;
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
                child: total,
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
    async fn close_session(&self, session: &str) {
        let argv = vec![
            self.inner.agent_browser.to_string_lossy().into_owned(),
            "--cdp".into(),
            self.inner.cdp.clone(),
            "--session".into(),
            session.into(),
            "--json".into(),
            "close".into(),
        ];
        let _ = exec::run_command(&argv, CALL_TIMEOUT, &self.inner.sock_dir).await;
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
        match self.run_tool(&agent, name, &args).await {
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
                let sessions = self.inner.state.lock().await.remove_agent(agent_id);
                for session in sessions {
                    self.close_session(&session).await;
                }
            }
            "hoocode/sessionEnding" => {
                self.shutdown().await;
                std::process::exit(0);
            }
            _ => {}
        }
    }
}

/// Idle sweeper: every minute, close leases idle over the limit.
pub async fn sweep_loop(helper: Helper) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        interval.tick().await;
        let idle = {
            let state = helper.inner.state.lock().await;
            state.idle(std::time::Instant::now())
        };
        for session in idle {
            helper.close_session(&session).await;
            let mut state = helper.inner.state.lock().await;
            let _ = state.close("root", &session);
        }
    }
}
