#!/bin/bash
# Fake agent-browser CLI for hoo-browser tests (no Chromium needed).
# Ignores global flags (--cdp/--session/--json/--pin-tab), answers canned
# --json envelopes on stdout, always exits 0 (like the real CLI).
#
# Test hooks (all read per call, so no process-wide env races):
# - FAKE_ARGV_LOG: appends the shell-quoted argv of every call (one line).
# - FAKE_FAIL_FILE: `window new` fails for sessions listed in this file
#   (one per line); the test creates/deletes entries mid-run.
# - Sessions containing "slow" sleep 30 s (timeout tests lower the timeout
#   via HOO_BROWSER_CALL_TIMEOUT_SECS).
set -u

# 1x1 transparent PNG, written for `screenshot <path>`.
FAKE_PNG_B64="iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="

if [ -n "${FAKE_ARGV_LOG:-}" ]; then
    printf '%q ' "$@" >> "$FAKE_ARGV_LOG"
    printf '\n' >> "$FAKE_ARGV_LOG"
fi

# N6: record the config env every child sees, one line per call.
if [ -n "${FAKE_ENV_LOG:-}" ]; then
    printf '%s\n' "${AGENT_BROWSER_CONFIG:-<unset>}" >> "$FAKE_ENV_LOG"
fi

# Version probe: the helper detects the fork by "-hoo." in this output
# (fork releases carry versions like 0.43.1-hoo.1); upstream has none.
for a in "$@"; do
    if [ "$a" = "--version" ]; then
        printf 'agent-browser %s\n' "${FAKE_VERSION:-0.43.0}"
        exit 0
    fi
done

# Drop globals (--cdp/--session and their values, --json/--pin-tab) so only
# the command remains. The session is captured for the fail hook.
args=()
session=""
prev=""
for a in "$@"; do
    if [ "$prev" = "--cdp" ]; then
        prev=""
        continue
    fi
    if [ "$prev" = "--session" ]; then
        session="$a"
        prev=""
        continue
    fi
    case "$a" in
        --cdp|--session) prev="$a" ;;
        --cdp=*) ;;
        --session=*) session="${a#--session=}" ;;
        --json|--pin-tab) ;;
        *) args+=("$a") ;;
    esac
done

cmd="${args[0]:-}"
rest=("${args[@]:1}")
# `--` ends option parsing upstream: positionals start after it.
if [ "${rest[0]:-}" = "--" ]; then
    rest=("${rest[@]:1}")
fi

ok() { printf '{"success":true,"data":%s,"error":null}\n' "$1"; }
fail() { printf '{"success":false,"data":null,"error":%s}\n' "$1"; }
jstr() { printf '"%s"' "$1"; }

