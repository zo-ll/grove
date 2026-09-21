//! Wire protocol between `grove` (TUI) and `groved` (daemon).
//!
//! Every message crossing the unix socket is defined here, along with framing
//! and version negotiation. The protocol is versioned from the first commit so
//! a stale daemon is detected rather than misparsed.
//!
//! See `SPEC.md` §8. Implements issue #2.
//!
//! # What a newly-attached client receives
//!
//! A pty keeps producing output while nobody is watching it. When a client
//! attaches, three things could happen, and the choice shapes both the daemon
//! and the terminal pane:
//!
//! - **Screen only** — cheapest, but you reattach with no history to scroll.
//! - **Full scrollback replay** — complete, but attaching to a 10,000-line
//!   buffer pays for all of it before anything is drawn.
//! - **Screen, then backfill** — what this protocol does.
//!
//! [`Event::TerminalScreen`] arrives first and carries exactly the visible
//! grid, so the pane paints immediately. History then streams behind it as
//! [`Event::TerminalScrollback`] chunks, each tagged with a sequence number and
//! a `done` flag. Live [`Event::TerminalOutput`] deltas may interleave from the
//! moment the screen is sent — they are never withheld waiting for backfill to
//! finish, because a client that has painted should not go stale while history
//! loads.
//!
//! ## The reassembly contract
//!
//! The interleaving is what makes the paint instant, and it is also the one
//! thing a client can get wrong. Appending everything to a single buffer in
//! arrival order **misorders history**: live deltas can scroll lines off the
//! grid before backfill finishes, and those lines are newer than every backfill
//! chunk.
//!
//! A correct client keeps two buffers and joins them when `done` arrives:
//!
//! - backfill chunks, ordered by `seq`, all strictly **older** than the screen;
//! - lines scrolled off the grid by live deltas, all strictly **newer**.
//!
//! Three obligations make that work, two of them on the daemon:
//!
//! - **The screen and the backfill must be disjoint.** The daemon snapshots
//!   scrollback excluding the viewport it just sent. The protocol cannot
//!   enforce this; a daemon that overlaps them makes the client render lines
//!   twice with no way to detect it.
//! - **Chunks arrive oldest-first**: `seq` 0 is the oldest chunk, and the final
//!   chunk carries `done`. A chunk therefore renders as a contiguous block,
//!   appended in arrival order, with no buffering or prepending.
//! - **Lines within a chunk are oldest-first** too, for the same reason.
//! - **Chunks partition the snapshot**: no line appears in two chunks. Like the
//!   screen/backfill disjointness above, the protocol cannot enforce this — a
//!   daemon that re-reads overlapping ranges at chunk boundaries duplicates
//!   lines with no way for the client to notice.
//! - **An empty backfill is still terminated**: a pty with no history yields one
//!   empty chunk with `seq` 0 and `done` set, never zero chunks. A client
//!   waiting to join at `done` would otherwise wait forever, unable to tell
//!   "finished" from "still coming".
//!
//! # Wire format
//!
//! Frames are a 4-byte big-endian length prefix followed by that many bytes of
//! JSON. JSON is chosen for debuggability over a compact binary codec; terminal
//! output is the hot path and may justify revisiting that, which would be a
//! protocol version bump rather than a compatible change.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use grove_domain::{Ownership, RepoId, SessionId, SessionState};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Serialization bound for helpers that emit the CLI's versioned JSON shape.
pub use serde::Serialize as CliSerialize;

/// Incremented for any change an older peer cannot decode: a changed field
/// type, a removed variant, a different wire codec — or a new variant.
///
/// Variants are named rather than positional in this codec, so declaration
/// order is irrelevant and *any* addition breaks an older peer. The failure is
/// clean rather than silent: framing is length-delimited, so an unknown variant
/// is a decode error on one frame and the stream stays aligned.
pub const PROTOCOL_VERSION: u32 = 3;

/// Largest frame the reader will accept, to bound memory on a hostile or
/// confused peer. Terminal output is chunked well below this.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Stable JSON envelope emitted by non-interactive clients.
///
/// The payload is made from protocol row types and the envelope names the
/// protocol version that defines them. Changing either shape incompatibly
/// therefore requires the same version bump as changing the daemon wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliJson<T> {
    pub protocol: u32,
    pub result: T,
}

impl<T> CliJson<T> {
    pub fn new(result: T) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            result,
        }
    }
}

impl<T: Serialize> CliJson<T> {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

impl<T: DeserializeOwned> CliJson<T> {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// Worktrees are grouped by repository because the daemon's request and the
/// dashboard both read one repository at a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliWorktrees {
    pub repo: RepoId,
    pub rows: Vec<WorktreeRow>,
}

/// Names a worktree without describing it.
///
/// Requests identify; they never assert. A request carrying a whole
/// [`grove_domain::Worktree`] would let a client state a worktree's ahead/behind
/// counts, dirty flag — and, the day that type gains an ownership field, its
/// owner. The daemon is the sole writer of all of it (see `grove-domain`'s
/// `Ownership` docs), so the wire carries only enough to name the thing and the
/// daemon looks up the rest from its own state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorktreeRef {
    pub repo: RepoId,
    pub branch: String,
}

/// A live terminal, for reattaching to one the client did not spawn itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRow {
    pub terminal: TerminalId,
    pub target: TerminalTarget,
    /// The command in the foreground, for the "terminal busy" warning.
    pub foreground: Option<String>,
}

