# hoo-browser — Design (locked, B2/B3)

A tier C native helper per hoocode's Native tier contract
(`docs/design/plugins.md`): one `hoo-browser` process per hoocode session,
speaking MCP Streamable HTTP on a Unix socket, shared by the root agent and
its subagents. It owns one Chromium and drives it through the agent-browser
CLI (external program, one invocation per tool call, `--json`).

## Leases

The caller is whoever owns the bearer token, never the `_meta` claim. Agent
`A` owns sessions `A-1`, `A-2`, … (agent id sanitized to
`[A-Za-z0-9_-]`). Tool arg `tab` (default 1) picks the agent's n-th session;
a new number opens a new session plus `window new`, so each lease gets its
own window on the shared browser. Limits: 4 tabs per agent, 24 per helper,
10 minutes idle (a sweeper closes idle leases).

`browser_tabs {action: list}` lists the caller's leases; `{action: close,
tab}` closes one; `{action: adopt, from}` is root-only and moves another
agent's leases to root without renaming, so `agentEnded` for the original
agent leaves them alone. An agent can never address another agent's session;
`browser_cli` refuses a `--session` it does not own and always injects the
helper's `--cdp`.

## Tools

Exactly the 8 declared tools (`browser_open`, `browser_snapshot`,
`browser_act`, `browser_screenshot`, `browser_tabs`, `browser_wait`,
`browser_nav`, `browser_eval`) plus the hidden `browser_cli` (callable,
never listed): the CLI fallback for the long tail. Permission hints live in
`plugin.json`: open/snapshot/screenshot/tabs(list)/wait read; act/nav/tabs
act; eval dangerous. Screenshots return MCP image content (the CLI writes a
file; the helper reads it back and base64s it).

Overhead: whole-call time minus the agent-browser spawn-to-exit, one JSON
line per call in `<data dir>/hoo-browser.log` (never tokens). Target < 20 ms
p95; the real integration suite prints p50/p95.

## Backends

`HOO_BROWSER_BACKEND`: `chromium-headless` (default) and `chromium-headed`
work; `chrome` (real Chrome via the 144+ toggle) and `lightpanda` return
"not yet supported" (B4). Chromium resolves from `HOO_BROWSER_CHROME`, else
agent-browser's own install, else common paths. agent-browser resolves from
`HOO_BROWSER_AGENT_BROWSER`, else the pinned data-dir file, else `PATH`; the
pinned download stays unwired until the fork release with AB-1..AB-4 exists
(TODO in `backend.rs`).

## Later (from the old chromiumoxide driver; not committed scope)

- Deterministic replay: record a flow once, replay it with zero LLM calls,
  DOM-invariant checkpoints, tamper-evident evidence bundles. If replay ever
  returns, it belongs behind these same leases, not beside them.
- Live view: a watchable stream of the browser synced to tool calls.
- Yield contract: typed `NeedsParent` requests (reidentify element, decide
  next action, classify, verify visual, extract semantic) for the one step a
  replay cannot do alone. Needs a replayer first; without one it is scope
  without a customer.
