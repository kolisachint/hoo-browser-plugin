#!/usr/bin/env python3
"""Overhead report for hoo-browser (M5).

Reads the JSON lines the helper appends to <data dir>/hoo-browser.log and
prints p50/p95 for both halves of every call:

- helper-only (overhead_us): request entry to response minus the child run
- spawn+connect (child_us): the agent-browser spawn-to-exit, including its
  daemon connect

Usage:
  scripts/overhead.py <data dir>/hoo-browser.log
  HOO_BROWSER_IT=1 ... cargo test && scripts/overhead.py <tmpdir>/hoo-browser.log
"""
import json
import sys


def percentile(sorted_values, pct):
    if not sorted_values:
        return 0
    index = min(int(len(sorted_values) * pct / 100), len(sorted_values) - 1)
    return sorted_values[index]


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <hoo-browser.log>", file=sys.stderr)
        return 2
    overheads = []
    children = []
    calls = 0
    with open(sys.argv[1], encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if "overhead_us" not in value or "child_us" not in value:
                continue
            calls += 1
            overheads.append(value["overhead_us"])
            children.append(value["child_us"])
    if calls == 0:
        print("no overhead lines found")
        return 1
    overheads.sort()
    children.sort()
    print(f"calls: {calls}")
    print(
        "helper-only overhead_us: p50={} p95={} max={}".format(
            percentile(overheads, 50),
            percentile(overheads, 95),
            overheads[-1],
        )
    )
    print(
        "spawn+connect child_us:  p50={} p95={} max={}".format(
            percentile(children, 50),
            percentile(children, 95),
            children[-1],
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
