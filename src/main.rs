//! `hoo-browser`: tier C native helper for the hoo-browser plugin.
//!
//! Two modes (spec B2):
//! - Helper mode: `hoo-browser <args> --parent-pid <pid>` with
//!   `HOOCODE_HELPER_SOCKET`, `HOOCODE_HELPER_TOKEN` (and `HOOCODE_AGENT_ID`)
//!   in the environment. Serves MCP Streamable HTTP on the socket, launches
//!   one shared Chromium, and drives it via the agent-browser CLI.
//! - CLI fallback: `hoo-browser cli <agent-browser args...>` (or bare
//!   `hoo-browser <args>` outside helper mode) calls the hidden `browser_cli`
//!   tool on the live helper, scoped to the caller's sessions.

use std::path::{Path, PathBuf};

use hoo_browser::backend;
use hoo_browser::client;
use hoo_browser::error::HooError;
use hoo_browser::server;

/// True while the parent process still exists (kill with signal 0 checks).
#[cfg(unix)]
fn parent_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks, it sends nothing.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    let error = std::io::Error::last_os_error();
    // EPERM: a process exists that we may not signal. Anything else (ESRCH,
    // EINVAL) means it is gone.
    error.raw_os_error() == Some(libc::EPERM)
}

fn env_required(name: &str) -> Result<String, HooError> {
    std::env::var(name).map_err(|_| {
        HooError::Usage(format!(
            "{name} is required (set by the hoocode supervisor)"
        ))
    })
}

/// The plugin data dir: `HOO_BROWSER_DATA` (from `mcp.json` env
/// `${PLUGIN_DATA}`), else `PLUGIN_DATA`, else the temp dir.
/// The plugin data dir: `HOO_BROWSER_DATA` (from `mcp.json` env
/// `${PLUGIN_DATA}`), else `PLUGIN_DATA`, else the per-session runtime dir
/// (the helper socket's parent, removed by hoocode at session end). No
/// `/tmp` fallback (M4): a predictable shared dir would leak browsing state
/// across sessions and users.
fn data_dir(runtime: &Path) -> PathBuf {
    for name in ["HOO_BROWSER_DATA", "PLUGIN_DATA"] {
        if let Ok(dir) = std::env::var(name) {
            let dir = PathBuf::from(dir);
            if !dir.as_os_str().is_empty() {
                return dir;
            }
        }
    }
    runtime.to_owned()
}

/// Split off `--parent-pid <pid>`; the rest are mcp.json args (ignored).
fn parse_args(args: &[String]) -> Result<(Option<u32>, Vec<String>), HooError> {
    let mut parent_pid = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--parent-pid" {
            let pid: u32 = args.get(i + 1).map_or_else(
                || Err(HooError::Usage("--parent-pid needs a value".into())),
                |v| {
                    v.parse()
                        .map_err(|_| HooError::Usage("--parent-pid needs a numeric value".into()))
                },
            )?;
            parent_pid = Some(pid);
            i += 2;
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }
    Ok((parent_pid, rest))
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("hoo-browser: {error}");
        std::process::exit(error.exit_code());
    }
}

