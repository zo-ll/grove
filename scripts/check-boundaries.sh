#!/usr/bin/env bash
# Enforce the crate boundaries from SPEC.md §8 and issue #31.
#
# The two lanes are kept apart by the dependency graph, not by discipline. A
# violation fails CI rather than waiting to be noticed in review.
set -uo pipefail

fail=0

deps_of() {
    cargo tree -p "$1" --edges normal --prefix none 2>/dev/null \
        | awk '{print $1}' | sort -u
}

forbid() {
    local pkg="$1" why="$2"; shift 2
    local deps; deps="$(deps_of "$pkg")"
    local f
    for f in "$@"; do
        if grep -qxF "$f" <<<"$deps"; then
            printf '  FAIL  %-16s must not depend on %-16s (%s)\n' "$pkg" "$f" "$why"
            fail=1
        fi
    done
}

echo "checking crate boundaries"

# The TUI talks to the daemon over the protocol and nothing else. Rendering
# crates (vt100, tui-term) are expected here; spawning a pty is not.
forbid grove "UI lane must not reach backend concerns" \
    grove-git grove-state portable-pty gix git2

# The daemon serves state; it does not render.
forbid groved "backend lane must not reach UI concerns" \
    ratatui crossterm tui-term

# Domain types are pure. Anything that performs I/O is a violation.
forbid grove-domain "domain types must stay I/O-free" \
    tokio std-fs reqwest gix git2 mlua portable-pty ratatui serde_json

# The protocol carries domain types; it does not implement behaviour.
forbid grove-proto "the contract must not embed implementations" \
    grove-git grove-state grove-lua ratatui portable-pty

if [ "$fail" -eq 0 ]; then
    echo "  ok    all boundaries hold"
fi
exit "$fail"
