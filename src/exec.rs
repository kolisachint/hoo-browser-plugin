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

/// Per-call timeout for the agent-browser child, under hoocode's 60 s
/// tool budget (M1). `browser_wait` is capped below this (see MAX_WAIT_MS).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(45);

/// `browser_wait` ceiling in ms: the whole CLI run (spawn, connect, wait)
/// must fit inside [`CALL_TIMEOUT`].
pub const MAX_WAIT_MS: u64 = 40_000;

/// Daemon idle shutdown for agent-browser children, matching the helper's
/// 10-minute lease sweep (H3): an idle daemon exits instead of lingering.
pub const IDLE_TIMEOUT_MS: &str = "600000";

/// Children must never inherit secrets or ambient overrides (L1): the helper
/// and agent tokens stay in the helper, and every `AGENT_BROWSER_*` var is
/// dropped except the two the helper sets itself (socket isolation and the
/// idle timeout above).
pub fn child_env(cmd: &mut tokio::process::Command, sock_dir: Option<&std::path::Path>) {
    for (key, _) in std::env::vars() {
        if key == "HOOCODE_HELPER_TOKEN"
            || key == "HOOCODE_AGENT_TOKEN"
            || key.starts_with("AGENT_BROWSER_")
        {
            cmd.env_remove(&key);
        }
    }
    cmd.env("AGENT_BROWSER_IDLE_TIMEOUT_MS", IDLE_TIMEOUT_MS);
    if let Some(dir) = sock_dir {
        cmd.env("AGENT_BROWSER_SOCKET_DIR", dir);
    }
}

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
            refuse_dash("url", &url)?;
            // `open` takes one positional (`scan_single`): `--` ends option
            // parsing so the url is always data, never a flag.
            push(&mut argv, &["open", "--", &url]);
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
            // Refs come from snapshots (`e3`); a leading `-` would parse as
            // a flag in the CLI (`scan_single` rejects it), so refuse early.
            refuse_dash("ref", &reference)?;
            // `click`/`hover`/`check`/`uncheck` take one positional via
            // `scan_single`, which honors `--`; `fill` skips `--` explicitly
            // in its parser. `select` joins the rest literally and has no
            // `--` handling, so its value passes through as data already.
            match action.as_str() {
                "click" => push(&mut argv, &["click", "--", &reference]),
                "hover" => push(&mut argv, &["hover", "--", &reference]),
                "check" => push(&mut argv, &["check", "--", &reference]),
                "uncheck" => push(&mut argv, &["uncheck", "--", &reference]),
                "fill" => {
                    let value = str_arg("value").ok_or_else(|| {
                        HooError::Failed("browser_act fill needs \"value\"".into())
                    })?;
                    push(&mut argv, &["fill", &reference, "--", &value]);
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
                refuse_dash("ref", &reference)?;
                argv.push(reference);
            } else if let Some(load) = str_arg("load").filter(|l| !l.is_empty()) {
                argv.extend(["--load".to_owned(), load]);
            } else if let Some(ms) = args.get("ms").and_then(Value::as_u64) {
                if ms > MAX_WAIT_MS {
                    return Err(HooError::Failed(format!(
                        "browser_wait ms={ms} exceeds the {MAX_WAIT_MS} ms per-call cap; wait again instead"
                    )));
                }
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
            // `eval` reads its first token for `-b`/`--base64`/`--stdin`
            // and has no `--` handling: a script starting with `-` goes
            // over as base64 (`eval -b`) so it cannot misparse as a flag.
            if js.starts_with('-') {
                use base64::Engine as _;
                let encoded = base64::engine::general_purpose::STANDARD.encode(js.as_bytes());
                push(&mut argv, &["eval", "-b", &encoded]);
            } else {
                push(&mut argv, &["eval", &js]);
            }
        }
        _ => return Err(HooError::Failed(format!("unknown tool {tool}"))),
    }
    Ok(argv)
}

