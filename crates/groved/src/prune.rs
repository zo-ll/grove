//! Prune candidate inspection and conservative default selection.

use grove_domain::{RepoId, SessionId, SessionState};
use grove_git::{Repository, Tracking};
use grove_state::{OwnedWorktree, StoredSession};
use std::collections::HashMap;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PruneHistory {
    Merged,
    UpstreamGone,
    Unmerged,
}

/// A machine-readable reason that a row is left unchecked by default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PruneDisqualifier {
    FetchFailed,
    Unmerged,
    Dirty,
    Unpushed {
        commits: u64,
    },
    OwnedByLiveSession {
        session: SessionId,
        state: SessionState,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneCandidate {
    pub repo: RepoId,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    pub history: PruneHistory,
    pub reasons: Vec<PruneDisqualifier>,
    /// Uncommitted files, for the wire's `Dirty { files }` blocker. Zero when
    /// the row is clean; a count is stronger than the bool the rule needs.
    pub dirty_files: u32,
}

impl PruneCandidate {
    pub fn preselected(&self) -> bool {
        self.reasons.is_empty()
    }
}

/// Operational fetch detail is kept beside enum reasons so a UI can explain
/// the failure without parsing prose to decide whether a row is safe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneBatch {
    pub candidates: Vec<PruneCandidate>,
    pub fetch_error: Option<String>,
}

#[derive(Debug, Error)]
pub enum PruneError {
    #[error("could not list worktrees for {repo}: {source}")]
    Worktrees {
        repo: RepoId,
        source: grove_git::Error,
    },
    #[error("could not inspect {worktree}: {source}")]
    Inspect {
        worktree: PathBuf,
        source: grove_git::Error,
    },
}

/// Fetches this repository before deciding merged/gone status. Ahead counts
/// are preserved immediately before fetch because pruning a deleted upstream
/// removes the only ref that can prove local commits were never pushed. A
/// failed fetch leaves every row unchecked while still returning the rows for
/// deliberate manual selection.
pub fn prune_candidates(
    repository: &Repository,
    sessions: &[StoredSession],
) -> Result<PruneBatch, PruneError> {
    prune_candidates_against(repository, sessions, repository.base_branch.as_deref())
}

/// Candidate inspection against an explicit base branch.
///
/// `base` is the branch mergedness is judged against — normally the repo's
/// discovered default, but a repo override (`grove.repo(name, { base = .. })`)
/// decides for its repo, and pruning must judge the user's integration
/// branch, not whatever origin advertises (#54).
pub fn prune_candidates_against(
    repository: &Repository,
    sessions: &[StoredSession],
    base: Option<&str>,
) -> Result<PruneBatch, PruneError> {
    let worktrees =
        grove_git::worktrees(&repository.path).map_err(|source| PruneError::Worktrees {
            repo: repository.id.clone(),
            source,
        })?;
    let mut prefetch_ahead = HashMap::new();
    for worktree in worktrees.iter().filter(|worktree| !worktree.is_clone) {
        let ahead =
            match grove_git::ahead_behind(&worktree.path).map_err(|source| PruneError::Inspect {
                worktree: worktree.path.clone(),
                source,
            })? {
                Tracking::Tracked(counts) => counts.ahead,
                Tracking::NoUpstream | Tracking::UpstreamGone => 0,
            };
        prefetch_ahead.insert(worktree.path.clone(), ahead);
    }
    let fetch_error = grove_git::fetch_prune(&repository.path)
        .err()
        .map(|error| error.to_string());
    let fetch_failed = fetch_error.is_some();
    let mut candidates = Vec::new();
    for worktree in worktrees.into_iter().filter(|worktree| !worktree.is_clone) {
        let dirty_files =
            grove_git::dirty_file_count(&worktree.path).map_err(|source| PruneError::Inspect {
                worktree: worktree.path.clone(),
                source,
            })?;
        let tracking =
            grove_git::ahead_behind(&worktree.path).map_err(|source| PruneError::Inspect {
                worktree: worktree.path.clone(),
                source,
            })?;
        let merged = match (&worktree.branch, base) {
            (Some(branch), Some(base)) => grove_git::is_merged(&repository.path, branch, base)
                .map_err(|source| PruneError::Inspect {
                    worktree: worktree.path.clone(),
                    source,
                })?,
            _ => false,
        };
        let history = if tracking == Tracking::UpstreamGone {
            PruneHistory::UpstreamGone
        } else if merged {
            PruneHistory::Merged
        } else {
            PruneHistory::Unmerged
        };
        let current_ahead = match tracking {
            Tracking::Tracked(counts) => counts.ahead,
            Tracking::NoUpstream | Tracking::UpstreamGone => 0,
        };
        let ahead = current_ahead.max(prefetch_ahead[&worktree.path]);
        let owner = worktree.branch.as_ref().and_then(|branch| {
            live_owner(
                sessions,
                &OwnedWorktree {
                    repo: repository.id.clone(),
                    branch: branch.clone(),
                },
            )
        });
        let reasons = disqualifiers(fetch_failed, history, dirty_files > 0, ahead, owner);
        candidates.push(PruneCandidate {
            repo: repository.id.clone(),
            worktree: worktree.path,
            branch: worktree.branch,
            history,
            reasons,
            dirty_files,
        });
    }
    Ok(PruneBatch {
        candidates,
        fetch_error,
    })
}

