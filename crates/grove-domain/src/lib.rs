//! Pure domain types shared by Grove's UI and daemon.

use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::Error as _};
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

/// Classifies who may manage a worktree, while distinguishing the protected clone.
///
/// The clone classification is daemon-internal state derived by comparing the
/// worktree path with the repository path. It is neither accepted from wire data nor
/// directly constructible by downstream crates.
#[derive(std::clone::Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ownership {
    /// The open session created or adopted this worktree and may manage it fully.
    Ours,
    /// A different session owns this worktree, so the open session sees it read-only.
    Other(SessionId),
    /// No session owns this worktree, so it may be adopted.
    Unowned,
    /// This is the repository's original checkout and must never become session-owned.
    ///
    /// Downstream crates cannot construct this daemon-derived classification directly:
    ///
    /// ```compile_fail
    /// use grove_domain::Ownership;
    ///
    /// let ownership = Ownership::Clone {};
    /// ```
    #[non_exhaustive]
    Clone {},
}

impl Serialize for Ownership {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        enum WireOwnership<'a> {
            Ours,
            Other(&'a SessionId),
            Unowned,
        }

        match self {
            Self::Ours => WireOwnership::Ours.serialize(serializer),
            Self::Other(session) => WireOwnership::Other(session).serialize(serializer),
            Self::Unowned => WireOwnership::Unowned.serialize(serializer),
            Self::Clone {} => Err(S::Error::custom(
                "the clone ownership classification is daemon-internal state",
            )),
        }
    }
}

impl<'de> Deserialize<'de> for Ownership {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        enum WireOwnership {
            Ours,
            Other(SessionId),
            Unowned,
        }

        Ok(match WireOwnership::deserialize(deserializer)? {
            WireOwnership::Ours => Self::Ours,
            WireOwnership::Other(session) => Self::Other(session),
            WireOwnership::Unowned => Self::Unowned,
        })
    }
}

/// Ownership states accepted from operations that can change session ownership.
///
/// The protected clone classification is deliberately absent. This prevents callers
/// from submitting that classification, but `Ours` can still be paired with any path.
/// The daemon must separately reject attempts to assign `Ours` to the repository path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnableOwnership {
    /// Assign the worktree to the open session.
    Ours,
    /// Record that a different session owns the worktree.
    Other(SessionId),
    /// Leave the worktree available for adoption.
    Unowned,
}

impl From<OwnableOwnership> for Ownership {
    fn from(value: OwnableOwnership) -> Self {
        match value {
            OwnableOwnership::Ours => Self::Ours,
            OwnableOwnership::Other(session) => Self::Other(session),
            OwnableOwnership::Unowned => Self::Unowned,
        }
    }
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
        round_trip(&OwnableOwnership::Ours);
        round_trip(&OwnableOwnership::Other(SessionId("other".into())));
        round_trip(&OwnableOwnership::Unowned);
        round_trip(&Terminal {
            worktree: worktree(),
            foreground: Some("cargo test".into()),
            alive: true,
        });
    }

    #[test]
    fn clone_ownership_cannot_cross_the_wire() {
        assert!(serde_json::from_str::<Ownership>(r#""Clone""#).is_err());
        assert!(serde_json::to_string(&Ownership::Clone {}).is_err());
    }
}
