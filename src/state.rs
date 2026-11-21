//! Agent tokens and tab leases.
//!
//! The helper trusts the bearer token map, never the `_meta` agent claim: the
//! sender of every notification and the owner of every tool call resolve from
//! the token (spec B2, same rule as hoocode's `echo_helper`).

use std::collections::HashMap;
use std::time::Instant;

use crate::error::HooError;

/// Tabs per agent (spec B2 limits).
pub const MAX_TABS_PER_AGENT: usize = 4;
/// Leases per helper (spec B2 limits).
pub const MAX_LEASES: usize = 24;
/// Idle seconds after which the sweeper closes a lease (10 minutes).
pub const IDLE_SECONDS: u64 = 600;

/// One registered agent: what the supervisor sent in `hoocode/agentStarted`.
/// `parent_id`/`depth` are identity, not routing: the bearer token decides.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct AgentRecord {
    pub agent_id: String,
    pub parent_id: Option<String>,
    pub depth: u64,
}

/// One tab lease: an agent-browser session owned by one agent.
#[derive(Clone, Debug)]
pub struct Lease {
    /// The agent-browser `--session` name, `<sanitized agent>-<n>`.
    pub session: String,
    /// Owning agent id. `adopt` moves this to `root` without renaming.
    pub owner: String,
    pub last_used: Instant,
}

/// Tokens plus leases. Guarded by one mutex in the app state.
#[derive(Debug, Default)]
pub struct State {
    /// token -> agent.
    pub tokens: HashMap<String, AgentRecord>,
    /// session -> lease.
    pub leases: HashMap<String, Lease>,
    /// agent -> sessions in tab-index order (tab N is `order[N-1]`).
    pub order: HashMap<String, Vec<String>>,
}

/// Agent ids may contain `/` (`root/task-1`); session names may not, so map
/// every other character to `_` (same rule as hoocode's `echo_helper`).
/// Capped at [`MAX_SANITIZED_ID`] characters: agent ids nest (`root/a/b/c`)
/// and the socket path has an OS limit, so a long id truncates (the
/// collision fallback in `ensure` still keeps sessions apart).
pub const MAX_SANITIZED_ID: usize = 32;

pub fn sanitize_agent_id(agent_id: &str) -> String {
    let out: String = agent_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "agent".to_owned()
    } else if out.len() > MAX_SANITIZED_ID {
        out[..MAX_SANITIZED_ID].to_owned()
    } else {
        out
    }
}

/// The session name for an agent's n-th tab (1-based).
pub fn lease_name(agent_id: &str, tab: usize) -> String {
    format!("{}-{tab}", sanitize_agent_id(agent_id))
}

/// True when `session` belongs to `agent_id` by name (the naming rule above).
#[allow(dead_code)]
pub fn session_belongs_to(session: &str, agent_id: &str) -> bool {
    session.starts_with(&format!("{}-", sanitize_agent_id(agent_id)))
}

impl State {
    /// Register the root agent for its pre-shared token.
    pub fn register_root(&mut self, token: String, agent_id: String) {
        self.tokens.insert(
            token,
            AgentRecord {
                agent_id,
                parent_id: None,
                depth: 0,
            },
        );
    }

    /// The registration rule (spec B2): only a known sender may register, and
    /// only its own children (`parentId == sender.agentId`).
    pub fn register_agent(
        &mut self,
        sender: &AgentRecord,
        agent_id: String,
        parent_id: Option<String>,
        depth: u64,
        token: String,
    ) -> Result<(), HooError> {
        if agent_id.is_empty() || token.is_empty() {
            return Err(HooError::Refused(
                "agentStarted needs agentId and token".into(),
            ));
        }
        if parent_id.as_deref() != Some(sender.agent_id.as_str()) {
            return Err(HooError::Refused(format!(
                "agentStarted refused: {} is not a child of {}",
                agent_id, sender.agent_id
            )));
        }
        self.tokens.insert(
            token,
            AgentRecord {
                agent_id,
                parent_id,
                depth,
            },
        );
        Ok(())
    }

    /// Drop every token minted for `agent_id`. Returns the owned sessions so
    /// the caller can close them (`agentEnded` closes sessions unless adopted
    /// by root: adopted leases are owned by root by then).
    pub fn remove_agent(&mut self, agent_id: &str) -> Vec<String> {
        self.tokens.retain(|_, record| record.agent_id != agent_id);
        let sessions = self.order.remove(agent_id).unwrap_or_default();
        for session in &sessions {
            self.leases.remove(session);
        }
        sessions
    }

