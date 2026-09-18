# Grove — v1 specification

Grove is a terminal UI for managing git worktrees across many repositories, with a
terminal attached to each worktree.

A normal clone shows one branch at a time; `git checkout` rewrites the files in place,
which means switching context costs you a stash, a rebuild and whatever was running in
that directory. A worktree is a second directory from the same clone, checked out at a
different branch, existing simultaneously and sharing one `.git`. Grove makes worktrees
cheap to create, group, work in and dispose of.

---

## 1. Principles

These constrain every decision below. When something in this document is ambiguous,
resolve it with these.

1. **Grove displays only what git — plus the filesystem, for sizes — can actually
   answer.** No inferred progress, no scraped output, no invented metadata. If git
   cannot produce it, grove does not show it.
2. **Git is authoritative and read live.** Worktree paths, branches, ahead/behind,
   dirty state and merge status are computed on demand, never cached to disk. Grove's
   own state file can never contradict git, because it does not store anything git
   knows.
3. **Only one action destroys anything.** `end session` removes worktrees. Everything
   else — detach, close, release, quit, reboot — leaves the filesystem alone.
4. **Defaults cannot lose work.** Any pre-selection grove makes (notably prune) must be
   provably safe. Unsafe items stay visible with their reason, selectable only by hand.
5. **Six screens, one keymap.** New functionality becomes a palette command, not a
   seventh screen.

---

## 2. Domain model

```
workspace  ──  a filesystem path; the scan root and the key sessions are stored under
  └── repo           any directory under the workspace containing .git
        └── worktree  a (repo, branch) checkout on disk
              └── terminal   zero or one pty, cwd = the worktree path
  └── session        a named, mutable set of member repos + owned worktrees
```

### 2.1 Workspace

`grove [path]`, defaulting to the current directory. The path **is** the workspace. It
serves as:

- the root grove walks to discover repositories,
- the key under which that workspace's sessions are stored.

Two workspaces never see each other's sessions. There is no global repo registry and no
`roots` setting — the invocation path replaces both.

### 2.2 Repo

Discovered by walking the workspace for directories containing `.git`, honouring an
`ignore` glob list. The walk result is held in memory for the process lifetime and
refreshed on demand; nothing about repos is written to disk.

A repo's base branch comes from `git symbolic-ref refs/remotes/origin/HEAD`. A repo
whose default is `origin/develop` therefore works with no configuration.

### 2.3 Session

A session has a name, a set of **member repos**, and a set of **owned worktrees**.

- **Membership is explicit.** You add a repo to the session. A member may hold zero
  worktrees — that is a valid and expected state, meaning "this task touches this repo,
  I haven't branched yet."
- **Ownership is separate from membership.** A session owns a worktree if it created it
  or adopted it.
- Exactly **one session is open at a time**. Many may be stored.

Branches are free per worktree. One repo may hold several owned worktrees at different
branches inside a single session. There is no shared session branch.

#### Session states

| Glyph | State | Terminals | Worktrees on disk |
|---|---|---|---|
| `●` | attached | alive | present |
| `◐` | detached | alive | present |
| `○` | closed | dead | present |
| — | ended | dead | **removed** |

Transitions:

```
●  attached ──detach──> ◐ detached      terminals keep running
●  attached ──close───> ○ closed        terminals killed, worktrees kept
◐○ ──────────resume───> ● attached      respawns shells if it was closed
any ─────────end──────> gone            worktrees removed (only destructive act)
```

A reboot with no snapshot puts every session in `○ closed`: the daemon died with every
pty, the worktrees are untouched on disk, and the grouping survives in the state file.

### 2.4 Worktree

Identified by `(repo, branch)`. Its path comes from the `worktree_path` template,
default `~/grove/{repo}/{branch_slug}`.

Ownership has four cases, and the WORKTREES pane distinguishes all of them:

| Row | Meaning | Actions |
|---|---|---|
| owned by this session | created or adopted here | full |
| owned by another session | another session's | read-only |
| unowned | made by hand, or orphaned | `^g a` adopt |
| the clone itself | `~/code/<repo>` | **never ownable** |

