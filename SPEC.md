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
   cannot produce it, grove does not show it. The one carve-out is a column the user
   registers themselves in Lua (§10): grove still doesn't know what a PR is, the user
   opted in, and the column is visibly theirs.
2. **Git is authoritative and read live.** Worktree paths, branches, ahead/behind,
   dirty state and merge status are computed on demand, never cached to disk. Grove's
   own state file can never contradict git, because it does not store anything git
   knows.
3. **Two actions destroy anything.** `end session` removes its worktrees, and the
   prune picker removes worktrees you tick. Everything else — detach, close, release,
   quit, reboot — leaves the filesystem alone.
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
any ─────────end──────> gone            worktrees removed (one of two destructive acts)
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
trailing count is the number of branched worktrees grove can see in that repo, the
clone excluded. A member with zero worktrees shows `·`.

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
still be ticked deliberately — with one exception: a worktree a live session owns is
refused at prune time (§2.4: another session's worktrees are read-only), because
ending that session is how its worktrees are released.

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
~/.config/grove/config.lua                       user settings + extensions
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
| branch age | `git -C <wt> show -s --format=%ct HEAD` |
| ahead / behind | `git -C <wt> rev-list --left-right --count @{upstream}...HEAD` |
| merged | `git -C <repo> merge-base --is-ancestor <branch> <base>` |
| upstream gone | absent remote ref after `fetch --prune` |
| diff | `git -C <wt> diff <base>...HEAD --numstat --name-status` + patch for the selected file |
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
  mlua ─ UI-side VM               mlua ─ daemon-side VM
```

Crates: `ratatui`, `crossterm`, `tui-term`, `vt100`, `portable-pty`, `mlua`, plus `gix`
or shelling out to `git`.

**Two Lua VMs, one config file.** A Lua value cannot cross a process boundary, so each
binary evaluates `config.lua` in its own VM and each exposes a *different* `grove`
module — see §10.4. This is what makes the config split structural rather than a
convention: the TUI's VM has no `grove.on("worktree_created")` to call, because the TUI
does not create worktrees.

`tui-term` abstracts the terminal backend behind traits, with `vt100` as the default
implementation — so the backend can be swapped for `alacritty_terminal` or `avt` later
without touching the rendering layer. `wezterm-term` is not an option: it is
deliberately unpublished to crates.io with no API stability guarantee.

**Daemon.** Owns every pty. Survives the TUI exiting, does not survive a reboot. A
second `grove` on the same workspace attaches to the existing daemon rather than
starting a second one.

**Protocol.** Nothing on this list ships in a crate — budget for it accordingly.

The message set is derived from §4's screens, not from this summary. An earlier
revision listed only the session and pty operations, and the protocol built to it
could not carry what the dash, prune picker or diff screen render — a gap that
survived review because the reviewer was asked whether the code matched this
paragraph rather than whether it matched the screens.

- **Handshake** — a version exchange before anything else, refused on mismatch
  rather than guessed at, checked from both sides.
- **Sessions** — list and state; create, rename, open, detach, close, end. A
  session row carries its member repos, live terminal count, how long it has
  held its state and the disk its worktrees hold, because §4.3 renders all four.
- **Repos** — the workspace's repos with worktree counts, dirtiness, and where
  each base branch came from, for the REPOS pane and §4.2's provenance line.
  Re-scanning the workspace is a request of its own.
- **Worktrees** — every worktree of one repo regardless of owner, plus the clone,
  each carrying its ownership, ahead/behind, **uncommitted file count**, age,
  size, attached terminal and foreground command, and whether its refs are
  stale. A count rather than a flag, because §4.6 warns "9 uncommitted files
  will be lost" before the only destructive action in the product.
- **Prune candidates** — with what makes each prunable *and* the reasons it is
  not safe to remove, so the client renders both columns rather than inferring
  one from the absence of the other. Pruning itself is a request, and because
  an unsafe row can still be ticked deliberately the daemon does not overrule
  the selection — it attempts each and reports per-row outcomes. The exception
  is a worktree a live session owns, which ending that session is for.
- **Diff** — file list for a worktree, and hunks for one selected file. Moving
  the cursor re-requests, so opening the screen does not pay for every patch.
- **Terminals** — spawn against a worktree *or* unattached for §4.5's scratch
  shell; kill, resize; input; output streaming; attach and detach. A spawn is
  answered with the new terminal's id, without which the client cannot attach to
  what it just created, and the live terminals can be listed so a reattaching
  client finds the scratch shell again.
- **Editor** — open a worktree in the configured editor. The daemon runs it: it
  holds the configuration, and the TUI does not touch the filesystem. The daemon
  has no terminal to give it, so a full-screen editor launched this way exits
  at once with nowhere to complain: name a graphical editor, or a command that
  does not need a tty — an editor that must own a terminal belongs in the
  worktree's own shell.
- **Snapshot** — save and restore.
- **Fetch** — refresh a session's member repos, per §7's policy. Never on a
  timer.
- **Failures** — typed where the UI must act on them. A branch already checked
  out elsewhere carries the conflicting worktree and path, because §7 requires
  offering to adopt it and prose cannot be adopted.

Whenever a screen gains something to render, this list and `grove-proto` change
together.

---

## 9. Configuration

One file, `~/.config/grove/config.lua`, evaluated by both binaries in separate VMs.

```lua
local grove = require("grove")

grove.setup({
  shell           = "/usr/bin/fish",            -- default: $SHELL
  editor          = os.getenv("EDITOR"),        -- or "cursor {path}"; the daemon
                                                -- has no tty, so this must not
                                                -- need one (see §8)
  scratch_cwd     = "~",
  scrollback      = 10000,                      -- lines per pty
  worktree_path   = "~/grove/{repo}/{branch_slug}",
  branch_template = "{type}/{ticket}-{slug}",   -- prefills `new`, never enforced
  stale_after     = "4h",                       -- when to mark refs stale
  ignore          = { "**/node_modules", "**/.cache" },

  theme = {
    accent  = "#fab387",
    clean   = "#a6e3a1",
    dirty   = "#f9e2af",
    error   = "#f38ba8",
    muted   = "#7f849c",
    corners = "rounded",   -- rounded ╭ · square ┌
    density = "airy",      -- airy · compact
  },
})
```

Truecolor when the terminal supports it. Where it does not, the colours degrade to the
palette the terminal has — and **degradation preserves meaning, not appearance**. The
distinction is not academic: the defaults are Catppuccin Mocha, whose colours are
pastels, and the nearest 16-colour match by any distance metric for `clean`, `dirty`
and `error` alike is plain white. Three roles that render identically satisfy "nearest"
and destroy the encoding §4.1's WORKTREES pane depends on.

So at 16 colours a role's hue chooses the colour, its lightness chooses the bright
variant, and only a colour with too little chroma to have a hue falls back to the
greys. Where two roles still want one slot — a peach accent and a pink error are both
honestly red — the palette is resolved as a set rather than a colour at a time: the
status colours claim first, since `clean`, `dirty` and `error` are read as meaning, and
a displaced role takes the other brightness of its own hue before it takes another
hue's. **At 16 colours, no two roles configured differently may render as the same
colour** — that is the invariant the set resolution exists to hold.

At 256 colours there is room for every role to keep its own value, so each is matched
independently, against the cube and the grey ramp both.

Defaults are Catppuccin Mocha, matching the source design.

`worktree_path` accepts a template string with `{repo}`, `{branch}`, `{branch_slug}`,
`{session}`, `{clone_parent}` — or a function, see §10.2. Using `{session}` disables
adopt and release; see §2.4.

**A broken config must never brick grove.** If `config.lua` throws, grove falls back to
defaults entirely, reports the error on the status bar, and keeps running. It does not
exit and it does not half-apply.

---

## 10. Extension API

Lua is Grove's extension mechanism, not merely its config format. The API is public
from v1, which means renaming a domain field is a breaking change — budget for that.

### 10.1 Events

```lua
grove.on("worktree_created", function(wt)
  -- gitignored files do not exist in a fresh worktree
  grove.copy(wt.clone .. "/.env.local", wt.path .. "/.env.local")
  if grove.exists(wt.path .. "/pnpm-lock.yaml") then
    grove.run(wt.path, "pnpm install --prefer-offline")
  end
  grove.run(wt.path, "direnv allow")
end)
```

This is the highest-value hook in the API. A fresh worktree is unusable until
bootstrapped, and bootstrapping is per-repo and per-person — a template cannot express
it. Without this, every `new` is followed by the same handful of commands typed by hand.

Events: `worktree_created`, `worktree_removed`, `worktree_adopted`, `terminal_spawned`,
`terminal_exited`, `session_opened`, `session_closed`, `session_ended`.

Worktree event payloads have `repo`, `branch`, `path`, `clone`, and `session` fields.
Terminal event payloads have `terminal`, `repo`, `branch`, `path`, and `session` fields.
Session event payloads have `id` and `name` fields. Paths are absolute whenever the
underlying checkout still exists; `worktree_removed.path` is empty when Grove is
forgetting an already-missing checkout.

Hooks for one event run in the order they were registered. An error disables only that
registration; later registrations still run for the current event. `grove.run(cwd,
command)` starts `/bin/sh -lc command` in `cwd` and returns a job id immediately;
completion and failure are reported by the daemon. `grove.sh(command)` runs the same
shell synchronously and returns trimmed stdout — it **blocks the daemon** for the
command's duration, since hooks fire while the service lock is held, so it is for
reading a value and `grove.run` is for anything slow. It is bounded: a command
exceeding the timeout is killed and reported rather than freezing the daemon.
`grove.run` is likewise capped in the number of jobs it will have in flight, so a
runaway hook exhausts itself rather than the daemon's threads and descriptors. `grove.copy(from, to)` copies a file,
`grove.exists(path)` tests path existence, and `grove.send(terminal, bytes)` writes to
the named live terminal.

### 10.2 Computed values

Any setting that takes a string may instead take a function.

```lua
grove.setup({
  worktree_path = function(repo, branch)
    if repo == "monorepo" then
      return "/mnt/nvme/" .. repo .. "/" .. branch   -- 8 GB checkouts
    end
    return "~/grove/" .. repo .. "/" .. branch
  end,
})
```

### 10.3 Registration

```lua
grove.keymap("^g w", function() grove.palette("new ") end)

grove.command("review", function(pr)
  local branch = grove.sh("gh pr view "..pr.." --json headRefName -q .headRefName")
  grove.new_worktree({ repo = grove.current_repo(), branch = branch })
end)

grove.column("pr", function(wt)
  return grove.sh("gh pr view " .. wt.branch .. " --json state -q .state")
end)

grove.repo("monorepo", {
  base  = "origin/develop",
  setup = function(wt) grove.run(wt.path, "make bootstrap") end,
})

grove.session_template("frontend", { repos = { "web-app", "design-system" } })
```

`grove.column` is how forge data gets into Grove without Grove knowing what a forge is
(§11). The column is the user's, and renders as theirs.

### 10.4 Which VM owns what

Both binaries evaluate the same file. Each exposes only the API it can honour, and
silently accepts registrations belonging to the other — so one file works for both
without guards.

| Surface | TUI VM | Daemon VM |
|---|---|---|
| `theme`, `corners`, `density` | ✓ | — |
| `keymap`, `command`, `column`, `session_template` | ✓ | — |
| `shell`, `scrollback`, `scratch_cwd`, `editor` | — | ✓ |
| `worktree_path`, `branch_template`, `ignore`, `stale_after` | — | ✓ |
| `on(...)` lifecycle events | — | ✓ |
| `repo(...)` overrides | theme parts | the rest |

Helpers: `grove.run`, `grove.sh`, `grove.copy`, `grove.exists`, `grove.send`,
`grove.palette`, `grove.new_worktree`, `grove.open_session`, `grove.current_repo`.
Stateful helpers called from the TUI VM proxy to the daemon over the protocol.

### 10.5 Safety

- **User code must not hang the UI.** `grove.column` and `grove.keymap` callbacks run
  under a timeout; on expiry the column renders empty and the error surfaces once.
- **Errors are contained.** A throwing callback disables that one registration and
  reports it. It never unwinds into a render.
- **Long work belongs to the daemon.** `grove.run` in a lifecycle hook is async and
  reports completion; it does not block worktree creation.
- **No repo-local config in v1.** A `.grove.lua` committed to a repository would mean
  cloning and opening a repo executes its code. Neovim hit exactly this with `exrc` and
  now demands explicit per-file trust. If repo-local config is ever added, the trust
  prompt ships with it, not after.

---

## 11. Not in v1

Deliberately excluded, with the reasoning, so these don't get relitigated by accident.

| Cut | Why |
|---|---|
| Bulk git (push / rebase / stage across repos) | every worktree already has a shell; partial-failure UI is large |
| Progress percentages per worktree | git cannot answer it; the source mock faked it |
| Any GitHub or forge integration **built in** | needs auth, rate limits, offline states. Available as a user `grove.column` instead — see §10.3 |
| Multiple terminals per worktree, splits, zoom | a full multiplexer; one pty per worktree covers the use case |
| Two sessions open simultaneously | one open, many stored |
| Kanban board, outline tree, tiled-terminal, worktree-detail screens | already cut between mock v5 and v6; redundant with the dash |
| Automatic snapshots or timed writes | snapshots are manual, as in tmux-resurrect |
| Repo-local `.grove.lua` | executing code from a cloned repo needs a trust model; see §10.5 |

---

## 12. Risks

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
