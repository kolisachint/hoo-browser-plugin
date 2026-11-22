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

/// Fork detection (B3): the hoo fork's releases MUST carry a `-hoo.N`
/// version (e.g. `0.43.1-hoo.1`), so `agent-browser --version` prints
/// `agent-browser 0.43.1-hoo.1`. Upstream prints `agent-browser 0.43.0`
/// (no `-hoo.`): the helper runs in safe mode there.
///
/// Two modes, chosen once at startup (see DESIGN.md):
/// - scoped: the fork scopes `--pin-tab` sessions' `tab list`/switch to
///   their owned tabs, so listings pass through and closes take every
///   listed target.
/// - safe: upstream agent-browser, whose `tab list` shows the whole
///   shared browser. Closes take only the lease's bound target recorded
///   at window open, `browser_tabs list` returns only the lease's own
///   bound entry, and `browser_cli tab` is refused.
pub fn version_is_scoped(version_output: &str) -> bool {
    version_output.contains("-hoo.")
}

/// Env override for tests: `HOO_BROWSER_AB_SCOPED=1` forces scoped mode,
/// `=0` forces safe mode. Anything else (or unset) probes the binary.
pub fn ab_scoped_from_env() -> Option<bool> {
    match std::env::var("HOO_BROWSER_AB_SCOPED")
        .ok()?
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

/// Chosen at startup (B3): the env override wins, else probe the
/// agent-browser binary's `--version`. Anything unparsable (missing
/// binary, timeout, odd output) means safe mode.
pub async fn probe_ab_scoped(agent_browser: &Path) -> bool {
    probe_ab_scoped_with(agent_browser, ab_scoped_from_env()).await
}

/// The version number from `agent-browser --version` (`0.43.0-hoo.1`),
/// `None` when the binary will not answer. Used for the one startup log
/// line; anything unparsable (missing binary, timeout, odd output) is
/// `None`, never an error.
pub async fn agent_browser_version(agent_browser: &Path) -> Option<String> {
    let output = tokio::process::Command::new(agent_browser)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let output = tokio::time::timeout(Duration::from_secs(10), output)
        .await
        .ok()?;
    let output = output.ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let version = line
        .trim()
        .strip_prefix("agent-browser")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| line.trim());
    if version.is_empty() {
        None
    } else {
        Some(version.to_owned())
    }
}

/// `probe_ab_scoped` with the override passed explicitly, so tests can
/// exercise both branches without touching process-global env (parallel
/// tests share it: `set_var` here would leak into other tests' helpers).
/// `None` probes the binary; `Some` short-circuits without spawning.
pub async fn probe_ab_scoped_with(agent_browser: &Path, forced: Option<bool>) -> bool {
    if let Some(forced) = forced {
        return forced;
    }
    let probe = tokio::process::Command::new(agent_browser)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = tokio::time::timeout(Duration::from_secs(10), probe).await else {
        return false;
    };
    matches!(output, Ok(output)
        if output.status.success()
            && version_is_scoped(&String::from_utf8_lossy(&output.stdout)))
}

/// Pinned agent-browser release (spec B3): the hoo fork
/// (`kolisachint/agent-browser`). The helper downloads the pinned asset for
/// the current platform on first need (helper start) into
/// `<data dir>/agent-browser/<tag>/agent-browser`, verifies its sha256,
/// and reuses it while the hash matches. `HOO_BROWSER_AGENT_BROWSER`
/// overrides the pin (local fork builds); `PATH` stays the fallback only
/// for platforms with no pin, which then run in safe mode (see DESIGN.md).
///
/// The fork's releases MUST carry a `-hoo.N` version (see
/// `version_is_scoped`): without it the helper cannot tell the fork from
/// upstream and stays in safe mode.
pub struct AgentBrowserPin {
    pub platform: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
}

