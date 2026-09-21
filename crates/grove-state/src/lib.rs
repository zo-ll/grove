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
const SNAPSHOT_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// Metadata needed to recreate a terminal shell after the daemon has stopped.
/// `last_command` is informational and must never be executed during restore.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotTerminal {
    pub worktree: PathBuf,
    pub last_command: Option<String>,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session: SessionId,
    pub terminals: Vec<SnapshotTerminal>,
}

/// Existing worktrees can be restored while deleted ones are reported to the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestorePlan {
    pub terminals: Vec<SnapshotTerminal>,
    pub missing: Vec<SnapshotTerminal>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SnapshotFile {
    version: u32,
    session: SessionId,
    terminals: Vec<SnapshotTerminal>,
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
    #[error("session name {name:?} is already held by session {}", owner.0)]
    SessionNameTaken { name: String, owner: SessionId },
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
    #[error("no snapshot exists for session {session:?} at {path}")]
    SnapshotMissing { session: SessionId, path: PathBuf },
    #[error("could not read snapshot at {path}: {source}")]
    SnapshotRead { path: PathBuf, source: io::Error },
    #[error("could not parse snapshot at {path}: {source}")]
    SnapshotParse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("snapshot at {path} uses unsupported format version {version}")]
    SnapshotVersion { path: PathBuf, version: u32 },
    #[error("snapshot at {path} belongs to {actual:?}, not {expected:?}")]
    SnapshotSession {
        path: PathBuf,
        expected: SessionId,
        actual: SessionId,
    },
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
        let name = normalize_session_name(name);
        if let Some(owner) = self
            .state
            .sessions
            .iter()
            .find(|session| session.name.trim() == name)
            .map(|session| session.id.clone())
        {
            return Err(Error::SessionNameTaken { name, owner });
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

    pub fn rename(&mut self, session: &SessionId, name: String) -> Result<(), Error> {
        self.session(session)?;
        let name = normalize_session_name(name);
        if let Some(owner) = self
            .state
            .sessions
            .iter()
            .find(|candidate| candidate.id != *session && candidate.name.trim() == name)
            .map(|candidate| candidate.id.clone())
        {
            return Err(Error::SessionNameTaken { name, owner });
        }
        self.session_mut(session)?.name = name;
        self.persist()
    }

    pub fn remove_member(&mut self, session: &SessionId, repo: &RepoId) -> Result<(), Error> {
        let target = self.session_mut(session)?;
        target.members.retain(|member| member != repo);
        self.persist()
    }

    pub fn open(&mut self, id: &SessionId) -> Result<(), Error> {
        self.session(id)?;
        if let Some(previous) = self.state.open.take()
            && previous != *id
            && let Ok(session) = self.session_mut(&previous)
        {
            session.state = SessionState::Detached;
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
        self.claim(session, worktree, worktree_path, clone_path)
    }

    /// Records a worktree just created at the configured destination. This is
    /// not a move, so `{session}` templates do not disable it.
    pub fn own_created(
        &mut self,
        session: &SessionId,
        worktree: OwnedWorktree,
        worktree_path: &Path,
        clone_path: &Path,
    ) -> Result<(), Error> {
        self.claim(session, worktree, worktree_path, clone_path)
    }

    fn claim(
        &mut self,
        session: &SessionId,
        worktree: OwnedWorktree,
        worktree_path: &Path,
        clone_path: &Path,
    ) -> Result<(), Error> {
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

    pub fn ownership_movable(&self) -> bool {
        !self.session_paths
    }

    /// Writes a snapshot only when explicitly called. No lifecycle transition
    /// invokes this method, so snapshots are never automatic.
    pub fn save_snapshot(
        &self,
        session: &SessionId,
        terminals: Vec<SnapshotTerminal>,
    ) -> Result<(), Error> {
        self.session(session)?;
        let path = self.snapshot_path(session);
        let file = SnapshotFile {
            version: SNAPSHOT_FORMAT_VERSION,
            session: session.clone(),
            terminals,
        };
        let bytes = serde_json::to_vec_pretty(&file)?;
        write_atomic(&path, &bytes)
    }

    pub fn load_snapshot(&self, session: &SessionId) -> Result<SessionSnapshot, Error> {
        self.session(session)?;
        let path = self.snapshot_path(session);
        let bytes = fs::read(&path).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                Error::SnapshotMissing {
                    session: session.clone(),
                    path: path.clone(),
                }
            } else {
                Error::SnapshotRead {
                    path: path.clone(),
                    source,
                }
            }
        })?;
        let file: SnapshotFile =
            serde_json::from_slice(&bytes).map_err(|source| Error::SnapshotParse {
                path: path.clone(),
                source,
            })?;
        if file.version != SNAPSHOT_FORMAT_VERSION {
            return Err(Error::SnapshotVersion {
                path,
                version: file.version,
            });
        }
        if file.session != *session {
            return Err(Error::SnapshotSession {
                path,
                expected: session.clone(),
                actual: file.session,
            });
        }
        Ok(SessionSnapshot {
            session: session.clone(),
            terminals: file.terminals,
        })
    }

    /// Separates restorable terminals from worktrees deleted since the manual
    /// snapshot was taken. The caller can restore the first group and surface
    /// every gap from the second group.
    pub fn restore_plan(&self, session: &SessionId) -> Result<RestorePlan, Error> {
        let snapshot = self.load_snapshot(session)?;
        let (terminals, missing) = snapshot
            .terminals
            .into_iter()
            .partition(|terminal| terminal.worktree.is_dir());
        Ok(RestorePlan { terminals, missing })
    }

    pub fn snapshot_path(&self, session: &SessionId) -> PathBuf {
        self.path
            .parent()
            .expect("sessions.json has a parent")
            .join("snapshots")
            .join(format!("{}.json", snapshot_filename(session)))
    }

    fn check_movable(&self) -> Result<(), Error> {
        if self.session_paths {
            Err(Error::SessionPathTemplate)
        } else {
            Ok(())
        }
    }

    pub fn session(&self, id: &SessionId) -> Result<&StoredSession, Error> {
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

    /// Drops ownership after the daemon has successfully removed a worktree
    /// while ending a session. Unlike `release`, this never moves a worktree
    /// and therefore remains valid with a `{session}` path template.
    pub fn forget_owned(
        &mut self,
        session: &SessionId,
        worktree: &OwnedWorktree,
    ) -> Result<(), Error> {
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

    fn persist(&self) -> Result<(), Error> {
        let bytes = serde_json::to_vec_pretty(&self.state)?;
        write_atomic(&self.path, &bytes)
    }
}

fn normalize_session_name(name: String) -> String {
    name.trim().to_owned()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let parent = path.parent().expect("state file has a parent");
    fs::create_dir_all(parent).map_err(|source| Error::Persist {
        path: parent.to_owned(),
        source,
    })?;
    let temporary = path.with_extension("json.tmp");
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
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| Error::Persist {
            path: temporary.clone(),
            source,
        })?;
    fs::rename(&temporary, path).map_err(|source| Error::Persist {
        path: path.to_owned(),
        source,
    })?;
    #[cfg(unix)]
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| Error::Persist {
            path: parent.to_owned(),
            source,
        })?;
    Ok(())
}