pub(crate) fn live_owner(
    sessions: &[StoredSession],
    worktree: &OwnedWorktree,
) -> Option<(SessionId, SessionState)> {
    sessions
        .iter()
        .find(|session| session.state != SessionState::Closed && session.owned.contains(worktree))
        .map(|session| (session.id.clone(), session.state))
}

fn disqualifiers(
    fetch_failed: bool,
    history: PruneHistory,
    dirty: bool,
    ahead: u64,
    owner: Option<(SessionId, SessionState)>,
) -> Vec<PruneDisqualifier> {
    let mut reasons = Vec::new();
    if fetch_failed {
        reasons.push(PruneDisqualifier::FetchFailed);
    }
    if history == PruneHistory::Unmerged {
        reasons.push(PruneDisqualifier::Unmerged);
    }
    if dirty {
        reasons.push(PruneDisqualifier::Dirty);
    }
    if ahead > 0 {
        reasons.push(PruneDisqualifier::Unpushed { commits: ahead });
    }
    if let Some((session, state)) = owner {
        reasons.push(PruneDisqualifier::OwnedByLiveSession { session, state });
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("grove-prune-{unique}"));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn merged_verdict_follows_the_base_passed_in() {
        // #54: a repo override's base decides for its repo. A branch merged
        // into develop but not into main is prunable against the override and
        // unmerged against origin's advertised default.
        let temp = TempDir::new();
        let remote = temp.0.join("remote.git");
        let clone = temp.0.join("clone");
        let linked = temp.0.join("linked");
        fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--bare", "-q"]);
        fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "Grove Test"]);
        git(&clone, &["config", "user.email", "grove@example.test"]);
        fs::write(clone.join("tracked"), "base\n").unwrap();
        git(&clone, &["add", "tracked"]);
        git(&clone, &["commit", "-qm", "base"]);
        git(
            &clone,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&clone, &["push", "-qu", "origin", "main"]);
        git(
            &clone,
            &["worktree", "add", "-qb", "feat", linked.to_str().unwrap()],
        );
        fs::write(linked.join("work"), "work\n").unwrap();
        git(&linked, &["add", "work"]);
        git(&linked, &["commit", "-qm", "work"]);
        git(&linked, &["push", "-qu", "origin", "feat"]);
        // main advances past the branch point, develop takes the merge: the
        // branch is an ancestor of develop and of nothing on main.
        git(&clone, &["checkout", "-q", "-b", "develop", "main"]);
        git(&clone, &["merge", "-q", "--ff-only", "feat"]);
        git(&clone, &["push", "-qu", "origin", "develop"]);
        git(&clone, &["checkout", "-q", "main"]);
        fs::write(clone.join("main-only"), "main\n").unwrap();
        git(&clone, &["add", "main-only"]);
        git(&clone, &["commit", "-qm", "main"]);

        let repository = Repository {
            id: RepoId("repo".into()),
            name: "repo".into(),
            path: clone.clone(),
            base_branch: Some("origin/main".into()),
        };

        // The discovered default: unmerged, and left unchecked.
        let batch = prune_candidates(&repository, &[]).unwrap();
        let row = batch
            .candidates
            .iter()
            .find(|row| row.branch.as_deref() == Some("feat"))
            .unwrap();
        assert_eq!(row.history, PruneHistory::Unmerged);
        assert!(row.reasons.contains(&PruneDisqualifier::Unmerged));

        // The override: the user's integration branch, where the branch is
        // merged — pre-checked, no blockers.
        let batch = prune_candidates_against(&repository, &[], Some("origin/develop")).unwrap();
        let row = batch
            .candidates
            .iter()
            .find(|row| row.branch.as_deref() == Some("feat"))
            .unwrap();
        assert_eq!(row.history, PruneHistory::Merged);
        assert!(row.preselected());
    }

    #[test]
    fn all_four_safety_conditions_are_required_for_preselection() {
        let owner = (SessionId("owner".into()), SessionState::Detached);
        for bits in 0_u8..16 {
            let history_safe = bits & 1 != 0;
            let clean = bits & 2 != 0;
            let pushed = bits & 4 != 0;
            let unowned = bits & 8 != 0;
            let reasons = disqualifiers(
                false,
                if history_safe {
                    PruneHistory::Merged
                } else {
                    PruneHistory::Unmerged
                },
                !clean,
                if pushed { 0 } else { 2 },
                (!unowned).then(|| owner.clone()),
            );
            assert_eq!(
                reasons.is_empty(),
                history_safe && clean && pushed && unowned,
                "combination {bits:04b} had {reasons:?}"
            );
            assert_eq!(
                reasons.contains(&PruneDisqualifier::Unmerged),
                !history_safe
            );
            assert_eq!(reasons.contains(&PruneDisqualifier::Dirty), !clean);
            assert_eq!(
                reasons.contains(&PruneDisqualifier::Unpushed { commits: 2 }),
                !pushed
            );
            assert_eq!(
                reasons
                    .iter()
                    .any(|reason| matches!(reason, PruneDisqualifier::OwnedByLiveSession { .. })),
                !unowned
            );
        }
    }

    #[test]
    fn closed_ownership_is_safe_but_attached_and_detached_are_not() {
        let worktree = OwnedWorktree {
            repo: RepoId("repo".into()),
            branch: "topic".into(),
        };
        for (state, expected) in [
            (SessionState::Attached, true),
            (SessionState::Detached, true),
            (SessionState::Closed, false),
        ] {
            let sessions = vec![StoredSession {
                id: SessionId("session".into()),
                name: "Session".into(),
                members: vec![RepoId("repo".into())],
                owned: vec![worktree.clone()],
                state,
                state_changed_at: 0,
            }];
            assert_eq!(live_owner(&sessions, &worktree).is_some(), expected);
        }
    }

    #[test]
    fn candidate_scan_fetches_before_deciding_an_upstream_is_gone() {
        let temp = TempDir::new();
        let remote = temp.0.join("remote.git");
        let clone = temp.0.join("clone");
        let linked = temp.0.join("linked");
        fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--bare", "-q"]);
        fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "Grove Test"]);
        git(&clone, &["config", "user.email", "grove@example.test"]);
        fs::write(clone.join("tracked"), "base\n").unwrap();
        git(&clone, &["add", "tracked"]);
        git(&clone, &["commit", "-qm", "base"]);
        git(
            &clone,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&clone, &["push", "-qu", "origin", "main"]);
        git(
            &clone,
            &["worktree", "add", "-qb", "topic", linked.to_str().unwrap()],
        );
        fs::write(linked.join("topic"), "work\n").unwrap();
        git(&linked, &["add", "topic"]);
        git(&linked, &["commit", "-qm", "topic"]);
        git(&linked, &["push", "-qu", "origin", "topic"]);
        assert!(matches!(
            grove_git::ahead_behind(&linked).unwrap(),
            Tracking::Tracked(_)
        ));
        git(&remote, &["update-ref", "-d", "refs/heads/topic"]);

        let repository = Repository {
            id: RepoId("repo".into()),
            name: "repo".into(),
            path: clone,
            base_branch: Some("origin/main".into()),
        };
        let batch = prune_candidates(&repository, &[]).unwrap();
        assert!(batch.fetch_error.is_none());
        assert_eq!(batch.candidates.len(), 1);
        assert_eq!(batch.candidates[0].history, PruneHistory::UpstreamGone);
        assert!(batch.candidates[0].preselected());
    }

    #[test]
    fn fetch_prune_cannot_hide_commits_made_after_the_last_push() {
        let temp = TempDir::new();
        let remote = temp.0.join("remote.git");
        let clone = temp.0.join("clone");
        let linked = temp.0.join("linked");
        fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--bare", "-q"]);
        fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "Grove Test"]);
        git(&clone, &["config", "user.email", "grove@example.test"]);
        fs::write(clone.join("tracked"), "base\n").unwrap();
        git(&clone, &["add", "tracked"]);
        git(&clone, &["commit", "-qm", "base"]);
        git(
            &clone,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&clone, &["push", "-qu", "origin", "main"]);
        git(
            &clone,
            &["worktree", "add", "-qb", "topic", linked.to_str().unwrap()],
        );
        git(&linked, &["push", "-qu", "origin", "topic"]);
        fs::write(linked.join("local-only"), "work\n").unwrap();
        git(&linked, &["add", "local-only"]);
        git(&linked, &["commit", "-qm", "local only"]);
        assert_eq!(
            grove_git::ahead_behind(&linked).unwrap(),
            Tracking::Tracked(grove_git::AheadBehind {
                ahead: 1,
                behind: 0,
            })
        );
        git(&remote, &["update-ref", "-d", "refs/heads/topic"]);

        let repository = Repository {
            id: RepoId("repo".into()),
            name: "repo".into(),
            path: clone,
            base_branch: Some("origin/main".into()),
        };
        let batch = prune_candidates(&repository, &[]).unwrap();
        assert_eq!(batch.candidates[0].history, PruneHistory::UpstreamGone);
        assert!(
            batch.candidates[0]
                .reasons
                .contains(&PruneDisqualifier::Unpushed { commits: 1 })
        );
        assert!(!batch.candidates[0].preselected());
    }

    #[test]
    fn failed_fetch_keeps_rows_visible_but_unchecked() {
        let temp = TempDir::new();
        let clone = temp.0.join("clone");
        let linked = temp.0.join("linked");
        fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "Grove Test"]);
        git(&clone, &["config", "user.email", "grove@example.test"]);
        fs::write(clone.join("tracked"), "base\n").unwrap();
        git(&clone, &["add", "tracked"]);
        git(&clone, &["commit", "-qm", "base"]);
        git(
            &clone,
            &["worktree", "add", "-qb", "topic", linked.to_str().unwrap()],
        );
        git(
            &clone,
            &[
                "remote",
                "add",
                "origin",
                temp.0.join("missing.git").to_str().unwrap(),
            ],
        );
        let repository = Repository {
            id: RepoId("repo".into()),
            name: "repo".into(),
            path: clone,
            base_branch: Some("main".into()),
        };

        let batch = prune_candidates(&repository, &[]).unwrap();
        assert!(batch.fetch_error.is_some());
        assert_eq!(batch.candidates.len(), 1);
        assert!(!batch.candidates[0].preselected());
        assert!(
            batch.candidates[0]
                .reasons
                .contains(&PruneDisqualifier::FetchFailed)
        );
    }
}