/// Commands the CLI fallback may run. Everything else is refused: the
/// helper owns the browser, the sessions, and the daemon, so session
/// management (`session`, `state`, `tab` switching, `close --all`),
/// provisioning (`connect`, `install`, `upgrade`, `dashboard`) and state
/// files stay out of reach.
const ALLOWED_CLI_COMMANDS: &[&str] = &[
    "open",
    "goto",
    "navigate",
    "back",
    "forward",
    "reload",
    "click",
    "dblclick",
    "fill",
    "type",
    "select",
    "check",
    "uncheck",
    "hover",
    "focus",
    "press",
    "key",
    "snapshot",
    "screenshot",
    "wait",
    "eval",
    "get",
    "tab",
    "window",
    "close",
];

/// Global flags agent-browser strips anywhere on the command line, mirrored
/// from its `cli/src/flags.rs` (`GLOBAL_FLAGS_WITH_VALUE`; `--session` and
/// `--cdp` get tailored errors below). The helper manages the browser, so
/// every one of these is refused wherever it appears before `--`.
const REFUSED_VALUE_GLOBALS: &[&str] = &[
    "--session",
    "--restore-save",
    "--restore-check-url",
    "--restore-check-fn",
    "--namespace",
    "--headers",
    "--executable-path",
    "--cdp",
    "--cdp-headers",
    "--extension",
    "--init-script",
    "--enable",
    "--profile",
    "--state",
    "--proxy",
    "--proxy-bypass",
    "--args",
    "--user-agent",
    "-p",
    "--provider",
    "--device",
    "--session-name",
    "--color-scheme",
    "--download-path",
    "--max-output",
    "--allowed-domains",
    "--capture",
    "--action-policy",
    "--confirm-actions",
    "--config",
    "--engine",
    "--input-mode",
    "--screenshot-dir",
    "--screenshot-quality",
    "--screenshot-format",
    "--idle-timeout",
    "--ca-cert",
    "--model",
];

/// Mirrored from agent-browser's `cli/src/flags.rs` (`GLOBAL_BOOL_FLAGS`,
/// minus `--json`, which only changes output shape and stays allowed).
/// `--restore` is stripped by the CLI even though it is not in that list,
/// and `--help`/`--version` never reach a browser; all refused here.
const REFUSED_BOOL_GLOBALS: &[&str] = &[
    "--headed",
    "--webgpu",
    "--no-webmcp",
    "--debug",
    "--ignore-https-errors",
    "--allow-file-access",
    "--hide-scrollbars",
    "--auto-connect",
    "--no-auto-connect",
    "--pin-tab",
    "--no-pin-tab",
    "--no-ca-cert",
    "--use-system-ca",
    "--annotate",
    "--content-boundaries",
    "--confirm-interactive",
    "--no-auto-dialog",
    "--relaunch",
    "-v",
    "--verbose",
    "-q",
    "--quiet",
    "--offline",
    "--quick",
    "--fix",
    "--restore",
    "-h",
    "--help",
    "-V",
    "--version",
];

/// True for a global flag the helper refuses (both spellings).
fn is_refused_global(arg: &str) -> bool {
    let name = arg.split('=').next().unwrap_or(arg);
    REFUSED_VALUE_GLOBALS.contains(&name) || REFUSED_BOOL_GLOBALS.contains(&name)
}

