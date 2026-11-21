//! Backend selection, Chromium launch, and agent-browser location.
//!
//! One shared browser per helper (spec B2): the helper launches Chromium
//! itself (headless by default), reads the DevTools endpoint, and passes it
//! to every agent-browser call via `--cdp`. The engine is always the
//! agent-browser CLI as an external program, never vendored.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};

use crate::error::HooError;

/// The browser backend (spec B2). Only Chromium headless/headed are
/// implemented; the rest return "not yet supported" (B4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    ChromiumHeadless,
    ChromiumHeaded,
    RealChrome,
    Lightpanda,
}

impl Backend {
    /// From `HOO_BROWSER_BACKEND` (default `chromium-headless`);
    /// `HOO_BROWSER_HEADED=1` also selects headed.
    pub fn from_env() -> Self {
        if std::env::var("HOO_BROWSER_HEADED")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        {
            return Self::ChromiumHeaded;
        }
        match std::env::var("HOO_BROWSER_BACKEND")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "chromium-headed" | "headed" => Self::ChromiumHeaded,
            "chrome" | "real-chrome" => Self::RealChrome,
            "lightpanda" => Self::Lightpanda,
            _ => Self::ChromiumHeadless,
        }
    }

    pub fn check_supported(self) -> Result<(), HooError> {
        match self {
            Self::ChromiumHeadless | Self::ChromiumHeaded => Ok(()),
            Self::RealChrome => Err(HooError::UnsupportedBackend(
                "backend \"chrome\" (real Chrome via the 144+ toggle) is not yet supported (B4)"
                    .into(),
            )),
            Self::Lightpanda => Err(HooError::UnsupportedBackend(
                "backend \"lightpanda\" is not yet supported (B4)".into(),
            )),
        }
    }
}

/// Pinned agent-browser release (spec B3). The pin points at the fork
/// (`kolisachint/agent-browser`); the fork has no release with AB-1..AB-4
/// yet, so the download stays unwired and the helper reports exactly where
/// to put the binary.
///
/// TODO(B3): cut the first fork release, fill `sha256` below from its
/// assets, wire the download. Until then use a local build via
/// `HOO_BROWSER_AGENT_BROWSER` (see README).
#[allow(dead_code)]
pub struct AgentBrowserPin {
    pub platform: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
}

/// TODO(B3): placeholder URLs until the first fork release exists.
pub const AGENT_BROWSER_PIN_TAG: &str = "fork-first-release";
#[allow(dead_code)]
pub const AGENT_BROWSER_PINS: &[AgentBrowserPin] = &[
    AgentBrowserPin {
        platform: "linux-x86_64",
        url: "https://github.com/kolisachint/agent-browser/releases/download/TODO-first-fork-release/agent-browser-linux-x86_64",
        sha256: "TODO",
    },
    AgentBrowserPin {
        platform: "linux-aarch64",
        url: "https://github.com/kolisachint/agent-browser/releases/download/TODO-first-fork-release/agent-browser-linux-aarch64",
        sha256: "TODO",
    },
    AgentBrowserPin {
        platform: "darwin-aarch64",
        url: "https://github.com/kolisachint/agent-browser/releases/download/TODO-first-fork-release/agent-browser-darwin-aarch64",
        sha256: "TODO",
    },
    AgentBrowserPin {
        platform: "darwin-x86_64",
        url: "https://github.com/kolisachint/agent-browser/releases/download/TODO-first-fork-release/agent-browser-darwin-x86_64",
        sha256: "TODO",
    },
];

