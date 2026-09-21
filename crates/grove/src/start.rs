//! Starting where you are.
//!
//! `grove` run inside a repository opens that repository: the workspace is
//! the repository's root rather than the directory grove happened to be run
//! from, and a session named after it is resumed — or made, with the
//! repository as its member — without the user typing a command first.
//!
//! The decisions live here, away from the event loop, so they can be tested
//! against plain data: the loop only sends what [`Autostart::next`] returns.
//! Nothing here runs git. The repository is found the way git finds it — by a
//! `.git` entry in the directory or one above it — and the branch is read from
//! `HEAD`, both plain file reads; the TUI's address space stays git-free.

use std::fs;
use std::path::{Path, PathBuf};

use grove_domain::SessionId;
use grove_proto::{RepoRow, Request, SessionRow};

/// The repository `start` is in: the nearest directory, itself or above,
/// holding a `.git` directory or file (a linked worktree has a file).
pub fn repo_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The branch checked out at `root`, or `None` when `HEAD` is detached or
/// cannot be read.
pub fn checked_out_branch(root: &Path) -> Option<String> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        // A linked worktree: `.git` is a file pointing at its own git dir.
        let pointer = fs::read_to_string(&dot_git).ok()?;
        let target = PathBuf::from(pointer.trim().strip_prefix("gitdir: ")?);
        if target.is_absolute() {
            target
        } else {
            root.join(target)
        }
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_owned)
}

/// What grove was started in, when that is a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Here {
    /// The session's name: the repository's directory name.
    pub name: String,
    /// The branch to land the cursor on, if one is checked out.
    pub branch: Option<String>,
}

impl Here {
    /// `Some` when `start` is inside a repository, with the workspace to use
    /// — the repository's root — beside it.
    pub fn of(start: &Path) -> Option<(PathBuf, Self)> {
        let root = repo_root(start)?;
        let name = root.file_name()?.to_string_lossy().into_owned();
        let branch = checked_out_branch(&root);
        Some((root, Self { name, branch }))
    }
}

/// Opening the session for [`Here`], one daemon answer at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Autostart {
    name: String,
    heard_sessions: bool,
    phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting to learn which sessions exist.
    Deciding,
    /// `SessionNew` sent; waiting for it to open, then for the repo to add.
    Creating,
    Done,
}

