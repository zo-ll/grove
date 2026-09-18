# Design brief — aligning the mock to the v1 spec

Brief to paste into the design tool, project `8968081d-f5b5-41af-abab-4b4a14928ea9`
("Worktree manager TUI design"), targeting `Grove TUI v6.dc.html`.

See ../SPEC.md for the decisions this brief encodes.

---

Update `Grove TUI v6.dc.html` to match Grove's locked v1 spec. The current mock was
drawn before these decisions and contradicts them in several places.

## Context that changed

Grove is a **true terminal TUI**, not a web app. Keep the mock legible in a browser, but
treat rounded corners and shadows as stylisation of something that will be drawn with
box-drawing characters — don't add more of them.

The core model is now:

    workspace (the path grove was launched in)
      └── session — explicit member repos + the worktrees it owns
            └── worktree — a (repo, branch) checkout
                  └── zero or one terminal

Branches are **free per worktree**: one repo can hold several worktrees at different
branches inside one session. There is no single shared session branch. Drop any copy
implying otherwise.

Hard rule: **grove only displays what git, plus the filesystem for sizes, can answer.**
No progress percentages, no PR state, no CI, no review status.

## Global changes

1. **Keyhints gain the `^g` prefix — but only where they need it.** The rule: a focused
   pane that is a pty takes every key, and `^g` is how you address grove instead. Lists
   and non-pty overlays take keys directly. So the dash status bar becomes
   `^g tab pane · ^g 1-3 hide · ^g s sessions · ^g d diff · ^g / palette`, while inside
   the picker, palette and diff the hints stay unprefixed (`↑↓ file`, `enter resume`).

2. **Rename "pane" to "terminal" everywhere.** There is exactly one terminal per
   worktree; "panes" implied splits, which don't exist. `4 panes` → `4 terminals`.

3. **Delete the unused screen data.** `renderVals` still builds `board`, `tree`,
   `termTabs`, `panes`, `createRepos`, `files`, `meta`, `commits`, `siblings`,
   `configRows`, `defaults`, `startSteps` and `paletteRows` for screens with no markup.
   Remove them, and the `isBoard`/`isTree`/`isTerm`/`isCreate`/`isDetail`/`isPalette`/
   `isClean`/`isConfig`/`isEmpty` flags. Two of them survive in new form — see below.

4. **Fix `goDash` being defined twice** in the returned object; the second
   (`this.go('dash')`) silently overrides the first and doesn't clear `palOpen`.

5. **Make `tab` focus functional**, not decorative. Currently `↑↓` always drives the
   worktree list and `←→` always drives the repo list regardless of which pane is lit.
   `↑↓` should drive whichever list has focus.

## Dash

- **WORKTREES: remove the progress columns.** Delete `pie`, `pct` and the `jobFor()`
  method entirely — git cannot know how far along a build is, and that data was faked.
  Columns become: ownership glyph · branch · `↑ahead` · `↓behind` · age.

- **WORKTREES: show ownership, which is currently invisible.** The pane lists *every*
  worktree of the selected repo, and there are four distinct cases that need
  distinguishing:

      ▣ feat/ABC-4471    owned by this session, has a terminal
      ◆ fix/ABC-4402     owned by ANOTHER session — read-only
      ◇ spike/perf       unowned — adoptable with ^g a
      ─ main             the clone itself — never ownable

  Please design this encoding; a dim owner label in a trailing column is one option.
  It matters because it's what makes `end session` legibly safe: it removes only the
  first kind.

- **Add a stale-refs marker.** Grove fetches on session open and on demand, so
  ahead/behind can age. Show something like `↑1 ~` when the refs are older than the
  threshold.

- **REPOS: members can hold zero worktrees.** Membership is explicit — you add a repo
  before branching in it. Show a `·` count and a hollow dot for that state.

- **Add a dash empty state** for a fresh workspace with no repos or an empty session.
  The three-step copy from the old `empty` screen is good; re-home it inside the
  WORKTREES pane:

      1  point grove at your clones   ^g /  scan
      2  add repos to this session    ^g /  add <repo>
      3  name a branch                ^g /  new <branch>

## Command palette

The palette is now grove's **command line**, not a menu. It takes arguments inline and
turns its row list into whatever picker the command needs.

- **Remove** `rebase session on origin/main` and `push all members`. Grove performs no
  bulk git — every worktree has its own shell for that.
- **Remove** anything referencing PR state.
- **New command list:** `scan`, `session new <name>`, `session rename <name>`,
  `open <session>`, `add <repo>…`, `remove <repo>`, `new <branch>`, `fetch [repo]`,
  `prune`, `snapshot`, `defaults`, `keys`.
- **Add a second palette state — argument mode.** This is the most important new screen,
  because it replaces the cut `create` wizard:

      ❯ new feat/ABC-4471-invoice-split
        base: origin/main                        (from origin/HEAD)
        ──────────────────────────────────────────────────────────
        [x] billing-service   member
        [x] web-app           member
        [x] sdk-js            member
        [ ] api-gateway       workspace
        [ ] design-system     workspace
        space toggle · enter create 3 · esc cancel

- **Add a third palette state — the prune picker.** This is the old `clean` screen,
  re-homed into the palette. Rows are pre-checked **only** when merged-or-gone AND clean
  AND nothing unpushed AND unowned; everything else is listed with its disqualifying
  reason, unchecked. Replace the `open PR` state with `unmerged` (no forge integration).

      ❯ prune                                              4 selected · 1.0 GB
       [x] web-app       feat/ABC-3980   merged    clean      412 MB
       [ ] sdk-js        feat/ABC-3980   merged    2 files     96 MB
       [ ] web-app       wip/checkout    ↑6        31 files   404 MB
       [ ] design-system feat/ABC-4110   unmerged  clean      112 MB
       [ ] billing-svc   feat/ABC-4471   ● owned by invoice split
       space toggle · a all safe · enter prune 4

## Session picker

- Add `c close` to the footer alongside `d detach` and `X end` — there are three states
  and three verbs now: detach keeps terminals alive, close kills them but keeps
  worktrees, end removes the worktrees.
- Keep the `● attached / ◐ detached / ○ closed` glyphs; they're correct.
- Worth showing: only one session is open at a time. Resuming replaces the current one.

## Diff

- **Remove `s stage` and `w session diff`** from the footer. Diff is read-only in v1;
  staging is a git write-verb and cross-repo diff was cut with the rest of bulk git.
  Footer becomes `↑↓ file` and `esc close`.
- Everything else about this screen is right.

## Scratch shell

- Change the cwd from `~/grove` to `$HOME` — it's a scratch terminal deliberately not
  attached to any worktree.
- **Remove `ctrl-s send to pane`**; that feature isn't in v1.
- Since this is a real pty, note in the footer that `^g` is how you get back to grove.

## End session

- `4 panes` → `4 terminals`.
- Otherwise this screen is the model for how destructive confirms should look — per-repo
  consequences first, aggregate second, the loss warning last. Keep it exactly.

## Keep as-is

The `corners`, `density`, `accent` and `softFrames` props are real user settings in the
spec, not just design knobs — keep them wired. Catppuccin Mocha stays the default
palette. IBM Plex Mono stays.

Ignore the linked `_ds/organic-*` design system entirely — it's a light cream-and-
terracotta web system with no relationship to this TUI, and nothing in the mock uses it.