The clone being structurally unownable is what makes `end session` safe: it cannot
delete your main checkout.

`^g r` releases an owned worktree back to unowned without removing it.

> **Template constraint.** If `worktree_path` contains `{session}`, adopt and release
> would require `git worktree move`, rewriting the path under any running process.
> Grove refuses adopt/release in that configuration and says why, rather than moving
> files silently.

### 2.5 Terminal

Zero or one pty per worktree, spawned on demand, cwd set to the worktree path, running
`config.shell`. Killing a terminal does not remove its worktree.

The scratch shell is separate: not attached to any worktree, cwd `config.scratch_cwd`
(default `$HOME`).

---

## 3. Keyboard

### 3.1 The prefix rule

`^g` is the prefix. **A focused pane that is a pty takes every keystroke; `^g` is how
you address grove instead.**

Derived from that, and worth stating explicitly:

- The REPOS and WORKTREES panes are lists, not ptys. When focus is on them, arrows and
  list keys are handled by grove directly — there is nothing to send them to.
- Overlays that are not ptys (palette, picker, diff, end) take keys directly. You type
  into the palette without a prefix.
- The two ptys — the worktree terminal and the scratch shell — require `^g` to escape.

This keeps the prefix where it earns its keep and out of the way everywhere else.

### 3.2 Global bindings

| Key | Action |
|---|---|
| `^g /` | command palette |
| `^g s` | session picker |
| `^g d` | diff (read-only) |
| `^g i` | scratch shell |
| `^g n` | palette, prefilled `new ` |
| `^g X` | end session — destructive, confirms |
| `^g S` | snapshot the current session |
| `^g ?` | help overlay for the current screen |
| `^g q` | quit the TUI; daemon and terminals keep running |

### 3.3 Dash bindings

| Key | Action |
|---|---|
| `^g tab` / `^g shift-tab` | cycle focus REPOS → WORKTREES → terminal |
| `^g 1` `^g 2` `^g 3` | toggle visibility of each pane |
| `↑` `↓` | move within the focused list |
| `^g a` | adopt the selected unowned worktree |
| `^g r` | release the selected owned worktree |
| `^g o` | open the selected worktree in `config.editor` |

Focus is functional, not decorative: `↑`/`↓` drive whichever list currently has focus.

### 3.4 Picker bindings

Raw keys, no prefix — the picker is not a pty.

| Key | Action |
|---|---|
| `↑` `↓` | move |
| `enter` | resume (open, replacing the current session) |
| `d` | detach the highlighted session |
| `c` | close the highlighted session |
| `X` | end the highlighted session |
| `esc` | back to dash |

---

## 4. Screens

Six. Everything else is a palette command.

### 4.1 Dash

```
┌─ REPOS ────────┬─ WORKTREES ────────────────┬─ billing-service · feat/ABC-4471 ─┐
│ ● billing-svc 2│ ▣ feat/ABC-4471   ↑4 ↓2  2h│ $ pnpm test billing/invoice       │
│ ● web-app    1 │ ◇ fix/ABC-4402    ↑1     1d│  PASS  src/invoice/split.test.ts  │
│ ● sdk-js     1 │ ◇ spike/perf             4d│  FAIL  src/invoice/proration.ts   │
│ ○ search-idx ·│ ▣ main            ↓11  now│    expected 4 items, received 3    │
└────────────────┴────────────────────────────┴───────────────────────────────────┘
 ^g tab pane  ^g 1-3 hide  ^g s sessions  ^g d diff  ^g / palette   session: invoice split
```

Three panes, each hideable, at least one always visible.

**REPOS** — the session's member repos. Dot shows worktree presence and dirtiness;
trailing count is the number of worktrees grove can see in that repo. A member with
zero worktrees shows `·`.

**WORKTREES** — every worktree of the selected repo, regardless of owner, plus the
clone. Columns: ownership glyph · branch · `↑ahead` · `↓behind` · age. A stale-refs
marker appears when the underlying refs have aged past the threshold.