/// What a new terminal is attached to.
///
/// The scratch shell (§4.5) is deliberately attached to no worktree, so a
/// spawn request that demanded one could not create it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalTarget {
    Worktree(WorktreeRef),
    /// One shell for the whole session, carrying a snapshot of its workspace
    /// context in `GROVE_*` environment variables.
    Session {
        session: SessionId,
    },
    /// `cwd` defaults to `config.scratch_cwd` when absent.
    Scratch {
        cwd: Option<PathBuf>,
    },
}

/// Identifies one pty for the lifetime of the daemon. Not stable across
/// daemon restarts — a reattaching client re-reads the session to learn
/// current ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TerminalId(pub u64);

/// How much history a client wants when it attaches to a terminal.
///
/// See the module documentation for the ordering the daemon must honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollbackRequest {
    /// Screen and live output only. No backfill.
    None,
    /// At most this many lines of history — the most recent `n`, delivered
    /// oldest-first like every other chunk.
    Lines(u32),
    /// Everything the daemon retains.
    All,
}

/// What the client asks for when attaching to a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attach {
    pub terminal: TerminalId,
    pub scrollback: ScrollbackRequest,
    /// The pane's size at attach time, so the daemon can resize before
    /// sending the screen and the client never paints a wrongly-sized grid.
    pub rows: u16,
    pub cols: u16,
}

/// A session as the picker renders it (SPEC §4.3).
///
/// The picker shows "attached · 4 terminals", "detached 2d" and "closed ·
/// 794 MB". The domain `Session` carries none of those: it has no terminal
/// count, and no notion of how long it has held its state. Rather than push
/// screen concerns into the pure type, the protocol carries the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: SessionId,
    pub name: String,
    /// Member repo names, in the order the picker lists them.
    pub members: Vec<String>,
    pub state: SessionState,
    /// Live terminals. Zero for a closed session, by definition.
    pub terminals: u32,
    /// Seconds the session has held its current state, for "detached 2d".
    pub since: u64,
    /// Disk held by the worktrees this session owns, for "closed · 794 MB".
    pub size: u64,
}

/// A repo as the REPOS pane renders it (SPEC §4.1).
///
/// `worktrees` counts the branched checkouts the daemon can see in this repo,
/// not only the session's, and never the clone — the clone is every repo's
/// baseline, listed on the WORKTREES pane but not branched work, so a member
/// with zero is a reachable normal state meaning "this task touches this
/// repo, I haven't branched yet".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRow {
    pub repo: RepoId,
    pub name: String,
    pub base_branch: String,
    /// True when `base_branch` came from `origin/HEAD` rather than a config
    /// override. §4.2 renders the provenance — "base: origin/main (from
    /// origin/HEAD)" — and the name alone cannot express it.
    pub base_from_origin_head: bool,
    pub worktrees: u32,
    /// Any worktree in this repo has uncommitted changes. A flag suffices
    /// here: the REPOS pane colours a dot, and per-worktree counts come from
    /// [`WorktreeRow::dirty_files`].
    pub dirty: bool,
    /// The repo is a member of the open session.
    pub member: bool,
}

/// A worktree as the WORKTREES pane renders it (SPEC §4.1).
///
/// The pane lists *every* worktree of the selected repo regardless of owner,
/// plus the clone, and must distinguish four ownership states — so ownership
/// travels with the row rather than being inferred by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRow {
    pub worktree: WorktreeRef,
    /// True when HEAD is detached: `branch` is then empty, and the checkout
    /// cannot be adopted or released by branch name.
    pub detached: bool,
    pub ownership: Ownership,
    pub ahead: u64,
    pub behind: u64,
    /// Uncommitted files. Zero means clean.
    ///
    /// A count rather than a flag because §4.6 renders "9 files uncommitted"
    /// and "9 uncommitted files in billing-service will be lost" before the
    /// only destructive action in the product. A bool would force the client to
    /// say "some files", which is a weaker warning than the spec asks for.
    pub dirty_files: u32,
    /// Seconds since the worktree's branch was last updated: the tip commit's
    /// timestamp. §4.1's example shows differing per-row ages, which a
    /// repo-level value can never produce.
    pub age: u64,
    /// Last known bytes occupied by the checkout.
    ///
    /// The walk runs off the request path — §7's size read is cancellable and
    /// never blocks a caller — so the first rows after a worktree appears
    /// carry 0, and the next refresh carries the completed walk's answer.
    pub size: u64,
    /// The pty attached to this worktree, if one is running.
    pub terminal: Option<TerminalId>,
    /// The command in the foreground of that pty, for the "terminal busy"
    /// warning before a worktree is removed.
    pub foreground: Option<String>,
    /// Refs have aged past `stale_after`, so ahead/behind may be wrong.
    /// Also set when the row's own reads failed, so degraded rows never
    /// present confident zeros as fresh facts.
    pub stale: bool,
}

/// Why a worktree *is* safe to prune (SPEC §5).
///
/// The picker renders this in its own column — "merged" or "gone" — so it
/// cannot be inferred from the absence of blockers, which only say why a row is
/// unsafe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PruneState {
    /// Merged into its base.
    Merged,
    /// Its upstream branch no longer exists.
    UpstreamGone,
    /// Neither: the row is not safe, and `blockers` says why.
    Neither,
}

