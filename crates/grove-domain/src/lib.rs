//! Pure domain types shared by Grove's UI and daemon.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Identifies the scan root whose repositories and sessions belong together.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(pub String);

/// Identifies a repository independently of its display name or current path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoId(pub String);

/// Identifies a stored session even when the session is later renamed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

/// Describes a repository discovered beneath the current workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    /// Stable identity used by sessions and worktrees.
    pub id: RepoId,
    /// Human-readable name shown in repository pickers.
    pub name: String,
    /// Location of the original checkout.
    pub path: PathBuf,
    /// Remote branch against which worktree changes are compared.
    pub base_branch: String,
}

/// Groups the repositories and worktrees belonging to one task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Stable identity that survives renaming.
    pub id: SessionId,
    /// Human-readable task name shown in the dashboard and picker.
    pub name: String,
    /// Repositories participating in the task, including those without worktrees.
    pub members: Vec<RepoId>,
    /// Worktrees created by or explicitly adopted into this session.
    pub owned: Vec<Worktree>,
    /// Whether the session currently has a UI and live terminals.
    pub state: SessionState,
}

/// Captures the relationship between a saved session and its terminals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    /// The session is open in the TUI and its terminals are alive.
    Attached,
    /// The terminals are alive while the TUI is displaying another session.
    Detached,
    /// The terminals are dead while the session's worktrees remain on disk.
    Closed,
}

/// Describes a branch checkout and the live Git and filesystem facts shown for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// Repository whose object database backs this checkout.
    pub repo: RepoId,
    /// Branch currently checked out in this worktree.
    pub branch: String,
    /// Filesystem location of the checkout.
    pub path: PathBuf,
    /// Commits present locally but not in the upstream branch.
    pub ahead: u64,
    /// Commits present in the upstream branch but not locally.
    pub behind: u64,
    /// Whether tracked or untracked files differ from the checked-out commit.
    pub dirty: bool,
    /// Seconds since the worktree's branch was last updated.
    pub age: u64,
    /// Bytes occupied by the checkout on disk.
    pub size: u64,
}

/// Who may manage a worktree.
///
/// # Where the protection lives
///
/// A session must never own the clone — the developer's original checkout —
/// because `end session` removes everything a session owns. That protection is
/// deliberately **not** in this type. An earlier revision made the clone state
/// unconstructible and unserializable: it closed the type-level hole, and in
/// doing so made the state unreachable by the TUI that must render it
/// (SPEC §4.1) and unproducible by the daemon that derives it. It also never
/// addressed the real gap, since `Ours` could be paired with the clone's path
/// regardless.
///
/// The protection belongs at the trust boundary, and both halves are in
/// place:
///
/// - **No `grove-proto` request may carry an `Ownership` — issue #2.** A client
///   must be unable to assert ownership at all, only to ask that a worktree be
///   adopted or released, leaving the daemon the sole writer of this value.
///   SPEC §8's protocol list contains no set-ownership request, and the
///   tripwire test in `grove-proto` fails if a request ever grows one.
/// - **The daemon refuses to own the repository path — issues #10–#12.**
///   `groved`'s `SessionOrchestrator` derives [`Ownership::Clone`] by comparing
///   the worktree path with the repository path, refuses to adopt it, and
///   refuses to remove it in `end session`; the session store independently
///   rejects a claim on the clone's path.
///
/// This type is plain data: it is meant to travel daemon to client for
/// display, and never the other way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ownership {
    /// The open session created or adopted this worktree and may manage it fully.
    Ours,
    /// A different session owns this worktree, so the open session sees it read-only.
    Other(SessionId),
    /// No session owns this worktree, so it may be adopted.
    Unowned,
    /// The repository's original checkout. Never session-owned, never removed by
    /// `end session`, and rendered as its own state in the WORKTREES pane.
    Clone,
}

/// Describes the process, if any, attached to a worktree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terminal {
    /// Checkout used as the terminal's working directory.
    pub worktree: Worktree,
    /// Best available description of the process currently in the foreground.
    pub foreground: Option<String>,
    /// Whether the terminal process is still running.
    pub alive: bool,
}

#[cfg(test)]
mod tests {

    #[test]
    fn clone_state_can_reach_the_tui() {
        // SPEC §4.1 requires the WORKTREES pane to distinguish the clone as one
        // of four ownership states, so it must survive the wire.
        let json = serde_json::to_string(&Ownership::Clone {}).expect("clone must serialize");
        let back: Ownership = serde_json::from_str(&json).expect("clone must deserialize");
        assert_eq!(back, Ownership::Clone {});
    }

    use super::*;
    use serde::{Serialize, de::DeserializeOwned};

    fn round_trip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).expect("domain value should serialize");
        let decoded = serde_json::from_str(&json).expect("domain value should deserialize");
        assert_eq!(value, &decoded);
    }

    fn worktree() -> Worktree {
        Worktree {
            repo: RepoId("billing-service".into()),
            branch: "feat/ABC-4471-invoice-split".into(),
            path: PathBuf::from("/work/billing-service"),
            ahead: 4,
            behind: 2,
            dirty: true,
            age: 7_200,
            size: 300_000_000,
        }
    }

    #[test]
    fn every_domain_type_round_trips_through_serde() {
        round_trip(&WorkspaceId("workspace".into()));
        round_trip(&RepoId("repo".into()));
        round_trip(&SessionId("session".into()));
        round_trip(&Repo {
            id: RepoId("billing-service".into()),
            name: "Billing Service".into(),
            path: PathBuf::from("/code/billing-service"),
            base_branch: "origin/main".into(),
        });
        round_trip(&Session {
            id: SessionId("invoice-split".into()),
            name: "invoice split".into(),
            members: vec![RepoId("billing-service".into())],
            owned: vec![worktree()],
            state: SessionState::Detached,
        });
        round_trip(&SessionState::Attached);
        round_trip(&SessionState::Detached);
        round_trip(&SessionState::Closed);
        round_trip(&worktree());
        round_trip(&Ownership::Ours);
        round_trip(&Ownership::Other(SessionId("other".into())));
        round_trip(&Ownership::Unowned);
        round_trip(&Terminal {
            worktree: worktree(),
            foreground: Some("cargo test".into()),
            alive: true,
        });
    }
}