Glyphs: `▣` has a terminal · `◆` dirty, no terminal · `◇` clean, no terminal.

**Terminal** — the live pty of the currently selected worktree, following the WORKTREES
cursor. This is a real terminal, not a preview: with focus on it, keystrokes go to the
pty.

#### Empty state

When the workspace has no repos or the session has nothing in it, the dash renders
guidance in place of empty panes rather than a blank grid:

```
┌─ REPOS ────────┬─ WORKTREES ──────────────────────────┐
│                │  grove is empty                      │
│  no repos      │                                      │
│  registered    │  1  point grove at your clones       │
│                │     ^g /  scan                       │
│                │  2  add repos to this session        │
│                │     ^g /  add <repo>                 │
│                │  3  name a branch                    │
│                │     ^g /  new <branch>               │
└────────────────┴──────────────────────────────────────┘
```

### 4.2 Command palette — `^g /`

The palette is grove's command line. It takes arguments inline and turns its row list
into whatever picker the command needs.

```
❯ new feat/ABC-4471-invoice-split
  base: origin/main                        (from origin/HEAD)
  ──────────────────────────────────────────────────────────
  [x] billing-service   member
  [x] web-app           member
  [x] sdk-js            member
  [ ] api-gateway       workspace
  [ ] design-system     workspace
  space toggle · enter create 3 · esc cancel
```

With no argument typed it lists available commands and fuzzy-filters as you type.

### 4.3 Session picker — `^g s`

```
 session ❯                                                    6 sessions
 ● invoice split    billing-service, web-app, sdk-js    attached · 4 terminals
 ● retry jitter     billing-service, api-gateway        attached · 1 terminal
 ◐ tokens review    design-system, web-app              detached 2d
 ◐ vite 7 bump      web-app                             detached 3d
 ○ tax codes        3 repos                             closed · 794 MB
 ○ grpc spike       api-gateway                         closed · 188 MB
 enter resume   d detach   c close   X end                          esc close
```

Resuming replaces the open session. The outgoing session becomes `◐ detached` — or is
dropped silently if it is the unnamed launch session with no worktrees.

### 4.4 Diff — `^g d`

Read-only. File list plus unified hunks for the selected worktree against its base.

```
┌ diff · billing-service · feat/ABC-4471-invoice-split vs origin/main   +412  −137 ┐
│ M src/invoice/split.ts      +184 − 22 │ @@ -198,12 +198,26 @@ export function s │
│ M src/invoice/proration.ts   +96 − 41 │    const items = invoice.lineItems;      │
│ A src/invoice/__tests__/…    +74      │ -  return items.map(toLine);             │
│ M src/invoice/types.ts       +31 −  8 │ +  const boundary = cycleBoundary(inv);  │
│ D src/legacy/prorate.ts          − 26 │ +  const [before, after] = partition(…); │
│ ? .env.local                          │ +  if (!after.length) return before…     │
│ ↑↓ file                                                              esc close   │
└──────────────────────────────────────────────────────────────────────────────────┘
```

No staging, no committing, no cross-repo diff. Those are git write-verbs and live in the
worktree's own shell.

### 4.5 Scratch shell — `^g i`

A pty not attached to any worktree, cwd `config.scratch_cwd` (default `$HOME`). Being a
pty, it needs `^g` to escape. `esc` is passed through to the shell.

### 4.6 End session — `^g X`

The only destructive screen. Per-repo consequences first, aggregate second, loss warning
last, and it requires a deliberate confirm.

```
 end session      invoice split                    4 terminals · 3 worktrees · 794 MB
 ◆ billing-service   ↑4 pushed · 9 files uncommitted · 286 MB          will be lost
 ● web-app           ↑2 pushed · clean · 412 MB
 ◐ sdk-js            ↑1 pushed · install running · 96 MB               terminal busy

 closes 4 terminals and removes all 3 worktrees · 794 MB reclaimed
 9 uncommitted files in billing-service will be lost

 enter remove everything                                                  esc cancel
```

Worktrees owned by other sessions, and the clone, are never listed and never touched.

---

