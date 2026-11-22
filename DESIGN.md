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
never listed): the CLI fallback for the long tail. From Shell it runs as
`"$HOOCODE_HELPER_BIN_HOO_BROWSER" cli <args>` — hoocode sets that env var
once the helper has started on the first browser tool call. Permission hints live in
`plugin.json`: open/snapshot/screenshot/tabs(list)/wait read; act/nav/tabs
act; eval dangerous. Screenshots return MCP image content (the CLI writes a
file; the helper reads it back and base64s it).

Overhead: whole-call time (stamped at request entry in the auth layer)
minus the agent-browser spawn-to-exit minus `window new`, one JSON
line per call in `<data dir>/hoo-browser.log` (never tokens). Target < 20 ms
p95; the real integration suite prints p50/p95.

## Backends

`HOO_BROWSER_BACKEND`: `chromium-headless` (default) and `chromium-headed`
work; `chrome` (real Chrome via the 144+ toggle) and `lightpanda` return
"not yet supported" (B4). Chromium resolves from `HOO_BROWSER_CHROME`, else
agent-browser's own install, else common paths. agent-browser resolves from
`HOO_BROWSER_AGENT_BROWSER` (use this for a local fork build; it overrides
the pin), else the pinned download for the current platform
(`v0.43.0-hoo.1`, verified by sha256 into
`<data dir>/agent-browser/<tag>/agent-browser`), else `PATH` — but only on
platforms with no pin.

Chromium runs in its own process group so shutdown kills the whole tree;
the DevTools port is `--remote-debugging-port=0` (a loopback-only,
unauthenticated endpoint — the helper passes it to its own children over the
command line, never over the network). The agent-browser daemon dir lives in
the per-session runtime dir (the helper socket's parent, 0700, removed by
hoocode at session end; cleaned at startup). Daemons attached via --cdp
never idle-exit while attached, so every shutdown path (sessionEnding,
SIGTERM/SIGINT, parent death) SIGTERMs every daemon listed in the socket
dir's `<session>.pid` files after the capped lease closes (SIGKILL after
~300 ms, inside the 2 s budget), verifying each pid's command line names
the helper's agent-browser binary; startup does the same for stale pid
files before recreating the dir, and logs one line (`hoo-browser:
agent-browser <version> at <path>, mode scoped|safe`) to hoo-browser.log.
The browser profile is
`PLUGIN_DATA/profile` behind a lock file; a second live helper falls back to
a fresh profile in its runtime dir. Every lease carries `--pin-tab` so its
background window never takes focus (upstream AB-1).

## Modes: scoped vs safe

Chosen once at startup by probing `agent-browser --version`
(`HOO_BROWSER_AB_SCOPED=1/0` forces it for tests). **Scoped** (the hoo
fork: releases carry `-hoo.N` versions like `0.43.1-hoo.1`) scopes
`--pin-tab` sessions' `tab list`/switch to their owned tabs, so listings
pass through and lease close removes every listed target (bound, opened,
adopted popups) via DevTools `/json/close`. **Safe** (upstream
agent-browser, whose `tab list` shows the whole shared browser): the
lease binds the target its window opened with (matched by `tabId`, never
the live `active` flag); closes remove only that bound target,
`browser_tabs list` returns only the lease's own bound entry per tab
slot, and `browser_cli tab` is refused. Full isolation needs the fork.
Safe mode logs a one-line warning at startup.

## Later (from the old chromiumoxide driver; not committed scope)

- Deterministic replay: record a flow once, replay it with zero LLM calls,
  DOM-invariant checkpoints, tamper-evident evidence bundles. If replay ever
  returns, it belongs behind these same leases, not beside them.
- Live view: a watchable stream of the browser synced to tool calls.
- Yield contract: typed `NeedsParent` requests (reidentify element, decide
  next action, classify, verify visual, extract semantic) for the one step a
  replay cannot do alone. Needs a replayer first; without one it is scope
  without a customer.