fn snapshot_filename(session: &SessionId) -> String {
    let mut encoded = String::new();
    for byte in session.0.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to a string cannot fail");
        }
    }
    if encoded.is_empty() {
        "%".into()
    } else {
        encoded
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
        store
            .own_created(
                &sid("one"),
                owned("repo", "created"),
                Path::new("/linked"),
                Path::new("/clone"),
            )
            .unwrap();
        assert_eq!(store.owner(&owned("repo", "created")), Some(&sid("one")));
        assert!(matches!(
            store.release(&sid("one"), &owned("repo", "created")),
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

    #[test]
    fn manual_snapshot_round_trips_and_reports_deleted_worktrees() {
        let temp = TempDir::new();
        let existing = temp.0.join("existing");
        let deleted = temp.0.join("deleted");
        fs::create_dir_all(&existing).unwrap();
        fs::create_dir_all(&deleted).unwrap();
        let mut store = Store::load_at(&temp.0, Path::new("/workspace"), "{repo}/{branch}");
        store.create(sid("one"), "One".into()).unwrap();
        let terminals = vec![
            SnapshotTerminal {
                worktree: existing.clone(),
                last_command: Some("cargo test".into()),
                rows: 24,
                cols: 80,
            },
            SnapshotTerminal {
                worktree: deleted.clone(),
                last_command: Some("cargo run".into()),
                rows: 40,
                cols: 120,
            },
        ];
        store.save_snapshot(&sid("one"), terminals.clone()).unwrap();
        assert_eq!(
            store.load_snapshot(&sid("one")).unwrap(),
            SessionSnapshot {
                session: sid("one"),
                terminals: terminals.clone(),
            }
        );

        fs::remove_dir_all(&deleted).unwrap();
        let plan = store.restore_plan(&sid("one")).unwrap();
        assert_eq!(plan.terminals, vec![terminals[0].clone()]);
        assert_eq!(plan.missing, vec![terminals[1].clone()]);
    }

    #[test]
    fn snapshot_names_cannot_escape_the_workspace_state_directory() {
        let temp = TempDir::new();
        let mut store = Store::load_at(&temp.0, Path::new("/workspace"), "{repo}/{branch}");
        let hostile = sid("../../other/session");
        store.create(hostile.clone(), "Hostile".into()).unwrap();
        store.save_snapshot(&hostile, Vec::new()).unwrap();
        let path = store.snapshot_path(&hostile);
        assert_eq!(
            path.parent(),
            Some(store.path().parent().unwrap().join("snapshots").as_path())
        );
        assert!(path.exists());
        assert_eq!(store.load_snapshot(&hostile).unwrap().session, hostile);
    }

    #[test]
    fn empty_and_nul_session_ids_have_distinct_snapshot_files() {
        let temp = TempDir::new();
        let mut store = Store::load_at(&temp.0, Path::new("/workspace"), "{repo}/{branch}");
        let empty = sid("");
        let nul = sid("\0");
        store.create(empty.clone(), "Empty".into()).unwrap();
        store.create(nul.clone(), "Nul".into()).unwrap();
        assert_ne!(store.snapshot_path(&empty), store.snapshot_path(&nul));
        store.save_snapshot(&empty, Vec::new()).unwrap();
        store.save_snapshot(&nul, Vec::new()).unwrap();
        assert_eq!(store.load_snapshot(&empty).unwrap().session, empty);
        assert_eq!(store.load_snapshot(&nul).unwrap().session, nul);
    }
}