## 5. Palette commands

| Command | Effect |
|---|---|
| `scan` | re-walk the workspace for repos |
| `session new <name>` | create and switch to a named session |
| `session rename <name>` | rename the current session |
| `open <session>` | open a stored session, replacing the current |
| `add <repo>…` | add member repos to the current session |
| `remove <repo>` | drop a member repo (refuses while it owns worktrees) |
| `new <branch>` | create worktrees — repo multi-select follows |
| `fetch [repo]` | fetch member repos, or one named repo |
| `prune` | cleanup picker, see below |
| `snapshot` | write a restorable snapshot of the current session |
| `defaults` | show and edit config values |
| `keys` | help overlay |

### Prune

Pre-checks a row **only** if all of these hold:

- merged into its base **or** its upstream is gone, **and**
- the working tree is clean, **and**
- nothing is unpushed, **and**
- no attached or detached session owns it.

Everything else is listed with its disqualifying reason visible and unchecked. It can
still be ticked deliberately.

```
❯ prune                                              4 selected · 1.0 GB
 [x] web-app       feat/ABC-3980   merged    clean      412 MB
 [x] billing-svc   feat/ABC-3980   merged    clean      286 MB
 [x] api-gateway   spike/grpc      gone      clean      188 MB
 [x] data-pipeline chore/airflow   merged    clean      154 MB
 [ ] sdk-js        feat/ABC-3980   merged    2 files     96 MB
 [ ] web-app       wip/checkout    ↑6        31 files   404 MB
 [ ] design-system feat/ABC-4110   unmerged  clean      112 MB
 [ ] billing-svc   feat/ABC-4471   ● owned by invoice split
 space toggle · a all safe · enter prune 4
```

Prune fetches its candidate repos first, so "merged" reflects the remote.

---

## 6. Persistence

```
~/.config/grove/config.toml                      user settings, hand-edited
~/.local/state/grove/<workspace-hash>/
    sessions.json                                written automatically
    snapshots/<session>.json                     written only by ^g S
$XDG_RUNTIME_DIR/grove/<workspace-hash>.sock     daemon socket
```

**`sessions.json`** holds only what git cannot know: session name, member repos, owned
worktrees (as `repo` + `branch`), and state. Written whenever it changes. Deleting it
costs you grouping, never work — the worktrees remain and prune can still find them.

**Snapshots** are manual, mirroring tmux-resurrect in manual mode. A snapshot records
each terminal, its worktree and its last command. Restoring recreates the terminals at
the right worktrees; commands are not re-executed automatically.

Nothing is written on a timer.

---

## 7. Git operations

The complete set grove performs. Anything not on this list, grove does not do.

**Read**

| Purpose | Command |
|---|---|
| discover repos | walk for `.git` |
| list worktrees | `git -C <repo> worktree list --porcelain` |
| base branch | `git -C <repo> symbolic-ref refs/remotes/origin/HEAD` |
| dirty state | `git -C <wt> status --porcelain` |
| ahead / behind | `git -C <wt> rev-list --left-right --count @{upstream}...HEAD` |
| merged | `git -C <repo> merge-base --is-ancestor <branch> <base>` |
| upstream gone | absent remote ref after `fetch --prune` |
| diff | `git -C <wt> diff <base>...HEAD --numstat` + patch per file |
| size | filesystem walk of the worktree path |

**Write**

| Purpose | Command |
|---|---|
| create | `git -C <repo> worktree add <path> -b <branch> <base>` |
| create from existing branch | `git -C <repo> worktree add <path> <branch>` |
| remove | `git -C <repo> worktree remove <path>` |
| refresh refs | `git -C <repo> fetch --prune` |

Grove never pushes, rebases, stages, commits or deletes branches.

**Fetch policy.** On session open, member repos only, in parallel. Otherwise on demand
via `fetch`. Rows carry a staleness marker once refs age past the threshold.

**Error handling.** `worktree add` failing because the branch is already checked out
elsewhere is surfaced with the conflicting path, and grove offers to adopt that existing
worktree instead.

---

## 8. Architecture