/// Scope raw CLI-fallback args to the caller's own sessions (spec B2): only
/// allow-listed commands run, and only inside the caller's own leases. A
/// `--session` naming another agent's session is refused; without one, the
/// caller's default lease is injected; `--cdp` is always the helper's (the
/// shared browser is not optional). Every global flag is refused anywhere
/// before `--` — leading or trailing — so `--profile`/`--config`/
/// `--executable-path` smuggled after the command cannot escape the shared
/// browser. Past `--`, args are literal data and pass through untouched,
/// matching agent-browser's own parser.
pub fn scope_cli_args(
    raw: &[String],
    cdp: &str,
    default_session: &str,
    owned: &[String],
) -> Result<Vec<String>, HooError> {
    // Split at the first `--`: everything past it is literal data.
    let end_of_options = raw.iter().position(|a| a == "--").unwrap_or(raw.len());
    let (head, tail) = raw.split_at(end_of_options);

    // Leading globals: `--json` passes, `--session` is checked, anything
    // else starting with `-` is refused before the command is even known.
    let mut out: Vec<String> = Vec::with_capacity(raw.len() + 4);
    let mut i = 0;
    let mut saw_session = false;
    while i < head.len() {
        let arg = &head[i];
        if arg == "--json" {
            out.push(arg.clone());
            i += 1;
        } else if arg == "--session" {
            let value = head
                .get(i + 1)
                .cloned()
                .ok_or_else(|| HooError::Refused("browser_cli: --session needs a value".into()))?;
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
        } else if arg.starts_with('-') {
            return Err(refused_global(arg));
        } else {
            break;
        }
    }
    let command = head.get(i).cloned().ok_or_else(|| {
        HooError::Refused("browser_cli: no command (usage: browser_cli {args: [...]})".into())
    })?;
    if !ALLOWED_CLI_COMMANDS.contains(&command.as_str()) {
        return Err(HooError::Refused(format!(
            "browser_cli: {command:?} is not allowed ({})",
            disallowed_hint(&command)
        )));
    }
    let rest = &head[i + 1..];
    check_command_rest(&command, rest)?;
    // Trailing globals: refused even here, since agent-browser strips them
    // anywhere on the line. Command values that happen to start with `-`
    // must go past `--` (e.g. `fill e3 -- -x`).
    let mut rest_iter = rest.iter().peekable();
    while let Some(arg) = rest_iter.next() {
        if arg == "--state" && command == "wait" {
            // `wait <selector> --state <visible|hidden|...>` is wait's own
            // element-state flag, not the global session-settings flag —
            // but only with one of its four values.
            match rest_iter.peek() {
                Some(value)
                    if ["visible", "hidden", "attached", "detached"].contains(&value.as_str()) =>
                {
                    rest_iter.next();
                }
                _ => {
                    return Err(HooError::Refused(
                        "browser_cli: --state outside `wait <selector> --state <visible|hidden|attached|detached>` is managed by the helper".into(),
                    ));
                }
            }
            continue;
        }
        if arg.starts_with('-') && is_refused_global(arg) {
            return Err(refused_global(arg));
        }
    }

    let mut scoped = vec!["--cdp".to_owned(), cdp.to_owned(), "--pin-tab".to_owned()];
    if !saw_session {
        scoped.push("--session".to_owned());
        scoped.push(default_session.to_owned());
    }
    scoped.extend(out);
    scoped.push(command);
    scoped.extend(rest.iter().cloned());
    scoped.extend(tail.iter().cloned());
    Ok(scoped)
}

