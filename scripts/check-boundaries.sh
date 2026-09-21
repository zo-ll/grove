#!/usr/bin/env bash
# Enforce the crate boundaries from SPEC.md §8 and issue #31.
#
# The two lanes are kept apart by the dependency graph, not by discipline. A
# violation fails CI rather than waiting to be noticed in review.
#
# Two complementary checks:
#
#   1. declared  — what EVERY workspace member asks for, in every dependency
#                  section, optional or not. This is the load-bearing check.
#                  It needs no feature resolution, so it sees optional deps
#                  whether or not anything enables them, and it covers dev-
#                  and build-dependencies, which no dependency tree shows.
#   2. transitive — what the two binaries can actually reach. Catches indirect
#                  reach through a crate that is itself allowed.
#
# Why the declared check must cover every member, not just the binaries:
# `cargo tree -p X --all-features` enables X's OWN optional dependencies only,
# never those of X's dependencies. An optional `gix` declared inside
# grove-proto is therefore invisible to `cargo tree -p grove --all-features`
# while `cargo clippy --workspace --all-features` compiles it. Declaration is
# the only place that cannot hide.
#
# The script fails CLOSED: any tooling error is a failure, never a silent pass.

set -uo pipefail

fail=0
note() { printf '  %-6s %s\n' "$1" "$2"; }
die()  { note FAIL "$1"; fail=1; }

# --- crate families ---------------------------------------------------------

BACKEND_CRATES="grove-git grove-state"
GIT_CRATES="gix git2 gitoxide-core libgit2-sys"
PTY_CRATES="portable-pty pty-process nix-pty"
RENDER_CRATES="ratatui crossterm tui-term termion vt100"
UI_ONLY_CRATES="ratatui crossterm tui-term termion"

# A typo that empties one of these would make its rules vacuous.
for v in BACKEND_CRATES GIT_CRATES PTY_CRATES RENDER_CRATES UI_ONLY_CRATES; do
    if [ -z "${!v:-}" ]; then
        die "$v is empty — a rule would pass vacuously"
    fi
done

# --- metadata ---------------------------------------------------------------

METADATA=""
load_metadata() {
    local rc
    METADATA="$(cargo metadata --no-deps --format-version 1 2>/dev/null)"
    rc=$?
    if [ "$rc" -ne 0 ]; then
        die "cargo metadata failed (exit $rc) — refusing to pass"
        return 1
    fi
    if ! jq -e . >/dev/null 2>&1 <<<"$METADATA"; then
        die "jq could not parse cargo metadata (is jq installed?) — refusing to pass"
        return 1
    fi
}

members() {
    jq -r '.packages[].name' <<<"$METADATA" 2>/dev/null | sort
}

# Every rule registers the crate it targets, so coverage can be checked in both
# directions from one source of truth rather than a hand-maintained list.
RULED=""
target() {
    local pkg="$1"
    if ! grep -qxF -- "$pkg" <<<"$(members)"; then
        die "a rule targets $pkg, which is not a workspace member (typo?)"
        return 1
    fi
    RULED="$RULED$pkg"$'\n'
}

# name<TAB>kind for every declared dependency of a member, every section,
# optional or not.
declared_of() {
    jq -r --arg p "$1" \
        '.packages[] | select(.name == $p) | .dependencies[] | "\(.name)\t\(.kind // "normal")"' \
        <<<"$METADATA" 2>/dev/null
}

# --- 1. declared ------------------------------------------------------------

deny_declared() {
    local pkg="$1" why="$2"; shift 2
    target "$pkg" || return
    local rows name kind f
    rows="$(declared_of "$pkg")" || { die "could not read declared deps of $pkg"; return; }
    while IFS=$'\t' read -r name kind; do
        [ -n "$name" ] || continue
        for f in "$@"; do
            [ "$name" = "$f" ] && die "$pkg declares $f as a $kind-dependency ($why)"
        done
    done <<<"$rows"
}

# Allow-list by DECLARATION, not by transitive closure. A closure allow-list
# breaks whenever an upstream crate restructures (serde splitting out
# serde_core is exactly that shape) and makes "edit the enforcement script"
# the runbook for a legitimate change.
allow_declared() {
    local pkg="$1" why="$2"; shift 2
    target "$pkg" || return
    local rows name kind f ok
    rows="$(declared_of "$pkg")" || { die "could not read declared deps of $pkg"; return; }
    while IFS=$'\t' read -r name kind; do
        [ -n "$name" ] || continue
        ok=0
        for f in "$@"; do [ "$name" = "$f" ] && ok=1; done
        [ "$ok" -eq 1 ] || die "$pkg declares $name as a $kind-dependency; only [$*] are allowed ($why)"
    done <<<"$rows"
}

# --- 2. transitive ----------------------------------------------------------