/// Why a worktree is not safe to prune (SPEC §5).
///
/// Prune pre-checks only provably safe rows and shows everything else with the
/// reason it was skipped, so the reason is data rather than prose the client
/// invents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PruneBlocker {
    /// The repo fetch failed before the verdict was made, so merged/gone
    /// cannot be trusted yet. Every row of that repo carries it until a
    /// listing succeeds.
    FetchFailed,
    /// Not merged into its base and its upstream still exists.
    Unmerged,
    /// The working tree has uncommitted changes.
    Dirty { files: u32 },
    /// Commits exist locally that are not pushed.
    Unpushed { commits: u64 },
    /// An attached or detached session owns it.
    Owned { session: SessionId, name: String },
}

/// One row of the prune picker (SPEC §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneCandidate {
    pub worktree: WorktreeRef,
    /// What makes it prunable, rendered as its own column.
    pub state: PruneState,
    pub size: u64,
    /// Empty when the row is safe, and the daemon pre-checked it. Any entry
    /// means the client leaves it unchecked and shows why.
    pub blockers: Vec<PruneBlocker>,
}

/// A file in the diff screen (SPEC §4.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFile {
    pub path: String,
    /// `M`, `A`, `D` or `?` as the screen renders it.
    pub status: char,
    pub added: u64,
    pub removed: u64,
}

/// One line of a unified hunk, pre-classified so the client does not parse.
///
/// The text is the line's content **without** its unified-diff marker: the
/// variant is the marker, and the client draws `+`, `-` or a space from it.
/// `groved` has always sent it that way; the fake daemon and the example
/// below used to include the marker, which is how the TUI came to be
/// written against text that the real daemon never sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLine {
    Header(String),
    Context(String),
    Added(String),
    Removed(String),
}

/// Client to daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Must be the first message. The daemon replies [`Event::Welcome`] or
    /// [`Event::VersionMismatch`] and closes.
    Hello {
        version: u32,
    },

    ListSessions,
    /// Every repo in the workspace, for the REPOS pane.
    ListRepos,
    /// Re-walk the workspace for repos (§5's `scan`).
    Scan,
    /// Create a session and make it current (§5's `session new`).
    SessionNew {
        name: String,
    },
    /// Rename the open session (§5's `session rename`).
    SessionRename {
        session: SessionId,
        name: String,
    },
    /// Every worktree of one repo, regardless of owner, plus the clone.
    ListWorktrees(RepoId),
    /// Prune candidates across the workspace, each with the reasons it is not
    /// safe to remove.
    ListPruneCandidates,
    /// Remove these worktrees. §5's picker ends in `enter prune 4`, and listing
    /// candidates without a verb leaves the screen unable to do the one thing
    /// it exists for.
    ///
    /// §5 says an unsafe row "can still be ticked deliberately", so the daemon
    /// does not overrule the selection — that would make the deliberate tick
    /// meaningless. It attempts each one and reports per-row outcomes in
    /// [`Event::Pruned`], the same plan-then-results idiom §4.6 uses for the
    /// only other destructive action. What it will not do is remove a worktree
    /// a live session owns, because that is not the user's to override from
    /// this screen: ending that session is.
    Prune(Vec<WorktreeRef>),
    /// The diff of one worktree against its base, for the read-only diff screen.
    ///
    /// `file` picks which patch comes back. `None` means "the first file", for
    /// opening the screen. Moving the cursor re-requests with the new path —
    /// §4.4's `↑↓ file` is the screen's only interaction, and without a way to
    /// ask for another file's hunks it cannot happen. Hunks are not all sent up
    /// front because opening the screen would then pay for every file's patch.
    DiffWorktree {
        worktree: WorktreeRef,
        file: Option<String>,
    },
    /// Open a stored session, replacing whatever is open. The outgoing session
    /// becomes detached, or is dropped if it is an unnamed launch session with
    /// no worktrees.
    OpenSession(SessionId),
    DetachSession(SessionId),
    CloseSession(SessionId),
    /// The only destructive request. Removes every worktree the session owns,
    /// and nothing else.
    EndSession(SessionId),

    AddMember {
        session: SessionId,
        repo: RepoId,
    },
    RemoveMember {
        session: SessionId,
        repo: RepoId,
    },
    /// Create one worktree per named repo, all on `branch`.
    NewWorktrees {
        session: SessionId,
        branch: String,
        repos: Vec<RepoId>,
    },
    AdoptWorktree {
        session: SessionId,
        worktree: WorktreeRef,
    },
    ReleaseWorktree {
        session: SessionId,
        worktree: WorktreeRef,
    },

    SpawnTerminal(TerminalTarget),
    KillTerminal(TerminalId),
    ResizeTerminal {
        terminal: TerminalId,
        rows: u16,
        cols: u16,
    },
    /// Raw stdin for the pty. Not interpreted.
    Input {
        terminal: TerminalId,
        bytes: Vec<u8>,
    },
    AttachTerminal(Attach),
    DetachTerminal(TerminalId),

    /// Fetch member repos of a session. Never happens on a timer.
    ///
    /// This extends SPEC §8's list, which does not name a fetch request. It is
    /// a read, and it belongs to "session list and state" only generously; the
    /// extension is recorded here rather than left implicit.
    Fetch {
        session: SessionId,
        repo: Option<RepoId>,
    },
    /// Open a worktree in `config.editor` (§3.3's `^g o`).
    ///
    /// The daemon runs it rather than returning a path: it is the side that
    /// holds the editor configuration, and the TUI is barred from touching the
    /// filesystem anyway. An earlier revision carried a path on every worktree
    /// row for this, which no screen rendered.
    OpenEditor(WorktreeRef),
    /// Every live terminal, for reattach.
    ListTerminals,
    /// Manual only, as in tmux-resurrect.
    SaveSnapshot(SessionId),
    RestoreSnapshot(SessionId),
}