/// Locate the agent-browser CLI: `HOO_BROWSER_AGENT_BROWSER` env, else the
/// pinned file in the plugin data dir, else PATH (spec B2).
pub fn locate_agent_browser(data_dir: &Path) -> Result<PathBuf, HooError> {
    if let Ok(path) = std::env::var("HOO_BROWSER_AGENT_BROWSER") {
        let path = PathBuf::from(&path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(HooError::NoAgentBrowser(format!(
            "HOO_BROWSER_AGENT_BROWSER={} is not a file",
            path.display()
        )));
    }
    let pinned = data_dir.join("agent-browser");
    if pinned.is_file() {
        return Ok(pinned);
    }
    if let Some(path) = which_on_path("agent-browser") {
        return Ok(path);
    }
    Err(HooError::NoAgentBrowser(format!(
        "no agent-browser CLI found: set HOO_BROWSER_AGENT_BROWSER, put the pinned binary at {}, or add agent-browser to PATH (pin: {AGENT_BROWSER_PIN_TAG}, download not wired yet, see backend.rs)",
        pinned.display()
    )))
}

fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Locate Chromium: `HOO_BROWSER_CHROME` env, else agent-browser's own install
/// (`~/.agent-browser/browsers/chrome-*/...`), else common paths (spec B2).
pub fn locate_chromium() -> Result<PathBuf, HooError> {
    if let Ok(path) = std::env::var("HOO_BROWSER_CHROME") {
        let path = PathBuf::from(&path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(HooError::NoChromium(format!(
            "HOO_BROWSER_CHROME={} is not a file",
            path.display()
        )));
    }
    if let Some(path) = agent_browser_chrome() {
        return Ok(path);
    }
    const COMMON: &[&str] = &[
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/opt/google/chrome/chrome",
    ];
    for candidate in COMMON {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(HooError::NoChromium(
        "no Chromium found: set HOO_BROWSER_CHROME, run `agent-browser install`, or install Chrome/Chromium".into(),
    ))
}

/// Mirror of agent-browser's own search: `~/.agent-browser/browsers/chrome-*`.
fn agent_browser_chrome() -> Option<PathBuf> {
    let home = std::env::home_dir()?;
    let dir = home.join(".agent-browser").join("browsers");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("chrome-"))
        })
        .collect();
    versions.sort();
    versions.reverse();
    for base in versions {
        for candidate in [
            base.join("chrome"),
            base.join("chrome-headless-shell"),
            base.join("Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"),
            base.join("chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"),
            base.join("chrome-linux64/chrome"),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Socket paths have an OS limit (macOS 104, Linux 108); hoocode's
/// supervisor keeps them under 100 bytes and falls back to a hash name.
/// Same rule here, two tiers: the daemon dir is `ab` under the per-session
/// runtime dir (the helper socket's parent, C3: one daemon per hoocode
/// session, removed with the session), unless a session socket would pass
/// the budget, then a stable hash of the runtime dir under the temp dir,
/// then under `/tmp` (macOS TMPDIR alone is ~50 bytes, so the temp dir is
/// not always short enough).
const MAX_SOCK_PATH: usize = 100;
/// Budget for `/` + session name + `.sock` on top of the daemon dir.
/// Worst lease name: 32-char id + `-` + tab + collision suffix.
const SESSION_BUDGET: usize = 1 + 40 + 5;

/// FNV-1a 64: stable across processes (unlike `DefaultHasher`), so a helper
/// restarting on the same data dir reuses its daemon dir.
fn fnv1a64(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn fits(dir: &Path) -> bool {
    dir.to_string_lossy().len() + SESSION_BUDGET <= MAX_SOCK_PATH
}

pub fn socket_dir(runtime_dir: &Path) -> PathBuf {
    let primary = runtime_dir.join("ab");
    if fits(&primary) {
        return primary;
    }
    let hash = format!("hoo-{:016x}", fnv1a64(&runtime_dir.to_string_lossy()));
    let fallback = std::env::temp_dir().join(&hash);
    if fits(&fallback) {
        return fallback;
    }
    // Unix-only helper: /tmp always exists and is short.
    PathBuf::from("/tmp").join(hash)
}

/// `mkdir -p` with 0700 and an owner check (M4): plugin-data dirs hold
/// browsing state, so they must neither be world-readable nor owned by
/// someone else. Pre-existing dirs are tightened, not just new ones.
pub fn ensure_dir_0700(path: &Path) -> Result<(), HooError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| HooError::Usage(format!("cannot create {}: {e}", path.display())))?;
    let uid = std::fs::metadata(path)
        .map_err(|e| HooError::Usage(format!("cannot stat {}: {e}", path.display())))?
        .uid();
    // SAFETY: getuid has no failure mode.
    if uid != unsafe { libc::getuid() } {
        return Err(HooError::Usage(format!(
            "{} is owned by uid {uid}, refusing to share it",
            path.display()
        )));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| HooError::Usage(format!("cannot chmod 0700 {}: {e}", path.display())))?;
    Ok(())
}

/// An exclusive, non-blocking `flock` on the profile lock file, held open
/// for the helper's lifetime (N4). The kernel releases it when the holder
/// dies, so stale locks are impossible — unlike the pid file this replaces,
/// a crashed helper never blocks the next one. The file content (helper +
/// Chromium pids) is observability only, never authority.
pub struct ProfileLock {
    _file: std::fs::File,
    path: PathBuf,
}

impl ProfileLock {
    /// Record pids in the lock file for `ps`-level debugging. Best effort:
    /// the lock itself is the open fd, not these bytes.
    pub fn note_pids(&self, helper_pid: u32, chromium_pid: Option<u32>) {
        use std::io::Write as _;
        let mut content = format!("helper={helper_pid}\n");
        if let Some(pid) = chromium_pid {
            content.push_str(&format!("chromium={pid}\n"));
        }
        if let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(&self.path) {
            use std::io::Seek as _;
            let _ = file.set_len(0);
            let _ = file.seek(std::io::SeekFrom::Start(0));
            let _ = file.write_all(content.as_bytes());
        }
    }
}

/// Try to lock the profile dir: `Ok(Some)` while we hold it, `Ok(None)`
/// when another live helper holds it (N4: `EWOULDBLOCK`, never stale).
/// The non-blocking take retries briefly: on a loaded machine the kernel
/// can report `EWOULDBLOCK` for a lock whose close is still being processed
/// (observed as flakes under parallel test IO). A live holder keeps its
/// lock for its whole lifetime, so a short window can never misclassify one
/// as free — it only absorbs the release race.
fn try_profile_lock(lock_path: &Path) -> Result<Option<ProfileLock>, HooError> {
    use std::os::unix::io::AsRawFd as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)
        .map_err(|e| HooError::Usage(format!("cannot open {}: {e}", lock_path.display())))?;
    let start = std::time::Instant::now();
    let held = loop {
        // SAFETY: flock on our own open fd only.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break true;
        }
        let busy = std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK);
        if !busy {
            return Err(HooError::Usage(format!(
                "cannot lock {}: {}",
                lock_path.display(),
                std::io::Error::last_os_error()
            )));
        }
        if start.elapsed() > std::time::Duration::from_millis(50) {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    if !held {
        return Ok(None);
    }
    let lock = ProfileLock {
        _file: file,
        path: lock_path.to_owned(),
    };
    lock.note_pids(std::process::id(), None);
    Ok(Some(lock))
}

/// What the profile's `SingletonLock` tells us (N1): Chromium writes
/// `<hostname>-<pid>` there while it runs.
#[derive(Debug)]
enum SingletonState {
    /// No lock file, garbage, or a dead pid: Chromium will take it itself.
    Free,
    /// A live process whose command line names our profile dir: a stale
    /// Chromium from a dead helper. Kill it and take the profile.
    Stale(libc::pid_t),
    /// A live process that is not ours: leave the profile alone.
    Foreign,
}

/// Command line of `pid`, via /proc (Linux) or `ps` (macOS fallback).
fn process_cmdline(pid: libc::pid_t) -> Option<String> {
    if let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        let text = String::from_utf8_lossy(&bytes).replace('\0', " ");
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    // `-ww`: never truncate; the profile path sits at the end of the line.
    std::process::Command::new("ps")
        .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|out| {
            if !out.status.success() {
                return None;
            }
            let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        })
}