    /// Root takes every lease of `from` (ownership moves, names stay).
    pub fn adopt(&mut self, from: &str, by_root: &str) -> Result<usize, HooError> {
        if from == by_root {
            return Err(HooError::Failed("cannot adopt your own sessions".into()));
        }
        let mut sessions = self.order.remove(from).unwrap_or_default();
        if sessions.is_empty() {
            return Err(HooError::Failed(format!("agent {from} has no sessions")));
        }
        for session in &sessions {
            if let Some(lease) = self.leases.get_mut(session) {
                lease.owner = by_root.to_owned();
            }
        }
        self.order
            .entry(by_root.to_owned())
            .or_default()
            .append(&mut sessions);
        Ok(self.order.get(by_root).map(Vec::len).unwrap_or(0))
    }

    /// Resolve tab N (1-based) for `agent_id` without creating anything.
    pub fn resolve(&self, agent_id: &str, tab: usize) -> Result<String, HooError> {
        if tab == 0 {
            return Err(HooError::Failed("tab numbers start at 1".into()));
        }
        self.order
            .get(agent_id)
            .and_then(|sessions| sessions.get(tab - 1))
            .cloned()
            .ok_or_else(|| {
                HooError::Failed(format!(
                    "agent {agent_id} has no tab {tab} (it has {})",
                    self.order.get(agent_id).map(Vec::len).unwrap_or(0),
                ))
            })
    }

    /// Resolve tab N, creating the lease when N is exactly one past the end
    /// (a new tab). Returns the session plus whether it is fresh (the caller
    /// then runs `window new` so the lease gets its own window).
    pub fn ensure(&mut self, agent_id: &str, tab: usize) -> Result<(String, bool), HooError> {
        if tab == 0 {
            return Err(HooError::Failed("tab numbers start at 1".into()));
        }
        let owned = self.order.get(agent_id).map(Vec::len).unwrap_or(0);
        if tab <= owned {
            return Ok((self.order[agent_id][tab - 1].clone(), false));
        }
        if tab > owned + 1 {
            return Err(HooError::Failed(format!(
                "agent {agent_id} has {owned} tab(s); open tab {} first",
                owned + 1
            )));
        }
        if owned >= MAX_TABS_PER_AGENT {
            return Err(HooError::Failed(format!(
                "agent {agent_id} already has {MAX_TABS_PER_AGENT} tabs (limit)"
            )));
        }
        if self.leases.len() >= MAX_LEASES {
            return Err(HooError::Failed(format!(
                "helper already has {MAX_LEASES} sessions (limit)"
            )));
        }
        let session = lease_name(agent_id, tab);
        // Name collisions across agents (two agent ids sanitizing alike) fall
        // back to a suffixed name rather than sharing a session: an agent can
        // never address another agent's session.
        let mut session = session;
        let mut suffix = 0;
        while self.leases.contains_key(&session) {
            suffix += 1;
            session = format!("{}-{tab}-{suffix}", sanitize_agent_id(agent_id));
        }
        self.leases.insert(
            session.clone(),
            Lease {
                session: session.clone(),
                owner: agent_id.to_owned(),
                last_used: Instant::now(),
            },
        );
        self.order
            .entry(agent_id.to_owned())
            .or_default()
            .push(session.clone());
        Ok((session, true))
    }

    pub fn touch(&mut self, session: &str) {
        if let Some(lease) = self.leases.get_mut(session) {
            lease.last_used = Instant::now();
        }
    }

    /// Close one lease; only its owner (or root) may do it.
    pub fn close(&mut self, agent_id: &str, session: &str) -> Result<(), HooError> {
        let owner = self
            .leases
            .get(session)
            .map(|lease| lease.owner.clone())
            .ok_or_else(|| HooError::Failed(format!("no such session {session}")))?;
        if owner != agent_id && agent_id != "root" {
            return Err(HooError::Failed(format!(
                "session {session} belongs to {owner}"
            )));
        }
        self.leases.remove(session);
        for sessions in self.order.values_mut() {
            sessions.retain(|s| s != session);
        }
        Ok(())
    }

    /// Sessions idle longer than the limit, for the sweeper.
    pub fn idle(&self, now: Instant) -> Vec<String> {
        self.leases
            .values()
            .filter(|lease| now.duration_since(lease.last_used).as_secs() > IDLE_SECONDS)
            .map(|lease| lease.session.clone())
            .collect()
    }

