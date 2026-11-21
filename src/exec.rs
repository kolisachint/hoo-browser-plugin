//! agent-browser invocation: arg mapping, spawn, output parsing, overhead.
//!
//! One tool call is one CLI invocation with `--json`. The helper's own
//! overhead is measured as whole-call time minus the child runtime and logged
//! per call at debug level to the log file (never with tokens).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::process::Command;

use crate::error::{short_stderr, HooError};

/// Per-call timeout for the agent-browser child, under hoocode's 60 s
/// tool budget (M1). `browser_wait` is capped below this (see MAX_WAIT_MS).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(45);

/// Total budget for one tool call (M1): acquire-wait + window new + the
/// command share it, so a call can never run 3x the per-spawn timeout.
/// Under hoocode's 60 s tool budget with headroom.
pub const CALL_BUDGET: Duration = Duration::from_secs(50);

/// Timeout for one spawn, bounded by the call budget: the smaller of the
/// per-spawn cap and the time left on the deadline. Zero remaining means
/// the budget is spent — fail without spawning.
pub fn spawn_timeout(deadline: Instant) -> Result<Duration, HooError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let timeout = remaining.min(call_timeout());
    if timeout.is_zero() {
        return Err(HooError::Spawn(
            "agent-browser call budget exhausted".into(),
        ));
    }
    Ok(timeout)
}

