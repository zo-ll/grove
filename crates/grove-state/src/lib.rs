//! Session persistence. Backend lane only.
//!
//! Holds only what git cannot know: session name, membership, ownership and
//! state. Git stays authoritative for everything it can answer, so this store
//! can never contradict reality — delete it and you lose grouping, never work.
//!
//! See `SPEC.md` §6. Implemented in issues #6 and #7.

use grove_domain::{RepoId, SessionId, SessionState};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedWorktree {
    pub repo: RepoId,
    pub branch: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSession {
    pub id: SessionId,
    pub name: String,
    pub members: Vec<RepoId>,
    pub owned: Vec<OwnedWorktree>,
    pub state: SessionState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    sessions: Vec<StoredSession>,
    open: Option<SessionId>,
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            version: FORMAT_VERSION,
            sessions: Vec::new(),
            open: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("session {0:?} already exists")]
    SessionExists(SessionId),
    #[error("session {0:?} does not exist")]
    SessionMissing(SessionId),
    #[error("repo {repo:?} is not a member of session {session:?}")]
    NotMember { session: SessionId, repo: RepoId },
    #[error("worktree {repo:?}@{branch} is already owned by session {owner:?}")]
    OwnedByOther {
        repo: RepoId,
        branch: String,
        owner: SessionId,
    },
    #[error("worktree {repo:?}@{branch} is not owned by session {session:?}")]
    NotOwned {
        session: SessionId,
        repo: RepoId,
        branch: String,
    },
    #[error("the repository clone at {0} can never be session-owned")]
    CloneNotOwnable(PathBuf),
    #[error("adopt/release is unavailable because worktree_path contains {{session}}")]
    SessionPathTemplate,
    #[error("could not persist session state at {path}: {source}")]
    Persist { path: PathBuf, source: io::Error },
    #[error("could not serialize session state: {0}")]
    Serialize(#[from] serde_json::Error),
}

pub struct Store {
    path: PathBuf,
    session_paths: bool,
    state: StateFile,
}

impl Store {
    /// Loads the current process's store. Missing, unreadable, corrupt, or
    /// unknown-version files all degrade to an empty store.
    pub fn load(workspace: &Path, worktree_template: &str) -> Self {
        let state_home = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
            .unwrap_or_else(env::temp_dir);
        Self::load_at(&state_home, workspace, worktree_template)
    }

    pub fn load_at(state_home: &Path, workspace: &Path, worktree_template: &str) -> Self {
        let path = state_home
            .join("grove")
            .join(workspace_hash(workspace))
            .join("sessions.json");
        let mut state = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<StateFile>(&bytes).ok())
            .filter(|state| state.version == FORMAT_VERSION)
            .unwrap_or_default();

        // Loading happens when a daemon starts. No PTY survives that boundary.
        for session in &mut state.sessions {
            session.state = SessionState::Closed;
        }
        state.open = None;
        Self {
            path,
            session_paths: worktree_template.contains("{session}"),
            state,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn sessions(&self) -> &[StoredSession] {
        &self.state.sessions
    }

    pub fn open_session(&self) -> Option<&SessionId> {
        self.state.open.as_ref()
    }

    pub fn create(&mut self, id: SessionId, name: String) -> Result<(), Error> {
        if self.state.sessions.iter().any(|session| session.id == id) {
            return Err(Error::SessionExists(id));
        }
        self.state.sessions.push(StoredSession {
            id,
            name,
            members: Vec::new(),
            owned: Vec::new(),
            state: SessionState::Closed,
        });
        self.persist()
    }

    pub fn add_member(&mut self, session: &SessionId, repo: RepoId) -> Result<(), Error> {
        let target = self.session_mut(session)?;
        if !target.members.contains(&repo) {
            target.members.push(repo);
            self.persist()?;
        }
        Ok(())
    }

    pub fn remove_member(&mut self, session: &SessionId, repo: &RepoId) -> Result<(), Error> {
        let target = self.session_mut(session)?;
        target.members.retain(|member| member != repo);
        self.persist()
    }

    pub fn open(&mut self, id: &SessionId) -> Result<(), Error> {
        self.session(id)?;
        if let Some(previous) = self.state.open.take() {
            if previous != *id {
                if let Ok(session) = self.session_mut(&previous) {
                    session.state = SessionState::Detached;
                }
            }
        }
        self.session_mut(id)?.state = SessionState::Attached;
        self.state.open = Some(id.clone());
        self.persist()
    }

    pub fn detach(&mut self, id: &SessionId) -> Result<(), Error> {
        self.session_mut(id)?.state = SessionState::Detached;
        if self.state.open.as_ref() == Some(id) {
            self.state.open = None;
        }
        self.persist()
    }

    pub fn close(&mut self, id: &SessionId) -> Result<(), Error> {
        self.session_mut(id)?.state = SessionState::Closed;
        if self.state.open.as_ref() == Some(id) {
            self.state.open = None;
        }
        self.persist()
    }

    /// Ends the grouping record. Worktree deletion is orchestrated by the daemon.
    pub fn end(&mut self, id: &SessionId) -> Result<StoredSession, Error> {
        let index = self
            .state
            .sessions
            .iter()
            .position(|session| &session.id == id)
            .ok_or_else(|| Error::SessionMissing(id.clone()))?;
        let removed = self.state.sessions.remove(index);
        if self.state.open.as_ref() == Some(id) {
            self.state.open = None;
        }
        self.persist()?;
        Ok(removed)
    }

    pub fn adopt(
        &mut self,
        session: &SessionId,
        worktree: OwnedWorktree,
        worktree_path: &Path,
        clone_path: &Path,
    ) -> Result<(), Error> {
        self.check_movable()?;
        if same_path(worktree_path, clone_path) {
            return Err(Error::CloneNotOwnable(worktree_path.to_owned()));
        }
        if let Some(owner) = self.owner(&worktree) {
            if owner == session {
                return Ok(());
            }
            return Err(Error::OwnedByOther {
                repo: worktree.repo,
                branch: worktree.branch,
                owner: owner.clone(),
            });
        }
        let target = self.session_mut(session)?;
        if !target.members.contains(&worktree.repo) {
            return Err(Error::NotMember {
                session: session.clone(),
                repo: worktree.repo,
            });
        }
        target.owned.push(worktree);
        self.persist()
    }

    pub fn release(&mut self, session: &SessionId, worktree: &OwnedWorktree) -> Result<(), Error> {
        self.check_movable()?;
        let target = self.session_mut(session)?;
        let Some(index) = target.owned.iter().position(|owned| owned == worktree) else {
            return Err(Error::NotOwned {
                session: session.clone(),
                repo: worktree.repo.clone(),
                branch: worktree.branch.clone(),
            });
        };
        target.owned.remove(index);
        self.persist()
    }

    pub fn owner(&self, worktree: &OwnedWorktree) -> Option<&SessionId> {
        self.state
            .sessions
            .iter()
            .find(|session| session.owned.contains(worktree))
            .map(|session| &session.id)
    }

    fn check_movable(&self) -> Result<(), Error> {
        if self.session_paths {
            Err(Error::SessionPathTemplate)
        } else {
            Ok(())
        }
    }

    fn session(&self, id: &SessionId) -> Result<&StoredSession, Error> {
        self.state
            .sessions
            .iter()
            .find(|session| &session.id == id)
            .ok_or_else(|| Error::SessionMissing(id.clone()))
    }

    fn session_mut(&mut self, id: &SessionId) -> Result<&mut StoredSession, Error> {
        self.state
            .sessions
            .iter_mut()
            .find(|session| &session.id == id)
            .ok_or_else(|| Error::SessionMissing(id.clone()))
    }

    fn persist(&self) -> Result<(), Error> {
        let parent = self.path.parent().expect("sessions.json has a parent");
        fs::create_dir_all(parent).map_err(|source| Error::Persist {
            path: parent.to_owned(),
            source,
        })?;
        let temporary = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.state)?;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|source| Error::Persist {
            path: temporary.clone(),
            source,
        })?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| Error::Persist {
                path: temporary.clone(),
                source,
            })?;
        fs::rename(&temporary, &self.path).map_err(|source| Error::Persist {
            path: self.path.clone(),
            source,
        })
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    fs::canonicalize(left).unwrap_or_else(|_| left.to_owned())
        == fs::canonicalize(right).unwrap_or_else(|_| right.to_owned())
}