    /// Sessions owned by `agent_id` (tab order), for `browser_tabs list`.
    pub fn owned(&self, agent_id: &str) -> Vec<String> {
        self.order.get(agent_id).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_ids_truncate_but_stay_valid() {
        let long = sanitize_agent_id("root/task-1/leaf-node-with-a-very-long-name");
        assert!(long.len() <= MAX_SANITIZED_ID);
        assert!(long
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn sanitize_matches_session_name_rules() {
        assert_eq!(sanitize_agent_id("root"), "root");
        assert_eq!(sanitize_agent_id("root/task-1"), "root_task-1");
        assert_eq!(sanitize_agent_id("a b.c:d"), "a_b_c_d");
        assert_eq!(sanitize_agent_id(""), "agent");
        for name in ["root-1", &sanitize_agent_id("root/task-1")] {
            assert!(name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_'));
        }
    }

    #[test]
    fn lease_names_follow_the_rule() {
        assert_eq!(lease_name("root", 1), "root-1");
        assert_eq!(lease_name("root/task-1", 2), "root_task-1-2");
        assert!(session_belongs_to("root_task-1-2", "root/task-1"));
        assert!(!session_belongs_to("root-1", "root/task-1"));
        assert!(!session_belongs_to("root-1", "other"));
    }

    fn root_state() -> State {
        let mut state = State::default();
        state.register_root("root-token".into(), "root".into());
        state
    }

    #[test]
    fn registration_only_accepts_own_children() {
        let mut state = root_state();
        let sender = state.tokens["root-token"].clone();
        state
            .register_agent(
                &sender,
                "root/task-1".into(),
                Some("root".into()),
                1,
                "child-token".into(),
            )
            .expect("child registers");
        // A grandchild registered by root is refused: parentId != sender.
        let sender = state.tokens["root-token"].clone();
        assert!(state
            .register_agent(
                &sender,
                "root/task-9".into(),
                Some("root/task-1".into()),
                2,
                "x".into(),
            )
            .is_err());
        // The child can register its own child.
        let sender = state.tokens["child-token"].clone();
        state
            .register_agent(
                &sender,
                "root/task-1/leaf".into(),
                Some("root/task-1".into()),
                2,
                "leaf-token".into(),
            )
            .expect("grandchild registers");
        assert!(state.tokens.contains_key("leaf-token"));
    }

    #[test]
    fn ensure_creates_in_order_and_enforces_limits() {
        let mut state = root_state();
        let (s1, fresh) = state.ensure("root/task-1", 1).unwrap();
        assert!(fresh && s1 == "root_task-1-1");
        let (s1b, fresh) = state.ensure("root/task-1", 1).unwrap();
        assert!(!fresh && s1b == s1);
        // Skipping a tab is refused.
        assert!(state.ensure("root/task-1", 3).is_err());
        let (s2, fresh) = state.ensure("root/task-1", 2).unwrap();
        assert!(fresh && s2 == "root_task-1-2");
        state.ensure("root/task-1", 3).unwrap();
        state.ensure("root/task-1", 4).unwrap();
        assert!(state.ensure("root/task-1", 5).is_err());
    }

    #[test]
    fn helper_wide_limit_is_24() {
        let mut state = root_state();
        for agent in 0..6 {
            for tab in 1..=4 {
                state.ensure(&format!("agent-{agent}"), tab).unwrap();
            }
        }
        assert_eq!(state.leases.len(), MAX_LEASES);
        assert!(state.ensure("agent-new", 1).is_err());
    }

    #[test]
    fn name_collisions_never_share_a_session() {
        let mut state = root_state();
        // "a/b" and "a_b" sanitize alike.
        let (first, _) = state.ensure("a/b", 1).unwrap();
        let (second, _) = state.ensure("a_b", 1).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn remove_agent_returns_sessions_and_drops_tokens() {
        let mut state = root_state();
        let sender = state.tokens["root-token"].clone();
        state
            .register_agent(
                &sender,
                "root/task-1".into(),
                Some("root".into()),
                1,
                "child-token".into(),
            )
            .unwrap();
        state.ensure("root/task-1", 1).unwrap();
        let closed = state.remove_agent("root/task-1");
        assert_eq!(closed, vec!["root_task-1-1".to_owned()]);
        assert!(!state.tokens.contains_key("child-token"));
        assert!(state.leases.is_empty());
    }

    #[test]
    fn adopt_moves_leases_to_root() {
        let mut state = root_state();
        state.ensure("root/task-1", 1).unwrap();
        state.ensure("root/task-1", 2).unwrap();
        state.adopt("root/task-1", "root").unwrap();
        assert_eq!(state.owned("root/task-1"), Vec::<String>::new());
        assert_eq!(state.owned("root").len(), 2);
        // Ended agents no longer own anything to close.
        assert!(state.remove_agent("root/task-1").is_empty());
        assert_eq!(state.leases.len(), 2);
    }

    #[test]
    fn close_is_owner_or_root_only() {
        let mut state = root_state();
        state.ensure("root/task-1", 1).unwrap();
        assert!(state.close("root/task-2", "root_task-1-1").is_err());
        state.close("root", "root_task-1-1").unwrap();
        assert!(state.leases.is_empty());
    }
}
