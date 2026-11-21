//! agent-browser invocation: arg mapping, spawn, output parsing, overhead.
//!
//! One tool call is one CLI invocation with `--json`. The helper's own
//! overhead is measured as whole-call time minus the child runtime and logged
//! per call at debug level to the log file (never with tokens).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

use crate::error::{short_stderr, HooError};

/// Default per-call timeout for the agent-browser child.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// What a tool call produced: plain JSON or a PNG for MCP image content.
#[derive(Debug)]
pub enum ExecOutput {
    Json(Value),
    Png(Vec<u8>),
}

/// Build the full agent-browser argv for a tool call (globals first, then the
/// command). Pure: unit-tested against the CLI's exact command shapes.
pub fn tool_argv(
    agent_browser: &Path,
    cdp: &str,
    session: &str,
    tool: &str,
    args: &Value,
    shot_path: Option<&Path>,
) -> Result<Vec<String>, HooError> {
    let mut argv = vec![
        agent_browser.to_string_lossy().into_owned(),
        "--cdp".into(),
        cdp.into(),
        "--session".into(),
        session.into(),
        "--json".into(),
    ];
    let str_arg = |name: &str| args.get(name).and_then(Value::as_str).map(str::to_owned);
    let bool_arg = |name: &str| args.get(name).and_then(Value::as_bool).unwrap_or(false);
    let push = |argv: &mut Vec<String>, parts: &[&str]| {
        argv.extend(parts.iter().map(|s| (*s).to_owned()));
    };
    match tool {
        "browser_open" => {
            let url = str_arg("url")
                .filter(|u| !u.is_empty())
                .ok_or_else(|| HooError::Failed("browser_open needs \"url\"".into()))?;
            push(&mut argv, &["open", &url]);
        }
        "browser_snapshot" => {
            push(&mut argv, &["snapshot"]);
            if bool_arg("interactive") {
                push(&mut argv, &["--interactive"]);
            }
        }
        "browser_act" => {
            let action = str_arg("action").unwrap_or_default();
            let reference = str_arg("ref")
                .filter(|r| !r.is_empty())
                .ok_or_else(|| HooError::Failed("browser_act needs \"ref\"".into()))?;
            match action.as_str() {
                "click" => push(&mut argv, &["click", &reference]),
                "hover" => push(&mut argv, &["hover", &reference]),
                "check" => push(&mut argv, &["check", &reference]),
                "uncheck" => push(&mut argv, &["uncheck", &reference]),
                "fill" => {
                    let value = str_arg("value").ok_or_else(|| {
                        HooError::Failed("browser_act fill needs \"value\"".into())
                    })?;
                    push(&mut argv, &["fill", &reference, &value]);
                }
                "select" => {
                    let value = str_arg("value").ok_or_else(|| {
                        HooError::Failed("browser_act select needs \"value\"".into())
                    })?;
                    push(&mut argv, &["select", &reference, &value]);
                }
                "press" => {
                    // `press` takes only a key: prefer "value", fall back to "ref".
                    let key = str_arg("value").unwrap_or(reference);
                    push(&mut argv, &["press", &key]);
                }
                _ => {
                    return Err(HooError::Failed(format!(
                        "browser_act action must be one of click|fill|select|check|uncheck|hover|press, got {action:?}"
                    )));
                }
            }
        }
        "browser_screenshot" => {
            push(&mut argv, &["screenshot"]);
            if bool_arg("full_page") {
                push(&mut argv, &["--full"]);
            }
            let path =
                shot_path.ok_or_else(|| HooError::Spawn("screenshot needs a file path".into()))?;
            argv.push(path.to_string_lossy().into_owned());
        }
        "browser_tabs" => {
            let action = str_arg("action").unwrap_or_default();
            match action.as_str() {
                "list" => push(&mut argv, &["tab", "list"]),
                // One lease is one tab: closing the tab closes the session.
                "close" => push(&mut argv, &["close"]),
                "adopt" => {
                    return Err(HooError::Failed(
                        "adopt is handled by the helper, not agent-browser".into(),
                    ));
                }
                _ => {
                    return Err(HooError::Failed(format!(
                        "browser_tabs action must be one of list|close|adopt, got {action:?}"
                    )));
                }
            }
        }
        "browser_wait" => {
            push(&mut argv, &["wait"]);
            if let Some(text) = str_arg("text").filter(|t| !t.is_empty()) {
                argv.extend(["--text".to_owned(), text]);
            } else if let Some(reference) = str_arg("ref").filter(|r| !r.is_empty()) {
                argv.push(reference);
            } else if let Some(load) = str_arg("load").filter(|l| !l.is_empty()) {
                argv.extend(["--load".to_owned(), load]);
            } else if let Some(ms) = args.get("ms").and_then(Value::as_u64) {
                argv.push(ms.to_string());
            } else {
                argv.push("1000".to_owned());
            }
        }
        "browser_nav" => {
            let action = str_arg("action").unwrap_or_default();
            match action.as_str() {
                "back" | "forward" | "reload" => argv.push(action),
                _ => {
                    return Err(HooError::Failed(format!(
                        "browser_nav action must be one of back|forward|reload, got {action:?}"
                    )));
                }
            }
        }
        "browser_eval" => {
            let js = str_arg("js")
                .filter(|j| !j.is_empty())
                .ok_or_else(|| HooError::Failed("browser_eval needs \"js\"".into()))?;
            push(&mut argv, &["eval", &js]);
        }
        _ => return Err(HooError::Failed(format!("unknown tool {tool}"))),
    }
    Ok(argv)
}