/// Effective per-call timeout. `HOO_BROWSER_CALL_TIMEOUT_SECS` overrides it;
/// that hook exists for the timeout test (a 45 s test would be absurd) and
/// is never set in production.
pub fn call_timeout() -> Duration {
    std::env::var("HOO_BROWSER_CALL_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .map(Duration::from_secs)
        .unwrap_or(CALL_TIMEOUT)
}

/// `browser_wait` ceiling in ms: the whole CLI run (spawn, connect, wait)
/// must fit inside [`CALL_TIMEOUT`].
pub const MAX_WAIT_MS: u64 = 40_000;

/// Daemon idle shutdown for agent-browser children, matching the helper's
/// 10-minute lease sweep (H3): an idle daemon exits instead of lingering.
pub const IDLE_TIMEOUT_MS: &str = "600000";

/// Children must never inherit secrets or ambient overrides (L1): the helper
/// and agent tokens stay in the helper, and every `AGENT_BROWSER_*` var is
/// dropped except the ones the helper sets itself (socket isolation, the
/// idle timeout below, and the helper-owned config from N6).
pub fn child_env(
    cmd: &mut tokio::process::Command,
    sock_dir: Option<&std::path::Path>,
    agent_browser_config: Option<&std::path::Path>,
) {
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
    // N6/B6: point every child at the helper-owned config so the user's
    // `~/.agent-browser/config.json` cannot change helper output
    // (e.g. screenshotFormat). The path is cached in the helper state
    // and created once at startup — spawning never stats or chmods.
    if let Some(path) = agent_browser_config {
        cmd.env("AGENT_BROWSER_CONFIG", path);
    }
}

/// Path of the helper-owned agent-browser config (N6): a file containing
/// `{}` in the runtime/socket dir, so ambient user config cannot change
/// helper output.
pub fn agent_browser_config_path(sock_dir: &Path) -> PathBuf {
    sock_dir.join("agent-browser-config.json")
}

/// Create the helper-owned config if missing: `{}` with owner-only
/// permissions. The socket dir itself is 0700 (main) or created here.
pub fn ensure_agent_browser_config(sock_dir: &Path) -> std::io::Result<PathBuf> {
    let path = agent_browser_config_path(sock_dir);
    if !path.is_file() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, "{}\n")?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// Prefix of the client-side connect error (agent-browser
/// `cli/src/connection.rs` `connect()`: `format!("Failed to connect: {e}")`,
/// plus the `"Failed to connect: daemon has not written its port"` case).
/// Only this prefix proves the command never reached the daemon: the CLI's
/// `connect()` failed before a single byte was written (cold-start daemon
/// race, N7: the CLI already respawns the daemon and retries once itself;
/// this covers the leftover race where that retry also met a starting
/// daemon). Everything else is excluded, even when it looks transient:
/// - page/model JS errors from `browser_eval` and the daemon's own
///   `"Failed to connect to CDP at …"`: different shape (exit 0, or no
///   colon after `connect`), so the prefix never matches them — retrying a
///   mutating command (click twice, open twice) could double-execute.
/// - `outcome_unknown` / `Failed to read` / `Failed to send`: the request
///   may have been written, so the daemon may have run it.
/// - timeouts and spawn errors: the CLI was killed mid-flight while the
///   daemon may still be running the command.
///
/// Callers retry at most once on true, never on false.
pub const CONNECT_ERROR_PREFIX: &str = "Failed to connect: ";

/// True when an agent-browser envelope `error` proves the command never
/// reached the daemon (client-side connect phase, see
/// [`CONNECT_ERROR_PREFIX`]). The `outcome_unknown` guard stays even
/// though a connect failure cannot produce one: a daemon-reported error
/// mentioning it must never retry.
pub fn is_connect_phase_error(message: &str) -> bool {
    message.starts_with(CONNECT_ERROR_PREFIX) && !message.contains("outcome_unknown")
}

/// The connect-phase failure in a `--json` envelope on a non-zero exit:
/// `Some(error)` only when the envelope reports failure, carries no
/// `outcome_unknown` code, and its `error` passes
/// [`is_connect_phase_error`]. Anything else (page errors, daemon errors,
/// non-JSON output) is `None`: it must surface, never retry.
pub fn envelope_connect_failure(envelope: &Value) -> Option<String> {
    if envelope.get("success") == Some(&Value::Bool(true)) {
        return None;
    }
    if envelope.get("code").and_then(Value::as_str) == Some("outcome_unknown") {
        return None;
    }
    let error = envelope.get("error").and_then(Value::as_str)?;
    is_connect_phase_error(error).then(|| error.to_owned())
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
    // Background leases: pinned tabs never take focus (upstream AB-1),
    // so every lease carries --pin-tab (H2).
    let mut argv = vec![
        agent_browser.to_string_lossy().into_owned(),
        "--cdp".into(),
        cdp.into(),
        "--session".into(),
        session.into(),
        "--json".into(),
        "--pin-tab".into(),
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
            // in its parser. `select` joins the rest literally with no `--`
            // handling and `press` takes its key literally, so a leading `-`
            // there can only be a forged flag: refuse it (M3).
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
                    refuse_dash("value", &value)?;
                    push(&mut argv, &["select", &reference, &value]);
                }
                "press" => {
                    // `press` takes only a key: prefer "value", fall back to "ref".
                    let key = str_arg("value").unwrap_or(reference);
                    refuse_dash("key", &key)?;
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
                // `--text` consumes the next token as its value: a leading
                // `-` would either misparse or smuggle a flag (M3).
                refuse_dash("text", &text)?;
                argv.extend(["--text".to_owned(), text]);
            } else if let Some(reference) = str_arg("ref").filter(|r| !r.is_empty()) {
                refuse_dash("ref", &reference)?;
                argv.push(reference);
            } else if let Some(load) = str_arg("load").filter(|l| !l.is_empty()) {
                refuse_dash("load", &load)?;
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
/// management (`session`, `state`, `tab` switching, `close`), provisioning
/// (`connect`, `install`, `upgrade`, `dashboard`), window management
/// (`window new`) and state files stay out of reach. Closing and windowing
/// go through the tools (`browser_tabs close`, tab leases), never the CLI:
/// a CLI `close` would desync the lease the helper still tracks.
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

/// Command-local flags verified against agent-browser's parser
/// (`cli/src/commands.rs`). `Some(true)` takes a value (the next token is
/// consumed as its value). Every other dash-token after the command is
/// refused — including `--json`, which clean_args strips anywhere and would
/// otherwise let `get --json cdp-url` slip past the subcommand position.
fn allowed_cli_flag(command: &str, flag: &str) -> Option<bool> {
    match command {
        "click" => match flag {
            "--human" | "--new-tab" => Some(false),
            "--button" | "--modifiers" => Some(true),
            _ => None,
        },
        "dblclick" => match flag {
            "--human" => Some(false),
            "--button" | "--modifiers" => Some(true),
            _ => None,
        },
        "type" => match flag {
            "--clear" => Some(false),
            "--delay" => Some(true),
            _ => None,
        },
        "snapshot" => match flag {
            "--interactive" | "-i" | "--compact" | "-c" | "--cursor" | "-C" | "--urls" | "-u"
            | "--delta" | "--full" => Some(false),
            "--depth" | "-d" | "--selector" | "-s" => Some(true),
            _ => None,
        },
        "screenshot" => match flag {
            "--full" | "-f" | "--if-changed" => Some(false),
            "--threshold" => Some(true),
            _ => None,
        },
        "wait" => match flag {
            "--download" | "-d" => Some(false),
            "--state" | "--timeout" | "--url" | "-u" | "--load" | "-l" | "--fn" | "-f"
            | "--text" | "-t" => Some(true),
            _ => None,
        },
        "eval" => match flag {
            "-b" | "--base64" | "--stdin" => Some(false),
            _ => None,
        },
        // open/goto/navigate/back/forward/reload/fill/select/hover/focus/
        // check/uncheck/press/key/get/tab take no flags upstream
        // (scan_single / parse_selector / positional-only arms reject them),
        // so none are allowed here either.
        _ => None,
    }
}

/// Scope raw CLI-fallback args to the caller's own sessions (spec B2): only
/// allow-listed commands run, and only inside the caller's own leases. A
/// `--session` naming another agent's session is refused; without one, the
/// caller's default lease is injected; `--cdp` is always the helper's (the
/// shared browser is not optional). Every global flag is refused anywhere
/// before `--` — leading or trailing. After the command, only the command's
/// own verified local flags pass (N2): agent-browser strips `--json` and
/// other globals before parsing, so `get --json cdp-url` would otherwise
/// read as `get cdp-url`. Past `--`, args are literal data and pass
/// through untouched, matching agent-browser's own parser.
pub fn scope_cli_args(
    raw: &[String],
    cdp: &str,
    default_session: &str,
    owned: &[String],
    scoped: bool,
) -> Result<Vec<String>, HooError> {
    // B3: fork tab scoping present
    // B3: safe mode refuses `tab` entirely — upstream `tab list` shows
    // the whole shared browser (other agents' URLs), and the helper's
    // own safe `browser_tabs` list is the only listing. Scoped mode
    // allows `tab list`: the fork scopes it to owned tabs.
    if raw.first().is_some_and(|first| first == "tab") && !scoped {
        return Err(HooError::Refused(
            "browser_cli: `tab` is not available in safe mode (this agent-browser is upstream, not the fork with tab scoping); use browser_tabs"
                .into(),
        ));
    }
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
    // Trailing flags: agent-browser strips globals anywhere on the line
    // before parsing, so only verified command-local flags may follow the
    // command (N2). `--session` stays leading-only (ownership-checked
    // there); values starting with `-` must go past `--`.
    let mut positional: Vec<&str> = Vec::new();
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
        if arg.starts_with('-') {
            if is_refused_global(arg) {
                return Err(refused_global(arg));
            }
            match allowed_cli_flag(&command, arg) {
                Some(takes_value) => {
                    // Consume the flag's value sight unseen: it belongs to
                    // the flag even when it starts with `-`.
                    if takes_value {
                        rest_iter.next();
                    }
                }
                None => {
                    return Err(HooError::Refused(format!(
                        "browser_cli: {arg:?} is not a {command} flag (--json and other flags are not allowed after the command)"
                    )));
                }
            }
            continue;
        }
        positional.push(arg);
    }
    check_command_rest(&command, positional.first().copied())?;

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

/// Per-command restrictions inside the caller's own session. Checks the
/// first positional, so flags can no longer hide the subcommand
/// (`get --json cdp-url` is refused for its `cdp-url`, not its `--json`).
fn check_command_rest(command: &str, first_positional: Option<&str>) -> Result<(), HooError> {
    // `get cdp-url` leaks the shared endpoint; the rest reads the page.
    if command == "get" && first_positional == Some("cdp-url") {
        return Err(HooError::Refused(
            "browser_cli: `get cdp-url` is not allowed (the shared browser is not optional)".into(),
        ));
    }
    // `tab <id>` would switch to another tab and `tab new/close` manage
    // tabs around the helper; only listing stays.
    if command == "tab" && first_positional.is_some_and(|s| s != "list") {
        return Err(HooError::Refused(
            "browser_cli: only `tab list` is allowed (use browser_tabs to close)".into(),
        ));
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
        "close" => "close your sessions with browser_tabs close",
        "window" => "windows are managed by the helper (one per tab lease)",
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
    agent_browser_config: &Path,
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
    child_env(&mut child, Some(sock_dir), Some(agent_browser_config));
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
        let raw_stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        // B1: only the client's own connect failure (non-zero exit *and*
        // a `--json` envelope whose error carries the connect() prefix)
        // proves the command never ran, so only it returns `NotSent`.
        // Page/model JS errors and the daemon's own "Failed to connect
        // to CDP at …" have a different shape and surface as-is: they
        // must never retry, or a mutating command could run twice. The
        // envelope parses from the raw stdout, before truncation below.
        if let Ok(envelope) = serde_json::from_str::<Value>(raw_stdout.trim()) {
            if let Some(error) = envelope_connect_failure(&envelope) {
                return Err(HooError::NotSent(error));
            }
        }
        let stdout = short_stderr(&raw_stdout);
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

/// One call's timing record for the overhead report: `total` is the whole
/// call (from the auth-layer entry stamp), `child` is agent-browser
/// spawn-to-exit, and `window_new` is the wall time of the `window new`
/// that gave a fresh lease its own window (zero when no window ran).
/// Helper-only overhead is `total − child − window_new` (M5).
pub struct CallRecord<'a> {
    pub tool: &'a str,
    pub agent: &'a str,
    pub session: &'a str,
    pub total: Duration,
    pub child: Duration,
    pub window_new: Duration,
    pub ok: bool,
    pub error: Option<&'a str>,
}

/// One JSON line per call for the overhead report (spec B2: debug level, log
/// file in the data dir, never tokens or argument values). Times are
/// microseconds from request entry: `child_us` is the agent-browser
/// spawn-to-exit (including its daemon connect), `window_new_us` is the
/// `window new` wall time for a fresh lease, and `overhead_us` is
/// everything else the helper adds (M5).
pub async fn log_call(log_path: &Path, record: CallRecord<'_>) {
    let line = serde_json::json!({
        "tool": record.tool,
        "agent": record.agent,
        "session": record.session,
        "total_us": record.total.as_micros(),
        "child_us": record.child.as_micros(),
        "window_new_us": record.window_new.as_micros(),
        "overhead_us": record
            .total
            .saturating_sub(record.child)
            .saturating_sub(record.window_new)
            .as_micros(),
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

    #[test]
    fn only_envelope_connect_failures_classify_not_sent() {
        // Cold-start race the helper may retry once: the CLI's connect()
        // failed before a byte was written, so the daemon got nothing.
        assert!(is_connect_phase_error(
            "Failed to connect: Connection refused (os error 61)"
        ));
        assert!(is_connect_phase_error(
            "Failed to connect: daemon has not written its port"
        ));
        // The daemon's own CDP dial failure has no colon after
        // `connect`: it must not look like a client connect failure.
        assert!(!is_connect_phase_error(
            "Failed to connect to CDP at ws://127.0.0.1:1: refused"
        ));
        // Post-send failures never retry: the command may have run.
        assert!(!is_connect_phase_error(
            "Failed to read: EAGAIN (outcome_unknown: the command may have run)"
        ));
        assert!(!is_connect_phase_error("Failed to read: EAGAIN"));
        assert!(!is_connect_phase_error("Failed to send: broken pipe"));
        assert!(!is_connect_phase_error("window new failed (injected)"));
    }

    #[test]
    fn envelope_classifier_needs_failure_shape() {
        let connect =
            json!({"success": false, "error": "Failed to connect: refused (os error 61)"});
        assert_eq!(
            envelope_connect_failure(&connect).as_deref(),
            Some("Failed to connect: refused (os error 61)")
        );
        // A success envelope never classifies, even with odd content.
        assert_eq!(
            envelope_connect_failure(&json!({"success": true, "data": {}})),
            None
        );
        // outcome_unknown code never classifies, whatever the text says.
        assert_eq!(
            envelope_connect_failure(&json!({
                "success": false,
                "error": "Failed to connect: refused",
                "code": "outcome_unknown",
            })),
            None
        );
        // Daemon errors (exit 0 path) never classify, even when the text
        // mentions connecting: they arrived *from* the daemon.
        assert_eq!(
            envelope_connect_failure(&json!({
                "success": false,
                "error": "Failed to connect to CDP at ws://x: refused",
            })),
            None
        );
        assert_eq!(envelope_connect_failure(&json!({"weird": true})), None);
    }

    /// A stand-in `agent-browser`: prints `body` on stdout, exits `code`.
    fn fake_cli(dir: &std::path::Path, name: &str, code: i32, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!("#!/bin/sh\nprintf '%s\\n' '{body}'\nexit {code}\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[tokio::test]
    async fn run_command_returns_not_sent_only_for_client_connect() {
        let dir = std::env::temp_dir().join(format!("hoo-b1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let timeout = std::time::Duration::from_secs(10);
        // Client connect failure (non-zero + envelope prefix): NotSent.
        let cli = fake_cli(
            &dir,
            "connect-fail.sh",
            1,
            r#"{"success":false,"data":null,"error":"Failed to connect: refused (os error 61)"}"#,
        );
        let error = run_command(
            &[cli.to_string_lossy().into_owned()],
            timeout,
            &dir,
            &dir.join("cfg.json"),
        )
        .await
        .expect_err("connect failure must error");
        assert!(
            matches!(error, HooError::NotSent(_)),
            "connect failure must be NotSent, got {error:?}"
        );
        // Daemon CDP failure (non-zero, no colon prefix): plain Failed.
        let cli = fake_cli(
            &dir,
            "cdp-fail.sh",
            1,
            r#"{"success":false,"data":null,"error":"Failed to connect to CDP at ws://x: refused"}"#,
        );
        let error = run_command(
            &[cli.to_string_lossy().into_owned()],
            timeout,
            &dir,
            &dir.join("cfg.json"),
        )
        .await
        .expect_err("cdp failure must error");
        assert!(
            matches!(error, HooError::Failed(_)),
            "cdp failure must be Failed, got {error:?}"
        );
        // Page/model JS error reported by the daemon (exit 0): Failed,
        // never NotSent, even though the text starts with the prefix.
        let cli = fake_cli(
            &dir,
            "page-fail.sh",
            0,
            r#"{"success":false,"data":null,"error":"Failed to connect: simulated page error"}"#,
        );
        let error = run_command(
            &[cli.to_string_lossy().into_owned()],
            timeout,
            &dir,
            &dir.join("cfg.json"),
        )
        .await
        .expect_err("page failure must error");
        assert!(
            matches!(error, HooError::Failed(_)),
            "page failure must be Failed, got {error:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        full[7..].to_vec()
    }

    #[test]
    fn globals_come_first() {
        let full = argv("browser_snapshot", json!({}));
        assert_eq!(
            &full[..7],
            &[
                "/bin/agent-browser",
                "--cdp",
                "ws://e",
                "--session",
                "root-1",
                "--json",
                "--pin-tab"
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
        assert_eq!(&full[7..], &["screenshot", "/tmp/shot-1.png"]);
        let full = tool_argv(
            Path::new("ab"),
            "e",
            "s",
            "browser_screenshot",
            &json!({"full_page": true}),
            Some(&path),
        )
        .unwrap();
        assert_eq!(&full[7..], &["screenshot", "--full", "/tmp/shot-1.png"]);
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
        assert!(CALL_BUDGET.as_secs() < 60);
        assert!(Duration::from_millis(MAX_WAIT_MS) < CALL_TIMEOUT);
        let plenty = Instant::now() + Duration::from_secs(3600);
        assert!(spawn_timeout(plenty).unwrap() <= CALL_TIMEOUT);
        let spent = Instant::now() - Duration::from_secs(1);
        assert!(spawn_timeout(spent).is_err());
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
        // Refs, urls, option values and keys that look like flags are
        // refused outright (M3). `select` has no `--` handling and `press`
        // takes its key literally, so a leading `-` there is never data.
        for (tool, args) in [
            ("browser_open", json!({"url": "--profile"})),
            ("browser_act", json!({"action": "click", "ref": "--json"})),
            (
                "browser_act",
                json!({"action": "fill", "ref": "-e", "value": "x"}),
            ),
            (
                "browser_act",
                json!({"action": "select", "ref": "e3", "value": "--x"}),
            ),
            (
                "browser_act",
                json!({"action": "press", "ref": "e1", "value": "--json"}),
            ),
            ("browser_wait", json!({"ref": "--state"})),
            ("browser_wait", json!({"text": "--foo"})),
            ("browser_wait", json!({"load": "--foo"})),
        ] {
            assert!(
                tool_argv(Path::new("ab"), "e", "s", tool, &args, None).is_err(),
                "{tool} {args} must be refused"
            );
        }
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
        assert_eq!(&full[7..9], &["eval", "-b"]);
        assert_eq!(
            full[9],
            base64::engine::general_purpose::STANDARD.encode("-1 + -2")
        );
    }

    #[test]
    fn cli_scoping_refuses_foreign_sessions() {
        let owned = vec!["root_task-1-1".to_owned()];
        // No --session: the default lease is injected, --cdp/--pin-tab first.
        let scoped = scope_cli_args(
            &["snapshot".to_owned()],
            "ws://e",
            "root_task-1-1",
            &owned,
            true,
        )
        .unwrap();
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
            true,
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
            true,
        )
        .is_err());
        assert!(scope_cli_args(
            &["--session=root-99".to_owned(), "snapshot".to_owned()],
            "ws://e",
            "root_task-1-1",
            &owned,
            true,
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
            true,
        )
        .is_err());
    }

    #[test]
    fn cli_scoping_tab_needs_scoped_mode() {
        let owned = vec!["root_task-1-1".to_owned()];
        // Scoped mode allows `tab list` (the fork scopes it to owned tabs).
        assert!(scope_cli_args(
            &["tab".to_owned(), "list".to_owned()],
            "ws://e",
            "root_task-1-1",
            &owned,
            true,
        )
        .is_ok());
        // Safe mode refuses even `tab list`: upstream shows the whole
        // shared browser through it.
        assert!(scope_cli_args(
            &["tab".to_owned(), "list".to_owned()],
            "ws://e",
            "root_task-1-1",
            &owned,
            false,
        )
        .is_err());
        // Safe mode refuses the mutating spellings too.
        assert!(scope_cli_args(
            &["tab".to_owned(), "close".to_owned()],
            "ws://e",
            "root_task-1-1",
            &owned,
            false,
        )
        .is_err());
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn cli_scoping_refuses_every_global_anywhere() {
        let owned = vec!["root_task-1-1".to_owned()];
        let scope =
            |raw: &[&str]| scope_cli_args(&args(raw), "ws://e", "root_task-1-1", &owned, true);
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
        // Leading --json stays allowed (output shape only); after the
        // command it is refused: agent-browser strips it before parsing,
        // so it would hide the real subcommand (N2).
        assert!(scope(&["--json", "snapshot"]).is_ok());
        assert!(scope(&["snapshot", "--json"]).is_err());
    }

    #[test]
    fn cli_scoping_refuses_flags_after_the_command() {
        let owned = vec!["root_task-1-1".to_owned()];
        let scope =
            |raw: &[&str]| scope_cli_args(&args(raw), "ws://e", "root_task-1-1", &owned, true);
        // The motivating bypass: --json stripped before parsing, leaving
        // `get cdp-url` to leak the shared endpoint.
        for raw in [
            vec!["get", "--json", "cdp-url"],
            vec!["get", "--session", "root_task-1-1", "cdp-url"],
            vec!["tab", "--json", "t2"],
            vec!["snapshot", "--session", "root_task-1-1"],
            vec!["open", "--json", "https://x.example/"],
            vec!["snapshot", "--bogus"],
            vec!["eval", "--json"],
            vec!["back", "--json"],
            vec!["fill", "e3", "--json"],
            vec!["type", "e3", "--clear", "--json"],
        ] {
            assert!(scope(&raw).is_err(), "{raw:?} must be refused");
        }
        // Verified command-local flags stay usable (mirrors the parser).
        for raw in [
            vec!["click", "e3", "--button", "left"],
            vec!["click", "e3", "--human", "--modifiers", "alt"],
            vec!["snapshot", "--interactive", "--depth", "3"],
            vec!["screenshot", "--full"],
            vec!["wait", "--text", "done"],
            vec!["wait", "--timeout", "5000"],
            vec!["type", "e3", "--clear"],
            vec!["eval", "-b", "aGk="],
            vec!["tab", "list"],
            vec!["get", "url"],
        ] {
            assert!(scope(&raw).is_ok(), "{raw:?} must pass");
        }
    }

    #[test]
    fn cli_scoping_is_an_allow_list() {
        let owned = vec!["root_task-1-1".to_owned()];
        let scope =
            |raw: &[&str]| scope_cli_args(&args(raw), "ws://e", "root_task-1-1", &owned, true);
        // Session management, provisioning, and state files are out.
        for raw in [
            vec!["session", "list"],
            vec!["state", "clear", "--all"],
            vec!["state", "list"],
            vec!["connect", "http://127.0.0.1:9999"],
            vec!["install"],
            vec!["upgrade"],
            vec!["dashboard"],
            vec!["close"],
            vec!["close", "--all"],
            vec!["tab", "t2"],
            vec!["tab", "new"],
            vec!["window", "new"],
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
            vec!["screenshot", "--full", "/tmp/shot.png"],
        ] {
            assert!(scope(&raw).is_ok(), "{raw:?} must pass");
        }
    }
}
