# hoo-browser

Tier C native helper for the hoo-browser plugin: one shared Chromium driven
via the agent-browser CLI, with one tab lease per agent over MCP Streamable
HTTP on a local socket. Spec: `docs/design/plugins.md` in hoocode ("Native
tier contract", "Browser plugin"); build spec sections B2/B3.

## Build

```bash
cargo build        # binary: target/debug/hoo-browser
```

Needs Rust 1.100 (rust-toolchain pins it if present). No Chromium or
agent-browser needed to build; both resolve at runtime:

- agent-browser CLI: `HOO_BROWSER_AGENT_BROWSER` env, else the pinned file in
  the data dir, else `PATH`. To use a local build (e.g. the fork with the
  AB-1..AB-4 fixes before its first release), point `HOO_BROWSER_AGENT_BROWSER`
  at the built binary — this is the supported route until the pinned fork
  release exists (see `backend.rs`).
- Two modes (see `DESIGN.md`): **scoped** when the agent-browser is the hoo
  fork (detected by `-hoo.` in `agent-browser --version`, e.g.
  `0.43.1-hoo.1`), **safe** with upstream agent-browser. Full isolation
  (listings pass through, closes take every owned target) needs the fork;
  safe mode still isolates leases but conservatively (own bound tab only,
  no `browser_cli tab`). Override for tests: `HOO_BROWSER_AB_SCOPED=1/0`.
- Chromium: `HOO_BROWSER_CHROME` env, else agent-browser's own install
  (`~/.agent-browser/browsers/`), else common install paths.

## Test

```bash
cargo test                                   # unit + fake-backend integration + manifest
HOO_BROWSER_IT=1 \
HOO_BROWSER_AGENT_BROWSER=$(which agent-browser) \
HOO_BROWSER_CHROME=/path/to/chrome \
  cargo test --test integration             # real agent-browser + headless Chromium
```

The real suite drives a local test page: open, snapshot, click by ref,
screenshot, 3 agents x 2 tabs in parallel, cross-agent refusal, agentEnded,
and prints the helper overhead p50/p95 (target < 20 ms).

Also useful:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
python3 scripts/update_manifest.py --version 0.1.0 --dir ./dist  # pin release sha256s
```

## Run

The hoocode supervisor starts the helper; humans rarely run it by hand:

```bash
# helper mode (env set by the supervisor, --parent-pid required)
HOOCODE_HELPER_SOCKET=/tmp/h.sock HOOCODE_HELPER_TOKEN=<root> \
HOO_BROWSER_DATA=~/.agents/plugin-data/hoo-browser \
  hoo-browser --parent-pid <hoocode-pid>

# CLI fallback from inside an agent Shell (env set by hoocode per helper;
# needs the helper to have started: make one browser tool call first)
"$HOOCODE_HELPER_BIN_HOO_BROWSER" cli snapshot
"$HOOCODE_HELPER_BIN_HOO_BROWSER" cli --session root_task-1-1 eval "document.title"
```

Per-call overhead lines go to `<data dir>/hoo-browser.log` (debug level;
never tokens); `scripts/overhead.py` prints helper-only vs window-new vs
spawn+connect p50/p95 from that log. Backend selection: `HOO_BROWSER_BACKEND`
(`chromium-headless` default, `chromium-headed`; `chrome` and `lightpanda`
return "not yet supported", B4) or `HOO_BROWSER_HEADED=1`.

## Release

```bash
git tag v0.2.0 && git push origin v0.2.0
```

The release workflow builds the four platform binaries, uploads them, then
opens a PR pinning their sha256s into `plugin/plugin.json` — merge that PR
before anything else references the tag. Only after the release exists (assets
uploaded, manifest PR merged) add the `hoo-browser` entry to hoo-plugins
(git-subdir, path `plugin`): the marketplace pins the tag, and a listing
merged earlier would resolve URLs and sha256s that do not exist yet.
