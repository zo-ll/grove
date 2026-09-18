#!/usr/bin/env bash
# Enforce the crate boundaries from SPEC.md §8 and issue #31.
#
# The two lanes are kept apart by the dependency graph, not by discipline. A
# violation fails CI rather than waiting to be noticed in review.
#
# Three complementary checks, because no single one is sufficient:
#
#   1. transitive   — what a crate can actually reach, with all features on.
#                     Catches indirect reach through an allowed crate.
#   2. declared     — what our own crates ask for in ANY dependency section.
#                     Catches dev- and build-dependencies, which never appear
#                     in a normal-edge tree, and build.rs running backend code
#                     during the TUI's build.
#   3. allow-list   — for grove-domain, whose purity a deny-list can never
#                     uphold. Enumerate what it may reach; reject the rest.
#
# The script fails CLOSED: any tooling error is a failure, never a silent pass.

set -uo pipefail

fail=0
note() { printf '  %-6s %s\n' "$1" "$2"; }

# --- 1. transitive ----------------------------------------------------------

deps_of() {
    local pkg="$1" out rc
    out="$(cargo tree -p "$pkg" --edges normal --all-features --prefix none 2>&1)"
    rc=$?
    if [ "$rc" -ne 0 ]; then
        note FAIL "cargo tree failed for $pkg (exit $rc) — refusing to pass" >&2
        printf '%s\n' "$out" | sed 's/^/         /' >&2
        return 1
    fi
    if [ -z "${out//[[:space:]]/}" ]; then
        note FAIL "cargo tree returned nothing for $pkg — refusing to pass" >&2
        return 1
    fi
    printf '%s\n' "$out" | awk 'NF {print $1}' | sort -u
}

forbid() {
    local pkg="$1" why="$2"; shift 2
    local deps
    if ! deps="$(deps_of "$pkg")"; then fail=1; return; fi
    local f
    for f in "$@"; do
        if grep -qxF "$f" <<<"$deps"; then
            note FAIL "$pkg must not reach $f ($why)"
            fail=1
        fi
    done
}

# --- 2. declared, across every dependency kind ------------------------------

declared_deps_of() {
    local pkg="$1" out rc
    out="$(cargo metadata --no-deps --format-version 1 2>&1)"
    rc=$?
    if [ "$rc" -ne 0 ]; then
        note FAIL "cargo metadata failed (exit $rc) — refusing to pass" >&2
        return 1
    fi
    jq -r --arg p "$pkg" \
        '.packages[] | select(.name == $p) | .dependencies[] | "\(.name)\t\(.kind // "normal")"' \
        <<<"$out"
}

forbid_declared() {
    local pkg="$1" why="$2"; shift 2
    local rows
    if ! rows="$(declared_deps_of "$pkg")"; then fail=1; return; fi
    local f name kind
    while IFS=$'\t' read -r name kind; do
        [ -n "$name" ] || continue
        for f in "$@"; do
            if [ "$name" = "$f" ]; then
                note FAIL "$pkg declares $f as a $kind-dependency ($why)"
                fail=1
            fi
        done
    done <<<"$rows"
}

# --- 3. allow-list ----------------------------------------------------------

allow_only() {
    local pkg="$1" why="$2"; shift 2
    local deps allowed
    if ! deps="$(deps_of "$pkg")"; then fail=1; return; fi
    allowed="$(printf '%s\n' "$@" | sort -u)"
    local extra
    extra="$(comm -23 <(printf '%s\n' "$deps") <(printf '%s\n' "$allowed"))"
    if [ -n "$extra" ]; then
        local c
        while read -r c; do
            [ -n "$c" ] && { note FAIL "$pkg may not reach $c ($why)"; fail=1; }
        done <<<"$extra"
    fi
}

# ---------------------------------------------------------------------------

echo "checking crate boundaries"

BACKEND_CRATES="grove-git grove-state"
GIT_CRATES="gix git2 gitoxide-core"
PTY_CRATES="portable-pty pty-process nix-pty"
RENDER_CRATES="ratatui crossterm tui-term termion vt100"

# The TUI talks to the daemon over the protocol and nothing else. Rendering
# crates are expected here; spawning a pty or reading a repo is not.
# grove-fakedaemon is UI-lane tooling and belongs to tests, not the binary.
# shellcheck disable=SC2086
forbid          grove "UI lane must not reach backend concerns" \
    $BACKEND_CRATES $GIT_CRATES $PTY_CRATES grove-fakedaemon
# shellcheck disable=SC2086
forbid_declared grove "UI lane must not reach backend concerns" \
    $BACKEND_CRATES $GIT_CRATES $PTY_CRATES

# The daemon serves state; it does not render. vt100 is UI-side per SPEC §8.
# shellcheck disable=SC2086
forbid          groved "backend lane must not render" $RENDER_CRATES
# shellcheck disable=SC2086
forbid_declared groved "backend lane must not render" $RENDER_CRATES

# The UI lane's own test harness must not become a backdoor into the backend.
# shellcheck disable=SC2086
forbid          grove-fakedaemon "UI-lane tooling must not reach backend concerns" \
    $BACKEND_CRATES $GIT_CRATES $PTY_CRATES
# shellcheck disable=SC2086
forbid_declared grove-fakedaemon "UI-lane tooling must not reach backend concerns" \
    $BACKEND_CRATES $GIT_CRATES $PTY_CRATES

# Domain types are pure. A deny-list cannot express that, so enumerate instead.
# Note: this constrains crate choices only. Std-only I/O (std::fs, std::net,
# std::process) is invisible to any dependency-graph audit and must be caught
# in review.
allow_only grove-domain "domain types must stay I/O-free" \
    grove-domain serde serde_core serde_derive proc-macro2 quote syn unicode-ident

# The protocol carries domain types; it does not implement behaviour.
# shellcheck disable=SC2086
forbid          grove-proto "the contract must not embed implementations" \
    grove-git grove-state grove-lua $RENDER_CRATES $PTY_CRATES
# shellcheck disable=SC2086
forbid_declared grove-proto "the contract must not embed implementations" \
    grove-git grove-state grove-lua $RENDER_CRATES $PTY_CRATES

if [ "$fail" -eq 0 ]; then
    note ok "all boundaries hold"
fi
exit "$fail"
