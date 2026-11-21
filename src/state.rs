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
    /// The DevTools target the helper saw for this lease right after its
    /// window opened (matched by `tabId`, B3). Safe-mode closes and
    /// listings use this — never the live `active` flag, which another
    /// lease may have moved. Travels with the lease across `adopt`.
    pub bound_target: Option<String>,
    pub last_used: Instant,
    /// Fresh leases stay initializing until `window new` gives them their
    /// own window (H2): concurrent first calls on a new tab wait on this
    /// instead of opening duplicate windows.
    pub initializing: bool,
    /// Set while the sweeper closes the lease: calls finding this error out
    /// instead of running on a session that is going away (M7).
    pub closing: bool,
}

/// Tokens plus leases. Guarded by one mutex in the app state.
#[derive(Debug, Default)]
pub struct State {
    /// token -> agent.
    pub tokens: HashMap<String, AgentRecord>,
    /// session -> lease.
    pub leases: HashMap<String, Lease>,
    /// Fixed tab slots per agent: `order[agent][tab - 1]` is the session
    /// name, or `None` for a closed tab. Closing clears the slot without
    /// renumbering the others (M6); reopening the number takes a fresh
    /// session in the same slot.
    pub order: HashMap<String, Vec<Option<String>>>,
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
    /// only its own children (`parentId == sender.agentId`). Hardened (C1):
    /// the id must nest under the sender (`sender/child`, depth exactly one
    /// deeper), the token must be fresh, and an id already known under another
    /// token is refused (re-registering your own token is idempotent).
    /// Without this any agent could mint `root` (or a sibling's id) and
    /// inherit its leases.
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
        let nested = format!("{}/", sender.agent_id);
        if !agent_id.starts_with(&nested) {
            return Err(HooError::Refused(format!(
                "agentStarted refused: {agent_id} does not nest under {}",
                sender.agent_id
            )));
        }
        if depth != sender.depth + 1 {
            return Err(HooError::Refused(format!(
                "agentStarted refused: depth {depth} is not {} + 1",
                sender.depth
            )));
        }
        if let Some(known) = self.tokens.get(&token) {
            // Re-registering your own token is idempotent (hoocode may
            // replay agentStarted); the same token for another id is refused.
            if known.agent_id != agent_id {
                return Err(HooError::Refused(
                    "agentStarted refused: token is already known for another agent".into(),
                ));
            }
            return Ok(());
        }
        if self
            .tokens
            .values()
            .any(|record| record.agent_id == agent_id)
        {
            return Err(HooError::Refused(format!(
                "agentStarted refused: {agent_id} is already registered under another token"
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
        for session in sessions.iter().flatten() {
            self.leases.remove(session);
        }
        sessions.into_iter().flatten().collect()
    }

    /// Root takes every lease of `from` (ownership moves, names stay).
    pub fn adopt(&mut self, from: &str, by_root: &str) -> Result<usize, HooError> {
        if from == by_root {
            return Err(HooError::Failed("cannot adopt your own sessions".into()));
        }
        let mut sessions = self.order.remove(from).unwrap_or_default();
        if sessions.iter().flatten().next().is_none() {
            return Err(HooError::Failed(format!("agent {from} has no sessions")));
        }
        for session in sessions.iter().flatten() {
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
            .and_then(|slot| slot.clone())
            .ok_or_else(|| {
                HooError::Failed(format!(
                    "agent {agent_id} has no tab {tab} (it has {})",
                    self.order
                        .get(agent_id)
                        .map(|slots| slots.iter().filter(|s| s.is_some()).count())
                        .unwrap_or(0),
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
        let slots = self.order.get(agent_id).map(Vec::len).unwrap_or(0);
        if tab <= slots {
            if let Some(session) = self.order[agent_id][tab - 1].clone() {
                return Ok((session, false));
            }
            // A closed slot: fall through and reopen it with a fresh
            // session (the limits below still apply).
        } else if tab > slots + 1 {
            return Err(HooError::Failed(format!(
                "agent {agent_id} has {slots} tab(s); open tab {} first",
                slots + 1
            )));
        }
        let open = self
            .order
            .get(agent_id)
            .map(|slots| slots.iter().filter(|s| s.is_some()).count())
            .unwrap_or(0);
        if open >= MAX_TABS_PER_AGENT {
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
                bound_target: None,
                last_used: Instant::now(),
                initializing: true,
                closing: false,
            },
        );
        let slots = self.order.entry(agent_id.to_owned()).or_default();
        if tab <= slots.len() {
            slots[tab - 1] = Some(session.clone());
        } else {
            slots.push(Some(session.clone()));
        }
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
        // Clear the slot so the other tabs keep their numbers (M6).
        self.remove_session(session);
        Ok(())
    }

    /// Mark a fresh lease ready after its window opened (H2).
    pub fn mark_ready(&mut self, session: &str) {
        if let Some(lease) = self.leases.get_mut(session) {
            lease.initializing = false;
            lease.last_used = Instant::now();
        }
    }

    /// Record the DevTools target the lease's window opened with (B3
    /// safe mode: matched by `tabId` at window open, never the live
    /// `active` flag). `None` when the post-open `tab list` failed: safe
    /// closes then skip the DevTools step rather than guess.
    pub fn set_bound_target(&mut self, session: &str, target: Option<String>) {
        if let Some(lease) = self.leases.get_mut(session) {
            lease.bound_target = target;
        }
    }

    /// True while a fresh lease still waits for its window; `None` when the
    /// lease is gone (rolled back under the waiter: re-ensure recreates it).
    pub fn is_initializing(&self, session: &str) -> Option<bool> {
        self.leases.get(session).map(|lease| lease.initializing)
    }

    /// Drop a lease that never became ready (H2 rollback after a failed
    /// `window new`): frees its slot without any permission check, since
    /// only the creator that just made it ever calls this.
    pub fn abandon(&mut self, session: &str) {
        self.leases.remove(session);
        self.clear_slot(session);
    }

    fn clear_slot(&mut self, session: &str) {
        for slots in self.order.values_mut() {
            for slot in slots.iter_mut() {
                if *slot == Some(session.to_owned()) {
                    *slot = None;
                }
            }
        }
    }

    /// Claim one idle lease for closing (M7): succeeds only if the lease
    /// is still untouched since `as_of` (the sweep's check time) and not
    /// already claimed. Anything touched after the check — a call that
    /// started before it (touch at call start) or during the close — fails
    /// the re-check and keeps its lease.
    pub fn try_claim_close(&mut self, session: &str, as_of: Instant) -> bool {
        let idle = self.leases.get(session).is_some_and(|lease| {
            !lease.closing
                && !lease.initializing
                && as_of.duration_since(lease.last_used).as_secs() > IDLE_SECONDS
        });
        if idle {
            if let Some(lease) = self.leases.get_mut(session) {
                lease.closing = true;
            }
        }
        idle
    }

    /// Still untouched since the sweep's check time? A call that started
    /// during the browser-side close touched the lease, so removal is
    /// skipped and the claim released instead.
    pub fn still_idle(&self, session: &str, as_of: Instant) -> bool {
        self.leases
            .get(session)
            .is_some_and(|lease| lease.closing && lease.last_used <= as_of)
    }

    /// True while the sweeper owns the lease's close claim.
    pub fn is_closing(&self, session: &str) -> bool {
        self.leases.get(session).is_some_and(|lease| lease.closing)
    }

    /// Release a close claim without removing the lease.
    pub fn unclaim(&mut self, session: &str) {
        if let Some(lease) = self.leases.get_mut(session) {
            lease.closing = false;
        }
    }

    /// Remove a lease regardless of owner (sweeper path; the claim is the
    /// authorization).
    pub fn remove_session(&mut self, session: &str) {
        self.leases.remove(session);
        self.clear_slot(session);
    }

    /// Sessions idle longer than the limit, for the sweeper. Claimed leases
    /// are skipped; `try_claim_close` re-checks before removal. Calls in
    /// flight are skipped by recency: every call touches its lease at start
    /// (well under the 10-minute limit, since calls time out at 45 s), so
    /// an in-flight lease is never idle here.
    pub fn idle(&self, now: Instant) -> Vec<String> {
        self.leases
            .values()
            .filter(|lease| {
                !lease.closing && now.duration_since(lease.last_used).as_secs() > IDLE_SECONDS
            })
            .map(|lease| lease.session.clone())
            .collect()
    }

    /// Live sessions owned by `agent_id` (tab order), for `browser_tabs
    /// list` and `browser_cli` scoping. Closed slots contribute nothing.
    pub fn owned(&self, agent_id: &str) -> Vec<String> {
        self.order
            .get(agent_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .filter(|s| self.leases.contains_key(s))
            .collect()
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
    fn registration_is_scoped_to_the_sender() {
        let mut state = root_state();
        let sender = state.tokens["root-token"].clone();
        let register = |state: &mut State,
                        sender: &AgentRecord,
                        agent_id: &str,
                        parent: Option<&str>,
                        depth: u64,
                        token: &str| {
            state.register_agent(
                sender,
                agent_id.into(),
                parent.map(str::to_owned),
                depth,
                token.into(),
            )
        };
        register(
            &mut state,
            &sender,
            "root/task-1",
            Some("root"),
            1,
            "child-1",
        )
        .unwrap();
        // Becoming root, a sibling, yourself, or an outsider is refused.
        for (agent_id, depth) in [
            ("root", 1),
            ("root/task-1", 1),
            ("root", 0),
            ("other/task-9", 1),
            ("root2/task-9", 1),
        ] {
            assert!(
                register(&mut state, &sender, agent_id, Some("root"), depth, "fresh").is_err(),
                "{agent_id} must be refused"
            );
        }
        // The depth must be exactly one deeper.
        assert!(register(&mut state, &sender, "root/task-2", Some("root"), 2, "t2").is_err());
        assert!(register(&mut state, &sender, "root/task-2", Some("root"), 0, "t2").is_err());
        // A token never registers twice.
        assert!(register(
            &mut state,
            &sender,
            "root/task-2",
            Some("root"),
            1,
            "child-1"
        )
        .is_err());
        // An id never registers under a second token …
        assert!(register(
            &mut state,
            &sender,
            "root/task-1",
            Some("root"),
            1,
            "second"
        )
        .is_err());
        // … but re-registering your own token is idempotent.
        register(
            &mut state,
            &sender,
            "root/task-1",
            Some("root"),
            1,
            "child-1",
        )
        .expect("same token is idempotent");
        // A grandchild nests under its own parent only.
        let child = state.tokens["child-1"].clone();
        assert!(register(
            &mut state,
            &child,
            "root/task-2",
            Some("root/task-1"),
            2,
            "grand"
        )
        .is_err());
        register(
            &mut state,
            &child,
            "root/task-1/leaf",
            Some("root/task-1"),
            2,
            "grand",
        )
        .expect("nested grandchild registers");
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
    fn closing_a_tab_does_not_renumber_the_others() {
        let mut state = root_state();
        let (s1, _) = state.ensure("root/task-1", 1).unwrap();
        let (s2, _) = state.ensure("root/task-1", 2).unwrap();
        state.close("root/task-1", &s1).unwrap();
        // Tab 2 keeps its session; tab 1 resolves to nothing now.
        assert_eq!(state.resolve("root/task-1", 2).unwrap(), s2);
        assert!(state.resolve("root/task-1", 1).is_err());
        // Reopening tab 1 takes a fresh session in the same slot.
        let (s1_new, fresh) = state.ensure("root/task-1", 1).unwrap();
        assert!(fresh && s1_new == s1);
        assert_eq!(state.resolve("root/task-1", 2).unwrap(), s2);
        // The per-agent limit counts live tabs only: 2 live + reopen = 3.
        state.ensure("root/task-1", 3).unwrap();
        state.ensure("root/task-1", 4).unwrap();
        assert!(state.ensure("root/task-1", 5).is_err());
    }

    #[test]
    fn sweeper_claims_only_untouched_leases() {
        use std::time::{Duration, Instant};
        let mut state = root_state();
        let (s1, _) = state.ensure("a", 1).unwrap();
        // Fresh (initializing) leases are never claimed.
        assert!(!state.try_claim_close(&s1, Instant::now()));
        state.mark_ready(&s1);
        // Just touched: not idle.
        assert!(!state.try_claim_close(&s1, Instant::now()));
        // Old enough: claimed once, and hidden from idle while claimed.
        let old = Instant::now() - Duration::from_secs(IDLE_SECONDS + 1);
        state.leases.get_mut(&s1).unwrap().last_used = old;
        assert!(state.try_claim_close(&s1, Instant::now()));
        assert!(!state.try_claim_close(&s1, Instant::now()));
        assert!(state.idle(Instant::now()).is_empty());
        assert!(state.is_closing(&s1));
        // A call starting mid-close touches: the re-check fails, the claim
        // is released and the lease survives.
        state.touch(&s1);
        assert!(!state.still_idle(&s1, old));
        state.unclaim(&s1);
        assert!(!state.is_closing(&s1));
        // Untouched: the re-check passes and removal clears the slot.
        state.leases.get_mut(&s1).unwrap().last_used = old;
        let check = Instant::now();
        assert!(state.try_claim_close(&s1, check));
        assert!(state.still_idle(&s1, check));
        state.remove_session(&s1);
        assert!(state.resolve("a", 1).is_err());
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
