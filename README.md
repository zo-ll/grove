# grove

A terminal UI for managing git worktrees across many repos, with a terminal attached to
each worktree.

```
┌─ REPOS ────────┬─ WORKTREES ────────────────┬─ billing-service · feat/ABC-4471 ─┐
│ ● billing-svc 2│ ▣ feat/ABC-4471   ↑4 ↓2  2h│ $ pnpm test billing/invoice       │
│ ● web-app    1 │ ◇ fix/ABC-4402    ↑1     1d│  PASS  src/invoice/split.test.ts  │
│ ● sdk-js     1 │ ◇ spike/perf             4d│  FAIL  src/invoice/proration.ts   │
│ ○ search-idx  ·│ ▣ main            ↓11  now │    expected 4 items, received 3   │
└────────────────┴────────────────────────────┴───────────────────────────────────┘
 ^g tab pane  ^g 1-3 hide  ^g s sessions  ^g d diff  ^g / palette   session: invoice split
```

> **Status: specification only.** There is no code yet. This repository currently holds
> the v1 spec and a design brief. See [SPEC.md](SPEC.md).

## Why

A clone shows one branch at a time. `git checkout` rewrites the files in place, so
switching context costs you a stash, a rebuild, and whatever was running in that
directory — your test watcher, your dev server, your language server's index.

A **worktree** is a second directory from the same clone, checked out at a different
branch, existing at the same time. They share one `.git`, so history is stored once and
creating one is fast:

```
~/code/billing-service                    main           ← the clone
~/grove/billing-service/feat-ABC-4471     feat/ABC-4471  ← worktree
~/grove/billing-service/fix-ABC-4402      fix/ABC-4402   ← worktree
```

All three are real directories with real files, simultaneously. Nothing you do in one
disturbs another.

Git gives you the primitive and stops there. It won't tell you which worktrees you have
across thirty repos, which are stale, which still hold uncommitted work, or which of
them belong to the thing you were doing on Tuesday. It won't keep a shell alive in each
one. And because a worktree carries its own `node_modules` and build artifacts, they
quietly cost gigabytes until you go looking.

Grove is the layer over that: discover, group, work in, and dispose of worktrees, with
one terminal per worktree that survives you closing the app.

## What it does

- **Workspace-scoped.** `grove [path]`, defaulting to the current directory. The path is
  the repo scan root and the key its sessions live under. Two workspaces never see each
  other's sessions.
- **Sessions.** A named set of member repos and the worktrees it owns. Branches are free
  per worktree, so one repo can hold several at once. Sessions detach with their
  terminals still running, close with the worktrees kept, or end — the only thing that
  deletes anything.
- **A terminal per worktree**, owned by a background daemon, alive across the TUI
  exiting.
- **Six screens, one keymap**: dash, command palette, session picker, read-only diff,
  scratch shell, end-session. New functionality becomes a palette command, not a seventh
  screen.
- **Safe cleanup.** Prune pre-selects a worktree only when it is merged-or-gone, clean,
  fully pushed, and unowned. Everything else is listed with the reason it was skipped.

## Design rules

Two constraints do most of the work in the spec:

**Grove displays only what git — plus the filesystem, for sizes — can answer.** No
inferred build progress, no scraped terminal output, no PR or CI state. If git cannot
produce it, grove does not show it.

**Git is authoritative and read live.** Worktree paths, branches, ahead/behind, dirty
state and merge status are computed on demand and never cached. Grove's own state file
stores only what git cannot know — session names, membership, ownership — so it can
never contradict reality. Delete it and you lose grouping, never work.

## Planned stack

Rust · [ratatui](https://github.com/ratatui/ratatui) ·
[tui-term](https://github.com/a-kenji/tui-term) ·
[vt100](https://github.com/doy/vt100-rust) · portable-pty, with a `groved` daemon
owning the ptys over a unix socket.

## Not in v1

Bulk git across repos (push, rebase, stage), any GitHub or forge integration, progress
percentages, multiple terminals per worktree, and more than one session open at a time.
Each exclusion and its reasoning is recorded in [SPEC.md](SPEC.md#10-not-in-v1).

## Repository

| Path | What |
|---|---|
| [SPEC.md](SPEC.md) | the v1 specification — domain model, keymap, screens, git operations, architecture |
| [docs/design-brief.md](docs/design-brief.md) | brief for realigning the UI mock to the spec |