/// The fork release the helper downloads. `SHA256SUMS` from the release's
/// assets, verified by hand before pinning.
pub const AGENT_BROWSER_PIN_TAG: &str = "v0.43.0-hoo.1";
pub const AGENT_BROWSER_PINS: &[AgentBrowserPin] = &[
    AgentBrowserPin {
        platform: "linux-x86_64",
        url: "https://github.com/kolisachint/agent-browser/releases/download/v0.43.0-hoo.1/agent-browser-linux-x86_64",
        sha256: "f6b8783cbe412e868c270b8380bee5c08e23e21301d0e6a0c9dd208190e06350",
    },
    AgentBrowserPin {
        platform: "linux-aarch64",
        url: "https://github.com/kolisachint/agent-browser/releases/download/v0.43.0-hoo.1/agent-browser-linux-aarch64",
        sha256: "f397f5f0c5019d1d48823d1b9c842a114085d51a8672f40cb58309a60ff131f3",
    },
    AgentBrowserPin {
        platform: "darwin-aarch64",
        url: "https://github.com/kolisachint/agent-browser/releases/download/v0.43.0-hoo.1/agent-browser-darwin-aarch64",
        sha256: "3dfc08dfae737cbd61c6f28f07b7e74e274b0f237073ccafef47fee406c7288a",
    },
    AgentBrowserPin {
        platform: "darwin-x86_64",
        url: "https://github.com/kolisachint/agent-browser/releases/download/v0.43.0-hoo.1/agent-browser-darwin-x86_64",
        sha256: "5065944658c1f44d3cf73f634c6d238c764395956ab6739b92118b3cc6e3dc5a",
    },
];

/// The pin for this build's platform, or `None` where the fork publishes
/// no asset (`platform` matches the release asset suffix).
pub fn pin_for_current_platform() -> Option<&'static AgentBrowserPin> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        _ => return None,
    };
    AGENT_BROWSER_PINS
        .iter()
        .find(|pin| pin.platform == platform)
}

/// Where the pinned binary lives:
/// `<data dir>/agent-browser/<tag>/agent-browser` (spec B3).
pub fn pinned_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join("agent-browser")
        .join(AGENT_BROWSER_PIN_TAG)
        .join("agent-browser")
}

/// Lowercase hex sha256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// True when `path` is a file whose sha256 matches `expected`.
fn file_matches_sha256(path: &Path, expected: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    sha256_hex(&bytes) == expected.to_ascii_lowercase()
}

/// Download `url` and check its sha256 against `expected` before returning
/// the bytes: a network failure, a non-2xx status, or a hash mismatch is
/// an error, never a file.
async fn fetch_verified(url: &str, expected: &str) -> Result<Vec<u8>, HooError> {
    // `Client::new` honors proxy env (`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`)
    // by default, so the download works behind the same proxy as curl.
    let response = reqwest::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| {
            HooError::NoAgentBrowser(format!(
                "cannot download the pinned agent-browser from {url}: {e}; check the network/proxy or set HOO_BROWSER_AGENT_BROWSER to a local binary"
            ))
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(HooError::NoAgentBrowser(format!(
            "cannot download the pinned agent-browser from {url}: HTTP {status}"
        )));
    }
    let bytes = response.bytes().await.map_err(|e| {
        HooError::NoAgentBrowser(format!(
            "cannot download the pinned agent-browser from {url}: {e}"
        ))
    })?;
    let actual = sha256_hex(&bytes);
    if actual != expected.to_ascii_lowercase() {
        return Err(HooError::NoAgentBrowser(format!(
            "pinned agent-browser from {url} failed sha256 verification: expected {expected}, got {actual}"
        )));
    }
    Ok(bytes.to_vec())
}