/// Stable FNV-1a hash of the canonical workspace spelling.
pub fn workspace_hash(workspace: &Path) -> String {
    let path = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_owned());
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!("grove-state-{unique}"));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sid(value: &str) -> SessionId {
        SessionId(value.into())
    }
    fn rid(value: &str) -> RepoId {
        RepoId(value.into())
    }
    fn owned(repo: &str, branch: &str) -> OwnedWorktree {
        OwnedWorktree {
            repo: rid(repo),
            branch: branch.into(),
        }
    }

    #[test]
    fn hash_is_stable_and_separates_workspaces() {
        assert_eq!(
            workspace_hash(Path::new("/one")),
            workspace_hash(Path::new("/one"))
        );
        assert_ne!(
            workspace_hash(Path::new("/one")),
            workspace_hash(Path::new("/two"))
        );
    }

    #[test]
    fn corrupt_and_absent_files_are_empty() {
        let temp = TempDir::new();
        let mut store = Store::load_at(&temp.0, Path::new("/workspace"), "{repo}/{branch}");
        assert!(store.sessions().is_empty());
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), "not json").unwrap();
        store = Store::load_at(&temp.0, Path::new("/workspace"), "{repo}/{branch}");
        assert!(store.sessions().is_empty());
    }

    #[test]
    fn transitions_keep_exactly_one_session_open_and_survive_reload_closed() {
        let temp = TempDir::new();
        let mut store = Store::load_at(&temp.0, Path::new("/workspace"), "{repo}/{branch}");
        store.create(sid("one"), "One".into()).unwrap();
        store.create(sid("two"), "Two".into()).unwrap();
        store.open(&sid("one")).unwrap();
        store.open(&sid("two")).unwrap();
        assert_eq!(store.open_session(), Some(&sid("two")));
        assert_eq!(
            store.session(&sid("one")).unwrap().state,
            SessionState::Detached
        );
        assert_eq!(
            store.session(&sid("two")).unwrap().state,
            SessionState::Attached
        );
        let reloaded = Store::load_at(&temp.0, Path::new("/workspace"), "{repo}/{branch}");
        assert!(reloaded.open_session().is_none());
        assert!(
            reloaded
                .sessions()
                .iter()
                .all(|s| s.state == SessionState::Closed)
        );
    }

    #[test]
    fn membership_and_ownership_are_separate_and_clone_is_refused() {
        let temp = TempDir::new();
        let clone = temp.0.join("clone");
        fs::create_dir_all(&clone).unwrap();
        let linked = temp.0.join("linked");
        fs::create_dir_all(&linked).unwrap();
        let mut store = Store::load_at(&temp.0, Path::new("/workspace"), "{repo}/{branch}");
        store.create(sid("one"), "One".into()).unwrap();
        store.add_member(&sid("one"), rid("repo")).unwrap();
        assert!(store.session(&sid("one")).unwrap().owned.is_empty());
        assert!(matches!(
            store.adopt(&sid("one"), owned("repo", "main"), &clone, &clone),
            Err(Error::CloneNotOwnable(_))
        ));
        let worktree = owned("repo", "topic");
        store
            .adopt(&sid("one"), worktree.clone(), &linked, &clone)
            .unwrap();
        assert_eq!(store.owner(&worktree), Some(&sid("one")));
        store.remove_member(&sid("one"), &rid("repo")).unwrap();
        assert_eq!(store.owner(&worktree), Some(&sid("one")));
        store.release(&sid("one"), &worktree).unwrap();
        assert!(store.owner(&worktree).is_none());
    }

    #[test]
    fn another_sessions_worktree_is_read_only() {
        let temp = TempDir::new();
        let clone = temp.0.join("clone");
        let linked = temp.0.join("linked");
        fs::create_dir_all(&clone).unwrap();
        fs::create_dir_all(&linked).unwrap();
        let mut store = Store::load_at(&temp.0, Path::new("/workspace"), "{repo}/{branch}");
        for id in ["one", "two"] {
            store.create(sid(id), id.into()).unwrap();
            store.add_member(&sid(id), rid("repo")).unwrap();
        }
        let worktree = owned("repo", "topic");
        store
            .adopt(&sid("one"), worktree.clone(), &linked, &clone)
            .unwrap();
        assert!(matches!(
            store.adopt(&sid("two"), worktree, &linked, &clone),
            Err(Error::OwnedByOther { owner, .. }) if owner == sid("one")
        ));
    }

    #[test]
    fn session_path_template_refuses_adopt_and_release() {
        let temp = TempDir::new();
        let mut store = Store::load_at(&temp.0, Path::new("/workspace"), "{session}/{repo}");
        store.create(sid("one"), "One".into()).unwrap();
        store.add_member(&sid("one"), rid("repo")).unwrap();
        assert!(matches!(
            store.adopt(
                &sid("one"),
                owned("repo", "topic"),
                Path::new("/linked"),
                Path::new("/clone")
            ),
            Err(Error::SessionPathTemplate)
        ));
    }

    #[test]
    fn close_and_end_complete_the_state_machine() {
        let temp = TempDir::new();
        let mut store = Store::load_at(&temp.0, Path::new("/workspace"), "{repo}/{branch}");
        store.create(sid("one"), "One".into()).unwrap();
        store.open(&sid("one")).unwrap();
        store.close(&sid("one")).unwrap();
        assert_eq!(
            store.session(&sid("one")).unwrap().state,
            SessionState::Closed
        );
        let ended = store.end(&sid("one")).unwrap();
        assert_eq!(ended.id, sid("one"));
        assert!(store.sessions().is_empty());
    }
}
