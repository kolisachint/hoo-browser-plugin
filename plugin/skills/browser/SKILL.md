---
name: browser
description: Drive the shared Chromium with the browser_* tools (open, snapshot, act, screenshot, tabs, wait, nav, eval). Use for any web task that needs clicks, forms, screenshots or JS. Each agent owns its tabs; refs come from snapshots.
---

# Browser: drive the shared Chromium

Eight tools, all from the `hoo-browser` plugin. One shared browser and one
shared login; **you own only your own tabs** (tab 1 by default, `tab: 2` opens
a second). Never address another agent's tab, focus, dialogs or downloads.

## The loop

1. **`browser_open {url}`** to navigate your tab.
2. **`browser_snapshot {}`** to get the accessibility tree and its refs.
3. **`browser_act {action, ref, value?}`** to click, fill, select, check,
   uncheck, hover or press. Actions run against **refs** (`e3`), never
   coordinates from memory.
4. **`browser_screenshot {}`** to look — the tool returns the image. Snapshot
   before the screenshot when you need fresh refs (a screenshot has none).

`action` is one of `click | fill | select | check | uncheck | hover | press`.
`fill` and `select` need `value` (text, or the option); `press` takes the key
in `value` (`Enter`, `Escape`) and falls back to `ref`.

## Tabs

- `tab` is 1-based. `tab: 2` opens a second tab (a new session with its own
  window); asking for tab 3 while you own 1 is an error — open tab 2 first.
- Limits: 4 tabs per agent, 24 per session, 10 minutes idle (idle tabs close).
- **`browser_tabs {action: "list"}`** shows your tabs; `{action: "close",
  tab}` closes one. Root only: `{action: "adopt", from}` takes another
  agent's tabs when it ends.
- Isolation: with the fork agent-browser the helper runs scoped (listings
  show your tabs, closes remove all of them); with upstream it runs safe
  (only your own bound tab, ever — other agents' tabs and URLs stay
  hidden, and `cli tab list` is refused).

## Waiting and reading

- **`browser_wait`** takes one of `ms`, `text`, `ref` or `load`
  (`domcontentloaded`, `networkidle`).
- **`browser_nav {action: "back" | "forward" | "reload"}`** for history.
- **`browser_eval {js}`** runs JavaScript in your tab and returns the result.
  It is marked dangerous and always asks: prefer snapshot, act and wait, and
  use eval only to read state the snapshot cannot show.

## CLI fallback (the long tail)

Cookies, network, recording and everything else the eight tools do not cover:
run the helper binary through Shell. hoocode puts its absolute path in
`HOOCODE_HELPER_BIN_HOO_BROWSER` (the binary is not on Shell's `PATH`). It
calls the helper scoped to **your** sessions — a `--session` naming another
agent's session is refused. The helper starts on the first browser tool call,
so run one tool before the fallback.

```bash
"$HOOCODE_HELPER_BIN_HOO_BROWSER" cli snapshot                # your default tab
"$HOOCODE_HELPER_BIN_HOO_BROWSER" cli --session <yours> eval "document.title"
```