/// Scope raw CLI-fallback args to the caller's own sessions (spec B2): a
/// `--session` naming another agent's session is refused; without one, the
/// caller's default lease is injected; `--cdp` is always the helper's (the
/// shared browser is not optional). Only leading flags are scanned, matching
/// the CLI's own global-flag position.
pub fn scope_cli_args(
    raw: &[String],
    cdp: &str,
    default_session: &str,
    owned: &[String],
) -> Result<Vec<String>, HooError> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len() + 4);
    let mut i = 0;
    let mut saw_session = false;
    let mut saw_cdp = false;
    while i < raw.len() {
        let arg = &raw[i];
        if !arg.starts_with("--") || arg == "--" {
            break;
        }
        if arg == "--session" {
            let value = raw
                .get(i + 1)
                .cloned()
                .ok_or_else(|| HooError::Failed("browser_cli: --session needs a value".into()))?;
            check_session(&value, owned)?;
            saw_session = true;
            out.push(arg.clone());
            out.push(value);
            i += 2;
        } else if let Some(value) = arg.strip_prefix("--session=") {
            check_session(value, owned)?;
            saw_session = true;
            out.push(arg.clone());
            i += 1;
        } else if arg == "--cdp" || arg.starts_with("--cdp=") {
            saw_cdp = true;
            // Replaced below with the helper's endpoint.
            if arg == "--cdp" {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            out.push(arg.clone());
            // Flags with values pass through untouched (only --session/--cdp
            // are interpreted).
            i += 1;
        }
    }
    if saw_cdp {
        return Err(HooError::Refused(
            "browser_cli: --cdp is managed by the helper (the shared browser is not optional)"
                .into(),
        ));
    }
    let mut scoped = vec!["--cdp".to_owned(), cdp.to_owned()];
    if !saw_session {
        scoped.push("--session".to_owned());
        scoped.push(default_session.to_owned());
    }
    scoped.extend(out);
    scoped.extend(raw[i..].iter().cloned());
    Ok(scoped)
}

fn check_session(value: &str, owned: &[String]) -> Result<(), HooError> {
    if owned.iter().any(|s| s == value) {
        Ok(())
    } else {
        Err(HooError::Refused(format!(
            "browser_cli: session {value:?} is not yours"
        )))
    }
}