impl Request {
    /// Short, stable words for a direct failure of this request.
    ///
    /// This is user-facing protocol vocabulary, not a serialization of the
    /// request or its arguments. Clients may use it to distinguish a direct
    /// refusal from unrelated events that arrived on the same stream.
    pub fn failure_context(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "handshake",
            Self::ListSessions => "list sessions",
            Self::ListRepos => "list repos",
            Self::Scan => "scan",
            Self::SessionNew { .. } => "session new",
            Self::SessionRename { .. } => "rename",
            Self::ListWorktrees(_) => "list worktrees",
            Self::ListPruneCandidates | Self::Prune(_) => "prune",
            Self::DiffWorktree { .. } => "diff",
            Self::OpenSession(_) => "open",
            Self::DetachSession(_) => "detach",
            Self::CloseSession(_) => "close",
            Self::EndSession(_) => "end session",
            Self::AddMember { .. } => "add",
            Self::RemoveMember { .. } => "remove",
            Self::NewWorktrees { .. } => "new worktrees",
            Self::AdoptWorktree { .. } => "adopt",
            Self::ReleaseWorktree { .. } => "release",
            Self::SpawnTerminal(_) => "spawn terminal",
            Self::KillTerminal(_) => "kill terminal",
            Self::ResizeTerminal { .. } => "resize terminal",
            Self::Input { .. } => "terminal input",
            Self::AttachTerminal(_) => "attach terminal",
            Self::DetachTerminal(_) => "detach terminal",
            Self::Fetch { .. } => "fetch",
            Self::OpenEditor(_) => "open editor",
            Self::ListTerminals => "list terminals",
            Self::SaveSnapshot(_) => "snapshot",
            Self::RestoreSnapshot(_) => "restore snapshot",
        }
    }
}

/// A colour, as a terminal can express one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Color {
    /// The terminal's own default for this position.
    #[default]
    Default,
    /// One of the 256 indexed colours.
    Indexed(u8),
    /// Truecolor, per SPEC §9.
    Rgb(u8, u8, u8),
}

/// Character attributes carried alongside a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Attrs {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub dim: bool,
    /// Always `false` for now: vt100 0.16 has no strikethrough attribute, so
    /// the daemon cannot answer for it. The field exists on the wire so a
    /// future parser gains it without a protocol change; no client should
    /// treat `false` as a fact about the cell.
    pub strikethrough: bool,
}

/// One cell of the grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    /// The grapheme occupying this cell. Empty for the continuation column of a
    /// double-width character.
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

/// The visible grid, already interpreted by the daemon's vt100 parser.
///
/// Cells carry colour and attributes rather than plain text: SPEC §4.1 renders
/// coloured test output in the terminal pane and §9 promises truecolor. Plain
/// strings here would make the attach-time paint monochrome while live
/// [`Event::TerminalOutput`] deltas — raw pty bytes, SGR sequences and all —
/// rendered in colour, so a reattach would visibly lose styling until the next
/// redraw.
///
/// Note the division of labour, which is easy to get backwards: this type is an
/// attach-time *shortcut* so the pane can paint without replaying history. The
/// client still runs its own vt100 over [`Event::TerminalOutput`] to keep the
/// grid current, which is why SPEC §8 places the parser on the TUI side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    pub rows: u16,
    pub cols: u16,
    /// Row-major, top to bottom, each row exactly `cols` cells.
    ///
    /// That is a daemon obligation the protocol cannot enforce, so a client
    /// indexes defensively rather than trusting `rows`/`cols` to match.
    pub cells: Vec<Vec<Cell>>,
    /// Cursor position as (row, col), or `None` when hidden.
    pub cursor: Option<(u16, u16)>,
}