/// Install `bytes` at `dest`: unique temp file in the same dir, chmod
/// 0755, atomic rename. A concurrent helper installing the same verified
/// bytes wins or loses identically — the content is pinned by the hash —
/// so the file is never corrupt, only ever one whole verified binary.
fn install_executable(dest: &Path, bytes: &[u8]) -> Result<(), HooError> {
    use std::os::unix::fs::PermissionsExt as _;
    let parent = dest.parent().ok_or_else(|| {
        HooError::NoAgentBrowser(format!("pinned path {} has no parent", dest.display()))
    })?;
    ensure_dir_0700(parent)?;
    // Unique per process attempt: concurrent helpers never share a temp.
    let tmp = parent.join(format!(
        ".agent-browser-{}-{}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    let install = (|| {
        std::fs::write(&tmp, bytes)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, dest)?;
        Ok::<(), std::io::Error>(())
    })();
    if install.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    install.map_err(|e| {
        HooError::NoAgentBrowser(format!(
            "cannot install the pinned agent-browser at {}: {e}",
            dest.display()
        ))
    })
}

/// Reuse `dest` while its hash matches; else download `url`, verify, and
/// atomically install. A concurrent helper may install between our check
/// and our rename: re-check after the download and take their winner
/// instead of re-downloading.
async fn ensure_verified(dest: &Path, url: &str, expected: &str) -> Result<(), HooError> {
    if file_matches_sha256(dest, expected) {
        return Ok(());
    }
    let bytes = fetch_verified(url, expected).await?;
    if file_matches_sha256(dest, expected) {
        return Ok(());
    }
    install_executable(dest, &bytes)
}

/// Resolve the agent-browser binary, downloading the pin on first need:
/// `HOO_BROWSER_AGENT_BROWSER` env, else the verified pinned download for
/// this platform, else — only on platforms with no pin — the legacy
/// data-dir file or `PATH` (safe mode).
pub async fn ensure_agent_browser(data_dir: &Path) -> Result<PathBuf, HooError> {
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
    if let Some(pin) = pin_for_current_platform() {
        let dest = pinned_path(data_dir);
        ensure_verified(&dest, pin.url, pin.sha256).await?;
        return Ok(dest);
    }
    locate_agent_browser(data_dir)
}