fn process_live(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // EPERM: a process exists that we may not signal.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn singleton_state(profile: &Path) -> SingletonState {
    let text = match std::fs::read_to_string(profile.join("SingletonLock")) {
        Ok(text) => text,
        Err(_) => return SingletonState::Free,
    };
    let Some(pid) = text.trim().rsplit('-').next().and_then(|s| s.parse().ok()) else {
        return SingletonState::Free;
    };
    if !process_live(pid) {
        return SingletonState::Free;
    }
    match process_cmdline(pid) {
        Some(cmdline) if cmdline.contains(&*profile.to_string_lossy()) => {
            SingletonState::Stale(pid)
        }
        _ => SingletonState::Foreign,
    }
}

/// SIGKILL a stale Chromium and its tree (N1). Only uses `killpg` when the
/// target leads a group that is not ours (M9 group leaders); otherwise a
/// single `kill`, so we can never signal an unrelated group. Re-checks the
/// command line first: pids are recycled.
fn kill_stale_chromium(pid: libc::pid_t, profile: &Path) {
    let ours = match process_cmdline(pid) {
        Some(cmdline) if cmdline.contains(&*profile.to_string_lossy()) => true,
        _ => return,
    };
    if !ours {
        return;
    }
    // SAFETY: SIGKILL to a pid re-verified above.
    unsafe {
        let pgid = libc::getpgid(pid);
        if pgid == pid && pgid != libc::getpgrp() {
            libc::killpg(pid, libc::SIGKILL);
        } else {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    eprintln!(
        "hoo-browser: killed stale Chromium {pid} holding {}",
        profile.display()
    );
}

fn runtime_fallback(runtime_dir: &Path, data_dir: &Path) -> (PathBuf, Option<ProfileLock>) {
    eprintln!(
        "hoo-browser: profile {} is held by another helper; using a fresh profile in {}",
        data_dir.display(),
        runtime_dir.display()
    );
    let fresh = runtime_dir.join("profile");
    let _ = std::fs::remove_dir_all(&fresh);
    (fresh, None)
}

/// Pick the Chromium profile: `PLUGIN_DATA/profile` while we hold its
/// `flock`, else a fresh profile in the per-session runtime dir (C3). A
/// live flock holder keeps the data profile; a stale Chromium that outlived
/// its helper (flock already gone, `SingletonLock` live) is killed when its
/// command line names our profile dir, else we fall back too (N1).
fn select_profile(
    data_dir: &Path,
    runtime_dir: &Path,
) -> Result<(PathBuf, Option<ProfileLock>), HooError> {
    let lock_path = data_dir.join("profile.lock");
    let Some(lock) = try_profile_lock(&lock_path)? else {
        return Ok(runtime_fallback(runtime_dir, data_dir));
    };
    let profile = data_dir.join("profile");
    match singleton_state(&profile) {
        SingletonState::Free => Ok((profile, Some(lock))),
        SingletonState::Stale(pid) => {
            kill_stale_chromium(pid, &profile);
            Ok((profile, Some(lock)))
        }
        SingletonState::Foreign => {
            drop(lock);
            Ok(runtime_fallback(runtime_dir, data_dir))
        }
    }
}

/// A launched Chromium: the child plus its DevTools endpoint.
pub struct Chromium {
    /// Owned by the helper after `set_chromium`; killed on shutdown.
    pub child: tokio::process::Child,
    /// `ws://127.0.0.1:<port>/devtools/browser/...`, passed as `--cdp`.
    pub endpoint: String,
    /// Lock this helper holds for the data-dir profile, released on
    /// shutdown; `None` when we fell back to the runtime-dir profile.
    pub profile_lock: Option<ProfileLock>,
}

/// Launch Chromium headless (or headed) and wait for the DevTools endpoint
/// (spec B2). The browser gets its own process group so shutdown can kill
/// the whole tree, and its stderr drains to a log file after the endpoint
/// line (L5) instead of filling a dead pipe.
pub async fn launch_chromium(
    chrome: &Path,
    data_dir: &Path,
    runtime_dir: &Path,
    headed: bool,
) -> Result<Chromium, HooError> {
    let (profile, profile_lock) = select_profile(data_dir, runtime_dir)?;
    ensure_dir_0700(&profile)?;
    let mut cmd = tokio::process::Command::new(chrome);
    // The browser inherits nothing secret or overriding (L1).
    crate::exec::child_env(&mut cmd, None);
    // Its own process group: `killpg` in shutdown takes the whole tree.
    {
        use std::os::unix::process::CommandExt as _;
        cmd.as_std_mut().process_group(0);
    }
    if !headed {
        cmd.arg("--headless=new");
    }
    cmd.arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-gpu")
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Headed launches without `--headless` still need a display; on Linux the
    // caller provides Xvfb. Nothing to add here.
    let mut child = cmd
        .spawn()
        .map_err(|e| HooError::NoChromium(format!("cannot launch {}: {e}", chrome.display())))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| HooError::NoEndpoint("cannot read Chromium stderr".into()))?;
    let mut lines = BufReader::new(stderr).lines();
    let deadline = Instant::now() + Duration::from_secs(30);
    let endpoint = loop {
        if Instant::now() >= deadline {
            return Err(HooError::NoEndpoint(
                "timed out waiting for the DevTools endpoint".into(),
            ));
        }
        let next = tokio::time::timeout(deadline - Instant::now(), lines.next_line())
            .await
            .map_err(|_| {
                HooError::NoEndpoint("timed out waiting for the DevTools endpoint".into())
            })?
            .map_err(|e| HooError::NoEndpoint(format!("cannot read Chromium stderr: {e}")))?;
        let Some(line) = next else {
            break None;
        };
        if let Some(endpoint) = parse_devtools_line(&line) {
            break Some(endpoint);
        }
    };
    let Some(endpoint) = endpoint else {
        return Err(HooError::NoEndpoint(
            "Chromium exited without printing a DevTools endpoint".into(),
        ));
    };
    // Keep draining stderr to a log file: after the take above nobody reads
    // the pipe, and a full pipe would stall the browser (L5).
    let log_path = data_dir.join("chromium-stderr.log");
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt as _;
        let mut stderr = lines.into_inner();
        let mut buf = vec![0u8; 8192];
        let mut file: Option<tokio::fs::File> = None;
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if file.is_none() {
                        file = tokio::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&log_path)
                            .await
                            .ok();
                    }
                    if let Some(file) = file.as_mut() {
                        use tokio::io::AsyncWriteExt as _;
                        if file.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }
    });
    // Record the browser pid in the lock file for `ps`-level debugging
    // (N1): authority stays with the open fd, not these bytes.
    if let Some(lock) = &profile_lock {
        lock.note_pids(std::process::id(), child.id());
    }
    Ok(Chromium {
        child,
        endpoint,
        profile_lock,
    })
}