/// Per-command restrictions inside the caller's own session.
fn check_command_rest(command: &str, rest: &[String]) -> Result<(), HooError> {
    match command {
        // `get cdp-url` leaks the shared endpoint; the rest reads the page.
        "get" => {
            if rest.first().is_some_and(|s| s == "cdp-url") {
                return Err(HooError::Refused(
                    "browser_cli: `get cdp-url` is not allowed (the shared browser is not optional)"
                        .into(),
                ));
            }
        }
        // `tab <id>` would switch to another tab and `tab new/close` manage
        // tabs around the helper; only listing stays.
        "tab" => {
            if rest.first().is_some_and(|s| s != "list") {
                return Err(HooError::Refused(
                    "browser_cli: only `tab list` is allowed (use browser_tabs to close)".into(),
                ));
            }
        }
        // `window` only builds new windows; keep it that way.
        "window" => {
            if !rest.first().is_some_and(|s| s == "new") {
                return Err(HooError::Refused(
                    "browser_cli: only `window new` is allowed".into(),
                ));
            }
        }
        // `close --all` would close every agent's sessions; a bare `close`
        // only ends the caller's own lease.
        "close" => {
            if rest.iter().any(|a| a.starts_with('-')) {
                return Err(HooError::Refused(
                    "browser_cli: `close --all` is not allowed (use browser_tabs close)".into(),
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

fn refused_global(arg: &str) -> HooError {
    let name = arg.split('=').next().unwrap_or(arg);
    if name == "--cdp" {
        return HooError::Refused(
            "browser_cli: --cdp is managed by the helper (the shared browser is not optional)"
                .into(),
        );
    }
    HooError::Refused(format!(
        "browser_cli: global flag {name:?} is managed by the helper"
    ))
}

fn disallowed_hint(command: &str) -> &'static str {
    match command {
        "session" => "sessions are managed by the helper",
        "state" => "state files are managed by the helper",
        "connect" | "install" | "upgrade" | "dashboard" | "doctor" | "profiles" => {
            "browser provisioning is managed by the helper"
        }
        _ => "not in the browser_cli allow-list",
    }
}

/// Values that would parse as CLI flags are refused: refs and urls
/// always travel past `--`, but a leading `-` there means the caller is
/// forging them rather than reading snapshots (M3).
fn refuse_dash(what: &str, value: &str) -> Result<(), HooError> {
    if value.starts_with('-') {
        return Err(HooError::Failed(format!(
            "{what} must not start with `-`: got {value:?}"
        )));
    }
    Ok(())
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
    let mut child = Command::new(program);
    child
        .args(args)
        .stdin(std::process::Stdio::null())
        // Timing out drops the future; without this the child keeps
        // running orphaned. Scrubbed env: no tokens leak (L1).
        .kill_on_drop(true);
    child_env(&mut child, Some(sock_dir));
    let output = tokio::time::timeout(timeout, child.output())
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
            vec!["open", "--", "https://x.example/"]
        );
        assert_eq!(tail("browser_snapshot", json!({})), vec!["snapshot"]);
        assert_eq!(
            tail("browser_snapshot", json!({"interactive": true})),
            vec!["snapshot", "--interactive"]
        );
        assert_eq!(
            tail("browser_act", json!({"action": "click", "ref": "e3"})),
            vec!["click", "--", "e3"]
        );
        assert_eq!(
            tail(
                "browser_act",
                json!({"action": "fill", "ref": "e3", "value": "hi"})
            ),
            vec!["fill", "e3", "--", "hi"]
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
            vec!["check", "--", "e3"]
        );
        assert_eq!(
            tail("browser_act", json!({"action": "uncheck", "ref": "e3"})),
            vec!["uncheck", "--", "e3"]
        );
        assert_eq!(
            tail("browser_act", json!({"action": "hover", "ref": "e3"})),
            vec!["hover", "--", "e3"]
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
    fn call_timeout_fits_hoocode_budget_and_wait_is_capped() {
        assert!(CALL_TIMEOUT.as_secs() < 60);
        assert!(Duration::from_millis(MAX_WAIT_MS) < CALL_TIMEOUT);
        assert!(tool_argv(
            Path::new("ab"),
            "e",
            "s",
            "browser_wait",
            &json!({"ms": 40_000}),
            None
        )
        .is_ok());
        assert!(tool_argv(
            Path::new("ab"),
            "e",
            "s",
            "browser_wait",
            &json!({"ms": 40_001}),
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
    fn values_travel_past_end_of_options() {
        use base64::Engine as _;
        // Refs and urls that look like flags are refused outright.
        for (tool, args) in [
            ("browser_open", json!({"url": "--profile"})),
            ("browser_act", json!({"action": "click", "ref": "--json"})),
            (
                "browser_act",
                json!({"action": "fill", "ref": "-e", "value": "x"}),
            ),
            ("browser_wait", json!({"ref": "--state"})),
        ] {
            assert!(
                tool_argv(Path::new("ab"), "e", "s", tool, &args, None).is_err(),
                "{tool} {args} must be refused"
            );
        }
        // `select` joins its value literally (no `--` handling upstream),
        // so a dash value is data, not a flag — but the ref is still checked.
        assert_eq!(
            tail(
                "browser_act",
                json!({"action": "select", "ref": "e3", "value": "--x"})
            ),
            vec!["select", "e3", "--x"]
        );
        // A script starting with `-` goes over as base64 (`eval -b`),
        // since `eval` has no `--` handling and reads `-b`/`--stdin` first.
        let full = tool_argv(
            Path::new("ab"),
            "e",
            "s",
            "browser_eval",
            &json!({"js": "-1 + -2"}),
            None,
        )
        .unwrap();
        assert_eq!(&full[6..8], &["eval", "-b"]);
        assert_eq!(
            full[8],
            base64::engine::general_purpose::STANDARD.encode("-1 + -2")
        );
    }

    #[test]
    fn cli_scoping_refuses_foreign_sessions() {
        let owned = vec!["root_task-1-1".to_owned()];
        // No --session: the default lease is injected, --cdp/--pin-tab first.
        let scoped =
            scope_cli_args(&["snapshot".to_owned()], "ws://e", "root_task-1-1", &owned).unwrap();
        assert_eq!(
            &scoped[..5],
            &["--cdp", "ws://e", "--pin-tab", "--session", "root_task-1-1"]
        );
        assert_eq!(scoped[5], "snapshot");
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

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn cli_scoping_refuses_every_global_anywhere() {
        let owned = vec!["root_task-1-1".to_owned()];
        let scope = |raw: &[&str]| scope_cli_args(&args(raw), "ws://e", "root_task-1-1", &owned);
        // Trailing globals are refused, not just leading ones.
        for raw in [
            vec!["snapshot", "--session", "root-99"],
            vec!["snapshot", "--session=root-99"],
            vec!["snapshot", "--cdp", "ws://evil"],
            vec!["snapshot", "--cdp=ws://evil"],
            vec!["snapshot", "--config", "/tmp/evil.json"],
            vec!["snapshot", "--profile", "/tmp/p"],
            vec!["snapshot", "--executable-path", "/tmp/evil-chrome"],
            vec!["snapshot", "--auto-connect", "false"],
            vec!["open", "https://x.example/", "--provider", "chrome"],
            vec!["snapshot", "--namespace", "other"],
            vec!["snapshot", "--args", "--no-sandbox"],
            vec!["snapshot", "--extension", "/tmp/evil"],
            vec!["snapshot", "--restore", "last"],
            vec!["wait", "e1", "--state", "/tmp/evil.json"],
        ] {
            assert!(scope(&raw).is_err(), "{raw:?} must be refused");
        }
        // `--state` is wait's own element-state flag: allowed there.
        assert!(scope(&["wait", "e1", "--state", "visible"]).is_ok());
        // Past `--`, tokens are literal data, matching agent-browser.
        let scoped = scope(&["fill", "e1", "--", "--session", "root-99"]).unwrap();
        assert!(scoped.contains(&"--".to_owned()));
        // --json stays allowed (output shape only), anywhere.
        assert!(scope(&["--json", "snapshot"]).is_ok());
        assert!(scope(&["snapshot", "--json"]).is_ok());
    }

    #[test]
    fn cli_scoping_is_an_allow_list() {
        let owned = vec!["root_task-1-1".to_owned()];
        let scope = |raw: &[&str]| scope_cli_args(&args(raw), "ws://e", "root_task-1-1", &owned);
        // Session management, provisioning, and state files are out.
        for raw in [
            vec!["session", "list"],
            vec!["state", "clear", "--all"],
            vec!["state", "list"],
            vec!["connect", "http://127.0.0.1:9999"],
            vec!["install"],
            vec!["upgrade"],
            vec!["dashboard"],
            vec!["close", "--all"],
            vec!["tab", "t2"],
            vec!["tab", "new"],
            vec!["window", "close"],
            vec!["get", "cdp-url"],
            vec!["batch"],
        ] {
            assert!(scope(&raw).is_err(), "{raw:?} must be refused");
        }
        // The in-session long tail stays usable.
        for raw in [
            vec!["open", "--", "https://x.example/"],
            vec!["snapshot", "--interactive"],
            vec!["click", "--", "e3"],
            vec!["fill", "e3", "hello"],
            vec!["wait", "--text", "done"],
            vec!["eval", "document.title"],
            vec!["get", "url"],
            vec!["tab", "list"],
            vec!["window", "new"],
            vec!["close"],
            vec!["screenshot", "--full", "/tmp/shot.png"],
        ] {
            assert!(scope(&raw).is_ok(), "{raw:?} must pass");
        }
    }
}