case "$cmd" in
    "window")
        # One-shot failure hook: when a file named after the pending
        # session exists in this dir, the first `window new` for that
        # session fails with the file's content as the error and consumes
        # the file, so the retry (if any) succeeds. Replays cold-start
        # races: connect-phase text (the command never ran) or a
        # post-send read failure (outcome_unknown: it may have run).
        if [ -n "${FAKE_FAIL_ONCE_DIR:-}" ] \
            && [ -f "$FAKE_FAIL_ONCE_DIR/$session" ]; then
            text=$(cat "$FAKE_FAIL_ONCE_DIR/$session")
            rm -f "$FAKE_FAIL_ONCE_DIR/$session"
            # Valid `--json` envelope like the real CLI prints (it exits
            # 1 with `{"success":false,"error": …}` on stdout): the
            # helper may only retry when `error` carries the client
            # connect prefix.
            fail "$(jstr "$text")"
            exit 1
        fi
        if [ -n "${FAKE_FAIL_FILE:-}" ] && [ -f "$FAKE_FAIL_FILE" ] \
            && grep -qxF "$session" "$FAKE_FAIL_FILE" 2>/dev/null; then
            fail '"window new failed (injected)"'
        else
            # Like the real CLI, the first command for a session leaves a
            # long-lived daemon behind: a detached sleeper whose pid goes
            # to `<session>.pid` in AGENT_BROWSER_SOCKET_DIR (see
            # agent-browser cli/src/session_files.rs `SessionFile::Pid`).
            # It never exits on its own — only the helper's shutdown kills
            # it — so the daemon-leak tests can prove shutdown reaps it.
            # `exec -a` names it after the binary path the helper invoked
            # (FAKE_AB_SELF, exported by the test's wrapper: this script's
            # own $0 is the inner path after the shebang re-exec), so the
            # helper's command-line check accepts it exactly like the real
            # daemon. An existing live pid is reused (a `window new`
            # retry must not double-spawn).
            # Redirections first: the sleeper must not inherit the CLI's
            # captured stdout pipe, or the helper's read would never end.
            if [ -n "${AGENT_BROWSER_SOCKET_DIR:-}" ] && [ -n "$session" ]; then
                pid_file="$AGENT_BROWSER_SOCKET_DIR/$session.pid"
                if ! pid=$(cat "$pid_file" 2>/dev/null) \
                    || ! kill -0 "$pid" 2>/dev/null; then
                    exec -a "${FAKE_AB_SELF:-$0}" sleep 300 >/dev/null 2>&1 < /dev/null &
                    printf '%s\n' "$!" > "$pid_file"
                fi
            fi
            case "$session" in
                *slow*) sleep 30 ;;
            esac
            ok '{"tabId":"t9","total":2}'
        fi ;;
    "open") ok "{\"url\":$(jstr "${rest[0]:-about:blank}")}" ;;
    "snapshot") ok '{"snapshot":"- link \"hi\" [ref=e1]\n- button \"go\" [ref=e2]","refs":{"e1":{"name":"hi","role":"link"},"e2":{"name":"go","role":"button"}}}' ;;
    "click"|"hover"|"check"|"uncheck")
        ref="${rest[0]:-}"
        if [ "$ref" = "e999" ]; then fail '"Unknown ref: e999"'; else ok "{\"clicked\":$(jstr "$ref")}"; fi ;;
    "fill"|"select")
        ref="${rest[0]:-}"
        if [ -z "${rest[1]:-}" ]; then fail '"missing value"'; else ok "{\"filled\":$(jstr "$ref")}"; fi ;;
    "press") ok "{\"pressed\":$(jstr "${rest[0]:-Enter}")}" ;;
    "screenshot")
        # last positional is the path (flags like --full have no value)
        path=""
        for a in "${rest[@]}"; do
            case "$a" in
                --*) ;;
                *) path="$a" ;;
            esac
        done
        if [ -z "$path" ]; then fail '"no path"'; else printf '%s' "$FAKE_PNG_B64" | base64 -d > "$path" && ok "{\"path\":$(jstr "$path")}"; fi ;;
    "tab")
        case "${rest[0]:-}" in
            # Two entries like the real shared browser: the lease's own
            # tab (tabId matches `window new`) plus another lease's page.
            # The own entry is deliberately NOT active, so tests prove the
            # helper binds by targetId, never the live `active` flag.
            "list") ok "{\"tabs\":[{\"tabId\":\"t9\",\"targetId\":\"TGT-$session\",\"active\":false,\"url\":\"https://own.example/$session\"},{\"tabId\":\"t1\",\"targetId\":\"TGT-other\",\"active\":true,\"url\":\"https://other-agent.example/\"}]}" ;;
            *) ok '{"closed":true}' ;;
        esac ;;
    "close") ok '{"closed":true}' ;;
    "wait") ok '{"state":"done"}' ;;
    "back"|"forward"|"reload") ok '{}' ;;
    "eval")
        # B1: a daemon-reported page error whose text starts with the
        # connect prefix (exit 0, like the real daemon wrapping a page
        # JS failure) must surface, never retry.
        if [ "${rest[0]:-}" = "FAILCONNECT-PAGE" ]; then
            printf '{"success":false,"data":null,"error":"Failed to connect: simulated page error"}\n'
        else
            ok '{"result":"fake"}'
        fi
        ;;
    *) fail '"unknown command"' ;;
esac
