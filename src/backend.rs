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

/// Pinned agent-browser release (spec B3). Until the fork
/// (`kolisachint/agent-browser`) has a release with AB-1..AB-4, the pin points
/// at the upstream release; asset names below must be confirmed against that
/// release before the download path is wired, so auto-download stays off and
/// the helper reports exactly where to put the binary.
///
/// TODO(B3): confirm upstream asset names, fill `sha256`, wire download.
#[allow(dead_code)]
pub struct AgentBrowserPin {
    pub platform: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
}

/// TODO(B3): placeholder URLs until the fork release exists.
pub const AGENT_BROWSER_PIN_TAG: &str = "upstream-latest";
#[allow(dead_code)]
pub const AGENT_BROWSER_PINS: &[AgentBrowserPin] = &[
    AgentBrowserPin {
        platform: "linux-x86_64",
        url: "https://github.com/vercel-labs/agent-browser/releases/latest/download/agent-browser-linux-x86_64",
        sha256: "TODO",
    },
    AgentBrowserPin {
        platform: "linux-aarch64",
        url: "https://github.com/vercel-labs/agent-browser/releases/latest/download/agent-browser-linux-aarch64",
        sha256: "TODO",
    },
    AgentBrowserPin {
        platform: "darwin-aarch64",
        url: "https://github.com/vercel-labs/agent-browser/releases/latest/download/agent-browser-darwin-aarch64",
        sha256: "TODO",
    },
    AgentBrowserPin {
        platform: "darwin-x86_64",
        url: "https://github.com/vercel-labs/agent-browser/releases/latest/download/agent-browser-darwin-x86_64",
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
/// Same rule here, two tiers: the daemon dir is `ab` under the data dir
/// unless a session socket would pass the budget, then a stable hash of the
/// data dir under the temp dir, then under `/tmp` (macOS TMPDIR alone is
/// ~50 bytes, so the temp dir is not always short enough).
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

pub fn socket_dir(data_dir: &Path) -> PathBuf {
    let primary = data_dir.join("ab");
    if fits(&primary) {
        return primary;
    }
    let hash = format!("hoo-{:016x}", fnv1a64(&data_dir.to_string_lossy()));
    let fallback = std::env::temp_dir().join(&hash);
    if fits(&fallback) {
        return fallback;
    }
    // Unix-only helper: /tmp always exists and is short.
    PathBuf::from("/tmp").join(hash)
}

/// A launched Chromium: the child plus its DevTools endpoint.
pub struct Chromium {
    /// Owned by the helper after `set_chromium`; killed on shutdown.
    pub child: tokio::process::Child,
    /// `ws://127.0.0.1:<port>/devtools/browser/...`, passed as `--cdp`.
    pub endpoint: String,
}

/// Launch Chromium headless (or headed) with a fresh profile under the data
/// dir and wait for the DevTools endpoint (spec B2).
pub async fn launch_chromium(
    chrome: &Path,
    data_dir: &Path,
    headed: bool,
) -> Result<Chromium, HooError> {
    let profile = data_dir.join("profile");
    let _ = std::fs::create_dir_all(&profile);
    let mut cmd = tokio::process::Command::new(chrome);
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
    while Instant::now() < deadline {
        let next = tokio::time::timeout(deadline - Instant::now(), lines.next_line())
            .await
            .map_err(|_| {
                HooError::NoEndpoint("timed out waiting for the DevTools endpoint".into())
            })?
            .map_err(|e| HooError::NoEndpoint(format!("cannot read Chromium stderr: {e}")))?;
        let Some(line) = next else {
            break;
        };
        if let Some(endpoint) = parse_devtools_line(&line) {
            return Ok(Chromium { child, endpoint });
        }
    }
    Err(HooError::NoEndpoint(
        "Chromium exited without printing a DevTools endpoint".into(),
    ))
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
    fn backend_from_env_defaults_headless() {
        assert!(Backend::ChromiumHeadless.check_supported().is_ok());
        assert!(Backend::ChromiumHeaded.check_supported().is_ok());
        assert!(Backend::RealChrome.check_supported().is_err());
        assert!(Backend::Lightpanda.check_supported().is_err());
    }
}