```
grove (TUI)  <──unix socket──>  groved (daemon)
  ratatui                         ├─ pty  billing-service @ feat/ABC-4471
  crossterm                       ├─ pty  web-app @ feat/ABC-4471
  tui-term  ─ widget              ├─ pty  sdk-js @ feat/ABC-4471
  vt100     ─ screen + scrollback └─ pty  scratch
  portable-pty ─ spawn
```

Crates: `ratatui`, `crossterm`, `tui-term`, `vt100`, `portable-pty`, plus `gix` or
shelling out to `git`.

`tui-term` abstracts the terminal backend behind traits, with `vt100` as the default
implementation — so the backend can be swapped for `alacritty_terminal` or `avt` later
without touching the rendering layer. `wezterm-term` is not an option: it is
deliberately unpublished to crates.io with no API stability guarantee.

**Daemon.** Owns every pty. Survives the TUI exiting, does not survive a reboot. A
second `grove` on the same workspace attaches to the existing daemon rather than
starting a second one.

**Protocol.** Session list and state; spawn, kill and resize ptys; input to a pty;
output streaming; attach and detach; snapshot. Nothing on this list ships in a crate —
budget for it accordingly.

---

## 9. Configuration

```toml
# ~/.config/grove/config.toml

shell           = "/usr/bin/fish"          # default: $SHELL
editor          = "$EDITOR"                # or e.g. "cursor {path}"
scratch_cwd     = "~"
scrollback      = 10000                    # lines per pty
worktree_path   = "~/grove/{repo}/{branch_slug}"
branch_template = "{type}/{ticket}-{slug}" # prefills `new`, never enforced
stale_after     = "4h"                     # when to mark refs stale
ignore          = ["**/node_modules", "**/.cache"]

[theme]
accent  = "#fab387"
clean   = "#a6e3a1"
dirty   = "#f9e2af"
error   = "#f38ba8"
muted   = "#7f849c"
corners = "rounded"   # rounded ╭ · square ┌
density = "airy"      # airy · compact
```

Truecolor when the terminal supports it, degrading to the nearest ANSI colour when it
does not. Defaults are Catppuccin Mocha, matching the source design.

`worktree_path` placeholders: `{repo}`, `{branch}`, `{branch_slug}`, `{session}`,
`{clone_parent}`. Using `{session}` disables adopt and release — see §2.4.

---

## 10. Not in v1

Deliberately excluded, with the reasoning, so these don't get relitigated by accident.

| Cut | Why |
|---|---|
| Bulk git (push / rebase / stage across repos) | every worktree already has a shell; partial-failure UI is large |
| Progress percentages per worktree | git cannot answer it; the source mock faked it |
| Any GitHub or forge integration | needs auth, rate limits, offline states; prune's safety rule does not require it |
| Multiple terminals per worktree, splits, zoom | a full multiplexer; one pty per worktree covers the use case |
| Two sessions open simultaneously | one open, many stored |
| Kanban board, outline tree, tiled-terminal, worktree-detail screens | already cut between mock v5 and v6; redundant with the dash |
| Automatic snapshots or timed writes | snapshots are manual, as in tmux-resurrect |

---

## 11. Risks

**The daemon is the bulk of v1.** Declining tmux means pty multiplexing, resize,
reflow, detach/reattach and scrollback are grove's own. `tui-term` and `vt100` cover
parsing and rendering; the socket protocol, process supervision and reattach handshake
are not in any crate. Expect most of the effort here, all of it before a single worktree
is managed.

**`tui-term` is work-in-progress.** It is used in production by Turborepo, which has
upstreamed `vt100` performance work for its code path, but it carries no stability
promise. Its backend traits are the mitigation: swapping to `alacritty_terminal` or
`avt` should not reach the rendering layer.

**The value proposition is narrow after the cuts.** What remains is "many worktrees,
each with a terminal, grouped and switchable." That competes directly with tmux plus a
shell function, so it has to win on the dashboard and on switching alone. This is the
argument for building the daemon well, and for measuring v1 against how fast a context
switch actually feels.