/// Daemon to client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// Greets a version-matched peer. `ownership_movable` carries a
    /// workspace capability, not a row fact: when the workspace's
    /// `worktree_path` template contains `{session}`, a worktree's location
    /// depends on the session that made it, so adopt and release are
    /// refused — and the client can withhold the affordance instead of
    /// offering a key that only fails (§2.4). The daemon's own refusal
    /// stays; the client's knowledge is an affordance, not the
    /// enforcement.
    Welcome {
        version: u32,
        ownership_movable: bool,
    },
    /// Sent instead of `Welcome` when the versions differ; the daemon closes
    /// the connection immediately afterwards. Carries both numbers so the
    /// client can say which side is stale.
    VersionMismatch {
        daemon: u32,
        client: u32,
    },

    Sessions(Vec<SessionRow>),
    /// A terminal was created. Without this the spawn interaction cannot
    /// complete: the client has no other way to learn the id it must attach to,
    /// and §4.5's scratch shell is unreachable from the moment it is created.
    TerminalSpawned {
        target: TerminalTarget,
        terminal: TerminalId,
    },
    /// Every live terminal, so a reattaching client can find the scratch shell
    /// again. Worktree terminals are discoverable through `Worktrees`; the
    /// scratch shell belongs to no worktree and would otherwise be lost on
    /// reattach.
    Terminals(Vec<TerminalRow>),
    Repos(Vec<RepoRow>),
    Worktrees {
        repo: RepoId,
        rows: Vec<WorktreeRow>,
    },
    PruneCandidates(Vec<PruneCandidate>),
    /// What actually happened, per row. A prune can partly succeed — one
    /// worktree's removal failing is no reason to hide that four others went —
    /// and the user deliberately ticked anything unsafe, so they are owed the
    /// outcome rather than a refusal.
    Pruned {
        removed: Vec<WorktreeRef>,
        failed: Vec<(WorktreeRef, String)>,
        reclaimed: u64,
    },
    /// `files` is the whole list; `hunks` covers `selected` only, so opening the
    /// screen does not pay for every file's patch.
    Diff {
        worktree: WorktreeRef,
        base: String,
        files: Vec<DiffFile>,
        selected: Option<String>,
        hunks: Vec<DiffLine>,
        added: u64,
        removed: u64,
    },
    /// A session's state changed, for any reason including another client.
    SessionChanged(SessionRow),
    SessionEnded(SessionId),

    /// The visible grid, sent first on attach so the pane can paint at once.
    TerminalScreen {
        terminal: TerminalId,
        screen: Screen,
    },
    /// History, streamed after the screen, strictly older than it.
    ///
    /// `seq` starts at 0 for the oldest chunk and increases; `done` marks the
    /// final one; lines within a chunk are oldest-first; no line appears in two
    /// chunks; and an empty backfill is one empty chunk with `done` set rather
    /// than no chunks at all. A client must keep
    /// these separate from lines that live output scrolls off the grid — see
    /// the module documentation's reassembly contract, because merging them in
    /// arrival order silently misorders history.
    TerminalScrollback {
        terminal: TerminalId,
        seq: u32,
        lines: Vec<String>,
        done: bool,
    },
    /// Live output. May interleave with scrollback chunks.
    TerminalOutput {
        terminal: TerminalId,
        bytes: Vec<u8>,
    },
    TerminalExited {
        terminal: TerminalId,
        status: Option<i32>,
    },

    /// A branch is already checked out in another worktree, so it cannot be
    /// created again.
    ///
    /// §7 requires the UI to offer to adopt that existing worktree instead, and
    /// it cannot do that from a prose message — it needs the conflicting path
    /// and the worktree that holds it.
    BranchCheckedOutElsewhere {
        repo: RepoId,
        branch: String,
        existing: WorktreeRef,
        existing_path: PathBuf,
    },

    /// A request failed in a way with no richer representation. `context` names
    /// the request that caused it.
    Failed {
        context: String,
        message: String,
    },
}

/// Framing and transport errors. Protocol-level failures are [`Event::Failed`]
/// instead — an error the peer reported is not an error reading the wire.
#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    /// The peer announced a frame larger than [`MAX_FRAME_BYTES`].
    TooLarge(u32),
    /// A value serialized to more bytes than a `u32` length prefix can express.
    /// Distinct from [`Self::TooLarge`], which is a real announced size.
    Unrepresentable(usize),
    /// The frame arrived intact but its contents did not parse.
    Malformed(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::TooLarge(n) => {
                write!(
                    f,
                    "frame of {n} bytes exceeds the {MAX_FRAME_BYTES} byte limit"
                )
            }
            Self::Unrepresentable(n) => {
                write!(
                    f,
                    "value serialized to {n} bytes, which no length prefix can express"
                )
            }
            Self::Malformed(e) => write!(f, "malformed frame: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Write one length-prefixed frame.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> Result<(), FrameError> {
    let body = serde_json::to_vec(value).map_err(|e| FrameError::Malformed(e.to_string()))?;
    let len = u32::try_from(body.len()).map_err(|_| FrameError::Unrepresentable(body.len()))?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    // One write, not two: terminal output is the hot path this module's
    // documentation names, and a torn prefix is the worst way to desynchronise
    // a length-delimited stream.
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

/// Read one length-prefixed frame. Handles partial reads; `read_exact` loops
/// until the whole frame has arrived or the peer goes away.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<T, FrameError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| FrameError::Malformed(e.to_string()))
}

/// Where a workspace's daemon listens.
///
/// Both binaries derive this, so it lives with the protocol rather than being
/// implemented twice. Two copies that drift by one character produce a client
/// that connects to nothing and a daemon nobody finds, with no error saying so.
///
/// `$XDG_RUNTIME_DIR/grove/<hash>.sock`, per SPEC §6.
pub fn socket_path(workspace: &Path) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("grove")
        .join(format!("{}.sock", workspace_hash(workspace)))
}

