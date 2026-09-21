# grove

A terminal UI for managing git worktrees across many repos, with a terminal attached to
each worktree.

```sh
curl -fsSL https://raw.githubusercontent.com/zo-ll/grove/main/scripts/install.sh | bash
```

That builds from source into `~/.local/bin`, so you need [Rust](https://rustup.rs)
1.88+ and git. Add `-s -- --sample-config` for a starter config, and see
[Install](#install) for the rest.

```
┌─ REPOS ────────┬─ WORKTREES ────────────────┬─ billing-service · feat/ABC-4471 ─┐
│ ● billing-svc 2│ ▣ feat/ABC-4471   ↑4 ↓2  2h│ $ pnpm test billing/invoice       │
│ ● web-app    1 │ ◇ fix/ABC-4402    ↑1     1d│  PASS  src/invoice/split.test.ts  │
│ ● sdk-js     1 │ ◇ spike/perf             4d│  FAIL  src/invoice/proration.ts   │
│ ○ search-idx  ·│ ▣ main            ↓11  now │    expected 4 items, received 3   │
└────────────────┴────────────────────────────┴───────────────────────────────────┘
 ^g tab pane  ^g 1-3 hide  ^g s sessions  ^g d diff  ^g / palette   session: invoice split
```

> **Status: v1 is built and has barely been used.** Every screen the spec
> describes is implemented and tested, the daemon serves the whole protocol,
> and Lua can add keys, commands and columns. What it has *not* had is hands:
> it has been run end to end once, by the install script's author, on a
> throwaway repository. Expect to be the first person to find whatever that
> missed. See [SPEC.md](SPEC.md) for what it is supposed to do.

## Install

The one-liner above clones grove to `~/.cache/grove/src`, builds it and installs
both binaries. From a checkout, the same script skips the clone:

```sh
scripts/install.sh                 # builds release, installs to ~/.local/bin
scripts/install.sh --sample-config # and writes a starter config.lua
```

Either form takes the same options. `--prefix DIR` installs elsewhere, `--debug`
skips optimisation for a faster loop, and `--uninstall` removes the binaries
while leaving your config alone. Piped through `bash`, options go after `-s --`:

```sh
curl -fsSL https://raw.githubusercontent.com/zo-ll/grove/main/scripts/install.sh \
  | bash -s -- --prefix /usr/local/bin
```

`GROVE_REF` builds a branch other than `main` and `GROVE_SRC` moves the checkout
it keeps.

To remove grove completely, run `grove --uninstall`. It lists what it will
remove and asks first: the running daemons (their terminals close), `grove`
and `groved`, your config, saved sessions, the installer's source copy and
the runtime sockets. Worktrees you made with grove are never touched. Add
`--yes` to skip the question in a script.

Point it at a directory that holds your clones:

```sh
grove ~/code
```

grove is **two programs**: `groved` owns the ptys and outlives the terminal,
and `grove` is the TUI that attaches to it. You do not have to start the daemon
— if none is running for that directory, `grove` starts the `groved` installed
beside it and waits for it. It keeps running when the TUI exits, which is the
point of it: `^g q` leaves grove without stopping your terminals, and the next
`grove ~/code` attaches to the same shells, still running.

Start it yourself when you want it under a supervisor, or want to watch it:

```sh
groved ~/code
```

`GROVE_NO_AUTOSTART=1` stops grove starting one, and `GROVE_DAEMON` names a
different daemon binary. The one grove starts logs beside its socket, and says
where that is if it fails.

`^g ?` lists the keys for whichever screen you are on.

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

## Extensible in Lua

`~/.config/grove/config.lua` is both configuration and extension mechanism. The
highest-value hook is worktree bootstrap — a fresh worktree is unusable until set up,
and that setup is per-repo and per-person, so no template can express it:

```lua
grove.on("worktree_created", function(wt)
  -- gitignored files don't exist in a fresh worktree
  grove.copy(wt.clone .. "/.env.local", wt.path .. "/.env.local")
  grove.run(wt.path, "pnpm install --prefer-offline")
  grove.run(wt.path, "direnv allow")
end)
```

Also: custom palette commands, extra worktree columns, keymaps, per-repo rules and
computed paths. This is how forge data gets in without Grove knowing what a forge is —
you register a `grove.column`, and it renders as yours.

Per-repo rules layer over the global settings:

```lua
grove.repo("monorepo", {
  worktree_path = "/mnt/nvme/{repo}/{branch_slug}", -- 8 GB checkouts
  base          = "origin/develop",                -- new worktrees branch from here
  setup         = function(wt) grove.run(wt.path, "make bootstrap") end,
  theme         = { accent = "#89b4fa" },
})

grove.session_template("frontend", { repos = { "web-app", "design-system" } })
```

For the named repo only:

- `worktree_path` — string template or `function(repo, branch)` — replaces the
  global `worktree_path`
- `base` replaces the branch git advertises as the default (`origin/HEAD`)
- `setup(wt)` runs when one of the repo's worktrees is created, after every
  global `worktree_created` hook, in registration order
- `theme` keys mirror `grove.setup`'s `theme` table and are applied by the TUI;
  the daemon applies everything else and ignores `theme` — one config file
  serves both processes
- registering the same name again layers the new keys over the previous
  override, and keys left unset fall back to the global setting
- an override naming a repo that is not in the workspace is reported, not
  silently ignored

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

## License

[MIT](LICENSE) © Andrea Zollini