deps_of() {
    local pkg="$1" out err rc
    err="$(mktemp)"
    out="$(cargo tree -p "$pkg" --edges normal --all-features --prefix none 2>"$err")"
    rc=$?
    if [ "$rc" -ne 0 ]; then
        note FAIL "cargo tree failed for $pkg (exit $rc) — refusing to pass" >&2
        sed 's/^/         /' "$err" >&2
        rm -f "$err"
        return 1
    fi
    rm -f "$err"
    # Only lines shaped like a cargo-tree package record ("name v1.2.3").
    # Parsing every non-blank line turned cargo chatter such as
    # "Blocking waiting for file lock on package cache" into a phantom
    # package named "Blocking", which failed the allow-list spuriously
    # whenever two cargo commands ran concurrently.
    local names
    names="$(printf '%s\n' "$out" | awk '$2 ~ /^v[0-9]/ {print $1}' | sort -u)"
    if [ -z "$names" ]; then
        note FAIL "cargo tree produced no package records for $pkg — refusing to pass" >&2
        return 1
    fi
    printf '%s\n' "$names"
}

deny_reach() {
    local pkg="$1" why="$2"; shift 2
    target "$pkg" || return
    local deps f
    if ! deps="$(deps_of "$pkg")"; then fail=1; return; fi
    for f in "$@"; do
        grep -qxF "$f" <<<"$deps" && die "$pkg must not reach $f ($why)"
    done
}

# ---------------------------------------------------------------------------

echo "checking crate boundaries"
load_metadata || exit 1

# --- declared rules, applied to EVERY member --------------------------------
# shellcheck disable=SC2086
{
# Domain types are pure. Allow-list its declarations; a deny-list cannot
# express purity, and a closure allow-list cannot survive an upstream bump.
# Note: this constrains crate choices only. Std-only I/O (std::fs, std::net,
# std::process) is invisible to any dependency audit and must be caught in
# review.
# serde_json is permitted deliberately: it is in-memory value formatting with
# no I/O of its own, intended for round-trip serialization tests. An earlier
# revision denied it by name; this is the considered position. (grove-domain
# does not declare it yet — the allowance is prospective.)
allow_declared grove-domain "domain types must stay I/O-free" \
    serde serde_json

deny_declared grove-proto "the contract must not embed implementations" \
    $BACKEND_CRATES $GIT_CRATES $PTY_CRATES $RENDER_CRATES grove-lua

deny_declared grove-lua "the shared runtime must not take a lane's side" \
    $BACKEND_CRATES $GIT_CRATES $PTY_CRATES $RENDER_CRATES

deny_declared grove-git "backend lane must not render" \
    $RENDER_CRATES

deny_declared grove-state "backend lane must not render or touch git directly" \
    $RENDER_CRATES $GIT_CRATES $PTY_CRATES

deny_declared grove "UI lane must not reach backend concerns" \
    $BACKEND_CRATES $GIT_CRATES $PTY_CRATES

deny_declared groved "daemon may parse terminals but must not render UI chrome" \
    $UI_ONLY_CRATES

deny_declared grove-fakedaemon "UI-lane tooling must not reach backend concerns" \
    $BACKEND_CRATES $GIT_CRATES $PTY_CRATES

allow_declared grove-cli "the non-interactive client speaks only the shared contract" \
    grove-domain grove-proto
}

# Coverage, both directions. members -> rules catches a new crate arriving with
# no rule; rules -> members is enforced by target() at each call site and catches
# a typo'd rule target, which would otherwise no-op silently and drop that
# crate's rule while the script still exited 0.
while read -r m; do
    [ -n "$m" ] || continue
    grep -qxF -- "$m" <<<"$RULED" || die "$m has no declared boundary rule — add one"
done < <(members)

# --- transitive rules, for reach through allowed crates ---------------------
# shellcheck disable=SC2086
{
deny_reach grove "UI lane must not reach backend concerns" \
    $BACKEND_CRATES $GIT_CRATES $PTY_CRATES grove-fakedaemon
deny_reach groved "daemon may parse terminals but must not render UI chrome" $UI_ONLY_CRATES
deny_reach grove-fakedaemon "UI-lane tooling must not reach backend concerns" \
    $BACKEND_CRATES $GIT_CRATES $PTY_CRATES
deny_reach grove-cli "CLI must not reach either implementation lane" \
    $BACKEND_CRATES $GIT_CRATES $PTY_CRATES $RENDER_CRATES grove-lua grove-fakedaemon groved grove
deny_reach grove-proto "the contract must not embed implementations" \
    $BACKEND_CRATES $GIT_CRATES $PTY_CRATES $RENDER_CRATES grove-lua
deny_reach grove-domain "domain types must stay I/O-free" \
    $BACKEND_CRATES $GIT_CRATES $PTY_CRATES $RENDER_CRATES mlua tokio reqwest
}

if [ "$fail" -eq 0 ]; then
    note ok "all boundaries hold"
fi
exit "$fail"
