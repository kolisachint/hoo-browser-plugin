//! Hand-written error type for `hoo-browser`. No `anyhow`/`thiserror` by rule.

use std::fmt;

/// Every failure the helper can report.
#[derive(Debug)]
pub enum HooError {
    /// CLI usage or environment is wrong; the process exits 2.
    Usage(String),
    /// The requested backend is known but not implemented yet (B4).
    UnsupportedBackend(String),
    /// No usable Chromium was found; tells the user what to set.
    NoChromium(String),
    /// Chromium launched but no DevTools endpoint appeared.
    NoEndpoint(String),
    /// No usable agent-browser CLI was found.
    NoAgentBrowser(String),
    /// The agent-browser run failed (spawn error or timeout).
    Spawn(String),
    /// An authorization rule refused the call (wrong agent, unknown token).
    /// Maps to an MCP protocol error, never a tool result.
    Refused(String),
    /// The agent-browser CLI never sent the command: its client-side
    /// connect failed before a byte reached the daemon (B1, cold-start
    /// race). Safe to send once more; callers retry at most once on this
    /// and surface it as a tool error when the retry also fails.
    NotSent(String),
    /// A tool-level failure: bad arguments, limits, spawn problems, or
    /// agent-browser answering `{"success": false}`. Maps to an MCP
    /// `isError` result so the model sees it and can react.
    Failed(String),
    /// The MCP transport or protocol failed.
    Transport(String),
}

impl fmt::Display for HooError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message)
            | Self::UnsupportedBackend(message)
            | Self::NoChromium(message)
            | Self::NoEndpoint(message)
            | Self::NoAgentBrowser(message)
            | Self::Spawn(message)
            | Self::Refused(message)
            | Self::Failed(message)
            // Same rendering as `Failed`: the model sees the message;
            // the variant only decides the retry, never the text.
            | Self::NotSent(message)
            | Self::Transport(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for HooError {}

impl HooError {
    /// Exit code for process-level failures: 2 for usage, 1 for the rest.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            _ => 1,
        }
    }
}

/// Format an agent-browser stderr tail for error messages. Never logs tokens:
/// callers must not pass argv containing secrets (there are none: auth is by
/// socket permissions, not CLI flags).
pub fn short_stderr(stderr: &str) -> String {
    const MAX: usize = 2000;
    let trimmed = stderr.trim();
    if trimmed.len() <= MAX {
        return trimmed.to_owned();
    }
    // Cut on a char boundary: byte-slicing mid-emoji panics (M2).
    let mut start = trimmed.len() - MAX;
    while !trimmed.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &trimmed[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_exits_2_and_other_errors_exit_1() {
        assert_eq!(HooError::Usage("x".into()).exit_code(), 2);
        assert_eq!(HooError::Refused("x".into()).exit_code(), 1);
        assert_eq!(HooError::Failed("x".into()).exit_code(), 1);
    }

    #[test]
    fn stderr_tail_is_bounded() {
        let long = "e".repeat(5000);
        assert!(short_stderr(&long).chars().count() <= 2001);
        assert_eq!(short_stderr("  hi  "), "hi");
    }

    #[test]
    fn stderr_tail_never_splits_a_char() {
        // The cut lands inside multibyte chars here; byte-slicing would panic.
        let long = "🖥".repeat(1500);
        let tail = short_stderr(&long);
        assert!(tail.chars().count() <= 2001);
        assert!(tail.starts_with('…'));
        let mixed = format!("{}{}", "e".repeat(1999), "🖥");
        let tail = short_stderr(&mixed);
        assert!(tail.chars().count() <= 2001);
        assert!(tail.ends_with('🖥'));
    }
}