impl Autostart {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            heard_sessions: false,
            phase: Phase::Deciding,
        }
    }

    /// The session list has arrived at least once, so "no session called
    /// this" is an answer rather than not having asked yet.
    pub fn sessions_arrived(&mut self) {
        self.heard_sessions = true;
    }

    pub fn done(&self) -> bool {
        self.phase == Phase::Done
    }

    /// The requests that move things along, given what the TUI knows now.
    /// Empty when there is nothing to do until the daemon says more.
    pub fn next(
        &mut self,
        sessions: &[SessionRow],
        open: Option<&SessionId>,
        repos: &[RepoRow],
    ) -> Vec<Request> {
        match self.phase {
            Phase::Deciding if !self.heard_sessions => Vec::new(),
            Phase::Deciding => {
                // A session already open is the user's, from an earlier grove
                // in this same repository; replacing it would be rude.
                if open.is_some() {
                    self.phase = Phase::Done;
                    return Vec::new();
                }
                if let Some(row) = sessions.iter().find(|row| row.name == self.name) {
                    self.phase = Phase::Done;
                    // Membership shows in the repo rows, and the ones already
                    // asked for were answered before the session opened.
                    return vec![Request::OpenSession(row.id.clone()), Request::ListRepos];
                }
                self.phase = Phase::Creating;
                vec![Request::SessionNew {
                    name: self.name.clone(),
                }]
            }
            Phase::Creating => {
                let Some(open) = open else {
                    return Vec::new();
                };
                let ours = sessions
                    .iter()
                    .any(|row| &row.id == open && row.name == self.name);
                if !ours {
                    // Something else opened in between; leave it be.
                    self.phase = Phase::Done;
                    return Vec::new();
                }
                // The workspace is the repository, so it is the one repo.
                let Some(repo) = repos.first() else {
                    return Vec::new();
                };
                self.phase = Phase::Done;
                vec![
                    Request::AddMember {
                        session: open.clone(),
                        repo: repo.repo.clone(),
                    },
                    // Membership shows in the repo rows; ask for them again.
                    Request::ListRepos,
                ]
            }
            Phase::Done => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_domain::{RepoId, SessionState};

    struct Dir(PathBuf);

    impl Dir {
        fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "grove-start-{label}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn session(id: &str, name: &str, state: SessionState) -> SessionRow {
        SessionRow {
            id: SessionId(id.into()),
            name: name.into(),
            members: Vec::new(),
            state,
            terminals: 0,
            since: 0,
            size: 0,
        }
    }

    fn repo(id: &str) -> RepoRow {
        RepoRow {
            repo: RepoId(id.into()),
            name: id.into(),
            base_branch: "main".into(),
            base_from_origin_head: true,
            worktrees: 0,
            dirty: false,
            member: false,
        }
    }

    #[test]
    fn a_subdirectory_of_a_repo_starts_at_its_root() {
        let dir = Dir::new("root");
        let repo = dir.0.join("shop");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("src/cart")).unwrap();
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feat/cart\n").unwrap();

        let (root, here) = Here::of(&repo.join("src/cart")).expect("inside a repo");
        assert_eq!(root, repo);
        assert_eq!(here.name, "shop");
        assert_eq!(here.branch.as_deref(), Some("feat/cart"));
    }

    #[test]
    fn a_linked_worktree_is_a_repo_too() {
        let dir = Dir::new("linked");
        let admin = dir.0.join("admin");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("HEAD"), "ref: refs/heads/fix/rounding\n").unwrap();
        let tree = dir.0.join("fix-rounding");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();

        let (root, here) = Here::of(&tree).expect("a linked worktree");
        assert_eq!(root, tree);
        assert_eq!(here.branch.as_deref(), Some("fix/rounding"));
    }

    #[test]
    fn outside_a_repo_nothing_changes() {
        let dir = Dir::new("plain");
        assert_eq!(Here::of(&dir.0), None);
    }

    #[test]
    fn a_detached_head_has_no_branch_to_land_on() {
        let dir = Dir::new("detached");
        fs::create_dir_all(dir.0.join(".git")).unwrap();
        fs::write(
            dir.0.join(".git/HEAD"),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
        )
        .unwrap();
        assert_eq!(checked_out_branch(&dir.0), None);
    }

    #[test]
    fn nothing_is_decided_before_the_session_list_arrives() {
        let mut auto = Autostart::new("shop");
        assert!(auto.next(&[], None, &[repo("shop")]).is_empty());
        assert!(!auto.done());
    }

    #[test]
    fn a_new_repo_gets_a_session_named_after_it_with_itself_as_member() {
        let mut auto = Autostart::new("shop");
        auto.sessions_arrived();
        assert_eq!(
            auto.next(&[], None, &[]),
            vec![Request::SessionNew {
                name: "shop".into()
            }]
        );
        // The daemon opens it; the repo list may not be back yet.
        let opened = [session("s1", "shop", SessionState::Attached)];
        let open = SessionId("s1".into());
        assert!(auto.next(&opened, Some(&open), &[]).is_empty());
        assert_eq!(
            auto.next(&opened, Some(&open), &[repo("shop")]),
            vec![
                Request::AddMember {
                    session: open.clone(),
                    repo: RepoId("shop".into()),
                },
                Request::ListRepos,
            ]
        );
        assert!(auto.done());
        assert!(auto.next(&opened, Some(&open), &[repo("shop")]).is_empty());
    }

    #[test]
    fn a_session_already_named_after_the_repo_is_resumed_not_duplicated() {
        let mut auto = Autostart::new("shop");
        auto.sessions_arrived();
        let stored = [
            session("s0", "other", SessionState::Closed),
            session("s1", "shop", SessionState::Detached),
        ];
        // With the repo list asked for again: the first one was answered
        // before the session opened, so it says the repo is not a member,
        // and REPOS read "none in session" until something else refreshed
        // it — the watcher, after the open's fetch, 2.3s later.
        assert_eq!(
            auto.next(&stored, None, &[repo("shop")]),
            vec![
                Request::OpenSession(SessionId("s1".into())),
                Request::ListRepos,
            ]
        );
        assert!(auto.done());
    }

    #[test]
    fn a_session_already_open_is_left_alone() {
        let mut auto = Autostart::new("shop");
        auto.sessions_arrived();
        let open = SessionId("s0".into());
        let stored = [session("s0", "other", SessionState::Attached)];
        assert!(auto.next(&stored, Some(&open), &[repo("shop")]).is_empty());
        assert!(auto.done());
    }
}