/// `DevTools listening on ws://127.0.0.1:9222/devtools/browser/...`.
pub fn parse_devtools_line(line: &str) -> Option<String> {
    const MARKER: &str = "DevTools listening on ";
    let at = line.find(MARKER)?;
    let endpoint = line[at + MARKER.len()..].trim().to_owned();
    if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        Some(endpoint)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devtools_line_parses() {
        let line = "[1:2:0101/000000.000:INFO:CONSOLE(0)] DevTools listening on ws://127.0.0.1:54321/devtools/browser/abc";
        assert_eq!(
            parse_devtools_line(line).as_deref(),
            Some("ws://127.0.0.1:54321/devtools/browser/abc")
        );
        assert_eq!(parse_devtools_line("noise"), None);
    }

    #[test]
    fn socket_dir_falls_back_to_a_short_hash() {
        let short = Path::new("/tmp/hoo");
        assert_eq!(socket_dir(short), short.join("ab"));
        let long =
            Path::new("/var/folders/rf/1zmzg09n0w99lgrmhyn_nv3r0000gn/T/hoo-browser-it-real-8360");
        let fallback = socket_dir(long);
        assert!(
            fits(&fallback),
            "fallback still too long: {}",
            fallback.display()
        );
        // Stable: the same data dir maps to the same daemon dir.
        assert_eq!(fallback, socket_dir(long));
    }

    #[test]
    fn dirs_are_0700_and_owned() {
        let dir = std::env::temp_dir().join(format!("hoo-0700-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        super::ensure_dir_0700(&dir.join("nested")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("nested"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "must be owner-only");
        // Pre-existing loose dirs are tightened too.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        super::ensure_dir_0700(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profile_lock_second_helper_falls_back_and_release_reacquires() {
        let base = std::env::temp_dir().join(format!("hoo-prof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("data");
        let runtime = base.join("runtime");
        std::fs::create_dir_all(&data).unwrap();
        // First helper takes the data profile.
        let (profile, lock) = super::select_profile(&data, &runtime).unwrap();
        assert_eq!(profile, data.join("profile"));
        assert!(lock.is_some());
        // A second holder (even in this process: flock conflicts per open
        // file description) falls back to the runtime profile.
        let (profile2, lock2) = super::select_profile(&data, &runtime).unwrap();
        assert_eq!(profile2, runtime.join("profile"));
        assert!(lock2.is_none());
        // Dropping the guard releases the lock: the next helper takes the
        // data profile again. No stale state survives a dead holder (N4).
        drop(lock);
        let (profile3, lock3) = super::select_profile(&data, &runtime).unwrap();
        assert_eq!(profile3, data.join("profile"));
        assert!(lock3.is_some());
        drop(lock3);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn singleton_dead_pid_is_ignored() {
        let base = std::env::temp_dir().join(format!("hoo-single-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("data");
        let runtime = base.join("runtime");
        std::fs::create_dir_all(&data).unwrap();
        // A SingletonLock from a dead Chromium never blocks the profile.
        std::fs::create_dir_all(data.join("profile")).unwrap();
        std::fs::write(
            data.join("profile").join("SingletonLock"),
            "host-2147483647\n",
        )
        .unwrap();
        let (profile, lock) = super::select_profile(&data, &runtime).unwrap();
        assert_eq!(profile, data.join("profile"));
        assert!(lock.is_some());
        // The lock file records pids for debugging.
        lock.as_ref().unwrap().note_pids(4242, Some(4343));
        let noted = std::fs::read_to_string(data.join("profile.lock")).unwrap();
        assert!(noted.contains("helper=4242") && noted.contains("chromium=4343"));
        drop(lock);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn singleton_foreign_process_falls_back() {
        let base = std::env::temp_dir().join(format!("hoo-foreign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("data");
        let runtime = base.join("runtime");
        std::fs::create_dir_all(&data).unwrap();
        // Our own pid is live but its command line cannot name this profile.
        let pid = std::process::id();
        std::fs::create_dir_all(data.join("profile")).unwrap();
        std::fs::write(
            data.join("profile").join("SingletonLock"),
            format!("host-{pid}\n"),
        )
        .unwrap();
        let (profile, lock) = super::select_profile(&data, &runtime).unwrap();
        assert_eq!(profile, runtime.join("profile"));
        assert!(lock.is_none());
        drop(lock);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn singleton_stale_ours_is_killed_and_taken() {
        let base = std::env::temp_dir().join(format!("hoo-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("data");
        let runtime = base.join("runtime");
        std::fs::create_dir_all(&data).unwrap();
        let profile = data.join("profile");
        std::fs::create_dir_all(&profile).unwrap();
        // A live sleeper whose command line names our profile: stale ours.
        // (`exec -a` keeps the marker in argv: plain `sh -c 'sleep 60 #…'`
        // execs sleep and the comment never reaches the final argv.)
        let mut sleeper = std::process::Command::new("sh")
            .args([
                "-c",
                &format!("exec -a stale-{} sleep 300", profile.display()),
            ])
            .spawn()
            .expect("sleeper");
        let pid = sleeper.id();
        std::fs::write(profile.join("SingletonLock"), format!("host-{pid}\n")).unwrap();
        let (taken, lock) = super::select_profile(&data, &runtime).unwrap();
        assert_eq!(taken, profile);
        assert!(lock.is_some());
        // The stale process is dead now (best-effort reap).
        for _ in 0..50 {
            if sleeper.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            sleeper.try_wait().unwrap().is_some(),
            "stale sleeper must be killed"
        );
        drop(lock);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn backend_from_env_defaults_headless() {
        assert!(Backend::ChromiumHeadless.check_supported().is_ok());
        assert!(Backend::ChromiumHeaded.check_supported().is_ok());
        assert!(Backend::RealChrome.check_supported().is_err());
        assert!(Backend::Lightpanda.check_supported().is_err());
    }
}