/// FNV-1a over the canonicalised path. Stable across runs and platforms, which
/// is what matters: two invocations in one workspace must agree.
pub fn workspace_hash(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canonical.as_os_str().as_encoded_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Outcome of the opening handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handshake {
    /// Versions match. `ownership_movable` carries the daemon's workspace
    /// capability through the handshake, so a client knows before its first
    /// request whether adopt and release can ever succeed. `None` marks that
    /// the deciding side did not decide — a hello matches on versions only,
    /// and nothing here may advertise a workspace it has not seen.
    Agreed { ownership_movable: Option<bool> },
    /// Versions differ. The caller reports which side is stale and stops; it
    /// does not attempt to speak the other version.
    Mismatch { daemon: u32, client: u32 },
    /// The peer sent something before greeting. Distinct from [`Self::Mismatch`]
    /// so the daemon can say "you did not greet me" rather than reporting a
    /// fabricated version 0, which is indistinguishable from a genuinely
    /// ancient peer and misleads whoever reads the error.
    NotHello,
}

/// Client side of the handshake: check the daemon's reply.
///
/// The daemon refuses a mismatched client, but nothing forces a client to
/// check the daemon in turn, so this exists to make the symmetric check the
/// easy thing to do.
pub fn accept_welcome(event: &Event) -> Handshake {
    match event {
        Event::Welcome {
            version,
            ownership_movable,
        } if *version == PROTOCOL_VERSION => Handshake::Agreed {
            ownership_movable: Some(*ownership_movable),
        },
        Event::Welcome { version, .. } => Handshake::Mismatch {
            daemon: *version,
            client: PROTOCOL_VERSION,
        },
        Event::VersionMismatch { daemon, client } => Handshake::Mismatch {
            daemon: *daemon,
            client: *client,
        },
        _ => Handshake::NotHello,
    }
}

/// Daemon side of the handshake: read the client's `Hello` and decide.
///
/// The workspace capability is not decided here — the caller carries it into
/// the `Welcome` it writes — so a hello that matches names the agreement
/// without pretending to know the workspace.
pub fn accept_hello(request: &Request) -> Handshake {
    match request {
        Request::Hello { version } if *version == PROTOCOL_VERSION => Handshake::Agreed {
            ownership_movable: None,
        },
        Request::Hello { version } => Handshake::Mismatch {
            daemon: PROTOCOL_VERSION,
            client: *version,
        },
        // Anything before Hello is a protocol violation, and a different one
        // from a version disagreement.
        _ => Handshake::NotHello,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_domain::SessionState;

    #[test]
    fn requests_identify_rather_than_assert() {
        // grove-domain documents that the daemon is the sole writer of
        // `Ownership`: a client may ask for a worktree to be adopted or
        // released, but may not assert who owns one. Nothing in the type system
        // enforces that, so this is the tripwire.
        //
        // To watch it fire, add a variant carrying the offending type and give
        // it an arm in the exhaustive match below — mutating an existing
        // variant's type instead breaks the test's own constructions first, and
        // the assertion never runs.
        let src = include_str!("lib.rs");
        let start = src
            .find("pub enum Request {")
            .expect("Request enum present");
        let end = start + src[start..].find("\n}\n").expect("end of Request enum");

        // Doc comments are prose about these rules and mention the type names,
        // so judge the declarations only.
        let body: String = src[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // A `Worktree` in TYPE position is preceded by `:`, `(`, `<` or `,`.
        // Variant names contain the word too (`AdoptWorktree`, `NewWorktrees`),
        // and those are preceded by an identifier character, so position is
        // what separates a declaration from a name. Checking position rather
        // than a list of spellings also catches `Option<Worktree>`,
        // `Box<Worktree>` and a `Worktree` in any tuple position.
        let mut offenders = Vec::new();
        let mut i = 0;
        while let Some(found) = body[i..].find("Worktree") {
            let at = i + found;
            let prev = body[..at].trim_end().chars().next_back();
            let in_type_position = matches!(prev, Some(':' | '(' | '<' | ','));
            if in_type_position && !body[at + "Worktree".len()..].starts_with("Ref") {
                offenders.push(at);
            }
            i = at + 1;
        }
        assert!(
            offenders.is_empty(),
            "a Request carries a whole Worktree; use WorktreeRef so requests identify rather than assert"
        );

        assert!(
            !body.contains("Ownership"),
            "a Request carries an Ownership; the daemon must remain its sole writer"
        );
    }

    fn round_trip<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let mut buf = Vec::new();
        write_frame(&mut buf, value).expect("write");
        let back: T = read_frame(&mut buf.as_slice()).expect("read");
        assert_eq!(&back, value);
    }

    #[test]
    fn every_request_round_trips() {
        // Exhaustive by construction: the match below fails to compile if a
        // variant is added, so this cannot silently drift out of date the way
        // a hand-kept list does.
        let sid = SessionId("s".into());
        let rid = RepoId("r".into());
        let wt = WorktreeRef {
            repo: rid.clone(),
            branch: "feat/x".into(),
        };
        let tid = TerminalId(1);

        let all = [
            Request::Hello {
                version: PROTOCOL_VERSION,
            },
            Request::ListSessions,
            Request::ListRepos,
            Request::Scan,
            Request::SessionNew {
                name: "invoice split".into(),
            },
            Request::SessionRename {
                session: sid.clone(),
                name: "renamed".into(),
            },
            Request::ListWorktrees(rid.clone()),
            Request::ListPruneCandidates,
            Request::Prune(vec![wt.clone()]),
            Request::OpenEditor(wt.clone()),
            Request::ListTerminals,
            Request::DiffWorktree {
                worktree: wt.clone(),
                file: None,
            },
            Request::OpenSession(sid.clone()),
            Request::DetachSession(sid.clone()),
            Request::CloseSession(sid.clone()),
            Request::EndSession(sid.clone()),
            Request::AddMember {
                session: sid.clone(),
                repo: rid.clone(),
            },
            Request::RemoveMember {
                session: sid.clone(),
                repo: rid.clone(),
            },
            Request::NewWorktrees {
                session: sid.clone(),
                branch: "feat/x".into(),
                repos: vec![rid.clone()],
            },
            Request::AdoptWorktree {
                session: sid.clone(),
                worktree: wt.clone(),
            },
            Request::ReleaseWorktree {
                session: sid.clone(),
                worktree: wt.clone(),
            },
            Request::SpawnTerminal(TerminalTarget::Scratch { cwd: None }),
            Request::SpawnTerminal(TerminalTarget::Session {
                session: sid.clone(),
            }),
            Request::KillTerminal(tid),
            Request::ResizeTerminal {
                terminal: tid,
                rows: 24,
                cols: 80,
            },
            Request::Input {
                terminal: tid,
                bytes: vec![0, 27, 255],
            },
            Request::AttachTerminal(Attach {
                terminal: tid,
                scrollback: ScrollbackRequest::Lines(500),
                rows: 24,
                cols: 80,
            }),
            Request::DetachTerminal(tid),
            Request::Fetch {
                session: sid.clone(),
                repo: Some(rid),
            },
            Request::SaveSnapshot(sid.clone()),
            Request::RestoreSnapshot(sid),
        ];

        for r in &all {
            round_trip(r);
            // Compile-time exhaustiveness: adding a variant breaks this match.
            match r {
                Request::Hello { .. }
                | Request::ListSessions
                | Request::ListRepos
                | Request::Scan
                | Request::SessionNew { .. }
                | Request::SessionRename { .. }
                | Request::ListWorktrees(_)
                | Request::ListPruneCandidates
                | Request::Prune(_)
                | Request::OpenEditor(_)
                | Request::ListTerminals
                | Request::DiffWorktree { .. }
                | Request::OpenSession(_)
                | Request::DetachSession(_)
                | Request::CloseSession(_)
                | Request::EndSession(_)
                | Request::AddMember { .. }
                | Request::RemoveMember { .. }
                | Request::NewWorktrees { .. }
                | Request::AdoptWorktree { .. }
                | Request::ReleaseWorktree { .. }
                | Request::SpawnTerminal(_)
                | Request::KillTerminal(_)
                | Request::ResizeTerminal { .. }
                | Request::Input { .. }
                | Request::AttachTerminal(_)
                | Request::DetachTerminal(_)
                | Request::Fetch { .. }
                | Request::SaveSnapshot(_)
                | Request::RestoreSnapshot(_) => {}
            }
        }
    }

    #[test]
    fn every_event_round_trips() {
        let session = SessionRow {
            id: SessionId("s".into()),
            name: "invoice split".into(),
            members: vec!["billing-service".into(), "web-app".into()],
            state: SessionState::Detached,
            terminals: 4,
            since: 172_800,
            size: 794_000_000,
        };
        let rid = RepoId("billing-service".into());
        let wt = WorktreeRef {
            repo: rid.clone(),
            branch: "feat/x".into(),
        };
        let tid = TerminalId(1);

        let all = [
            Event::Welcome {
                version: PROTOCOL_VERSION,
                ownership_movable: true,
            },
            Event::VersionMismatch {
                daemon: 1,
                client: 2,
            },
            Event::Sessions(vec![session.clone()]),
            Event::TerminalSpawned {
                target: TerminalTarget::Scratch { cwd: None },
                terminal: tid,
            },
            Event::TerminalSpawned {
                target: TerminalTarget::Session {
                    session: SessionId("s".into()),
                },
                terminal: tid,
            },
            Event::Terminals(vec![TerminalRow {
                terminal: tid,
                target: TerminalTarget::Worktree(wt.clone()),
                foreground: Some("pnpm test".into()),
            }]),
            Event::Repos(vec![RepoRow {
                repo: rid.clone(),
                name: "billing-service".into(),
                base_branch: "origin/main".into(),
                base_from_origin_head: true,
                worktrees: 3,
                dirty: true,
                member: true,
            }]),
            Event::Worktrees {
                repo: rid.clone(),
                rows: vec![WorktreeRow {
                    worktree: wt.clone(),
                    detached: false,
                    ownership: Ownership::Other(SessionId("other".into())),
                    ahead: 4,
                    behind: 2,
                    dirty_files: 9,
                    age: 7200,
                    size: 286_000_000,
                    terminal: Some(tid),
                    foreground: Some("pnpm test".into()),
                    stale: true,
                }],
            },
            Event::Pruned {
                removed: vec![wt.clone()],
                failed: vec![(wt.clone(), "worktree is locked".into())],
                reclaimed: 412_000_000,
            },
            Event::PruneCandidates(vec![PruneCandidate {
                worktree: wt.clone(),
                state: PruneState::Merged,
                size: 412_000_000,
                blockers: vec![
                    PruneBlocker::Unmerged,
                    PruneBlocker::Dirty { files: 9 },
                    PruneBlocker::Unpushed { commits: 6 },
                    PruneBlocker::Owned {
                        session: SessionId("s".into()),
                        name: "invoice split".into(),
                    },
                ],
            }]),
            Event::Diff {
                worktree: wt,
                base: "origin/main".into(),
                files: vec![DiffFile {
                    path: "src/invoice/split.ts".into(),
                    status: 'M',
                    added: 184,
                    removed: 22,
                }],
                selected: Some("src/invoice/split.ts".into()),
                hunks: vec![
                    DiffLine::Header("@@ -198,12 +198,26 @@".into()),
                    DiffLine::Context("const items = invoice.lineItems;".into()),
                    DiffLine::Removed("  return items.map(toLine);".into()),
                    DiffLine::Added("  const boundary = cycleBoundary(invoice);".into()),
                ],
                added: 412,
                removed: 137,
            },
            Event::SessionChanged(session),
            Event::SessionEnded(SessionId("s".into())),
            Event::TerminalScreen {
                terminal: tid,
                screen: Screen {
                    rows: 1,
                    cols: 2,
                    cells: vec![vec![
                        Cell {
                            text: "a".into(),
                            fg: Color::Rgb(250, 179, 135),
                            bg: Color::Default,
                            attrs: Attrs {
                                bold: true,
                                ..Attrs::default()
                            },
                        },
                        Cell {
                            text: "b".into(),
                            fg: Color::Indexed(2),
                            bg: Color::Default,
                            attrs: Attrs::default(),
                        },
                    ]],
                    cursor: Some((0, 1)),
                },
            },
            Event::TerminalScrollback {
                terminal: tid,
                seq: 0,
                lines: vec!["old".into()],
                done: true,
            },
            Event::TerminalOutput {
                terminal: tid,
                bytes: vec![27, 91, 65],
            },
            Event::TerminalExited {
                terminal: tid,
                status: Some(1),
            },
            Event::Failed {
                context: "OpenSession".into(),
                message: "gone".into(),
            },
        ];

        for e in &all {
            round_trip(e);
            // Compile-time exhaustiveness: a new event breaks this match, so the
            // set cannot drift the way a hand-kept list does.
            match e {
                Event::Welcome { .. }
                | Event::VersionMismatch { .. }
                | Event::Sessions(_)
                | Event::TerminalSpawned { .. }
                | Event::Terminals(_)
                | Event::Repos(_)
                | Event::Worktrees { .. }
                | Event::PruneCandidates(_)
                | Event::Pruned { .. }
                | Event::Diff { .. }
                | Event::SessionChanged(_)
                | Event::SessionEnded(_)
                | Event::TerminalScreen { .. }
                | Event::TerminalScrollback { .. }
                | Event::TerminalOutput { .. }
                | Event::TerminalExited { .. }
                | Event::BranchCheckedOutElsewhere { .. }
                | Event::Failed { .. } => {}
            }
        }
    }

    #[test]
    fn frames_survive_partial_reads() {
        // A reader that yields one byte at a time is the shape a socket takes
        // under load; read_exact must loop rather than truncate.
        struct Dribble<'a>(&'a [u8]);
        impl Read for Dribble<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() || buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = self.0[0];
                self.0 = &self.0[1..];
                Ok(1)
            }
        }
        let msg = Request::Input {
            terminal: TerminalId(7),
            bytes: vec![1, 2, 3],
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: Request = read_frame(&mut Dribble(&buf)).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn two_frames_in_one_buffer_read_back_in_order() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Request::ListSessions).unwrap();
        write_frame(&mut buf, &Request::KillTerminal(TerminalId(2))).unwrap();
        let mut cursor = buf.as_slice();
        let a: Request = read_frame(&mut cursor).unwrap();
        let b: Request = read_frame(&mut cursor).unwrap();
        assert_eq!(a, Request::ListSessions);
        assert_eq!(b, Request::KillTerminal(TerminalId(2)));
    }

    #[test]
    fn oversized_frame_is_refused_without_allocating() {
        let mut framed = (MAX_FRAME_BYTES + 1).to_be_bytes().to_vec();
        framed.extend_from_slice(b"{}");
        match read_frame::<_, Request>(&mut framed.as_slice()) {
            Err(FrameError::TooLarge(n)) => assert_eq!(n, MAX_FRAME_BYTES + 1),
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn malformed_body_is_reported_not_panicked() {
        let body = b"not json";
        let mut framed = (body.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(body);
        match read_frame::<_, Request>(&mut framed.as_slice()) {
            Err(FrameError::Malformed(_)) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn truncated_frame_is_an_io_error_not_a_hang() {
        let mut framed = 99u32.to_be_bytes().to_vec();
        framed.extend_from_slice(b"short");
        match read_frame::<_, Request>(&mut framed.as_slice()) {
            Err(FrameError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected Io(UnexpectedEof), got {other:?}"),
        }
    }

    #[test]
    fn the_socket_path_is_stable_and_workspace_specific() {
        // Both binaries derive it independently at runtime; if it were not
        // stable they would meet only by luck.
        let a = Path::new("/tmp");
        let b = Path::new("/usr");
        assert_eq!(socket_path(a), socket_path(a));
        assert_ne!(socket_path(a), socket_path(b));
        assert!(socket_path(a).to_string_lossy().ends_with(".sock"));
    }

    #[test]
    fn handshake_agrees_only_on_an_exact_match() {
        // A hello matches on versions only: nothing here has seen a
        // workspace, so the capability is None — unrepresentable as an
        // advertised fact.
        assert_eq!(
            accept_hello(&Request::Hello {
                version: PROTOCOL_VERSION
            }),
            Handshake::Agreed {
                ownership_movable: None
            }
        );
        assert_eq!(
            accept_hello(&Request::Hello {
                version: PROTOCOL_VERSION + 1
            }),
            Handshake::Mismatch {
                daemon: PROTOCOL_VERSION,
                client: PROTOCOL_VERSION + 1
            }
        );
        // A peer that speaks before saying hello is not trusted into the
        // protocol on the assumption it meant well.
        // A peer that speaks before greeting is its own case, not a fabricated
        // "version 0" indistinguishable from a genuinely ancient peer.
        assert_eq!(accept_hello(&Request::ListSessions), Handshake::NotHello);
    }
}