/// Run one agent-browser command; parse the `--json` envelope
/// (`{"success": true, "data": ...}` / `{"success": false, ...}`).
///
/// `sock_dir` isolates the daemon: every spawn gets
/// `AGENT_BROWSER_SOCKET_DIR=sock_dir`, so two helpers (or two test runs)
/// never share agent-browser sessions even when lease names match.
pub async fn run_command(
    argv: &[String],
    timeout: Duration,
    sock_dir: &Path,
) -> Result<Value, HooError> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| HooError::Spawn("empty argv".into()))?;
    let output = tokio::time::timeout(
        timeout,
        Command::new(program)
            .args(args)
            .env("AGENT_BROWSER_SOCKET_DIR", sock_dir)
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| {
        HooError::Spawn(format!(
            "agent-browser timed out after {}s",
            timeout.as_secs()
        ))
    })?
    .map_err(|e| HooError::Spawn(format!("cannot spawn agent-browser: {e}")))?;
    if !output.status.success() {
        // Failures can land on either stream: usage errors go to stderr,
        // `--json` errors to stdout. Report both so nothing is lost.
        let stderr = short_stderr(&String::from_utf8_lossy(&output.stderr));
        let stdout = short_stderr(&String::from_utf8_lossy(&output.stdout));
        let detail = match (stderr.is_empty(), stdout.is_empty()) {
            (false, false) => format!("stderr: {stderr} | stdout: {stdout}"),
            (false, true) => format!("stderr: {stderr}"),
            (true, false) => format!("stdout: {stdout}"),
            (true, true) => "(no output)".to_owned(),
        };
        return Err(HooError::Failed(format!(
            "agent-browser exited {}: {detail}",
            output.status
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: Value = serde_json::from_str(&stdout).map_err(|e| {
        HooError::Failed(format!(
            "agent-browser output is not JSON ({e}): {}",
            short_stderr(&stdout)
        ))
    })?;
    if envelope.get("success") == Some(&Value::Bool(true)) {
        Ok(envelope.get("data").cloned().unwrap_or(Value::Null))
    } else {
        let message = envelope
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown agent-browser error");
        Err(HooError::Failed(message.to_owned()))
    }
}

/// One call's timing record for the overhead report.
pub struct CallRecord<'a> {
    pub tool: &'a str,
    pub agent: &'a str,
    pub session: &'a str,
    pub total: Duration,
    pub child: Duration,
    pub ok: bool,
    pub error: Option<&'a str>,
}

/// One JSON line per call for the overhead report (spec B2: debug level, log
/// file in the data dir, never tokens or argument values).
pub async fn log_call(log_path: &Path, record: CallRecord<'_>) {
    let line = serde_json::json!({
        "tool": record.tool,
        "agent": record.agent,
        "session": record.session,
        "total_ms": record.total.as_millis(),
        "child_ms": record.child.as_millis(),
        "overhead_ms": record.total.saturating_sub(record.child).as_millis(),
        "ok": record.ok,
        "error": record.error.map(|e| e.chars().take(300).collect::<String>()),
    });
    let mut text = line.to_string();
    text.push('\n');
    if let Ok(mut file) = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .await
    {
        use tokio::io::AsyncWriteExt as _;
        let _ = file.write_all(text.as_bytes()).await;
    }
}

/// Unique shot path for screenshots.
pub fn shot_path(data_dir: &Path) -> PathBuf {
    data_dir.join(format!(
        "shots/shot-{}-{}.png",
        std::process::id(),
        rand::random::<u64>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn argv(tool: &str, args: Value) -> Vec<String> {
        tool_argv(
            Path::new("/bin/agent-browser"),
            "ws://e",
            "root-1",
            tool,
            &args,
            None,
        )
        .unwrap()
    }

    fn tail(tool: &str, args: Value) -> Vec<String> {
        let full = argv(tool, args);
        full[6..].to_vec()
    }

    #[test]
    fn globals_come_first() {
        let full = argv("browser_snapshot", json!({}));
        assert_eq!(
            &full[..6],
            &[
                "/bin/agent-browser",
                "--cdp",
                "ws://e",
                "--session",
                "root-1",
                "--json"
            ]
        );
    }

    #[test]
    fn open_snapshot_act_shapes() {
        assert_eq!(
            tail("browser_open", json!({"url": "https://x.example/"})),
            vec!["open", "https://x.example/"]
        );
        assert_eq!(tail("browser_snapshot", json!({})), vec!["snapshot"]);
        assert_eq!(
            tail("browser_snapshot", json!({"interactive": true})),
            vec!["snapshot", "--interactive"]
        );
        assert_eq!(
            tail("browser_act", json!({"action": "click", "ref": "e3"})),
            vec!["click", "e3"]
        );
        assert_eq!(
            tail(
                "browser_act",
                json!({"action": "fill", "ref": "e3", "value": "hi"})
            ),
            vec!["fill", "e3", "hi"]
        );
        assert_eq!(
            tail(
                "browser_act",
                json!({"action": "select", "ref": "e3", "value": "a"})
            ),
            vec!["select", "e3", "a"]
        );
        assert_eq!(
            tail("browser_act", json!({"action": "check", "ref": "e3"})),
            vec!["check", "e3"]
        );
        assert_eq!(
            tail("browser_act", json!({"action": "uncheck", "ref": "e3"})),
            vec!["uncheck", "e3"]
        );
        assert_eq!(
            tail("browser_act", json!({"action": "hover", "ref": "e3"})),
            vec!["hover", "e3"]
        );
        assert_eq!(
            tail(
                "browser_act",
                json!({"action": "press", "ref": "e1", "value": "Enter"})
            ),
            vec!["press", "Enter"]
        );
        assert!(tool_argv(
            Path::new("ab"),
            "e",
            "s",
            "browser_act",
            &json!({"action": "fill", "ref": "e1"}),
            None
        )
        .is_err());
        assert!(tool_argv(
            Path::new("ab"),
            "e",
            "s",
            "browser_act",
            &json!({"action": "bogus", "ref": "e1"}),
            None
        )
        .is_err());
    }

    #[test]
    fn screenshot_always_carries_a_path() {
        let path = PathBuf::from("/tmp/shot-1.png");
        let full = tool_argv(
            Path::new("ab"),
            "e",
            "s",
            "browser_screenshot",
            &json!({}),
            Some(&path),
        )
        .unwrap();
        assert_eq!(&full[6..], &["screenshot", "/tmp/shot-1.png"]);
        let full = tool_argv(
            Path::new("ab"),
            "e",
            "s",
            "browser_screenshot",
            &json!({"full_page": true}),
            Some(&path),
        )
        .unwrap();
        assert_eq!(&full[6..], &["screenshot", "--full", "/tmp/shot-1.png"]);
        assert!(tool_argv(
            Path::new("ab"),
            "e",
            "s",
            "browser_screenshot",
            &json!({}),
            None
        )
        .is_err());
    }

    #[test]
    fn wait_variants() {
        assert_eq!(tail("browser_wait", json!({})), vec!["wait", "1000"]);
        assert_eq!(
            tail("browser_wait", json!({"ms": 500})),
            vec!["wait", "500"]
        );
        assert_eq!(
            tail("browser_wait", json!({"text": "done"})),
            vec!["wait", "--text", "done"]
        );
        assert_eq!(
            tail("browser_wait", json!({"ref": "e2"})),
            vec!["wait", "e2"]
        );
        assert_eq!(
            tail("browser_wait", json!({"load": "networkidle"})),
            vec!["wait", "--load", "networkidle"]
        );
    }

    #[test]
    fn nav_eval_tabs_shapes() {
        assert_eq!(tail("browser_nav", json!({"action": "back"})), vec!["back"]);
        assert_eq!(
            tail("browser_nav", json!({"action": "reload"})),
            vec!["reload"]
        );
        assert!(tool_argv(
            Path::new("ab"),
            "e",
            "s",
            "browser_nav",
            &json!({"action": "sideways"}),
            None
        )
        .is_err());
        assert_eq!(
            tail("browser_eval", json!({"js": "1+1"})),
            vec!["eval", "1+1"]
        );
        assert_eq!(
            tail("browser_tabs", json!({"action": "list"})),
            vec!["tab", "list"]
        );
        assert_eq!(
            tail("browser_tabs", json!({"action": "close"})),
            vec!["close"]
        );
        assert!(tool_argv(
            Path::new("ab"),
            "e",
            "s",
            "browser_tabs",
            &json!({"action": "adopt"}),
            None
        )
        .is_err());
    }

    #[test]
    fn cli_scoping_refuses_foreign_sessions() {
        let owned = vec!["root_task-1-1".to_owned()];
        // No --session: the default lease is injected, --cdp first.
        let scoped =
            scope_cli_args(&["snapshot".to_owned()], "ws://e", "root_task-1-1", &owned).unwrap();
        assert_eq!(
            &scoped[..4],
            &["--cdp", "ws://e", "--session", "root_task-1-1"]
        );
        assert_eq!(scoped[4], "snapshot");
        // Own session passes through.
        let scoped = scope_cli_args(
            &[
                "--session".to_owned(),
                "root_task-1-1".to_owned(),
                "snapshot".to_owned(),
            ],
            "ws://e",
            "root_task-1-1",
            &owned,
        )
        .unwrap();
        assert!(scoped.contains(&"root_task-1-1".to_owned()));
        // Foreign sessions are refused, both spellings.
        assert!(scope_cli_args(
            &[
                "--session".to_owned(),
                "root-99".to_owned(),
                "snapshot".to_owned()
            ],
            "ws://e",
            "root_task-1-1",
            &owned,
        )
        .is_err());
        assert!(scope_cli_args(
            &["--session=root-99".to_owned(), "snapshot".to_owned()],
            "ws://e",
            "root_task-1-1",
            &owned,
        )
        .is_err());
        // A custom --cdp is refused: the shared browser is not optional.
        assert!(scope_cli_args(
            &[
                "--cdp".to_owned(),
                "ws://evil".to_owned(),
                "snapshot".to_owned()
            ],
            "ws://e",
            "root_task-1-1",
            &owned,
        )
        .is_err());
    }
}
