# Development Rules

## Repo Map

`hoo-browser` is one Rust binary crate: a tier C helper that calls the
agent-browser CLI once per tool call.

Docs:

- `README.md` — build, test, run
- `DESIGN.md` — helper design, lease model, Later notes (old flow/replay ideas)
- `plugin/` — the shippable package (`plugin.json`, `mcp.json`, `skills/`)

Code (`src/`):

- `main.rs` — mode dispatch (helper vs CLI fallback), env, Chromium launch
- `server.rs` — MCP tools, token map, notifications, Unix-socket serve, sweeper
- `state.rs` — agent/token registry, tab leases, limits, sanitize rules
- `exec.rs` — tool arg to agent-browser argv mapping, spawn, overhead log
- `backend.rs` — backend enum, Chromium locate/launch, agent-browser locate
- `client.rs` — CLI fallback MCP client over the Unix socket
- `error.rs` — hand-written error enum (no `anyhow`/`thiserror`)

Tests:

- `tests/integration.rs` — fake agent-browser (always) + real backend
  (`HOO_BROWSER_IT=1`)
- `tests/manifest.rs` — `plugin.json` validated against the native rules,
  schemas pinned to `tools/list`
- `tests/fake-agent-browser.sh` — canned `--json` CLI for the fake suite

Packaging/CI:

- `scripts/update_manifest.py` — pin release sha256s into `plugin.json`
- `.github/workflows/` — `ci.yml`, `release.yml`

## Rules

- MIT only. Never copy agent-browser code (Apache-2.0): call the CLI.
- After code changes run all three and fix everything:
  ```bash
  cargo fmt --all
  cargo clippy --all-targets -- -D warnings
  cargo test
  ```
- Match the surrounding style. Hand-written error enum; owned types, `Arc`
  and `.clone()`; plain `async fn`.
- Tool schemas live once in `server::tool_schemas()`; `plugin/plugin.json`
  must match (`tests/manifest.rs` enforces it). Bump both together.
