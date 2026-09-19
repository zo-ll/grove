//! Session transitions that coordinate persistent state, Git and PTYs.

use crate::fetch::{FetchPolicy, FetchResult, FetchStatus};
use crate::terminal::{TerminalError, TerminalManager};
use grove_domain::{RepoId, SessionId, SessionState};
use grove_git::{RemoveOptions, Repository};
use grove_lua::{DaemonRuntime, HookReport, LifecycleEvent, LifecyclePayload, WorktreePathContext};
use grove_proto::{Event, Request, SessionRow, TerminalId, WorktreeRef};
use grove_state::{OwnedWorktree, Store};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectFailure {
    pub worktree: Option<OwnedWorktree>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenReport {
    pub fetches: Vec<FetchResult>,
    pub terminals: Vec<TerminalId>,
    pub failed: Vec<EffectFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseReport {
    pub killed: Vec<TerminalId>,
    pub failed: Vec<EffectFailure>,
    pub closed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndWorktree {
    pub worktree: OwnedWorktree,
    pub path: PathBuf,
    pub dirty_files: u32,
    pub foreground: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndPlan {
    pub session: SessionId,
    pub worktrees: Vec<EndWorktree>,
    pub missing: Vec<OwnedWorktree>,
}

impl EndPlan {
    pub fn has_uncommitted_work(&self) -> bool {
        self.worktrees
            .iter()
            .any(|worktree| worktree.dirty_files > 0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndReport {
    pub removed: Vec<OwnedWorktree>,
    pub failed: Vec<EffectFailure>,
    pub ended: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedWorktree {
    pub worktree: OwnedWorktree,
    pub path: PathBuf,
    pub terminal: Option<TerminalId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateReport {
    pub created: Vec<CreatedWorktree>,
    pub failed: Vec<EffectFailure>,
}

#[derive(Debug, Error)]
pub enum OrchestrationError {
    #[error(transparent)]
    State(#[from] grove_state::Error),
    #[error("repository {0:?} is not available in this workspace")]
    RepositoryMissing(RepoId),
    #[error("worktree {0:?} does not exist")]
    WorktreeMissing(OwnedWorktree),
    #[error("repository {0:?} has no base branch")]
    BaseMissing(RepoId),
    #[error("repo {repo:?} is not a member of session {session:?}")]
    NotMember { session: SessionId, repo: RepoId },
    #[error("repo {repo:?} still owns worktrees in session {session:?}")]
    MemberOwnsWorktrees { session: SessionId, repo: RepoId },
    #[error("terminal operation failed: {0}")]
    Terminal(#[from] TerminalError),
    #[error("git inspection failed: {0}")]
    GitRead(#[from] grove_git::Error),
    #[error("git operation failed: {0}")]
    GitWrite(#[from] grove_git::WriteError),
    #[error("could not compute worktree path: {0}")]
    WorktreePath(String),
    #[error("request is outside session orchestration")]
    UnsupportedRequest,
}

#[derive(Clone, Debug)]
struct LiveTerminal {
    worktree: OwnedWorktree,
    id: TerminalId,
}

/// The one backend object allowed to combine state, Git and terminal effects.
pub struct SessionOrchestrator {
    store: Store,
    repositories: HashMap<RepoId, Repository>,
    terminals: TerminalManager,
    fetch: FetchPolicy,
    runtime: DaemonRuntime,
    live: HashMap<SessionId, Vec<LiveTerminal>>,
    hook_reports: Vec<HookReport>,
    notified_exits: HashSet<TerminalId>,
}

impl SessionOrchestrator {
    pub fn new(
        store: Store,
        repositories: Vec<Repository>,
        terminals: TerminalManager,
        fetch: FetchPolicy,
        runtime: DaemonRuntime,
    ) -> Self {
        let input = terminals.input_handle();
        let mut runtime = runtime;
        runtime.set_terminal_sender(move |terminal, bytes| {
            input
                .send(TerminalId(terminal), bytes)
                .map_err(|error| error.to_string())
        });
        Self {
            store,
            repositories: repositories
                .into_iter()
                .map(|repository| (repository.id.clone(), repository))
                .collect(),
            terminals,
            fetch,
            runtime,
            live: HashMap::new(),
            hook_reports: Vec::new(),
            notified_exits: HashSet::new(),
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn terminals(&self) -> &TerminalManager {
        &self.terminals
    }

    /// Applies the session-related protocol surface. Each call completes its
    /// effects before returning events, so the socket layer remains transport.
    pub fn handle_request(&mut self, request: Request) -> Vec<Event> {
        self.fire_observed_terminal_exits();
        let context = format!("{request:?}");
        let mut events = match self.apply_request(request) {
            Ok(events) => events,
            Err(error) => vec![Event::Failed {
                context,
                message: error.to_string(),
            }],
        };
        self.hook_reports.extend(self.runtime.drain_reports());
        events.extend(self.hook_reports.drain(..).filter_map(hook_report_event));
        events
    }

    fn apply_request(&mut self, request: Request) -> Result<Vec<Event>, OrchestrationError> {
        match request {
            Request::ListSessions => {
                let ids: Vec<_> = self
                    .store
                    .sessions()
                    .iter()
                    .map(|session| session.id.clone())
                    .collect();
                let rows = ids
                    .iter()
                    .map(|session| self.session_row(session))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(vec![Event::Sessions(rows)])
            }
            Request::SessionNew { name } => {
                let session = next_session_id();
                self.store.create(session.clone(), name)?;
                let report = self.open(&session)?;
                let mut events = fetch_failure_events(&report.fetches);
                events.push(self.session_changed(&session)?);
                Ok(events)
            }
            Request::SessionRename { session, name } => {
                self.store.rename(&session, name)?;
                Ok(vec![self.session_changed(&session)?])
            }
            Request::AddMember { session, repo } => {
                self.repository(&repo)?;
                self.store.add_member(&session, repo)?;
                Ok(vec![self.session_changed(&session)?])
            }
            Request::RemoveMember { session, repo } => {
                if self
                    .store
                    .session(&session)?
                    .owned
                    .iter()
                    .any(|worktree| worktree.repo == repo)
                {
                    return Err(OrchestrationError::MemberOwnsWorktrees { session, repo });
                }
                self.store.remove_member(&session, &repo)?;
                Ok(vec![self.session_changed(&session)?])
            }
            Request::OpenSession(session) => {
                let report = self.open(&session)?;
                let mut events = fetch_failure_events(&report.fetches);
                events.extend(effect_failure_events("open session", &report.failed));
                events.push(self.session_changed(&session)?);
                Ok(events)
            }
            Request::DetachSession(session) => {
                self.detach(&session)?;
                Ok(vec![self.session_changed(&session)?])
            }
            Request::CloseSession(session) => {
                let report = self.close(&session)?;
                let mut events = effect_failure_events("close session", &report.failed);
                events.push(self.session_changed(&session)?);
                Ok(events)
            }
            Request::EndSession(session) => {
                // EndSession is the confirmation verb. The UI builds its
                // warning from daemon-owned worktree facts before sending it;
                // direct callers can use `end_plan` for the same facts.
                let report = self.end_confirmed(&session)?;
                let mut events = effect_failure_events("end session", &report.failed);
                if report.ended {
                    events.push(Event::SessionEnded(session));
                } else {
                    events.push(self.session_changed(&session)?);
                }
                Ok(events)
            }
            Request::AdoptWorktree { session, worktree } => {
                self.adopt(&session, owned_ref(worktree))?;
                Ok(vec![self.session_changed(&session)?])
            }
            Request::ReleaseWorktree { session, worktree } => {
                self.release(&session, &owned_ref(worktree))?;
                Ok(vec![self.session_changed(&session)?])
            }
            Request::NewWorktrees {
                session,
                branch,
                repos,
            } => {
                let report = self.create_worktrees(&session, &branch, &repos)?;
                let mut events = effect_failure_events("create worktree", &report.failed);
                events.push(self.session_changed(&session)?);
                Ok(events)
            }
            Request::Fetch { session, repo } => {
                let stored = self.store.session(&session)?.clone();
                let repositories: Vec<_> = self.repositories.values().cloned().collect();
                let results = self
                    .fetch
                    .fetch_on_demand(&stored, &repositories, repo.as_ref());
                let mut events = fetch_failure_events(&results);
                events.push(self.session_changed(&session)?);
                Ok(events)
            }
            _ => Err(OrchestrationError::UnsupportedRequest),
        }
    }

    fn session_changed(&self, session: &SessionId) -> Result<Event, OrchestrationError> {
        Ok(Event::SessionChanged(self.session_row(session)?))
    }

    fn session_row(&self, session: &SessionId) -> Result<SessionRow, OrchestrationError> {
        let stored = self.store.session(session)?;
        let members = stored
            .members
            .iter()
            .map(|repo| {
                self.repositories
                    .get(repo)
                    .map(|repository| repository.name.clone())
                    .unwrap_or_else(|| repo.0.clone())
            })
            .collect();
        let terminals = self
            .live
            .get(session)
            .into_iter()
            .flatten()
            .filter(|terminal| self.terminals.is_alive(terminal.id).unwrap_or(false))
            .count();
        Ok(SessionRow {
            id: stored.id.clone(),
            name: stored.name.clone(),
            members,
            state: stored.state,
            terminals: u32::try_from(terminals).unwrap_or(u32::MAX),
            since: 0,
            size: 0,
        })
    }

    pub fn open(&mut self, session: &SessionId) -> Result<OpenReport, OrchestrationError> {
        let stored = self.store.session(session)?.clone();
        let repositories: Vec<_> = self.repositories.values().cloned().collect();
        let fetches = self.fetch.fetch_on_open(&stored, &repositories);
        let mut terminal_ids = Vec::new();
        let mut failed = Vec::new();
        for owned in &stored.owned {
            if let Some(id) = self.live_terminal(session, owned) {
                terminal_ids.push(id);
                continue;
            }
            match self.resolve_owned(owned).and_then(|path| {
                self.terminals
                    .spawn_worktree(&path, DEFAULT_ROWS, DEFAULT_COLS)
                    .map_err(OrchestrationError::from)
            }) {
                Ok(id) => {
                    self.live
                        .entry(session.clone())
                        .or_default()
                        .push(LiveTerminal {
                            worktree: owned.clone(),
                            id,
                        });
                    terminal_ids.push(id);
                    self.fire_terminal(LifecycleEvent::TerminalSpawned, session, owned, id);
                }
                Err(error) => failed.push(EffectFailure {
                    worktree: Some(owned.clone()),
                    message: error.to_string(),
                }),
            }
        }
        self.store.open(session)?;
        self.fire_session(LifecycleEvent::SessionOpened, session);
        Ok(OpenReport {
            fetches,
            terminals: terminal_ids,
            failed,
        })
    }

    pub fn detach(&mut self, session: &SessionId) -> Result<(), OrchestrationError> {
        self.store.detach(session)?;
        Ok(())
    }

    pub fn close(&mut self, session: &SessionId) -> Result<CloseReport, OrchestrationError> {
        self.store.session(session)?;
        let mut killed = Vec::new();
        let mut failed = Vec::new();
        let mut remaining = Vec::new();
        for terminal in self.live.remove(session).unwrap_or_default() {
            match self.terminals.kill(terminal.id) {
                Ok(()) => {
                    killed.push(terminal.id);
                    self.fire_terminal(
                        LifecycleEvent::TerminalExited,
                        session,
                        &terminal.worktree,
                        terminal.id,
                    );
                    self.notified_exits.insert(terminal.id);
                }
                Err(error) => {
                    failed.push(EffectFailure {
                        worktree: Some(terminal.worktree.clone()),
                        message: error.to_string(),
                    });
                    remaining.push(terminal);
                }
            }
        }
        if !remaining.is_empty() {
            self.live.insert(session.clone(), remaining);
        }
        let closed = failed.is_empty();
        if closed {
            self.store.close(session)?;
            self.fire_session(LifecycleEvent::SessionClosed, session);
        }
        Ok(CloseReport {
            killed,
            failed,
            closed,
        })
    }

    /// Returns all destructive facts before `end_confirmed` is called. The UI
    /// must show `dirty_files` and obtain explicit confirmation.
    pub fn end_plan(&self, session: &SessionId) -> Result<EndPlan, OrchestrationError> {
        let stored = self.store.session(session)?;
        let mut worktrees = Vec::new();
        let mut missing = Vec::new();
        for owned in &stored.owned {
            match self.resolve_owned(owned) {
                Ok(path) => {
                    let dirty_files = grove_git::dirty_file_count(&path)?;
                    let foreground = self
                        .live_terminal(session, owned)
                        .and_then(|id| self.terminals.foreground_process(id).ok().flatten());
                    worktrees.push(EndWorktree {
                        worktree: owned.clone(),
                        path,
                        dirty_files,
                        foreground,
                    });
                }
                Err(OrchestrationError::WorktreeMissing(_)) => missing.push(owned.clone()),
                Err(error) => return Err(error),
            }
        }
        Ok(EndPlan {
            session: session.clone(),
            worktrees,
            missing,
        })
    }

    /// Executes the destructive action after the caller displayed `end_plan`
    /// and received confirmation. This is the sole orchestration path that
    /// invokes `git worktree remove`.
    pub fn end_confirmed(&mut self, session: &SessionId) -> Result<EndReport, OrchestrationError> {
        let session_name = self.store.session(session)?.name.clone();
        let plan = self.end_plan(session)?;
        let close = self.close(session)?;
        if !close.closed {
            return Ok(EndReport {
                removed: Vec::new(),
                failed: close.failed,
                ended: false,
            });
        }
        let mut removed = Vec::new();
        let mut failed = Vec::new();
        for missing in plan.missing {
            self.store.forget_owned(session, &missing)?;
            self.fire_worktree(LifecycleEvent::WorktreeRemoved, session, &missing, None);
            removed.push(missing);
        }
        for item in plan.worktrees {
            let repository = self.repository(&item.worktree.repo)?;
            let options = RemoveOptions {
                force_dirty: true,
                foreground_process: item.foreground.clone(),
                force_busy: true,
            };
            match grove_git::remove_worktree(&repository.path, &item.path, &options) {
                Ok(()) => {
                    self.store.forget_owned(session, &item.worktree)?;
                    self.fire_worktree(
                        LifecycleEvent::WorktreeRemoved,
                        session,
                        &item.worktree,
                        Some(item.path.clone()),
                    );
                    removed.push(item.worktree);
                }
                Err(error) => failed.push(EffectFailure {
                    worktree: Some(item.worktree),
                    message: error.to_string(),
                }),
            }
        }
        let ended = failed.is_empty();
        if ended {
            self.store.end(session)?;
            self.fire_session_named(LifecycleEvent::SessionEnded, session, &session_name);
            self.live.remove(session);
        }
        Ok(EndReport {
            removed,
            failed,
            ended,
        })
    }

    pub fn adopt(
        &mut self,
        session: &SessionId,
        worktree: OwnedWorktree,
    ) -> Result<Option<TerminalId>, OrchestrationError> {
        let state = self.store.session(session)?.state;
        let repository = self.repository(&worktree.repo)?.clone();
        let path = self.resolve_owned_unowned(&worktree)?;
        self.store
            .adopt(session, worktree.clone(), &path, &repository.path)?;
        self.fire_worktree(
            LifecycleEvent::WorktreeAdopted,
            session,
            &worktree,
            Some(path.clone()),
        );
        if state == SessionState::Closed {
            return Ok(None);
        }
        let id = self
            .terminals
            .spawn_worktree(&path, DEFAULT_ROWS, DEFAULT_COLS)?;
        self.live
            .entry(session.clone())
            .or_default()
            .push(LiveTerminal {
                worktree: worktree.clone(),
                id,
            });
        self.fire_terminal(LifecycleEvent::TerminalSpawned, session, &worktree, id);
        Ok(Some(id))
    }

    pub fn release(
        &mut self,
        session: &SessionId,
        worktree: &OwnedWorktree,
    ) -> Result<(), OrchestrationError> {
        if !self.store.ownership_movable() {
            return Err(grove_state::Error::SessionPathTemplate.into());
        }
        let terminal = self.live.get_mut(session).and_then(|terminals| {
            terminals
                .iter()
                .position(|entry| &entry.worktree == worktree)
                .map(|index| terminals.remove(index))
        });
        if let Some(terminal) = terminal {
            self.terminals.kill(terminal.id)?;
            self.fire_terminal(
                LifecycleEvent::TerminalExited,
                session,
                &terminal.worktree,
                terminal.id,
            );
            self.notified_exits.insert(terminal.id);
        }
        self.store.release(session, worktree)?;
        Ok(())
    }

    pub fn create_worktrees(
        &mut self,
        session: &SessionId,
        branch: &str,
        repos: &[RepoId],
    ) -> Result<CreateReport, OrchestrationError> {
        let stored = self.store.session(session)?.clone();
        let mut created = Vec::new();
        let mut failed = Vec::new();
        for repo_id in repos {
            let worktree = OwnedWorktree {
                repo: repo_id.clone(),
                branch: branch.to_owned(),
            };
            let result = (|| {
                if !stored.members.contains(repo_id) {
                    return Err(OrchestrationError::NotMember {
                        session: session.clone(),
                        repo: repo_id.clone(),
                    });
                }
                let repository = self.repository(repo_id)?.clone();
                let base = repository
                    .base_branch
                    .as_deref()
                    .ok_or_else(|| OrchestrationError::BaseMissing(repo_id.clone()))?;
                let path = self.worktree_path(&repository, &stored.name, branch)?;
                grove_git::create_worktree(&repository.path, &path, branch, base)?;
                self.store
                    .own_created(session, worktree.clone(), &path, &repository.path)?;
                self.fire_worktree(
                    LifecycleEvent::WorktreeCreated,
                    session,
                    &worktree,
                    Some(path.clone()),
                );
                let terminal = if stored.state == SessionState::Closed {
                    None
                } else {
                    let id = self
                        .terminals
                        .spawn_worktree(&path, DEFAULT_ROWS, DEFAULT_COLS)?;
                    self.live
                        .entry(session.clone())
                        .or_default()
                        .push(LiveTerminal {
                            worktree: worktree.clone(),
                            id,
                        });
                    self.fire_terminal(LifecycleEvent::TerminalSpawned, session, &worktree, id);
                    Some(id)
                };
                Ok(CreatedWorktree {
                    worktree: worktree.clone(),
                    path,
                    terminal,
                })
            })();
            match result {
                Ok(item) => created.push(item),
                Err(error) => failed.push(EffectFailure {
                    worktree: Some(worktree),
                    message: error.to_string(),
                }),
            }
        }
        Ok(CreateReport { created, failed })
    }

    fn repository(&self, id: &RepoId) -> Result<&Repository, OrchestrationError> {
        self.repositories
            .get(id)
            .ok_or_else(|| OrchestrationError::RepositoryMissing(id.clone()))
    }

    fn resolve_owned(&self, worktree: &OwnedWorktree) -> Result<PathBuf, OrchestrationError> {
        let repository = self.repository(&worktree.repo)?;
        grove_git::worktrees(&repository.path)?
            .into_iter()
            .find(|candidate| {
                !candidate.is_clone && candidate.branch.as_deref() == Some(&worktree.branch)
            })
            .map(|candidate| candidate.path)
            .ok_or_else(|| OrchestrationError::WorktreeMissing(worktree.clone()))
    }

    fn resolve_owned_unowned(
        &self,
        worktree: &OwnedWorktree,
    ) -> Result<PathBuf, OrchestrationError> {
        self.resolve_owned(worktree)
    }

    fn live_terminal(&self, session: &SessionId, worktree: &OwnedWorktree) -> Option<TerminalId> {
        self.live.get(session)?.iter().find_map(|terminal| {
            (&terminal.worktree == worktree
                && self.terminals.is_alive(terminal.id).unwrap_or(false))
            .then_some(terminal.id)
        })
    }

    fn fire_observed_terminal_exits(&mut self) {
        for id in self.terminals.drain_exited() {
            if self.notified_exits.insert(id) {
                let found = self.live.iter().find_map(|(session, terminals)| {
                    terminals
                        .iter()
                        .find(|terminal| terminal.id == id)
                        .map(|terminal| (session.clone(), terminal.worktree.clone()))
                });
                if let Some((session, worktree)) = found {
                    self.fire_terminal(LifecycleEvent::TerminalExited, &session, &worktree, id);
                }
            }
        }
        // The set exists only to fire each exit once. An id whose terminal is no
        // longer live can never be drained again, so keeping it forever grows
        // the daemon's memory for its whole lifetime with nothing to show for
        // it — a daemon is expected to outlive many terminals.
        self.notified_exits.retain(|id| {
            self.live
                .values()
                .any(|terminals| terminals.iter().any(|terminal| terminal.id == *id))
        });
    }

    fn fire_session(&mut self, event: LifecycleEvent, session: &SessionId) {
        if let Ok(stored) = self.store.session(session) {
            let name = stored.name.clone();
            self.fire_session_named(event, session, &name);
        }
    }

    fn fire_session_named(&mut self, event: LifecycleEvent, session: &SessionId, name: &str) {
        self.hook_reports.extend(self.runtime.fire(
            event,
            &LifecyclePayload::Session {
                id: session.0.clone(),
                name: name.to_owned(),
            },
        ));
    }

    fn fire_worktree(
        &mut self,
        event: LifecycleEvent,
        session: &SessionId,
        worktree: &OwnedWorktree,
        path: Option<PathBuf>,
    ) {
        let Some(repository) = self.repositories.get(&worktree.repo) else {
            return;
        };
        let payload = LifecyclePayload::Worktree {
            repo: repository.name.clone(),
            branch: worktree.branch.clone(),
            path: path
                .or_else(|| self.resolve_owned(worktree).ok())
                .unwrap_or_default(),
            clone: repository.path.clone(),
            session: session.0.clone(),
        };
        self.hook_reports.extend(self.runtime.fire(event, &payload));
    }

    fn fire_terminal(
        &mut self,
        event: LifecycleEvent,
        session: &SessionId,
        worktree: &OwnedWorktree,
        terminal: TerminalId,
    ) {
        let Some(repository) = self.repositories.get(&worktree.repo) else {
            return;
        };
        let payload = LifecyclePayload::Terminal {
            terminal: terminal.0,
            repo: repository.name.clone(),
            branch: worktree.branch.clone(),
            path: self.resolve_owned(worktree).unwrap_or_default(),
            session: session.0.clone(),
        };
        self.hook_reports.extend(self.runtime.fire(event, &payload));
    }

    fn worktree_path(
        &self,
        repository: &Repository,
        session: &str,
        branch: &str,
    ) -> Result<PathBuf, OrchestrationError> {
        let clone_parent = repository
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_string_lossy();
        let value = self
            .runtime
            .worktree_path(WorktreePathContext {
                repo: &repository.name,
                branch,
                session,
                clone_parent: &clone_parent,
            })
            .map_err(|error| OrchestrationError::WorktreePath(error.to_string()))?;
        if let Some(rest) = value.strip_prefix("~/") {
            Ok(std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("~"))
                .join(rest))
        } else {
            Ok(PathBuf::from(value))
        }
    }
}

fn owned_ref(worktree: WorktreeRef) -> OwnedWorktree {
    OwnedWorktree {
        repo: worktree.repo,
        branch: worktree.branch,
    }
}

fn next_session_id() -> SessionId {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    SessionId(format!("{nanos:x}-{sequence:x}"))
}

fn effect_failure_events(context: &str, failures: &[EffectFailure]) -> Vec<Event> {
    failures
        .iter()
        .map(|failure| Event::Failed {
            context: context.into(),
            message: failure.message.clone(),
        })
        .collect()
}

fn hook_report_event(report: HookReport) -> Option<Event> {
    match report {
        HookReport::HookDisabled {
            event,
            registration,
            message,
        } => Some(Event::Failed {
            context: format!("lifecycle hook {event:?} #{registration}"),
            message,
        }),
        HookReport::CommandFinished {
            job,
            command,
            success,
            message,
        } => {
            eprintln!(
                "groved: hook job {job} {}: {command}: {message}",
                if success { "completed" } else { "failed" }
            );
            (!success).then_some(Event::Failed {
                context: format!("lifecycle command job {job}"),
                message: format!("{command}: {message}"),
            })
        }
    }
}

fn fetch_failure_events(results: &[FetchResult]) -> Vec<Event> {
    results
        .iter()
        .filter_map(|result| match &result.status {
            FetchStatus::Fetched => None,
            status => Some(Event::Failed {
                context: format!("fetch {:?}", result.repo),
                message: match status {
                    FetchStatus::Failed(message) => message.clone(),
                    FetchStatus::RepositoryMissing => "repository is not available".into(),
                    FetchStatus::NotMember => "repository is not a session member".into(),
                    FetchStatus::Fetched => unreachable!(),
                },
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::process::Command;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!("grove-session-{unique}"));
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

    fn sid(value: &str) -> SessionId {
        SessionId(value.into())
    }

    fn owned(branch: &str) -> OwnedWorktree {
        OwnedWorktree {
            repo: RepoId("repo".into()),
            branch: branch.into(),
        }
    }

    #[test]
    fn fixture_repo_covers_every_session_transition_and_owned_only_end() {
        let temp = TempDir::new();
        let clone = temp.0.join("repo");
        let ours_path = temp.0.join("ours");
        let other_path = temp.0.join("other");
        fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "Grove Test"]);
        git(&clone, &["config", "user.email", "grove@example.test"]);
        fs::write(clone.join("tracked"), "base\n").unwrap();
        git(&clone, &["add", "tracked"]);
        git(&clone, &["commit", "-qm", "base"]);
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-qb",
                "ours",
                ours_path.to_str().unwrap(),
            ],
        );
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-qb",
                "other",
                other_path.to_str().unwrap(),
            ],
        );

        let template = temp.0.join("trees/{repo}/{branch_slug}");
        let template = template.to_string_lossy().into_owned();
        let mut store = Store::load_at(&temp.0.join("state"), &temp.0, &template);
        for id in [sid("one"), sid("two")] {
            store.create(id.clone(), id.0.clone()).unwrap();
            store.add_member(&id, RepoId("repo".into())).unwrap();
        }
        store
            .adopt(&sid("one"), owned("ours"), &ours_path, &clone)
            .unwrap();
        store
            .adopt(&sid("two"), owned("other"), &other_path, &clone)
            .unwrap();
        let repository = Repository {
            id: RepoId("repo".into()),
            name: "repo".into(),
            path: clone.clone(),
            base_branch: Some("main".into()),
        };
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let lifecycle_log = temp.0.join("lifecycle.log");
        let runtime = DaemonRuntime::load_source(
            &format!(
                r#"
                local grove = require('grove')
                grove.setup({{ worktree_path = {template:?} }})
                grove.on('worktree_created', function() error('bootstrap failed') end)
                for _, event in ipairs({{
                  'worktree_created', 'worktree_removed', 'worktree_adopted',
                  'terminal_spawned', 'terminal_exited', 'session_opened',
                  'session_closed', 'session_ended'
                }}) do
                  grove.on(event, function()
                    local file = assert(io.open({lifecycle_log:?}, 'a'))
                    file:write(event .. '\n'); file:close()
                  end)
                end
                "#,
                template = template,
                lifecycle_log = lifecycle_log.to_string_lossy(),
            ),
            "test-config",
        )
        .runtime;
        let mut daemon =
            SessionOrchestrator::new(store, vec![repository], terminals, fetch, runtime);

        let opened_one = daemon.open(&sid("one")).unwrap();
        assert_eq!(opened_one.terminals.len(), 1);
        assert!(opened_one.failed.is_empty());
        assert_eq!(
            daemon.store().session(&sid("one")).unwrap().state,
            SessionState::Attached
        );
        daemon.detach(&sid("one")).unwrap();
        assert_eq!(
            daemon.store().session(&sid("one")).unwrap().state,
            SessionState::Detached
        );
        assert!(
            daemon
                .terminals()
                .is_alive(opened_one.terminals[0])
                .unwrap()
        );

        let resumed_one = daemon.open(&sid("one")).unwrap();
        assert_eq!(resumed_one.terminals, opened_one.terminals);
        assert_eq!(
            daemon.store().session(&sid("one")).unwrap().state,
            SessionState::Attached
        );

        let opened_two = daemon.open(&sid("two")).unwrap();
        assert_eq!(opened_two.terminals.len(), 1);
        assert_eq!(
            daemon.store().session(&sid("two")).unwrap().state,
            SessionState::Attached
        );
        assert_eq!(
            daemon.store().session(&sid("one")).unwrap().state,
            SessionState::Detached
        );
        let resumed_one = daemon.open(&sid("one")).unwrap();
        assert_eq!(resumed_one.terminals, opened_one.terminals);
        assert_eq!(
            daemon.store().session(&sid("two")).unwrap().state,
            SessionState::Detached
        );
        let closed = daemon.close(&sid("one")).unwrap();
        assert!(closed.closed);
        assert_eq!(closed.killed, opened_one.terminals);
        assert!(ours_path.exists(), "close must keep worktrees");

        fs::write(other_path.join("uncommitted"), "do not hide this\n").unwrap();
        let plan = daemon.end_plan(&sid("two")).unwrap();
        assert!(plan.has_uncommitted_work());
        assert_eq!(plan.worktrees.len(), 1);
        assert_eq!(plan.worktrees[0].dirty_files, 1);
        let ended = daemon.end_confirmed(&sid("two")).unwrap();
        assert!(ended.ended);
        assert_eq!(ended.removed, vec![owned("other")]);
        assert!(!other_path.exists());
        assert!(ours_path.exists(), "another session's worktree was removed");
        assert!(clone.exists(), "the clone was removed");
        assert!(daemon.store().session(&sid("two")).is_err());

        let adopt_path = temp.0.join("adopt");
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-qb",
                "adopt",
                adopt_path.to_str().unwrap(),
            ],
        );
        assert_eq!(daemon.adopt(&sid("one"), owned("adopt")).unwrap(), None);
        assert_eq!(daemon.store().owner(&owned("adopt")), Some(&sid("one")));
        daemon.release(&sid("one"), &owned("adopt")).unwrap();
        assert!(daemon.store().owner(&owned("adopt")).is_none());
        assert!(adopt_path.exists(), "release must keep the worktree");

        let created = daemon
            .create_worktrees(&sid("one"), "created", &[RepoId("repo".into())])
            .unwrap();
        assert_eq!(created.created.len(), 1);
        assert!(created.failed.is_empty());
        assert!(created.created[0].path.join(".git").is_file());
        assert_eq!(daemon.store().owner(&owned("created")), Some(&sid("one")));

        let events = daemon.handle_request(Request::OpenSession(sid("one")));
        assert!(matches!(
            events.iter().find(|event| matches!(event, Event::SessionChanged(_))),
            Some(Event::SessionChanged(row))
                if row.state == SessionState::Attached && row.terminals == 2
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Failed { context, message }
                if context.contains("WorktreeCreated") && message.contains("bootstrap failed")
        )));
        let events = daemon.handle_request(Request::DetachSession(sid("one")));
        assert!(matches!(
            events.as_slice(),
            [Event::SessionChanged(row)] if row.state == SessionState::Detached
        ));
        let lifecycle = fs::read_to_string(lifecycle_log).unwrap();
        for event in [
            "worktree_created",
            "worktree_removed",
            "worktree_adopted",
            "terminal_spawned",
            "terminal_exited",
            "session_opened",
            "session_closed",
            "session_ended",
        ] {
            assert!(
                lifecycle.lines().any(|line| line == event),
                "missing {event}"
            );
        }
    }
}