/// Locate the agent-browser CLI without downloading: `HOO_BROWSER_AGENT_BROWSER`
/// env, else the pinned file in the plugin data dir (placed by
/// `ensure_agent_browser`), else the legacy data-dir file, else — only for
/// platforms with no pin — `PATH` (spec B2).
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
    let pinned = pinned_path(data_dir);
    if pinned.is_file() {
        return Ok(pinned);
    }
    let legacy = data_dir.join("agent-browser");
    if legacy.is_file() {
        return Ok(legacy);
    }
    // PATH stays the fallback only for platforms with no pin: where a pin
    // exists the binary must come from the verified download, never
    // silently from wherever PATH happens to point.
    if pin_for_current_platform().is_none() {
        if let Some(path) = which_on_path("agent-browser") {
            return Ok(path);
        }
    }
    Err(HooError::NoAgentBrowser(match pin_for_current_platform() {
        Some(pin) => format!(
            "no agent-browser CLI found: the pinned {} for {} is not downloaded yet ({}); it downloads on helper start, or set HOO_BROWSER_AGENT_BROWSER to a local binary",
            AGENT_BROWSER_PIN_TAG,
            pin.platform,
            pinned.display()
        ),
        None => format!(
            "no agent-browser CLI found: set HOO_BROWSER_AGENT_BROWSER, put the binary at {}, or add agent-browser to PATH (no pinned build for {}/{})",
            pinned.display(),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    }))
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
/// someone else. Pre-existing dirs are tightened, not just new ones. A
/// symlink is refused outright: `metadata` would follow it and check (or
/// chmod) the target, so `symlink_metadata` gates first.
pub fn ensure_dir_0700(path: &Path) -> Result<(), HooError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    if std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(HooError::Usage(format!(
            "{} is a symlink, refusing to use it as a data dir",
            path.display()
        )));
    }
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
        .truncate(false)
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
    crate::exec::child_env(&mut cmd, None, None);
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
    fn symlinked_dirs_are_refused() {
        let base = std::env::temp_dir().join(format!("hoo-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("real");
        std::fs::create_dir_all(&target).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // Even though the target is a fine directory, the symlink itself
        // is refused: metadata/chmod would follow it.
        assert!(super::ensure_dir_0700(&link).is_err());
        let _ = std::fs::remove_dir_all(&base);
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
        // Bash, not `sh`: on Ubuntu `sh` is dash, which rejects `exec -a`
        // and exits at once, leaving a zombie whose ps line no longer names
        // the profile (reads as Foreign and falls back instead of being
        // killed and taken). Both sides of the bash exec race name the
        // profile: before, the `bash -c` line itself; after, argv[0].
        let mut sleeper = std::process::Command::new("bash")
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

    #[test]
    fn pin_table_covers_four_platforms_with_real_sha256() {
        // The URLs and digests below were verified against the release's
        // SHA256SUMS before pinning; the helper keys scoped mode on the
        // `-hoo.` marker in `--version`.
        assert_eq!(AGENT_BROWSER_PIN_TAG, "v0.43.0-hoo.1");
        let mut platforms: Vec<&str> = AGENT_BROWSER_PINS.iter().map(|pin| pin.platform).collect();
        platforms.sort_unstable();
        assert_eq!(
            platforms,
            [
                "darwin-aarch64",
                "darwin-x86_64",
                "linux-aarch64",
                "linux-x86_64"
            ]
        );
        for pin in AGENT_BROWSER_PINS {
            assert!(pin.url.ends_with(pin.platform));
            assert!(pin.url.contains(AGENT_BROWSER_PIN_TAG));
            assert!(!pin.url.contains("TODO"), "pin URL still a placeholder");
            assert_eq!(pin.sha256.len(), 64, "pin sha256 not a digest");
            assert!(
                pin.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "pin sha256 not hex"
            );
        }
        assert!(version_is_scoped("agent-browser 0.43.0-hoo.1"));
        assert!(!version_is_scoped("agent-browser 0.43.0"));
    }

    #[test]
    fn pin_covers_this_machine() {
        // CI runs ubuntu + macos; both arches are pinned. Unknown platforms
        // get None and keep the PATH fallback (safe mode).
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64")
            | ("macos", "x86_64")
            | ("linux", "x86_64")
            | ("linux", "aarch64") => {
                let pin = pin_for_current_platform().expect("pin for this platform");
                assert!(pin.url.ends_with(pin.platform));
                assert_eq!(
                    pinned_path(Path::new("/data")),
                    Path::new("/data")
                        .join("agent-browser")
                        .join(AGENT_BROWSER_PIN_TAG)
                        .join("agent-browser")
                );
            }
            _ => assert!(pin_for_current_platform().is_none()),
        }
    }

    /// Minimal local HTTP server for the download tests: serves `body` with
    /// a `Content-Length` on every connection until aborted. Returns its
    /// address and the server task; `hits` counts accepted connections.
    async fn serve_bytes(
        body: Vec<u8>,
        hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server addr");
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                let hits = hits.clone();
                tokio::spawn(async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mut head = vec![0u8; 4096];
                    let _ = socket.read(&mut head).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        (addr, server)
    }

    fn hit_counter() -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0))
    }

    fn hit_count(hits: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> usize {
        hits.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn download_verifies_reuses_and_replaces_stale() {
        let body = b"fake-agent-browser-bytes".to_vec();
        let sha = sha256_hex(&body);
        let served = hit_counter();
        let (addr, server) = serve_bytes(body.clone(), served.clone()).await;
        let url = format!("http://{addr}/agent-browser-test");
        let dir = std::env::temp_dir().join(format!("hoo-pin-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dest = dir.join("agent-browser");

        ensure_verified(&dest, &url, &sha).await.expect("download");
        assert_eq!(std::fs::read(&dest).expect("read install"), body);
        assert_eq!(hit_count(&served), 1);
        // Reuse: the hash matches, so no second download.
        ensure_verified(&dest, &url, &sha).await.expect("reuse");
        assert_eq!(hit_count(&served), 1);
        // A stale file is replaced by the verified bytes.
        std::fs::write(&dest, b"stale").expect("corrupt install");
        ensure_verified(&dest, &url, &sha)
            .await
            .expect("replace stale");
        assert_eq!(std::fs::read(&dest).expect("read replacement"), body);
        assert_eq!(hit_count(&served), 2);
        // Installed executable: 0755.
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&dest)
                .expect("stat install")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn download_refuses_mismatch_and_leaves_no_file() {
        let body = b"fake-agent-browser-bytes".to_vec();
        let served = hit_counter();
        let (addr, server) = serve_bytes(body, served.clone()).await;
        let url = format!("http://{addr}/agent-browser-test");
        let dir = std::env::temp_dir().join(format!("hoo-pin-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dest = dir.join("agent-browser");

        let wrong = "0".repeat(64);
        let error = ensure_verified(&dest, &url, &wrong)
            .await
            .expect_err("mismatch must fail");
        assert!(error.to_string().contains("sha256 verification"), "{error}");
        assert!(!dest.exists(), "refused download must leave no file");
        // A stale file is never clobbered by bytes that fail verification.
        std::fs::create_dir_all(&dir).expect("seed dir");
        std::fs::write(&dest, b"stale").expect("seed stale");
        ensure_verified(&dest, &url, &wrong)
            .await
            .expect_err("mismatch must fail over stale");
        assert_eq!(std::fs::read(&dest).expect("stale intact"), b"stale");
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_downloads_converge_on_one_valid_file() {
        let body = b"fake-agent-browser-bytes".to_vec();
        let sha = sha256_hex(&body);
        let served = hit_counter();
        let (addr, server) = serve_bytes(body.clone(), served.clone()).await;
        let url = format!("http://{addr}/agent-browser-test");
        let dir = std::env::temp_dir().join(format!("hoo-pin-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dest = dir.join("agent-browser");

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (dest, url, sha) = (dest.clone(), url.clone(), sha.clone());
            tasks.push(tokio::spawn(async move {
                ensure_verified(&dest, &url, &sha).await
            }));
        }
        for task in tasks {
            task.await
                .expect("task panicked")
                .expect("concurrent ensure");
        }
        // Whoever won, the file is exactly the verified bytes.
        assert!(file_matches_sha256(&dest, &sha));
        assert_eq!(std::fs::read(&dest).expect("read winner"), body);
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real check: downloads the actual darwin-aarch64 asset and asserts
    /// the `-hoo.` fork marker that scoped mode keys on. Gated on
    /// `HOO_BROWSER_NET=1` (needs network) and on macOS/arm64 (the binary
    /// must execute); without the gate it passes vacuously.
    #[tokio::test]
    async fn net_downloads_darwin_arm64_pin_and_reports_hoo_version() {
        if std::env::var("HOO_BROWSER_NET").as_deref() != Ok("1") {
            return;
        }
        if std::env::consts::OS != "macos" || std::env::consts::ARCH != "aarch64" {
            return;
        }
        let pin = AGENT_BROWSER_PINS
            .iter()
            .find(|pin| pin.platform == "darwin-aarch64")
            .expect("darwin-aarch64 pin");
        let dir = std::env::temp_dir().join(format!("hoo-pin-net-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("net test dir");
        let dest = dir.join("agent-browser");
        ensure_verified(&dest, pin.url, pin.sha256)
            .await
            .expect("download the pinned fork asset");
        let output = tokio::process::Command::new(&dest)
            .arg("--version")
            .output()
            .await
            .expect("run the downloaded agent-browser");
        assert!(output.status.success(), "{output:?}");
        let version = String::from_utf8_lossy(&output.stdout);
        assert!(
            version.contains("0.43.0-hoo.1"),
            "unexpected --version: {version}"
        );
        assert!(version_is_scoped(&version));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