async fn run() -> Result<(), HooError> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (parent_pid, rest) = parse_args(&argv)?;
    let helper_mode = parent_pid.is_some() || std::env::var("HOOCODE_HELPER_SOCKET").is_ok();

    // CLI fallback: `hoo-browser cli ...`, or bare args outside helper mode.
    if rest.first().is_some_and(|first| first == "cli") {
        client::run_cli_fallback(&rest[1..]).await?;
        return Ok(());
    }
    if !helper_mode {
        if rest.is_empty() {
            return Err(HooError::Usage(
                "usage: hoo-browser [--parent-pid <pid>] | hoo-browser cli <agent-browser args...>"
                    .into(),
            ));
        }
        client::run_cli_fallback(&rest).await?;
        return Ok(());
    }

    let Some(parent_pid) = parent_pid else {
        return Err(HooError::Usage("--parent-pid <pid> is required".into()));
    };
    let socket = PathBuf::from(env_required("HOOCODE_HELPER_SOCKET")?);
    let root_token = env_required("HOOCODE_HELPER_TOKEN")?;
    let root_agent = std::env::var("HOOCODE_AGENT_ID").unwrap_or_else(|_| "root".into());
    // The per-session runtime dir: the helper socket's parent, owned by
    // hoocode and removed at session end (C3). No /tmp fallback (M4).
    let runtime = socket
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_owned)
        .ok_or_else(|| HooError::Usage("HOOCODE_HELPER_SOCKET has no parent dir".into()))?;
    let data_dir = data_dir(&runtime);
    backend::ensure_dir_0700(&data_dir)?;
    // The daemon dir is per session and cleaned at startup, so a crashed
    // helper's sockets never shadow the new daemon (H3).
    let sock_dir = backend::socket_dir(&runtime);
    let _ = std::fs::remove_dir_all(&sock_dir);
    backend::ensure_dir_0700(&sock_dir)?;
    // N6: the helper-owned agent-browser config lives here (0600,
    // containing `{}`), so user config cannot change helper output.
    hoo_browser::exec::ensure_agent_browser_config(&sock_dir).map_err(|e| {
        HooError::Usage(format!(
            "cannot create the helper-owned agent-browser config: {e}"
        ))
    })?;

    let backend_kind = backend::Backend::from_env();
    backend_kind.check_supported()?;
    let agent_browser = backend::locate_agent_browser(&data_dir)?;
    let chrome = backend::locate_chromium()?;
    let headed = backend_kind == backend::Backend::ChromiumHeaded;

    let chromium = backend::launch_chromium(&chrome, &data_dir, &runtime, headed).await?;
    eprintln!(
        "hoo-browser: agent-browser={} chromium={} cdp={}",
        agent_browser.display(),
        chrome.display(),
        chromium.endpoint
    );
    // B3: scoped (fork agent-browser) or safe (upstream) mode, chosen
    // once at startup by probing the binary (`HOO_BROWSER_AB_SCOPED`
    // forces it for tests).
    let scoped = backend::probe_ab_scoped(&agent_browser).await;
    let helper = server::Helper::new(
        agent_browser,
        chromium.endpoint.clone(),
        data_dir,
        sock_dir,
        root_token,
        root_agent,
    );
    helper.set_scoped(scoped);
    if !scoped {
        eprintln!(
            "hoo-browser: safe mode — the agent-browser here is upstream, not the fork with tab scoping. \
             Lease closes take only the lease's bound tab, browser_tabs list shows only your own tabs, \
             and browser_cli tab is refused. For full isolation use the fork agent-browser (see README)."
        );
    }
    helper
        .set_chromium(chromium.child, chromium.profile_lock)
        .await;

    // The helper dies with its parent (spec B2): kill the browser tree
    // first, then close leases under a hard cap and exit fast (N1).
    // `process::exit` skips destructors, so `kill_on_drop` would not fire.
    let watcher = helper.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if !parent_alive(parent_pid) {
                watcher.shutdown_fast().await;
                std::process::exit(0);
            }
        }
    });

    // SIGTERM/SIGINT run the same fast shutdown (N1): the supervisor
    // kills the helper on uninstall/upgrade, and the default disposition
    // would kill only the helper, orphaning Chromium (stale profile lock)
    // and the leases' windows. Killpg takes the browser tree first, the
    // capped lease close keeps us under hoocode's ~2 s wait, and the exit
    // code stays 0 so a plain stop is not reported as a crash.
    let signaled = helper.clone();
    tokio::spawn(watch_signals(signaled));

    // Idle leases close themselves; the Chromium child dies with us.
    tokio::spawn(server::sweep_loop(helper.clone()));
    server::serve(helper, socket).await
}

/// Wait for SIGTERM or SIGINT, then run the sessionEnding fast shutdown
/// and exit 0. Installed alongside the parent-death watcher so every
/// supervisor-initiated stop cleans up the same way.
#[cfg(unix)]
async fn watch_signals(helper: server::Helper) {
    use tokio::signal::unix::{signal, SignalKind};
    let Ok(mut term) = signal(SignalKind::terminate()) else {
        return;
    };
    let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
        return;
    };
    tokio::select! {
        _ = term.recv() => {},
        _ = interrupt.recv() => {},
    }
    helper.shutdown_fast().await;
    std::process::exit(0);
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    #[test]
    fn parent_alive_sees_live_and_dead_pids() {
        // Our own pid is alive; a pid above any real pid_max (2^22) is not.
        // (u32::MAX would wrap to -1 as pid_t and signal everything, so a
        // large-but-valid pid is used instead.)
        assert!(parent_alive(std::process::id()));
        assert!(!parent_alive(2_000_000_000));
    }

    #[test]
    fn parent_pid_arg_parses_and_rejects() {
        assert_eq!(
            parse_args(&["--parent-pid".to_owned(), "42".to_owned()]).unwrap(),
            (Some(42), vec![])
        );
        assert!(parse_args(&["--parent-pid".to_owned()]).is_err());
        assert!(parse_args(&["--parent-pid".to_owned(), "nope".to_owned()]).is_err());
    }
}
