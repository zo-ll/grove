//! Session transitions that coordinate persistent state, Git and PTYs.

use crate::fetch::{FetchPolicy, FetchResult, FetchStatus, RefFreshness};
use crate::prune::{PruneDisqualifier, PruneHistory};
use crate::terminal::{TerminalError, TerminalKey, TerminalManager};
use crate::watch::{RepoWatch, WatchPlan};
use grove_domain::{Ownership, RepoId, SessionId, SessionState};
use grove_git::{RemoveOptions, Repository, SizeTask, Tracking, Workspace};
use grove_lua::{DaemonRuntime, HookReport, LifecycleEvent, LifecyclePayload, WorktreePathContext};
use grove_proto::{
    Attach, Event, PruneBlocker, PruneCandidate, PruneState, RepoRow, Request, ScrollbackRequest,
    SessionRow, TerminalId, TerminalRow, TerminalTarget, WorktreeRef, WorktreeRow,
};
use grove_state::{OwnedWorktree, Store, StoredSession};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
/// Lines per scrollback chunk. One chunk per line would flood the wire with
/// headers; all history in one chunk would pay a 10,000-line buffer at once.
const SCROLLBACK_CHUNK: usize = 200;

/// The parts of an attach answer, in the order the socket layer must send
/// them: screen, live feed started, then the backfill chunks.
pub struct AttachOutcome {
    /// The visible grid, sent first so the pane paints at once.
    pub screen: Event,
    /// History, strictly older than the screen, oldest-first, `done` on the
    /// final chunk. Sent after the live feed is running.
    pub backfill: Vec<Event>,
    /// Live output, drained by the socket layer's forwarder.
    pub output: mpsc::Receiver<Vec<u8>>,
}
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
    #[error("worktree {0:?} is the repository's own checkout and cannot be session-owned")]
    CloneNotAdoptable(OwnedWorktree),
    #[error("worktree path {path:?} is the repository's own checkout")]
    WorktreePathIsClone { path: PathBuf },
    #[error("worktree path {path:?} is already claimed by branch {branch:?}")]
    PathClaimed { path: PathBuf, branch: String },
    #[error("{0:?} is the repository's own checkout; end session must not remove it")]
    CloneNotRemovable(OwnedWorktree),
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
    #[error("workspace scan failed: {0}")]
    Scan(String),
    #[error("prune inspection failed: {0}")]
    Prune(String),
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
    scan_root: PathBuf,
    socket_path: PathBuf,
    terminals: TerminalManager,
    fetch: FetchPolicy,
    runtime: DaemonRuntime,
    live: HashMap<SessionId, Vec<LiveTerminal>>,
    session_shells: HashMap<SessionId, TerminalId>,
    hook_reports: Vec<HookReport>,
    notified_exits: HashSet<TerminalId>,
    unknown_overrides: Vec<String>,
    /// Size walks in flight, keyed by checkout path. §7's size read is
    /// cancellable and never blocks a caller, so a request starts the walk and
    /// reports the last known value; the walk's answer lands on a later
    /// request.
    pending_sizes: HashMap<PathBuf, SizeTask>,
    /// Completed size walks, keyed by checkout path. Scoped to the repo the
    /// pane is currently showing: bounded memory beats a daemon-lifetime map
    /// of paths for repos the pane may never show again.
    known_sizes: HashMap<PathBuf, u64>,
    /// Checkouts whose size walk failed. Their rows stay at 0 and are marked
    /// stale rather than presenting an unmeasured checkout as measured.
    failed_sizes: HashSet<PathBuf>,
}

impl SessionOrchestrator {
    pub fn new(
        store: Store,
        repositories: Vec<Repository>,
        scan_root: PathBuf,
        terminals: TerminalManager,
        fetch: FetchPolicy,
        runtime: DaemonRuntime,
    ) -> Self {
        let socket_path =
            crate::socket_path(&scan_root).unwrap_or_else(|_| scan_root.join(".grove/groved.sock"));
        let input = terminals.input_handle();
        let mut runtime = runtime;
        runtime.set_terminal_sender(move |terminal, bytes| {
            input
                .send(TerminalId(terminal), bytes)
                .map_err(|error| error.to_string())
        });
        let known: HashSet<&str> = repositories
            .iter()
            .map(|repository| repository.name.as_str())
            .collect();
        let unknown_overrides: Vec<String> = {
            let mut names: Vec<String> = runtime
                .repo_overrides()
                .iter()
                .map(|rule| rule.name.clone())
                .collect();
            names.sort();
            names.dedup();
            names
                .into_iter()
                .filter(|name| !known.contains(name.as_str()))
                .collect()
        };
        Self {
            store,
            repositories: repositories
                .into_iter()
                .map(|repository| (repository.id.clone(), repository))
                .collect(),
            scan_root,
            socket_path,
            terminals,
            fetch,
            runtime,
            live: HashMap::new(),
            session_shells: HashMap::new(),
            hook_reports: Vec::new(),
            notified_exits: HashSet::new(),
            unknown_overrides,
            pending_sizes: HashMap::new(),
            known_sizes: HashMap::new(),
            failed_sizes: HashSet::new(),
        }
    }

    /// Overrides the daemon socket advertised to session shells. The daemon
    /// uses this with the path it actually bound; tests can supply an isolated
    /// path without changing process-global runtime-directory variables.
    pub fn with_socket_path(mut self, socket_path: impl Into<PathBuf>) -> Self {
        self.socket_path = socket_path.into();
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub(crate) fn watch_plan(&self) -> WatchPlan {
        WatchPlan {
            repos: self
                .repositories
                .values()
                .filter_map(|repository| {
                    grove_git::watch_paths(&repository.path)
                        .ok()
                        .map(|paths| RepoWatch {
                            repo: repository.id.clone(),
                            paths,
                        })
                })
                .collect(),
        }
    }

    pub(crate) fn worktrees_event(&mut self, repo: &RepoId) -> Result<Event, OrchestrationError> {
        self.repository(repo)?;
        Ok(Event::Worktrees {
            repo: repo.clone(),
            rows: self.worktree_rows(repo)?,
        })
    }

    pub fn terminals(&self) -> &TerminalManager {
        &self.terminals
    }

    /// Applies the session-related protocol surface. Each call completes its
    /// effects before returning events, so the socket layer remains transport.
    pub fn handle_request(&mut self, request: Request) -> Vec<Event> {
        self.fire_observed_terminal_exits();
        self.drain_sizes();
        let mut events = self.drain_unknown_overrides();
        let context = request.failure_context();
        events.extend(match self.apply_request(request) {
            Ok(events) => events,
            Err(error) => vec![Event::Failed {
                context: context.into(),
                message: error.to_string(),
            }],
        });
        self.hook_reports.extend(self.runtime.drain_reports());
        events.extend(self.hook_reports.drain(..).filter_map(hook_report_event));
        events
    }

    /// Reports config overrides that name no repository in the workspace.
    ///
    /// Checked against the repositories discovered at startup, because that is
    /// the population an override can ever apply to. An override that cannot
    /// apply anywhere is reported once rather than silently ignored.
    fn drain_unknown_overrides(&mut self) -> Vec<Event> {
        self.unknown_overrides
            .drain(..)
            .map(|name| Event::Failed {
                context: "config: repo override".into(),
                message: format!(
                    "no repository named {name:?} is in this workspace; \
                     its override is ignored"
                ),
            })
            .collect()
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
            Request::ListRepos => Ok(vec![Event::Repos(self.repo_rows()?)]),
            Request::Scan => {
                self.scan_workspace()?;
                Ok(vec![Event::Repos(self.repo_rows()?)])
            }
            Request::ListWorktrees(repo) => Ok(vec![Event::Worktrees {
                repo: repo.clone(),
                rows: self.worktree_rows(&repo)?,
            }]),
            // Manual, as in tmux-resurrect: only an explicit request writes
            // a snapshot, and only an explicit request reads one back.
            Request::SaveSnapshot(session) => {
                // The session's live terminals are the layout being saved;
                // a snapshot of dead ids would restore shells nobody sees.
                let live: Vec<TerminalId> = self
                    .live
                    .get(&session)
                    .into_iter()
                    .flatten()
                    .filter(|terminal| self.terminals.is_alive(terminal.id).unwrap_or(false))
                    .map(|terminal| terminal.id)
                    .collect();
                self.terminals
                    .save_session_snapshot(&self.store, &session, &live)?;
                Ok(vec![Event::Sessions(self.session_rows()?)])
            }
            Request::RestoreSnapshot(session) => {
                let report = self
                    .terminals
                    .restore_session_snapshot(&self.store, &session)?;
                let mut events = Vec::new();
                for missing in &report.missing {
                    events.push(Event::Failed {
                        context: "restore snapshot".into(),
                        message: format!(
                            "recorded worktree {} no longer exists",
                            missing.worktree.display()
                        ),
                    });
                }
                for failure in &report.failed {
                    events.push(Event::Failed {
                        context: "restore snapshot".into(),
                        message: format!(
                            "could not restore a terminal at {}: {message}",
                            failure.terminal.worktree.display(),
                            message = failure.message
                        ),
                    });
                }
                // One worktree listing per repo for this request, not one
                // git subprocess per terminal per repo under the lock.
                let index = self.worktree_index();
                for restored in &report.restored {
                    if let Some(owned) = index.get(&restored.worktree) {
                        self.live
                            .entry(session.clone())
                            .or_default()
                            .push(LiveTerminal {
                                worktree: owned.clone(),
                                id: restored.terminal,
                            });
                        self.fire_terminal(
                            LifecycleEvent::TerminalSpawned,
                            &session,
                            owned,
                            restored.terminal,
                        );
                    }
                }
                // §2.3's table: a session with live terminals is detached,
                // not closed. The reboot case lands exactly here — the
                // daemon restart made every session closed, and restoring
                // brings its terminals back to life.
                if !report.restored.is_empty()
                    && self.store.session(&session)?.state == SessionState::Closed
                {
                    self.store.detach(&session)?;
                }
                events.push(Event::Sessions(self.session_rows()?));
                Ok(events)
            }
            Request::OpenEditor(worktree) => {
                let repository = self.repository(&worktree.repo)?.clone();
                let checkout = self
                    .find_checkout(&repository, &worktree.branch)?
                    .ok_or_else(|| {
                        OrchestrationError::WorktreeMissing(OwnedWorktree {
                            repo: worktree.repo.clone(),
                            branch: worktree.branch.clone(),
                        })
                    })?;
                Ok(self.open_editor(&checkout.path))
            }
            Request::DiffWorktree { worktree, file } => {
                let repository = self.repository(&worktree.repo)?.clone();
                let checkout = self
                    .find_checkout(&repository, &worktree.branch)?
                    .ok_or_else(|| {
                        OrchestrationError::WorktreeMissing(OwnedWorktree {
                            repo: worktree.repo.clone(),
                            branch: worktree.branch.clone(),
                        })
                    })?;
                // Read-only by construction: every git call on this path is a
                // §7 read. The base is the repo's own — an override decides
                // for its repo, like the prune verdict does.
                let outcome = crate::diff::diff_screen(
                    &checkout.path,
                    self.effective_base(&repository).as_deref(),
                    file.as_deref(),
                )
                .map_err(|error| match error {
                    crate::diff::DiffScreenError::Git(source) => {
                        OrchestrationError::GitRead(source)
                    }
                })?;
                Ok(vec![Event::Diff {
                    worktree,
                    base: outcome.base,
                    files: outcome.files,
                    selected: outcome.selected,
                    hunks: outcome.hunks,
                    added: outcome.added,
                    removed: outcome.removed,
                }])
            }
            Request::ListPruneCandidates => Ok(vec![Event::PruneCandidates(self.prune_rows()?)]),
            Request::Prune(selection) => Ok(vec![self.prune(&selection)?]),
            Request::SpawnTerminal(target) => {
                let id = self.spawn_terminal(&target)?;
                Ok(vec![Event::TerminalSpawned {
                    target,
                    terminal: id,
                }])
            }
            Request::KillTerminal(terminal) => {
                // The pty is gone; its hook still fires once through the
                // observed-exit drain, and the killer gets one event — not one
                // per attached client. Other clients attached to the same pty
                // see their output channel close instead, which is their
                // signal that the pane ended.
                self.terminals.kill(terminal)?;
                Ok(vec![Event::TerminalExited {
                    terminal,
                    status: None,
                }])
            }
            Request::ResizeTerminal {
                terminal,
                rows,
                cols,
            } => {
                self.terminals.resize(terminal, rows, cols)?;
                Ok(Vec::new())
            }
            Request::Input { terminal, bytes } => {
                self.terminals.input(terminal, &bytes)?;
                Ok(Vec::new())
            }
            // Detach stops this client's output feed only: the socket layer
            // owns that feed and drops it before routing the request here. The
            // pty itself keeps running — only `end` is destructive.
            //
            // AttachTerminal never reaches this match: the socket layer routes
            // it to `attach_terminal`, because its answer includes a live
            // output stream no Vec<Event> can carry.
            Request::ListTerminals => Ok(vec![Event::Terminals(self.terminal_rows()?)]),
            Request::DetachTerminal(_) => Ok(Vec::new()),
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

    /// Moves completed size walks into the known cache. A walk that failed —
    /// typically because its checkout was removed mid-walk — answers nothing
    /// and is dropped.
    fn drain_sizes(&mut self) {
        let mut finished: Vec<(PathBuf, u64)> = Vec::new();
        let mut failed: Vec<PathBuf> = Vec::new();
        self.pending_sizes
            .retain(|path, task| match task.try_result() {
                Ok(Some(size)) => {
                    finished.push((path.clone(), size));
                    false
                }
                Ok(None) => true,
                Err(_) => {
                    failed.push(path.clone());
                    false
                }
            });
        self.known_sizes.extend(finished);
        self.failed_sizes.extend(failed);
    }

    /// The REPOS pane's rows (SPEC §4.1), in display order.
    fn repo_rows(&self) -> Result<Vec<RepoRow>, OrchestrationError> {
        let open_members = self
            .store
            .open_session()
            .and_then(|id| self.store.session(id).ok())
            .map(|stored| &stored.members);
        let mut rows = Vec::new();
        for repository in self.repositories.values() {
            // A repo git cannot answer for is omitted from the pane rather
            // than blanking it with a failure; worktree_rows degrades per row
            // for the same reason.
            let Ok(checkouts) = grove_git::worktrees(&repository.path) else {
                continue;
            };
            // The count covers branched work only: the clone is listed on the
            // WORKTREES pane, but a repo whose checkout is just the clone is
            // the documented "haven't branched yet" state, and §4.1 renders
            // it as ·. A count including the clone would make that state
            // unreachable, since every repo has its clone.
            let worktrees = checkouts
                .iter()
                .filter(|checkout| !checkout.is_clone)
                .count();
            let dirty = checkouts
                .iter()
                .any(|checkout| grove_git::is_dirty(&checkout.path).unwrap_or(false));
            let override_base = self
                .runtime
                .repo_override(&repository.name)
                .and_then(|rule| rule.base.clone());
            rows.push(RepoRow {
                repo: repository.id.clone(),
                name: repository.name.clone(),
                base_branch: override_base
                    .clone()
                    .or_else(|| repository.base_branch.clone())
                    .unwrap_or_default(),
                // Provenance the pane renders: only a base that came from
                // git's advertised default may claim "(from origin/HEAD)".
                base_from_origin_head: override_base.is_none() && repository.base_branch.is_some(),
                worktrees: u32::try_from(worktrees).unwrap_or(u32::MAX),
                dirty,
                member: open_members.is_some_and(|members| members.contains(&repository.id)),
            });
        }
        // HashMap order is arbitrary; display order is not, so a rescan of an
        // unchanged workspace produces byte-identical rows.
        rows.sort_by(|a, b| {
            (a.name.as_str(), a.repo.0.as_str()).cmp(&(b.name.as_str(), b.repo.0.as_str()))
        });
        Ok(rows)
    }

    /// The WORKTREES pane's rows (SPEC §4.1): every checkout of one repo,
    /// regardless of owner, plus the clone.
    fn worktree_rows(&mut self, repo: &RepoId) -> Result<Vec<WorktreeRow>, OrchestrationError> {
        let repository = self.repository(repo)?.clone();
        // Staleness tracks the repo's fetch, not the branch: refs that are
        // unreadable or too old mean ahead/behind may be wrong, exactly as the
        // fetch policy judges them.
        let freshness = self
            .fetch
            .ref_freshness(&repository)
            .unwrap_or(RefFreshness {
                age: None,
                stale: true,
            });
        let checkouts = grove_git::worktrees(&repository.path)?;
        let mut rows = Vec::new();
        let mut paths = HashSet::new();
        for checkout in checkouts {
            paths.insert(checkout.path.clone());
            let detached = checkout.branch.is_none();
            let mut degraded = false;
            let (ahead, behind) = match grove_git::ahead_behind(&checkout.path) {
                Ok(Tracking::Tracked(counts)) => (counts.ahead, counts.behind),
                // No upstream and a gone upstream are data, not failure.
                Ok(Tracking::NoUpstream | Tracking::UpstreamGone) => (0, 0),
                // An unreadable checkout degrades to zeros rather than
                // failing the whole pane — and is marked stale below.
                Err(_) => {
                    degraded = true;
                    (0, 0)
                }
            };
            // Asked of the terminal manager, which knows every live pty by
            // its path, rather than of `self.live`, which knows only the ones
            // a session owns. §2 hangs a terminal off a worktree, not off an
            // owned worktree: `^g enter` on an unowned one spawned a shell
            // that the dash then said did not exist.
            let terminal = self.live_terminal_at(&checkout.path);
            let foreground =
                terminal.and_then(|id| self.terminals.foreground_process(id).ok().flatten());
            let dirty_files = match grove_git::dirty_file_count(&checkout.path) {
                Ok(count) => count,
                Err(_) => {
                    degraded = true;
                    0
                }
            };
            // An unborn HEAD has no tip commit, so its age is unknown —
            // answering 0 would render "now" for a checkout that may be as
            // old as the repo.
            let age = match grove_git::branch_age(&checkout.path) {
                Ok(Some(age)) => age,
                Ok(None) | Err(_) => {
                    degraded = true;
                    Duration::default()
                }
            };
            rows.push(WorktreeRow {
                worktree: WorktreeRef {
                    repo: repository.id.clone(),
                    // A detached HEAD names no branch, and the row must not
                    // smuggle a sentinel that a real branch could collide
                    // with: the branch is empty and `detached` says so.
                    branch: checkout.branch.clone().unwrap_or_default(),
                },
                detached,
                ownership: self.ownership(&repository, &checkout),
                ahead,
                behind,
                dirty_files,
                age: age.as_secs(),
                size: self.known_size(&checkout.path),
                terminal,
                foreground,
                // A degraded row must not present its confident zeros as
                // fresh facts, so it is marked stale too. That includes an
                // unmeasured size: a checkout whose walk failed keeps 0 and
                // is marked rather than shown as measured-and-empty.
                stale: freshness.stale || degraded || self.failed_sizes.contains(&checkout.path),
            });
        }
        // The size cache is scoped to the repo the pane is showing: switching
        // repos re-measures, and removed checkouts do not linger.
        self.pending_sizes.retain(|path, _| paths.contains(path));
        self.known_sizes.retain(|path, _| paths.contains(path));
        self.failed_sizes.retain(|path| paths.contains(path));
        rows.sort_by(|a, b| a.worktree.branch.cmp(&b.worktree.branch));
        Ok(rows)
    }

    /// The last known size of a checkout, starting the background walk when
    /// none is in flight. The first request after a worktree appears answers
    /// 0; the walk's answer lands on a later request. A walk that failed is
    /// not restarted while its checkout stays listed: its row keeps 0 and is
    /// marked stale instead of churning a doomed walk every refresh.
    fn known_size(&mut self, path: &Path) -> u64 {
        if let Some(size) = self.known_sizes.get(path) {
            return *size;
        }
        if self.failed_sizes.contains(path) {
            return 0;
        }
        if !self.pending_sizes.contains_key(path) {
            self.pending_sizes
                .insert(path.to_path_buf(), grove_git::worktree_size(path));
        }
        0
    }

    /// Attaches to a terminal: resize to the pane's size, then the parts of
    /// the reassembly contract, in the order the caller must send them:
    /// screen, then history chunks.
    ///
    /// The split is the point. The socket layer sends the screen, starts the
    /// live feed, and only then sends the backfill — so output produced while
    /// history loads drains on the client's own channel instead of being
    /// evicted by the bounded subscriber buffer and ending the stream
    /// silently. Live output may interleave from the moment the screen is
    /// sent; a client that has painted must not go stale while history loads.
    pub fn attach_terminal(&mut self, attach: Attach) -> Result<AttachOutcome, OrchestrationError> {
        // The pane's size at attach time: resizing before the screen means the
        // client never paints a wrongly-sized grid, and vt100 reflows.
        self.terminals
            .resize(attach.terminal, attach.rows, attach.cols)?;
        let attachment = self.terminals.attach(attach.terminal)?;
        let snapshot = &attachment.snapshot;
        let screen = Event::TerminalScreen {
            terminal: attach.terminal,
            screen: snapshot.screen.clone(),
        };
        let lines = match attach.scrollback {
            ScrollbackRequest::None => Vec::new(),
            ScrollbackRequest::Lines(count) => {
                let count = usize::try_from(count).unwrap_or(usize::MAX);
                let start = snapshot.scrollback.len().saturating_sub(count);
                snapshot.scrollback[start..].to_vec()
            }
            ScrollbackRequest::All => snapshot.scrollback.clone(),
        };
        // Chunks are oldest-first, `seq` counts from the oldest, and an empty
        // backfill is one empty chunk with `done` set — never zero chunks, so
        // a client that joins with nothing behind the screen still sees a
        // terminal `done` and does not wait forever.
        let mut backfill = Vec::new();
        let mut sent = 0_usize;
        let total = lines.len();
        loop {
            let end = (sent + SCROLLBACK_CHUNK).min(total);
            let seq = u32::try_from(sent / SCROLLBACK_CHUNK).unwrap_or(u32::MAX);
            backfill.push(Event::TerminalScrollback {
                terminal: attach.terminal,
                seq,
                lines: lines[sent..end].to_vec(),
                done: end == total,
            });
            sent = end;
            if end == total {
                break;
            }
        }
        Ok(AttachOutcome {
            screen,
            backfill,
            output: attachment.output,
        })
    }

    /// The scratch shell spawns with no worktree (§4.5); a worktree target
    /// spawns one terminal for that checkout. A worktree owned by a session
    /// joins that session's live set, so closing the session kills it.
    fn spawn_terminal(
        &mut self,
        target: &TerminalTarget,
    ) -> Result<TerminalId, OrchestrationError> {
        match target {
            TerminalTarget::Session { session } => {
                self.store.session(session)?;
                if let Some(id) = self.session_shells.get(session).copied()
                    && self.terminals.is_alive(id).unwrap_or(false)
                {
                    return Ok(id);
                }
                let environment = self.session_environment(session)?;
                let id = self.terminals.spawn_session(
                    session.clone(),
                    &self.scan_root,
                    &environment,
                    DEFAULT_ROWS,
                    DEFAULT_COLS,
                )?;
                self.session_shells.insert(session.clone(), id);
                Ok(id)
            }
            TerminalTarget::Worktree(reference) => {
                let repository = self.repository(&reference.repo)?.clone();
                let checkout = self
                    .find_checkout(&repository, &reference.branch)?
                    .ok_or_else(|| {
                        OrchestrationError::WorktreeMissing(OwnedWorktree {
                            repo: reference.repo.clone(),
                            branch: reference.branch.clone(),
                        })
                    })?;
                let id =
                    self.terminals
                        .spawn_worktree(&checkout.path, DEFAULT_ROWS, DEFAULT_COLS)?;
                let owned = OwnedWorktree {
                    repo: reference.repo.clone(),
                    branch: reference.branch.clone(),
                };
                if let Some(session) = self.store.owner(&owned).cloned() {
                    self.live
                        .entry(session.clone())
                        .or_default()
                        .push(LiveTerminal {
                            worktree: owned.clone(),
                            id,
                        });
                    self.fire_terminal(LifecycleEvent::TerminalSpawned, &session, &owned, id);
                }
                Ok(id)
            }
            TerminalTarget::Scratch { cwd } => {
                let id = match cwd {
                    Some(cwd) => self.terminals.spawn_scratch_at(
                        expand_home(cwd),
                        DEFAULT_ROWS,
                        DEFAULT_COLS,
                    )?,
                    None => self.terminals.spawn_scratch(DEFAULT_ROWS, DEFAULT_COLS)?,
                };
                Ok(id)
            }
        }
    }

    /// Every live terminal, so a reattaching client can find the scratch shell
    /// again. Worktree targets are resolved back through git: the manager
    /// stores paths, the wire names (repo, branch).
    fn terminal_rows(&self) -> Result<Vec<TerminalRow>, OrchestrationError> {
        // One worktree listing per repo for this request, not per terminal:
        // the manager knows paths, and resolving them one git subprocess per
        // terminal would run under the service lock for no reason.
        let names: HashMap<PathBuf, OwnedWorktree> = self.worktree_index();
        let mut rows = Vec::new();
        for (id, key, alive) in self.terminals.list() {
            if !alive {
                continue;
            }
            let target = match &key {
                TerminalKey::Scratch => TerminalTarget::Scratch { cwd: None },
                TerminalKey::Session(session) => TerminalTarget::Session {
                    session: session.clone(),
                },
                // A checkout git no longer names (removed while its terminal
                // was alive) is omitted from the list.
                TerminalKey::Worktree(path) => match names.get(path) {
                    Some(owned) => TerminalTarget::Worktree(WorktreeRef {
                        repo: owned.repo.clone(),
                        branch: owned.branch.clone(),
                    }),
                    None => continue,
                },
            };
            let foreground = self.terminals.foreground_process(id).ok().flatten();
            rows.push(TerminalRow {
                terminal: id,
                target,
                foreground,
            });
        }
        Ok(rows)
    }

    /// Builds the immutable context exported when a session shell starts.
    /// Repository and worktree collections are JSON so names and paths need
    /// no shell-specific escaping. A caller needing later changes asks the
    /// daemon for live state instead of relying on this snapshot.
    fn session_environment(
        &self,
        session: &SessionId,
    ) -> Result<Vec<(String, String)>, OrchestrationError> {
        let stored = self.store.session(session)?;
        let repos: Vec<_> = stored
            .members
            .iter()
            .filter_map(|id| self.repositories.get(id))
            .map(|repository| {
                serde_json::json!({
                    "id": repository.id.0,
                    "name": repository.name,
                    "path": repository.path.to_string_lossy(),
                })
            })
            .collect();
        let worktrees: Vec<_> = stored
            .owned
            .iter()
            .filter_map(|owned| {
                self.resolve_owned(owned).ok().map(|path| {
                    serde_json::json!({
                        "repo": owned.repo.0,
                        "branch": owned.branch,
                        "path": path.to_string_lossy(),
                    })
                })
            })
            .collect();
        Ok(vec![
            ("GROVE_SESSION".into(), stored.name.clone()),
            ("GROVE_SESSION_ID".into(), stored.id.0.clone()),
            (
                "GROVE_WORKSPACE".into(),
                self.scan_root.to_string_lossy().into_owned(),
            ),
            (
                "GROVE_REPOS".into(),
                serde_json::to_string(&repos).expect("JSON values serialize"),
            ),
            (
                "GROVE_WORKTREES".into(),
                serde_json::to_string(&worktrees).expect("JSON values serialize"),
            ),
            (
                "GROVE_SOCKET".into(),
                self.socket_path.to_string_lossy().into_owned(),
            ),
        ])
    }

    /// Spawns the configured editor for a checkout, detached (§3.3's `^g o`).
    ///
    /// The configured string is split into words first and `{path}` is
    /// substituted inside whichever word carries it, so a worktree path with
    /// spaces stays one argument; a configuration without the placeholder
    /// receives the path as its last argument.
    ///
    /// Detached means what it says: the child gets its own process group and
    /// a thread reaps it, so its exit is not grove's business and it holds no
    /// part of the request loop. It inherits no terminal — the daemon has
    /// none to give — so an editor that must own a tty belongs in the
    /// worktree's own shell, and the config example is a graphical editor.
    /// Everything that can fail — no editor named, program missing — is one
    /// `Failed` event; nothing here can take the loop down.
    fn open_editor(&mut self, path: &Path) -> Vec<Event> {
        let editor = self.runtime.config().editor.clone();
        if editor.is_empty() {
            return vec![Event::Failed {
                context: "open editor".into(),
                message: "no editor configured; set `editor = \"cursor {path}\"` \
                     in config.lua or export $EDITOR"
                    .into(),
            }];
        }
        let path = path.to_string_lossy().into_owned();
        let mut words = editor.split_whitespace();
        let Some(program) = words.next() else {
            return vec![Event::Failed {
                context: "open editor".into(),
                message: "the configured editor is empty".into(),
            }];
        };
        let mut args: Vec<String> = words.map(|word| word.replace("{path}", &path)).collect();
        if !editor.contains("{path}") {
            // No placeholder: the path is the last argument, so the command
            // can stay unaware of grove.
            args.push(path);
        }
        let spawn = Command::new(program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn();
        match spawn {
            Ok(child) => {
                // Its exit is reaped here and nowhere the request loop waits:
                // a thread, not a join, because nobody is waiting on it.
                thread::spawn(move || {
                    let mut child = child;
                    let _ = child.wait();
                });
                Vec::new()
            }
            Err(error) => vec![Event::Failed {
                context: "open editor".into(),
                message: format!("could not launch {program:?}: {error}"),
            }],
        }
    }

    /// The branch mergedness is judged against for a repo: a repo override
    /// decides for its repo, otherwise git's advertised default (#54).
    fn effective_base(&self, repository: &Repository) -> Option<String> {
        self.runtime
            .repo_override(&repository.name)
            .and_then(|rule| rule.base.clone())
            .or_else(|| repository.base_branch.clone())
    }

    /// The prune picker's rows (SPEC §5): every non-clone checkout of every
    /// repo, with the state column and the reasons it is not pre-checked.
    fn prune_rows(&mut self) -> Result<Vec<PruneCandidate>, OrchestrationError> {
        let sessions = self.store.sessions().to_vec();
        let repositories: Vec<Repository> = self.repositories.values().cloned().collect();
        let mut rows = Vec::new();
        for repository in &repositories {
            // A repo whose inspection fails contributes no rows rather than
            // blanking the whole picker: there is nothing honest to say
            // about its rows, and the pane rows omit an unreadable repo for
            // the same reason.
            let Ok(batch) = crate::prune::prune_candidates_against(
                repository,
                &sessions,
                self.effective_base(repository).as_deref(),
            ) else {
                continue;
            };
            for candidate in batch.candidates {
                rows.push(PruneCandidate {
                    worktree: WorktreeRef {
                        repo: candidate.repo.clone(),
                        branch: candidate.branch.clone().unwrap_or_default(),
                    },
                    state: match candidate.history {
                        PruneHistory::Merged => PruneState::Merged,
                        PruneHistory::UpstreamGone => PruneState::UpstreamGone,
                        PruneHistory::Unmerged => PruneState::Neither,
                    },
                    // The same last-known size the worktree pane serves: the
                    // walk runs off the request path, and a row pruned before
                    // its walk answered reports what was known.
                    size: self.known_size(&candidate.worktree),
                    blockers: candidate
                        .reasons
                        .iter()
                        .map(|reason| match reason {
                            PruneDisqualifier::FetchFailed => PruneBlocker::FetchFailed,
                            PruneDisqualifier::Unmerged => PruneBlocker::Unmerged,
                            PruneDisqualifier::Dirty => PruneBlocker::Dirty {
                                files: candidate.dirty_files,
                            },
                            PruneDisqualifier::Unpushed { commits } => {
                                PruneBlocker::Unpushed { commits: *commits }
                            }
                            PruneDisqualifier::OwnedByLiveSession { session, .. } => {
                                let name = sessions
                                    .iter()
                                    .find(|stored| stored.id == *session)
                                    .map(|stored| stored.name.clone())
                                    .unwrap_or_default();
                                PruneBlocker::Owned {
                                    session: session.clone(),
                                    name,
                                }
                            }
                        })
                        .collect(),
                });
            }
        }
        rows.sort_by(|a, b| {
            (a.worktree.repo.0.as_str(), a.worktree.branch.as_str())
                .cmp(&(b.worktree.repo.0.as_str(), b.worktree.branch.as_str()))
        });
        Ok(rows)
    }

    /// Prunes the selected rows, one outcome per row (§5). The user may tick
    /// anything except what is never theirs to tick from this screen: a
    /// worktree a live session owns is refused here, because ending that
    /// session is the way to release it. A row that became dirty between
    /// listing and pruning fails on git's own dirty gate (force_dirty is
    /// deliberately unset) and is left on disk.
    fn prune(&mut self, selection: &[WorktreeRef]) -> Result<Event, OrchestrationError> {
        let sessions = self.store.sessions().to_vec();
        let mut removed = Vec::new();
        let mut failed = Vec::new();
        let mut reclaimed = 0_u64;
        for want in selection {
            match self.prune_row(want, &sessions) {
                Ok(size) => {
                    removed.push(want.clone());
                    reclaimed = reclaimed.saturating_add(size);
                }
                Err(message) => failed.push((want.clone(), message)),
            }
        }
        Ok(Event::Pruned {
            removed,
            failed,
            reclaimed,
        })
    }

    /// One row's removal: the safety facts are re-checked at prune time, not
    /// trusted from the listing, because the picker lets the user tick unsafe
    /// rows deliberately and the disk state may have changed since.
    fn prune_row(&mut self, want: &WorktreeRef, sessions: &[StoredSession]) -> Result<u64, String> {
        let owned = OwnedWorktree {
            repo: want.repo.clone(),
            branch: want.branch.clone(),
        };
        if let Some((session, state)) = crate::prune::live_owner(sessions, &owned) {
            return Err(format!(
                "session {session:?} ({state:?}) owns this worktree; \
                 end that session instead"
            ));
        }
        let repository = self
            .repository(&want.repo)
            .map_err(|error| error.to_string())?
            .clone();
        let checkout = self
            .find_checkout(&repository, &want.branch)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "worktree does not exist".to_string())?;
        // The clone is never listed as a candidate; a hand-crafted request
        // naming it is refused rather than let near the main checkout.
        if checkout.is_clone {
            return Err("the repository's own checkout is never pruned".into());
        }
        // A checkout with a live terminal is busy: the shell keeps running
        // after `git worktree remove` would delete its cwd.
        let foreground = self
            .terminals
            .terminal_by_key(&TerminalKey::Worktree(checkout.path.clone()))
            .and_then(|id| self.terminals.foreground_process(id).ok().flatten());
        // Last known size: the picker listing started the walk, and any
        // request since has drained it. A row pruned without a known size
        // contributes 0 rather than stalling the destructive verb on a walk.
        let size = self.known_size(&checkout.path);
        grove_git::remove_worktree(
            &repository.path,
            &checkout.path,
            &RemoveOptions {
                force_dirty: false,
                force_busy: false,
                foreground_process: foreground,
            },
        )
        .map_err(|error| error.to_string())?;
        // A worktree that was never owned leaves no hook trail: the
        // worktree_removed payload names the owning session, and a row with
        // none has nothing to put there — so per-worktree cleanup hooks do
        // not run for rows that never joined a session. Rows a session once
        // owned do, and the hook sees the session that owned them.
        //
        // Ownership outlives sessions in the store, including closed ones;
        // pruning their worktree must forget the claim, or the store would
        // name an owner for a checkout that no longer exists.
        if let Some(session) = self.store.owner(&owned).cloned() {
            self.store
                .forget_owned(&session, &owned)
                .map_err(|error| error.to_string())?;
            self.fire_worktree(
                LifecycleEvent::WorktreeRemoved,
                &session,
                &owned,
                Some(checkout.path),
            );
        }
        Ok(size)
    }

    /// Whether worktree ownership can change hands, as the store's path
    /// template decides: a `{session}` template makes a worktree's location
    /// depend on the session that made it, so adopt and release are refused.
    /// Carried on the handshake as the client's affordance; the store's
    /// refusal remains the enforcement.
    pub fn ownership_movable(&self) -> bool {
        self.store.ownership_movable()
    }

    /// Re-walks the workspace with the configured `ignore` globs (§5's `scan`)
    /// and adopts the result as the workspace's repositories.
    fn scan_workspace(&mut self) -> Result<(), OrchestrationError> {
        let ignores = self.runtime.config().ignore.clone();
        let workspace = Workspace::discover(&self.scan_root, ignores)
            .map_err(|error| OrchestrationError::Scan(error.to_string()))?;
        self.repositories = workspace
            .repositories()
            .iter()
            .cloned()
            .map(|repository| (repository.id.clone(), repository))
            .collect();
        // The scan changed the population an override can apply to, so the
        // unknown set is recomputed: a rescan is a deliberate re-sync, and its
        // report is fresh even when a name was reported before the scan.
        // Overrides apply by display name — the same key every override
        // lookup uses — so the check is against names, not repo ids: a nested
        // repo's id ("gamma/nested") differs from its name ("nested").
        let known: HashSet<&str> = self
            .repositories
            .values()
            .map(|repository| repository.name.as_str())
            .collect();
        let mut names: Vec<String> = self
            .runtime
            .repo_overrides()
            .iter()
            .map(|rule| rule.name.clone())
            .collect();
        names.sort();
        names.dedup();
        self.unknown_overrides = names
            .into_iter()
            .filter(|name| !known.contains(name.as_str()))
            .collect();
        Ok(())
    }

    fn session_changed(&self, session: &SessionId) -> Result<Event, OrchestrationError> {
        Ok(Event::SessionChanged(self.session_row(session)?))
    }

    /// One worktree listing per repo, as the path-to-identity index the
    /// snapshot restore and terminal listing speak. Per repo, not per
    /// terminal: O(T x R) git subprocesses would run inline under the lock.
    fn worktree_index(&self) -> HashMap<PathBuf, OwnedWorktree> {
        let mut index = HashMap::new();
        for repository in self.repositories.values() {
            let Ok(checkouts) = grove_git::worktrees(&repository.path) else {
                continue;
            };
            for checkout in checkouts {
                index.insert(
                    checkout.path,
                    OwnedWorktree {
                        repo: repository.id.clone(),
                        branch: checkout.branch.unwrap_or_default(),
                    },
                );
            }
        }
        index
    }

    /// Every session as the picker renders it — the same answer the list
    /// request gives, so snapshot replies keep the picker fresh without a
    /// dedicated event.
    fn session_rows(&self) -> Result<Vec<SessionRow>, OrchestrationError> {
        let ids: Vec<_> = self
            .store
            .sessions()
            .iter()
            .map(|session| session.id.clone())
            .collect();
        ids.iter().map(|id| self.session_row(id)).collect()
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
            .count()
            + usize::from(
                self.session_shells
                    .get(session)
                    .is_some_and(|id| self.terminals.is_alive(*id).unwrap_or(false)),
            );
        Ok(SessionRow {
            id: stored.id.clone(),
            name: stored.name.clone(),
            members,
            state: stored.state,
            terminals: u32::try_from(terminals).unwrap_or(u32::MAX),
            since: self.store.seconds_in_state(session)?,
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
        if let Some(id) = self.session_shells.get(session).copied() {
            match self.terminals.kill(id) {
                Ok(()) => {
                    killed.push(id);
                    self.session_shells.remove(session);
                }
                Err(TerminalError::Missing(_)) => {
                    self.session_shells.remove(session);
                }
                Err(error) => failed.push(EffectFailure {
                    worktree: None,
                    message: error.to_string(),
                }),
            }
        }
        let mut remaining = Vec::new();
        for terminal in self.live.remove(session).unwrap_or_default() {
            match self.terminals.kill(terminal.id) {
                Ok(()) => {
                    killed.push(terminal.id);
                    // A terminal that exited on its own may already have been
                    // reported by the drain; the notified set is the single
                    // record of "fired once", so its answer decides.
                    if self.notified_exits.insert(terminal.id) {
                        self.fire_terminal(
                            LifecycleEvent::TerminalExited,
                            session,
                            &terminal.worktree,
                            terminal.id,
                        );
                    }
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
    /// and received confirmation. One of the two orchestration paths that
    /// invoke `git worktree remove` — `prune_row` is the other, with its own
    /// safety facts re-checked at prune time rather than trusting a listing.
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
            // `end session` removes everything a session owns, so the
            // repository's own checkout must never reach this call — not even
            // from a state file written by a daemon that predates the adopt
            // guard.
            if paths_equal(&item.path, &repository.path) {
                failed.push(EffectFailure {
                    worktree: Some(item.worktree.clone()),
                    message: OrchestrationError::CloneNotRemovable(item.worktree.clone())
                        .to_string(),
                });
                continue;
            }
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
        let checkout = self
            .find_checkout(&repository, &worktree.branch)?
            .ok_or_else(|| OrchestrationError::WorktreeMissing(worktree.clone()))?;
        // The clone is derived by comparing the checkout's path with the
        // repository's, not trusted from anywhere. Adopting it would make the
        // worktree session-owned, and `end session` removes everything a
        // session owns — so this is where the domain's obligation lands.
        if checkout.is_clone || paths_equal(&checkout.path, &repository.path) {
            return Err(OrchestrationError::CloneNotAdoptable(worktree.clone()));
        }
        let path = checkout.path;
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
            // Same once-only rule as close(): the drain may have reported it.
            if self.notified_exits.insert(terminal.id) {
                self.fire_terminal(
                    LifecycleEvent::TerminalExited,
                    session,
                    &terminal.worktree,
                    terminal.id,
                );
            }
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
                let base = self
                    .effective_base(&repository)
                    .ok_or_else(|| OrchestrationError::BaseMissing(repo_id.clone()))?;
                let path = self.worktree_path(&repository, &stored.name, branch)?;
                // `branch_slug` is deliberately non-injective: two branch names
                // can resolve to one path, and the second must not silently
                // take the first's checkout.
                if paths_equal(&path, &repository.path) {
                    return Err(OrchestrationError::WorktreePathIsClone { path: path.clone() });
                }
                if let Some(claimant) = grove_git::worktrees(&repository.path)?
                    .into_iter()
                    .find(|checkout| paths_equal(&checkout.path, &path))
                {
                    return Err(OrchestrationError::PathClaimed {
                        path: path.clone(),
                        branch: claimant.branch.unwrap_or_else(|| "<detached HEAD>".into()),
                    });
                };
                grove_git::create_worktree(&repository.path, &path, branch, &base)?;
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

    /// The checkout of `branch`, clone included. Callers decide what a clone
    /// row means; this lookup must not decide for them.
    fn find_checkout(
        &self,
        repository: &Repository,
        branch: &str,
    ) -> Result<Option<grove_git::Worktree>, OrchestrationError> {
        Ok(grove_git::worktrees(&repository.path)?
            .into_iter()
            .find(|candidate| candidate.branch.as_deref() == Some(branch)))
    }

    /// The ownership a client is shown for a checkout.
    ///
    /// Derived here, at the trust boundary: `Ownership` is display data that
    /// travels daemon to client, and clients never assert it.
    pub fn ownership(&self, repository: &Repository, checkout: &grove_git::Worktree) -> Ownership {
        if checkout.is_clone || paths_equal(&checkout.path, &repository.path) {
            return Ownership::Clone;
        }
        // A detached HEAD names no branch, so no session can own it.
        let Some(branch) = checkout.branch.as_ref() else {
            return Ownership::Unowned;
        };
        let worktree = OwnedWorktree {
            repo: repository.id.clone(),
            branch: branch.clone(),
        };
        match self.store.owner(&worktree) {
            Some(owner) if self.store.open_session() == Some(owner) => Ownership::Ours,
            Some(owner) => Ownership::Other(owner.clone()),
            None => Ownership::Unowned,
        }
    }

    fn resolve_owned(&self, worktree: &OwnedWorktree) -> Result<PathBuf, OrchestrationError> {
        let repository = self.repository(&worktree.repo)?;
        self.find_checkout(repository, &worktree.branch)?
            .map(|candidate| candidate.path)
            .ok_or_else(|| OrchestrationError::WorktreeMissing(worktree.clone()))
    }

    fn live_terminal(&self, session: &SessionId, worktree: &OwnedWorktree) -> Option<TerminalId> {
        self.live.get(session)?.iter().find_map(|terminal| {
            (&terminal.worktree == worktree
                && self.terminals.is_alive(terminal.id).unwrap_or(false))
            .then_some(terminal.id)
        })
    }

    /// The live terminal of a checkout, whichever session runs it.
    /// The live pty running in this checkout, whoever owns it.
    ///
    /// `self.live` is a session's bookkeeping — which terminals end when that
    /// session ends — and it was standing in for "does this worktree have a
    /// terminal". It cannot: a worktree no session owns can still have one,
    /// which is most of them on a fresh dash. The manager is the authority on
    /// what is running.
    fn live_terminal_at(&self, path: &Path) -> Option<TerminalId> {
        self.terminals
            .list()
            .into_iter()
            .find_map(|(id, key, alive)| {
                (alive && matches!(&key, TerminalKey::Worktree(at) if at == path)).then_some(id)
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

/// Whether two paths name the same directory, tolerating symlinks and
/// case-stable mounts on whichever side exists to canonicalize.
fn paths_equal(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Expands a leading `~` like the daemon's own config does, so a spawn
/// request naming `~/notes` works without the client resolving it.
pub fn expand_home(path: &Path) -> PathBuf {
    if path == Path::new("~") {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| path.to_path_buf());
    }
    if let Ok(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
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
        HookReport::RepoSetupDisabled { repo, message } => Some(Event::Failed {
            context: format!("repo setup for {repo:?}"),
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
    use grove_proto::Attach;
    use std::env;
    use std::fs;
    use std::process::Command;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

    fn wait_for_terminal(manager: &TerminalManager, id: TerminalId, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if manager.snapshot(id).unwrap().contents.contains(text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "terminal never displayed {text:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn repo_override_changes_path_and_base_for_that_repo_only() {
        let temp = TempDir::new();
        let repo_a = temp.0.join("repo-a");
        let repo_b = temp.0.join("repo-b");
        for repo in [&repo_a, &repo_b] {
            fs::create_dir_all(repo).unwrap();
            git(repo, &["init", "-q", "-b", "main"]);
            git(repo, &["config", "user.name", "Grove Test"]);
            git(repo, &["config", "user.email", "grove@example.test"]);
            fs::write(repo.join("from-main"), "main\n").unwrap();
            git(repo, &["add", "from-main"]);
            git(repo, &["commit", "-qm", "main"]);
        }
        git(&repo_a, &["branch", "develop"]);
        git(&repo_a, &["checkout", "-q", "develop"]);
        fs::write(repo_a.join("from-develop"), "develop\n").unwrap();
        git(&repo_a, &["add", "from-develop"]);
        git(&repo_a, &["commit", "-qm", "develop"]);
        git(&repo_a, &["checkout", "-q", "main"]);
        // Advance main past the branch point, so only a worktree cut from the
        // overridden base has the develop file and lacks this one.
        fs::write(repo_a.join("later-on-main"), "main\n").unwrap();
        git(&repo_a, &["add", "later-on-main"]);
        git(&repo_a, &["commit", "-qm", "later"]);

        let template = temp.0.join("global/{repo}/{branch_slug}");
        let template = template.to_string_lossy().into_owned();
        let runtime = DaemonRuntime::load_source(
            &format!(
                r#"
                local grove = require('grove')
                grove.setup({{ worktree_path = {template:?} }})
                grove.repo('repo-a', {{
                  worktree_path = {override_template:?},
                  base = 'develop',
                }})
                "#,
                template = template,
                override_template = temp
                    .0
                    .join("override/{repo}/{branch_slug}")
                    .to_string_lossy(),
            ),
            "repo-override",
        )
        .runtime;
        let mut store = Store::load_at(&temp.0.join("state"), &temp.0, &template);
        store.create(sid("one"), "one".to_string()).unwrap();
        store
            .add_member(&sid("one"), RepoId("repo-a".into()))
            .unwrap();
        store
            .add_member(&sid("one"), RepoId("repo-b".into()))
            .unwrap();
        let repositories = vec![
            Repository {
                id: RepoId("repo-a".into()),
                name: "repo-a".into(),
                path: repo_a.clone(),
                // The discovered default: the override must win over it.
                base_branch: Some("main".into()),
            },
            Repository {
                id: RepoId("repo-b".into()),
                name: "repo-b".into(),
                path: repo_b.clone(),
                base_branch: Some("main".into()),
            },
        ];
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let mut daemon = SessionOrchestrator::new(
            store,
            repositories,
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        );

        let report = daemon
            .create_worktrees(
                &sid("one"),
                "feature",
                &[RepoId("repo-a".into()), RepoId("repo-b".into())],
            )
            .unwrap();
        assert!(report.failed.is_empty(), "{report:?}");
        assert_eq!(report.created.len(), 2);

        // repo-a: the override picked its path and its base.
        let path_a = &report.created[0].path;
        assert_eq!(
            path_a,
            &temp.0.join("override/repo-a/feature"),
            "the override path must apply to repo-a"
        );
        assert!(
            path_a.join("from-develop").exists(),
            "worktree must branch from the overridden base"
        );
        assert!(!path_a.join("later-on-main").exists());

        // repo-b: no override, so the global template and discovered base.
        let path_b = &report.created[1].path;
        assert_eq!(path_b, &temp.0.join("global/repo-b/feature"));
        assert!(path_b.join("from-main").exists());
    }

    #[test]
    fn override_naming_an_unknown_repo_is_reported_once() {
        let temp = TempDir::new();
        let runtime = DaemonRuntime::load_source(
            "local grove = require('grove')\n\
             grove.repo('ghost', { base = 'origin/develop' })\n",
            "unknown-repo",
        )
        .runtime;
        let mut store = Store::load_at(&temp.0.join("state"), &temp.0, "");
        store.create(sid("one"), "one".to_string()).unwrap();
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let mut daemon =
            SessionOrchestrator::new(store, Vec::new(), temp.0.clone(), terminals, fetch, runtime);

        let events = daemon.handle_request(Request::ListSessions);
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Failed { context, message }
                if context == "config: repo override"
                    && message.contains("ghost")
                    && message.contains("ignored")
        )));
        // Reported once, not on every request.
        let events = daemon.handle_request(Request::ListSessions);
        assert!(!events.iter().any(|event| matches!(
            event,
            Event::Failed { context, .. } if context == "config: repo override"
        )));
    }

    #[test]
    fn session_new_refuses_a_trimmed_duplicate_name_but_keeps_case_distinct() {
        let temp = TempDir::new();
        let mut store = Store::load_at(&temp.0.join("state"), &temp.0, "");
        store.create(sid("holder"), "invoice split".into()).unwrap();
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let runtime = DaemonRuntime::load_source("", "duplicate-session-name").runtime;
        let mut daemon =
            SessionOrchestrator::new(store, Vec::new(), temp.0.clone(), terminals, fetch, runtime);

        let events = daemon.handle_request(Request::SessionNew {
            name: "  invoice split  ".into(),
        });
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::Failed { context, message }
                    if context == "session new"
                        && message.contains("invoice split")
                        && message.contains("holder")
            )),
            "duplicate name must identify its holder: {events:?}"
        );
        assert!(events.iter().all(|event| match event {
            Event::Failed { context, .. } => !context.contains('{') && !context.contains("::"),
            _ => true,
        }));
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::Failed { message, .. }
                    if message.ends_with("held by session holder")
            )),
            "the holder is named plainly, not as a debug-printed id: {events:?}"
        );
        assert_eq!(daemon.store().sessions().len(), 1);

        let events = daemon.handle_request(Request::SessionNew {
            name: "Invoice Split".into(),
        });
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::SessionChanged(_)))
        );
        assert!(
            daemon
                .store()
                .sessions()
                .iter()
                .any(|session| session.name == "Invoice Split")
        );
    }

    #[test]
    fn session_rename_refuses_another_sessions_trimmed_name() {
        let temp = TempDir::new();
        let mut store = Store::load_at(&temp.0.join("state"), &temp.0, "");
        store.create(sid("holder"), "invoice split".into()).unwrap();
        store.create(sid("other"), "other".into()).unwrap();
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let runtime = DaemonRuntime::load_source("", "duplicate-session-rename").runtime;
        let mut daemon =
            SessionOrchestrator::new(store, Vec::new(), temp.0.clone(), terminals, fetch, runtime);

        let events = daemon.handle_request(Request::SessionRename {
            session: sid("other"),
            name: " invoice split ".into(),
        });
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::Failed { message, .. }
                    if message.contains("invoice split") && message.contains("holder")
            )),
            "duplicate name must identify its holder: {events:?}"
        );
        assert_eq!(daemon.store().session(&sid("other")).unwrap().name, "other");
    }

    #[test]
    fn legacy_duplicate_session_names_still_load() {
        let temp = TempDir::new();
        let state_home = temp.0.join("state");
        let state_dir = state_home
            .join("grove")
            .join(grove_state::workspace_hash(&temp.0));
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("sessions.json"),
            r#"{"version":1,"sessions":[
                {"id":"one","name":"duplicate","members":[],"owned":[],"state":"Closed"},
                {"id":"two","name":"duplicate","members":[],"owned":[],"state":"Closed"}
            ],"open":null}"#,
        )
        .unwrap();

        let store = Store::load_at(&state_home, &temp.0, "");
        assert_eq!(store.sessions().len(), 2);
        assert!(
            store
                .sessions()
                .iter()
                .all(|session| session.name == "duplicate")
        );
    }

    #[test]
    fn session_since_uses_an_injected_clock_and_survives_restart() {
        let temp = TempDir::new();
        let state_home = temp.0.join("state");
        let now = Arc::new(AtomicU64::new(1_000));
        let clock = {
            let now = Arc::clone(&now);
            Arc::new(move || now.load(Ordering::SeqCst))
        };
        let mut store = Store::load_at_with_clock(&state_home, &temp.0, "", clock);
        store.create(sid("one"), "one".into()).unwrap();
        store.open(&sid("one")).unwrap();
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let runtime = DaemonRuntime::load_source("", "session-since").runtime;
        let mut daemon =
            SessionOrchestrator::new(store, Vec::new(), temp.0.clone(), terminals, fetch, runtime);

        daemon.detach(&sid("one")).unwrap();
        now.store(1_061, Ordering::SeqCst);
        let events = daemon.handle_request(Request::ListSessions);
        let Some(Event::Sessions(rows)) = events.first() else {
            panic!("expected Sessions, got {events:?}")
        };
        let row = rows.iter().find(|row| row.id == sid("one")).unwrap();
        assert_eq!(row.state, SessionState::Detached);
        assert_eq!(row.since, 61);

        daemon.close(&sid("one")).unwrap();
        drop(daemon);
        now.store(1_122, Ordering::SeqCst);
        let clock = {
            let now = Arc::clone(&now);
            Arc::new(move || now.load(Ordering::SeqCst))
        };
        let store = Store::load_at_with_clock(&state_home, &temp.0, "", clock);
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let runtime = DaemonRuntime::load_source("", "session-since-restart").runtime;
        let mut daemon =
            SessionOrchestrator::new(store, Vec::new(), temp.0.clone(), terminals, fetch, runtime);
        let events = daemon.handle_request(Request::ListSessions);
        let Some(Event::Sessions(rows)) = events.first() else {
            panic!("expected Sessions, got {events:?}")
        };
        let row = rows.iter().find(|row| row.id == sid("one")).unwrap();
        assert_eq!(row.state, SessionState::Closed);
        assert_eq!(row.since, 61);
    }

    fn remote_base(path: &Path, branch: &str) {
        // Gives discovery an advertised default branch without needing a real
        // remote: base_branch reads refs/remotes/origin/HEAD only.
        git(path, &["remote", "add", "origin", "nowhere"]);
        git(
            path,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                &format!("refs/remotes/origin/{branch}"),
            ],
        );
    }

    #[test]
    fn dash_requests_serve_repos_worktrees_and_base_provenance() {
        let temp = TempDir::new();
        let clone_a = temp.0.join("repo-a");
        let clone_b = temp.0.join("repo-b");
        let feat_path = temp.0.join("feat");
        let other_path = temp.0.join("other");
        for repo in [&clone_a, &clone_b] {
            fs::create_dir_all(repo).unwrap();
            git(repo, &["init", "-q", "-b", "main"]);
            git(repo, &["config", "user.name", "Grove Test"]);
            git(repo, &["config", "user.email", "grove@example.test"]);
            fs::write(repo.join("tracked"), "base\n").unwrap();
            git(repo, &["add", "tracked"]);
            git(repo, &["commit", "-qm", "base"]);
            remote_base(repo, "main");
        }
        git(
            &clone_a,
            &[
                "worktree",
                "add",
                "-qb",
                "feat",
                feat_path.to_str().unwrap(),
            ],
        );
        git(
            &clone_a,
            &[
                "worktree",
                "add",
                "-qb",
                "other",
                other_path.to_str().unwrap(),
            ],
        );
        fs::write(feat_path.join("uncommitted"), "work\n").unwrap();
        // The age column renders seconds since the branch's tip commit, so
        // feat gets a tip commit with a known old timestamp.
        let committed = Command::new("git")
            .arg("-C")
            .arg(&feat_path)
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .args(["commit", "--allow-empty", "-qm", "old tip"])
            .status()
            .unwrap();
        assert!(committed.success());

        let template = temp.0.join("trees/{repo}/{branch_slug}");
        let template = template.to_string_lossy().into_owned();
        let mut store = Store::load_at(&temp.0.join("state"), &temp.0, &template);
        store.create(sid("one"), "one".to_string()).unwrap();
        store.create(sid("two"), "two".to_string()).unwrap();
        // repo-b stays out of the session: it must never be fetched, so its
        // rows answer the stale question from a missing FETCH_HEAD.
        store
            .add_member(&sid("one"), RepoId("repo-a".into()))
            .unwrap();
        store
            .add_member(&sid("two"), RepoId("repo-a".into()))
            .unwrap();
        store
            .adopt(
                &sid("one"),
                OwnedWorktree {
                    repo: RepoId("repo-a".into()),
                    branch: "feat".into(),
                },
                &feat_path,
                &clone_a,
            )
            .unwrap();
        store
            .adopt(
                &sid("two"),
                OwnedWorktree {
                    repo: RepoId("repo-a".into()),
                    branch: "other".into(),
                },
                &other_path,
                &clone_a,
            )
            .unwrap();
        let runtime = DaemonRuntime::load_source(
            &format!(
                "local grove = require('grove')\n\
                 grove.setup({{ worktree_path = {template:?} }})\n\
                 grove.repo('repo-a', {{ base = 'origin/develop' }})\n",
                template = template
            ),
            "dash-config",
        )
        .runtime;
        let repositories = vec![
            Repository {
                id: RepoId("repo-a".into()),
                name: "repo-a".into(),
                path: clone_a.clone(),
                base_branch: Some("origin/main".into()),
            },
            Repository {
                id: RepoId("repo-b".into()),
                name: "repo-b".into(),
                path: clone_b.clone(),
                base_branch: Some("origin/main".into()),
            },
        ];
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(3600));
        let mut daemon = SessionOrchestrator::new(
            store,
            repositories,
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        );
        daemon.open(&sid("one")).unwrap();

        let events = daemon.handle_request(Request::ListRepos);
        let Some(Event::Repos(rows)) = events.into_iter().next() else {
            panic!("expected Repos")
        };
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["repo-a", "repo-b"]
        );
        let row_a = &rows[0];
        // Branched work only: the clone is listed on the WORKTREES pane but
        // never counted, or §4.1's · state ("haven't branched yet") would be
        // unreachable — every repo has its clone.
        assert_eq!(row_a.worktrees, 2);
        assert!(row_a.dirty, "the feat worktree has an uncommitted file");
        assert!(row_a.member);
        // The override supplied the base, so the pane may not claim
        // "(from origin/HEAD)" — the provenance #53's review required.
        assert_eq!(row_a.base_branch, "origin/develop");
        assert!(!row_a.base_from_origin_head);
        let row_b = &rows[1];
        // repo-b holds only its clone: the documented zero-worktree state,
        // rendered · — reachable only when the clone is not counted.
        assert_eq!(row_b.worktrees, 0);
        assert!(!row_b.dirty);
        assert!(!row_b.member);
        assert_eq!(row_b.base_branch, "origin/main");
        assert!(row_b.base_from_origin_head);

        let events = daemon.handle_request(Request::ListWorktrees(RepoId("repo-a".into())));
        let Some(Event::Worktrees { repo, rows }) = events.into_iter().next() else {
            panic!("expected Worktrees")
        };
        assert_eq!(repo.0, "repo-a");
        assert_eq!(
            rows.iter()
                .map(|row| row.worktree.branch.as_str())
                .collect::<Vec<_>>(),
            ["feat", "main", "other"]
        );
        let feat = &rows[0];
        assert_eq!(feat.ownership, Ownership::Ours);
        assert_eq!(feat.dirty_files, 1);
        // `open` spawned the terminal of the worktree session one owns.
        assert!(feat.terminal.is_some());
        // §7's size read never blocks a caller: the first request answers the
        // last known value, 0, and the walk's answer lands on the next one.
        assert_eq!(feat.size, 0);
        assert!(feat.age > 0, "the branch has a tip commit");
        // Open's fetch attempt wrote a FETCH_HEAD, however briefly: the repo's
        // refs count as fresh.
        assert!(!rows[0].stale);
        let main = &rows[1];
        assert_eq!(main.ownership, Ownership::Clone);
        assert_eq!(main.terminal, None);
        let other = &rows[2];
        assert_eq!(other.ownership, Ownership::Other(sid("two")));

        // The background size walk answers on a later request; the handoff
        // completes within a deadline rather than assuming one refresh
        // suffices, since the walk runs at its own pace.
        let deadline = Instant::now() + Duration::from_secs(2);
        let size = loop {
            let events = daemon.handle_request(Request::ListWorktrees(RepoId("repo-a".into())));
            let Some(Event::Worktrees { rows, .. }) = events.into_iter().next() else {
                panic!("expected Worktrees")
            };
            assert_eq!(rows[0].ownership, Ownership::Ours);
            if rows[0].size > 0 {
                break rows[0].size;
            }
            assert!(
                Instant::now() < deadline,
                "the background size walk never answered"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(size > 0);

        // A repo that was never fetched has no FETCH_HEAD, and unknown refs
        // are stale exactly as the fetch policy judges them.
        let events = daemon.handle_request(Request::ListWorktrees(RepoId("repo-b".into())));
        let Some(Event::Worktrees { rows, .. }) = events.into_iter().next() else {
            panic!("expected Worktrees")
        };
        assert!(rows[0].stale);
        assert_eq!(
            rows[0].ownership,
            Ownership::Clone,
            "repo-b has only its clone"
        );
    }

    #[test]
    fn scan_rewalks_the_workspace_and_is_deterministic() {
        let temp = TempDir::new();
        for repo in ["alpha", "vendor/hidden", "gamma/nested"] {
            let path = temp.0.join(repo);
            fs::create_dir_all(&path).unwrap();
            git(&path, &["init", "-q"]);
        }
        let mut store = Store::load_at(&temp.0.join("state"), &temp.0, "");
        store.create(sid("one"), "one".to_string()).unwrap();
        let runtime = DaemonRuntime::load_source(
            "local grove = require('grove')\n\
             grove.setup({ ignore = { 'vendor/' } })\n\
             -- Nested below gamma: its id is 'gamma/nested', its name is 'nested'.\n\
             grove.repo('nested', { base = 'origin/main' })\n\
             grove.repo('ghost', { base = 'origin/main' })\n",
            "scan-config",
        )
        .runtime;
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let mut daemon = SessionOrchestrator::new(
            store,
            vec![Repository {
                id: RepoId("alpha".into()),
                name: "alpha".into(),
                path: temp.0.join("alpha"),
                base_branch: None,
            }],
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        );

        let events = daemon.handle_request(Request::ListRepos);
        // The config already carries overrides, so the response leads with
        // their report; find the rows among the events.
        let Some(Event::Repos(rows)) = events
            .into_iter()
            .find(|event| matches!(event, Event::Repos(_)))
        else {
            panic!("expected Repos")
        };
        assert_eq!(
            rows.iter()
                .map(|row| row.repo.0.as_str())
                .collect::<Vec<_>>(),
            ["alpha"],
            "only the constructor's view before a scan"
        );

        fs::create_dir_all(temp.0.join("beta")).unwrap();
        git(&temp.0.join("beta"), &["init", "-q"]);
        let events = daemon.handle_request(Request::Scan);
        let Some(Event::Repos(first)) = events.into_iter().next() else {
            panic!("expected Repos")
        };
        let ids: Vec<_> = first.iter().map(|row| row.repo.0.as_str()).collect();
        assert_eq!(ids, ["alpha", "beta", "gamma/nested"]);
        // Honours ignore globs.
        assert!(!ids.contains(&"vendor/hidden"));
        // Nested repos are repos per SPEC §2.2; the walk prunes only the
        // repository's .git metadata itself.
        assert!(ids.contains(&"gamma/nested"));

        // The recomputed unknown set is keyed like every override lookup, by
        // name — not by repo id, which for a nested repo is composite. The
        // quote after "named" makes the check exact: gamma/nested's id would
        // also contain "nested", but only a failure *about* "nested" counts.
        // This pair of assertions is the regression test for that bug: keyed
        // by id, 'nested' is falsely reported unknown and the count of 2 (and
        // the name check) fail; keyed by name, exactly ghost is reported.
        let events = daemon.handle_request(Request::ListRepos);
        let reported: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                Event::Failed { context, message } if context == "config: repo override" => {
                    Some(message.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            reported.len(),
            1,
            "exactly one unknown override after the scan: {reported:?}"
        );
        assert!(
            reported[0].contains("ghost"),
            "the unknown override is ghost: {reported:?}"
        );
        assert!(
            !reported[0].contains("named \"nested\""),
            "the nested override must be known by name despite its composite id: {reported:?}"
        );

        let events = daemon.handle_request(Request::Scan);
        let Some(Event::Repos(second)) = events
            .into_iter()
            .find(|event| matches!(event, Event::Repos(_)))
        else {
            panic!("expected Repos")
        };
        assert_eq!(
            first, second,
            "rescanning an unchanged workspace must produce the same rows"
        );
    }

    #[test]
    fn a_terminal_on_an_unowned_worktree_is_reported_on_its_row() {
        // §2 hangs a pty off a worktree, not off an *owned* worktree, and
        // §3.3 binds `^g enter` to "the selected worktree" with no condition.
        // The pty was spawned either way — the dash just went on saying "no
        // terminal here — ^g enter opens one", because the lookup asked the
        // session's own bookkeeping, which knows nothing about a worktree no
        // session owns. Pressing the key again spawned another.
        let temp = TempDir::new();
        let clone = temp.0.join("repo");
        let theirs_path = temp.0.join("theirs");
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
                "theirs",
                theirs_path.to_str().unwrap(),
            ],
        );

        // A session that is a member of the repo but owns nothing in it —
        // exactly what `add <repo>` leaves behind.
        let template = temp.0.join("trees/{repo}/{branch_slug}");
        let template = template.to_string_lossy().into_owned();
        let mut store = Store::load_at(&temp.0.join("state"), &temp.0, &template);
        store.create(sid("one"), "one".to_string()).unwrap();
        store
            .add_member(&sid("one"), RepoId("repo".into()))
            .unwrap();
        let repository = Repository {
            id: RepoId("repo".into()),
            name: "repo".into(),
            path: clone.clone(),
            base_branch: Some("main".into()),
        };
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 200);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let runtime = DaemonRuntime::load_source("", "unowned-terminal").runtime;
        let mut daemon = SessionOrchestrator::new(
            store,
            vec![repository],
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        );

        let events = daemon.handle_request(Request::SpawnTerminal(TerminalTarget::Worktree(
            WorktreeRef {
                repo: RepoId("repo".into()),
                branch: "theirs".into(),
            },
        )));
        let Some(Event::TerminalSpawned { terminal, .. }) = events.first() else {
            panic!("expected TerminalSpawned, got {events:?}")
        };
        let spawned = *terminal;

        let events = daemon.handle_request(Request::ListWorktrees(RepoId("repo".into())));
        let Some(Event::Worktrees { rows, .. }) = events.first() else {
            panic!("expected Worktrees, got {events:?}")
        };
        let row = rows
            .iter()
            .find(|row| row.worktree.branch == "theirs")
            .expect("the unowned worktree is still listed");
        assert_eq!(
            row.terminal,
            Some(spawned),
            "the row must carry the terminal that is running in it"
        );

        // And the key does not spawn a second shell into the same checkout,
        // which is what "no terminal here" invited.
        let again = daemon.handle_request(Request::SpawnTerminal(TerminalTarget::Worktree(
            WorktreeRef {
                repo: RepoId("repo".into()),
                branch: "theirs".into(),
            },
        )));
        assert!(
            again.iter().any(|event| matches!(
                event,
                Event::Failed { message, .. } if message.contains("already exists")
            )),
            "a second spawn must be refused: {again:?}"
        );
    }

    #[test]
    fn terminal_requests_serve_spawn_attach_input_and_lifecycle() {
        let temp = TempDir::new();
        let clone = temp.0.join("repo");
        let ours_path = temp.0.join("ours");
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

        let template = temp.0.join("trees/{repo}/{branch_slug}");
        let template = template.to_string_lossy().into_owned();
        let mut store = Store::load_at(&temp.0.join("state"), &temp.0, &template);
        store.create(sid("one"), "one".to_string()).unwrap();
        store
            .add_member(&sid("one"), RepoId("repo".into()))
            .unwrap();
        store
            .adopt(&sid("one"), owned("ours"), &ours_path, &clone)
            .unwrap();
        let repository = Repository {
            id: RepoId("repo".into()),
            name: "repo".into(),
            path: clone.clone(),
            base_branch: Some("main".into()),
        };
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 200);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let runtime = DaemonRuntime::load_source(
            &format!(
                "local grove = require('grove'); grove.setup({{ worktree_path = {template:?} }})",
                template = template
            ),
            "terminal-config",
        )
        .runtime;
        let mut daemon = SessionOrchestrator::new(
            store,
            vec![repository],
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        );

        // Spawn: the answer carries the id, and a worktree owned by the open
        // session joins its live set.
        let events = daemon.handle_request(Request::SpawnTerminal(TerminalTarget::Worktree(
            WorktreeRef {
                repo: RepoId("repo".into()),
                branch: "ours".into(),
            },
        )));
        let Some(Event::TerminalSpawned { terminal, target }) = events.first() else {
            panic!("expected TerminalSpawned, got {events:?}")
        };
        assert!(matches!(target, TerminalTarget::Worktree(_)));
        let id = *terminal;
        assert!(daemon.store().session(&sid("one")).is_ok(),);
        // One terminal per checkout.
        let events = daemon.handle_request(Request::SpawnTerminal(TerminalTarget::Worktree(
            WorktreeRef {
                repo: RepoId("repo".into()),
                branch: "ours".into(),
            },
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Failed { message, .. } if message.contains("already exists")
        )));

        // Attach: screen first, exactly the visible grid, then one empty
        // scrollback chunk with `done` set — never zero chunks.
        let outcome = daemon
            .attach_terminal(Attach {
                terminal: id,
                scrollback: ScrollbackRequest::All,
                rows: 10,
                cols: 40,
            })
            .unwrap();
        let output = outcome.output;
        assert!(matches!(outcome.screen, Event::TerminalScreen { .. }));
        let events: Vec<Event> = std::iter::once(outcome.screen.clone())
            .chain(outcome.backfill.clone())
            .collect();
        match &events[0] {
            Event::TerminalScreen { screen, .. } => {
                assert_eq!((screen.rows, screen.cols), (10, 40));
                assert_eq!(screen.cells.len(), 10);
                assert_eq!(screen.cells[0].len(), 40);
                assert!(screen.cells.iter().all(|row| row.len() == 40));
            }
            _ => unreachable!(),
        }
        match &events[1] {
            Event::TerminalScrollback {
                seq, lines, done, ..
            } => {
                assert_eq!((*seq, lines.len(), *done), (0, 0, true));
            }
            other => panic!("expected scrollback, got {other:?}"),
        }

        // Input reaches the right pty: the shell echoes and the screen shows
        // it. Output produced after attach arrives on the live channel without
        // waiting for any backfill (there was none to wait for here).
        daemon
            .handle_request(Request::Input {
                terminal: id,
                bytes: b"echo attached-output\r".to_vec(),
            })
            .is_empty();
        let mut received = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while !received {
            match output.recv_timeout(Duration::from_millis(500)) {
                Ok(bytes) => {
                    received = bytes.windows(8).any(|window| window == b"attached");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "no live output after input");
                }
                Err(_) => panic!("the live channel closed before any output"),
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = daemon.terminals().snapshot(id).unwrap();
            if snapshot.contents.contains("attached-output") {
                break;
            }
            assert!(Instant::now() < deadline, "input never reached the pty");
            thread::sleep(Duration::from_millis(10));
        }

        // Resize reflows vt100's grid; the next attach reports the new shape.
        daemon
            .handle_request(Request::ResizeTerminal {
                terminal: id,
                rows: 12,
                cols: 50,
            })
            .is_empty();
        let outcome = daemon
            .attach_terminal(Attach {
                terminal: id,
                scrollback: ScrollbackRequest::Lines(5),
                rows: 12,
                cols: 50,
            })
            .unwrap();
        let events = std::iter::once(&outcome.screen)
            .chain(outcome.backfill.iter())
            .collect::<Vec<&Event>>();
        match &events[0] {
            Event::TerminalScreen { screen, .. } => {
                assert_eq!((screen.rows, screen.cols), (12, 50));
            }
            _ => unreachable!(),
        }

        // Detach stops only the feed: the pty keeps running.
        daemon
            .handle_request(Request::DetachTerminal(id))
            .is_empty();
        assert!(daemon.terminals().is_alive(id).unwrap());

        // Scratch shell spawns with no worktree, and ListTerminals finds both.
        let events = daemon.handle_request(Request::SpawnTerminal(TerminalTarget::Scratch {
            cwd: None,
        }));
        let Some(Event::TerminalSpawned {
            terminal: scratch, ..
        }) = events.first()
        else {
            panic!("expected scratch spawn")
        };
        let events = daemon.handle_request(Request::ListTerminals);
        let Some(Event::Terminals(rows)) = events.first() else {
            panic!("expected Terminals")
        };
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|row| matches!(row.target, TerminalTarget::Scratch { .. }))
        );
        let worktree_row = rows.iter().find(|row| row.terminal == id).unwrap();
        assert!(matches!(
            &worktree_row.target,
            TerminalTarget::Worktree(reference) if reference.branch == "ours"
        ));
        // An idle shell is the pane itself, not a foreground job worth warning about.
        assert_eq!(worktree_row.foreground, None);

        // Killing emits TerminalExited exactly once, on the killer's response.
        let events = daemon.handle_request(Request::KillTerminal(id));
        let exits = events
            .iter()
            .filter(
                |event| matches!(event, Event::TerminalExited { terminal, .. } if *terminal == id),
            )
            .count();
        assert_eq!(exits, 1);
        // A killed terminal is not merely dead but gone: a later attach must
        // be refused, not quietly served a corpse.
        assert!(matches!(
            daemon.terminals().is_alive(id),
            Err(TerminalError::Missing(_))
        ));
        // The killed terminal is gone from the list; the scratch is untouched.
        let events = daemon.handle_request(Request::ListTerminals);
        let Some(Event::Terminals(rows)) = events.first() else {
            panic!("expected Terminals")
        };
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].target, TerminalTarget::Scratch { .. }));
        assert!(daemon.terminals().is_alive(*scratch).unwrap());
    }

    #[test]
    fn session_terminal_exports_context_reuses_one_pty_and_ends_with_session() {
        let temp = TempDir::new();
        let clone = temp.0.join("repo");
        let worktree = temp.0.join("ours");
        fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "Grove Test"]);
        git(&clone, &["config", "user.email", "grove@example.test"]);
        fs::write(clone.join("tracked"), "base\n").unwrap();
        git(&clone, &["add", "tracked"]);
        git(&clone, &["commit", "-qm", "base"]);
        git(
            &clone,
            &["worktree", "add", "-qb", "ours", worktree.to_str().unwrap()],
        );

        let mut store = Store::load_at(&temp.0.join("state"), &temp.0, "");
        store
            .create(sid("session-1"), "invoice split".to_string())
            .unwrap();
        store
            .add_member(&sid("session-1"), RepoId("repo".into()))
            .unwrap();
        store
            .adopt(&sid("session-1"), owned("ours"), &worktree, &clone)
            .unwrap();
        let socket = temp.0.join("groved.sock");
        let repository = Repository {
            id: RepoId("repo".into()),
            name: "payments".into(),
            path: clone.clone(),
            base_branch: Some("main".into()),
        };
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 200);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let runtime = DaemonRuntime::load_source("", "session-terminal").runtime;
        let mut daemon = SessionOrchestrator::new(
            store,
            vec![repository],
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        )
        .with_socket_path(&socket);

        let target = TerminalTarget::Session {
            session: sid("session-1"),
        };
        let events = daemon.handle_request(Request::SpawnTerminal(target.clone()));
        let Some(Event::TerminalSpawned { terminal, .. }) = events.first() else {
            panic!("expected session terminal, got {events:?}")
        };
        let terminal = *terminal;

        let echo_disabled = temp.0.join("session-echo-disabled");
        daemon
            .terminals
            .input(
                terminal,
                format!("stty -echo; touch '{}'\r", echo_disabled.display()).as_bytes(),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !echo_disabled.exists() {
            assert!(
                Instant::now() < deadline,
                "session shell did not disable echo"
            );
            thread::sleep(Duration::from_millis(10));
        }

        let expected_repos = format!(
            r#"[{{"id":"repo","name":"payments","path":"{}"}}]"#,
            clone.display()
        );
        let expected_worktrees = format!(
            r#"[{{"branch":"ours","path":"{}","repo":"repo"}}]"#,
            worktree.display()
        );
        let command = format!(
            "[ \"$GROVE_SESSION\" = 'invoice split' ] && echo SESSION_OK; \
             [ \"$GROVE_SESSION_ID\" = 'session-1' ] && echo ID_OK; \
             [ \"$GROVE_WORKSPACE\" = '{}' ] && echo WORKSPACE_OK; \
             [ \"$GROVE_REPOS\" = '{}' ] && echo REPOS_OK; \
             [ \"$GROVE_WORKTREES\" = '{}' ] && echo WORKTREES_OK; \
             [ \"$GROVE_SOCKET\" = '{}' ] && echo SOCKET_OK; \
             [ \"$PWD\" = '{}' ] && echo CWD_OK\r",
            temp.0.display(),
            expected_repos,
            expected_worktrees,
            socket.display(),
            temp.0.display(),
        );
        daemon
            .terminals
            .input(terminal, command.as_bytes())
            .unwrap();
        for marker in [
            "SESSION_OK",
            "ID_OK",
            "WORKSPACE_OK",
            "REPOS_OK",
            "WORKTREES_OK",
            "SOCKET_OK",
            "CWD_OK",
        ] {
            wait_for_terminal(&daemon.terminals, terminal, marker);
        }

        let again = daemon.handle_request(Request::SpawnTerminal(target.clone()));
        assert!(matches!(
            again.first(),
            Some(Event::TerminalSpawned { terminal: same, target: actual })
                if *same == terminal && actual == &target
        ));
        let listed = daemon.handle_request(Request::ListTerminals);
        let Some(Event::Terminals(rows)) = listed.first() else {
            panic!("expected terminal rows, got {listed:?}")
        };
        assert!(
            rows.iter()
                .any(|row| { row.terminal == terminal && row.target == target })
        );

        let scratch_events =
            daemon.handle_request(Request::SpawnTerminal(TerminalTarget::Scratch {
                cwd: None,
            }));
        let Some(Event::TerminalSpawned {
            terminal: scratch, ..
        }) = scratch_events.first()
        else {
            panic!("expected scratch terminal, got {scratch_events:?}")
        };
        let scratch_echo_disabled = temp.0.join("scratch-echo-disabled");
        daemon
            .terminals
            .input(
                *scratch,
                format!("stty -echo; touch '{}'\r", scratch_echo_disabled.display()).as_bytes(),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !scratch_echo_disabled.exists() {
            assert!(
                Instant::now() < deadline,
                "scratch shell did not disable echo"
            );
            thread::sleep(Duration::from_millis(10));
        }
        daemon
            .terminals
            .input(
                *scratch,
                b"env | grep '^GROVE_' >/dev/null || echo SCRATCH_CLEAN\r",
            )
            .unwrap();
        wait_for_terminal(&daemon.terminals, *scratch, "SCRATCH_CLEAN");

        let ended = daemon.handle_request(Request::EndSession(sid("session-1")));
        assert!(ended.iter().any(
            |event| matches!(event, Event::SessionEnded(session) if session == &sid("session-1"))
        ));
        assert!(matches!(
            daemon.terminals.is_alive(terminal),
            Err(TerminalError::Missing(_))
        ));
        assert!(daemon.terminals.is_alive(*scratch).unwrap());
    }

    #[test]
    fn prune_requests_serve_blockers_and_per_row_outcomes() {
        let temp = TempDir::new();
        let remote = temp.0.join("remote.git");
        let clone = temp.0.join("repo-a");
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
        // An advertised default without a real remote: the base_branch read.
        git(
            &clone,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        git(&clone, &["push", "-qu", "origin", "main"]);
        let done_path = temp.0.join("done");
        let dirty_path = temp.0.join("dirty");
        let mine_path = temp.0.join("mine");
        for (branch, path) in [
            ("done", &done_path),
            ("dirty", &dirty_path),
            ("mine", &mine_path),
        ] {
            git(
                &clone,
                &["worktree", "add", "-qb", branch, path.to_str().unwrap()],
            );
            git(&clone, &["push", "-qu", "origin", branch]);
        }
        // The row that will go dirty between listing and pruning.
        fs::write(dirty_path.join("uncommitted"), "work\n").unwrap();

        let template = temp.0.join("trees/{repo}/{branch_slug}");
        let template = template.to_string_lossy().into_owned();
        let mut store = Store::load_at(&temp.0.join("state"), &temp.0, &template);
        store.create(sid("one"), "one".to_string()).unwrap();
        store
            .add_member(&sid("one"), RepoId("repo-a".into()))
            .unwrap();
        store
            .adopt(
                &sid("one"),
                OwnedWorktree {
                    repo: RepoId("repo-a".into()),
                    branch: "mine".into(),
                },
                &mine_path,
                &clone,
            )
            .unwrap();
        // Live ownership: the open session owns the worktree, which is what
        // prune refuses to take.
        store.open(&sid("one")).unwrap();
        let removal_log = temp.0.join("removal.log");
        let runtime = DaemonRuntime::load_source(
            &format!(
                "local grove = require('grove')\n\
                 grove.setup({{ worktree_path = {template:?} }})\n\
                 grove.on('worktree_removed', function(wt)\n\
                   local file = assert(io.open({log:?}, 'a'))\n\
                   file:write(wt.repo .. ':' .. wt.branch .. ':' .. wt.session .. '\\n')\n\
                   file:close()\n\
                 end)\n",
                template = template,
                log = removal_log.to_string_lossy()
            ),
            "prune-config",
        )
        .runtime;
        let repositories = vec![Repository {
            id: RepoId("repo-a".into()),
            name: "repo-a".into(),
            path: clone.clone(),
            base_branch: Some("origin/main".into()),
        }];
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let mut daemon = SessionOrchestrator::new(
            store,
            repositories,
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        );

        let events = daemon.handle_request(Request::ListPruneCandidates);
        let Some(Event::PruneCandidates(rows)) = events.first() else {
            panic!("expected PruneCandidates, got {events:?}")
        };
        // Non-clone checkouts of the repo, in display order. Every row of a
        // repo whose fetch succeeded can be a candidate.
        assert_eq!(
            rows.iter()
                .map(|row| row.worktree.branch.as_str())
                .collect::<Vec<_>>(),
            ["dirty", "done", "mine"]
        );
        let done = &rows[1];
        assert_eq!(done.state, PruneState::Merged);
        assert!(
            done.blockers.is_empty(),
            "merged + clean + pushed + unowned: pre-checked"
        );
        let dirty = &rows[0];
        assert!(
            dirty
                .blockers
                .iter()
                .any(|blocker| matches!(blocker, PruneBlocker::Dirty { files: 1 }))
        );
        let mine = &rows[2];
        assert!(mine.blockers.iter().any(|blocker| matches!(
            blocker,
            PruneBlocker::Owned { name, .. } if name == "one"
        )));

        // The listing started the background size walks; a later request has
        // drained them, so the row the picker will remove carries a real size.
        let _ = daemon.handle_request(Request::ListPruneCandidates);

        let want = |branch: &str| WorktreeRef {
            repo: RepoId("repo-a".into()),
            branch: branch.into(),
        };
        let events = daemon.handle_request(Request::Prune(vec![
            want("done"),
            want("dirty"),
            want("mine"),
            want("never-existed"),
        ]));
        let Some(Event::Pruned {
            removed,
            failed,
            reclaimed,
        }) = events.first()
        else {
            panic!("expected Pruned, got {events:?}")
        };
        assert_eq!(*removed, vec![want("done")]);
        assert_eq!(failed.len(), 3);
        // The dirty row failed at prune time and is still on disk.
        assert!(
            failed
                .iter()
                .any(|(row, message)| row.branch == "dirty" && message.contains("dirty")),
            "a row that went dirty must fail, got {failed:?}"
        );
        assert!(dirty_path.exists(), "the dirty row was removed");
        // A live session's ownership is not the user's to override here.
        assert!(
            failed
                .iter()
                .any(|(row, message)| row.branch == "mine" && message.contains("owns")),
            "a live session's row must be refused, got {failed:?}"
        );
        assert!(mine_path.exists());
        assert!(
            failed
                .iter()
                .any(|(row, message)| row.branch == "never-existed"
                    && message.contains("does not exist"))
        );
        assert!(!done_path.exists(), "the safe row was removed");
        assert!(
            *reclaimed > 0,
            "reclaimed bytes come from the drained size walk"
        );

        // A closed session still owns its rows in the store. The picker is
        // the second chance to prune them — and the only prune path that
        // forgets a claim and fires worktree_removed, so it is pinned: the
        // row is removed, the ownership is gone, and the hook carries the
        // closed session's payload.
        let _ = daemon.handle_request(Request::CloseSession(sid("one")));
        let events = daemon.handle_request(Request::Prune(vec![want("mine")]));
        let Some(Event::Pruned {
            removed, failed, ..
        }) = events.first()
        else {
            panic!("expected Pruned, got {events:?}")
        };
        assert_eq!(*removed, vec![want("mine")]);
        assert!(failed.is_empty(), "{failed:?}");
        assert!(!mine_path.exists(), "the closed session's row was removed");
        assert_eq!(
            daemon.store().owner(&OwnedWorktree {
                repo: RepoId("repo-a".into()),
                branch: "mine".into(),
            }),
            None,
            "the claim must be forgotten, not left naming a removed checkout"
        );
        assert_eq!(
            fs::read_to_string(&removal_log).unwrap(),
            "repo-a:mine:one\n"
        );
    }

    #[test]
    fn clone_is_derived_and_refused() {
        let temp = TempDir::new();
        let clone = temp.0.join("repo");
        let ours_path = temp.0.join("ours");
        let other_path = temp.0.join("other");
        let spare_path = temp.0.join("spare");
        fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "Grove Test"]);
        git(&clone, &["config", "user.email", "grove@example.test"]);
        fs::write(clone.join("tracked"), "base\n").unwrap();
        git(&clone, &["add", "tracked"]);
        git(&clone, &["commit", "-qm", "base"]);
        for (branch, path) in [
            ("ours", &ours_path),
            ("other", &other_path),
            ("spare", &spare_path),
        ] {
            git(
                &clone,
                &["worktree", "add", "-qb", branch, path.to_str().unwrap()],
            );
        }

        let template = temp.0.join("trees/{repo}/{branch_slug}");
        let template = template.to_string_lossy().into_owned();
        let mut store = Store::load_at(&temp.0.join("state"), &temp.0, &template);
        store.create(sid("one"), "one".to_string()).unwrap();
        store.create(sid("two"), "two".to_string()).unwrap();
        store
            .add_member(&sid("one"), RepoId("repo".into()))
            .unwrap();
        store
            .add_member(&sid("two"), RepoId("repo".into()))
            .unwrap();
        store
            .adopt(&sid("one"), owned("ours"), &ours_path, &clone)
            .unwrap();
        store
            .adopt(&sid("two"), owned("other"), &other_path, &clone)
            .unwrap();
        store.open(&sid("one")).unwrap();
        let repository = Repository {
            id: RepoId("repo".into()),
            name: "repo".into(),
            path: clone.clone(),
            base_branch: Some("main".into()),
        };
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let runtime = DaemonRuntime::load_source(
            &format!(
                "local grove = require('grove'); grove.setup({{ worktree_path = {template:?} }})",
                template = template
            ),
            "ownership-config",
        )
        .runtime;
        let mut daemon = SessionOrchestrator::new(
            store,
            vec![repository.clone()],
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        );

        // Ownership is derived at the trust boundary: the clone by path
        // comparison, the rest by the store's records.
        let checkouts = grove_git::worktrees(&clone).unwrap();
        let ownership = |branch: &str| {
            let checkout = checkouts
                .iter()
                .find(|row| row.branch.as_deref() == Some(branch))
                .unwrap();
            daemon.ownership(&repository, checkout)
        };
        assert_eq!(ownership("main"), Ownership::Clone);
        assert_eq!(ownership("ours"), Ownership::Ours);
        assert_eq!(ownership("other"), Ownership::Other(sid("two")));
        assert_eq!(ownership("spare"), Ownership::Unowned);

        // Adopting the clone — whose branch is checked out in the main
        // checkout — is refused with a reason, not reported as missing.
        let error = daemon.adopt(&sid("one"), owned("main")).unwrap_err();
        assert!(
            matches!(error, OrchestrationError::CloneNotAdoptable(_)),
            "{error}"
        );
        assert_eq!(daemon.store().owner(&owned("main")), None);

        // A worktree template resolving to the repository path is refused
        // before git ever sees it.
        let runtime = DaemonRuntime::load_source(
            &format!(
                "local grove = require('grove'); grove.setup({{ worktree_path = {clone:?} }})",
                clone = clone.to_string_lossy()
            ),
            "clone-path-config",
        )
        .runtime;
        let mut daemon = SessionOrchestrator::new(
            Store::load_at(&temp.0.join("state"), &temp.0, &template),
            vec![repository],
            temp.0.clone(),
            TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100),
            FetchPolicy::new(2, Duration::from_secs(60)),
            runtime,
        );
        let failure = daemon
            .create_worktrees(&sid("one"), "newbranch", &[RepoId("repo".into())])
            .unwrap()
            .failed
            .remove(0);
        assert!(
            failure.message.contains("own checkout"),
            "expected a clone-path refusal, got {failure:?}"
        );
        assert!(clone.is_dir(), "the clone was never touched");
    }

    #[test]
    fn slug_collision_refuses_rather_than_reusing_the_first_checkout() {
        let temp = TempDir::new();
        let clone = temp.0.join("repo");
        let colliding_path = temp.0.join("trees/repo/feat-x");
        fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "Grove Test"]);
        git(&clone, &["config", "user.email", "grove@example.test"]);
        fs::write(clone.join("tracked"), "base\n").unwrap();
        git(&clone, &["add", "tracked"]);
        git(&clone, &["commit", "-qm", "base"]);
        fs::create_dir_all(colliding_path.parent().unwrap()).unwrap();
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-qb",
                "feat-x",
                colliding_path.to_str().unwrap(),
            ],
        );

        let template = temp.0.join("trees/{repo}/{branch_slug}");
        let template = template.to_string_lossy().into_owned();
        let mut store = Store::load_at(&temp.0.join("state"), &temp.0, &template);
        store.create(sid("one"), "one".to_string()).unwrap();
        store
            .add_member(&sid("one"), RepoId("repo".into()))
            .unwrap();
        let repository = Repository {
            id: RepoId("repo".into()),
            name: "repo".into(),
            path: clone.clone(),
            base_branch: Some("main".into()),
        };
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let runtime = DaemonRuntime::load_source(
            &format!(
                "local grove = require('grove'); grove.setup({{ worktree_path = {template:?} }})",
                template = template
            ),
            "slug-config",
        )
        .runtime;
        let mut daemon = SessionOrchestrator::new(
            store,
            vec![repository],
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        );

        // branch_slug("feat/x") and branch_slug("feat-x") are both "feat-x":
        // the second branch must not silently take the first's checkout.
        let failure = daemon
            .create_worktrees(&sid("one"), "feat/x", &[RepoId("repo".into())])
            .unwrap()
            .failed
            .remove(0);
        assert!(
            failure.message.contains("feat-x") && failure.message.contains("claimed"),
            "expected a claim refusal naming the branch, got {failure:?}"
        );
        assert!(
            grove_git::worktrees(&clone)
                .unwrap()
                .iter()
                .any(|row| row.branch.as_deref() == Some("feat-x")),
            "the first checkout was left untouched"
        );
        assert_eq!(daemon.store().owner(&owned("feat/x")), None);
    }

    #[test]
    fn end_session_never_removes_the_repository_path() {
        let temp = TempDir::new();
        let clone = temp.0.join("repo");
        fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "Grove Test"]);
        git(&clone, &["config", "user.email", "grove@example.test"]);
        fs::write(clone.join("tracked"), "base\n").unwrap();
        git(&clone, &["add", "tracked"]);
        git(&clone, &["commit", "-qm", "base"]);

        // Hand-crafted state as a daemon predating the adopt guard could have
        // written it: the clone claimed as session-owned. The destructive path
        // must refuse it rather than trust the file.
        let state_home = temp.0.join("state");
        let state_dir = state_home
            .join("grove")
            .join(grove_state::workspace_hash(&temp.0));
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("sessions.json"),
            r#"{"version":1,"sessions":[{"id":"legacy","name":"legacy",
                 "members":["repo"],"owned":[{"repo":"repo","branch":"main"}],
                 "state":"Closed"}],"open":null}"#,
        )
        .unwrap();

        let template = temp.0.join("trees/{repo}/{branch_slug}");
        let template = template.to_string_lossy().into_owned();
        let repository = Repository {
            id: RepoId("repo".into()),
            name: "repo".into(),
            path: clone.clone(),
            base_branch: Some("main".into()),
        };
        let store = Store::load_at(&state_home, &temp.0, &template);
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let runtime = DaemonRuntime::load_source(
            &format!(
                "local grove = require('grove'); grove.setup({{ worktree_path = {template:?} }})",
                template = template
            ),
            "legacy-config",
        )
        .runtime;
        let mut daemon = SessionOrchestrator::new(
            store,
            vec![repository],
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        );

        let report = daemon.end_confirmed(&sid("legacy")).unwrap();
        assert!(!report.ended);
        assert!(
            report
                .failed
                .iter()
                .any(|failure| failure.message.contains("must not remove")),
            "expected a refusal to remove the clone, got {report:?}"
        );
        assert!(clone.is_dir(), "end session removed the repository path");
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
        let mut daemon = SessionOrchestrator::new(
            store,
            vec![repository],
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        );

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

#[cfg(test)]
mod editor_tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("groved-editor-{label}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            Self(temp_dir("case"))
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

    fn runtime_with(editor: &str) -> DaemonRuntime {
        DaemonRuntime::load_source(
            &format!("local grove = require('grove'); grove.setup({{ editor = {editor:?} }})"),
            "editor-config",
        )
        .runtime
    }

    /// A checkout plus a recorder script: the "editor" appends its arguments,
    /// one per line, to a fixed log, so the test asserts what the editor
    /// actually received rather than trusting the configuration.
    fn orchestrator(
        editor: &str,
        branch: &str,
        spaced: bool,
    ) -> (SessionOrchestrator, TempDir, PathBuf) {
        let temp = TempDir::new();
        let clone = temp.0.join("repo");
        fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "Grove Test"]);
        git(&clone, &["config", "user.email", "grove@example.test"]);
        fs::write(clone.join("tracked"), "base\n").unwrap();
        git(&clone, &["add", "tracked"]);
        git(&clone, &["commit", "-qm", "base"]);
        let worktree = if spaced {
            temp.0.join("a b repo")
        } else {
            temp.0.join("plain")
        };
        git(
            &clone,
            &["worktree", "add", "-qb", branch, worktree.to_str().unwrap()],
        );
        let store = Store::load_at(&temp.0.join("state"), &temp.0, "");
        let terminals = TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let daemon = SessionOrchestrator::new(
            store,
            vec![Repository {
                id: RepoId("repo".into()),
                name: "repo".into(),
                path: clone.clone(),
                base_branch: None,
            }],
            temp.0.clone(),
            terminals,
            fetch,
            runtime_with(editor),
        );
        (daemon, temp, worktree)
    }

    fn open(daemon: &mut SessionOrchestrator, branch: &str) -> Vec<Event> {
        daemon.handle_request(Request::OpenEditor(WorktreeRef {
            repo: RepoId("repo".into()),
            branch: branch.into(),
        }))
    }

    fn recorder(temp: &Path, log: &Path) -> String {
        let script = temp.join("record.sh");
        let log = log.to_string_lossy().replace(' ', "\\ ");
        use std::os::unix::fs::PermissionsExt;
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{log}'\n"),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script.to_string_lossy().into_owned()
    }

    fn logged(log: &Path) -> String {
        // The child races the test by a scheduler tick; the wait thread is
        // the reaper, and the log exists once the editor has run.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(contents) = fs::read_to_string(log)
                && !contents.is_empty()
            {
                return contents;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the editor was never invoked"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_editor_receives_the_path_where_the_placeholder_sits() {
        let temp = temp_dir("placeholder");
        let log = temp.join("argv.log");
        let editor = format!("{} {{path}}", recorder(&temp, &log));
        let (mut daemon, _temp, worktree) = orchestrator(&editor, "spaced", true);
        assert!(open(&mut daemon, "spaced").is_empty(), "spawn is silent");
        let logged = logged(&log);
        // The path — spaces and all — was exactly one argument.
        assert_eq!(logged.lines().next().unwrap(), worktree.to_string_lossy());
    }

    #[test]
    fn an_editor_without_a_placeholder_gets_the_path_last() {
        let temp = temp_dir("no-placeholder");
        let log = temp.join("argv.log");
        let editor = recorder(&temp, &log);
        let (mut daemon, _temp, worktree) = orchestrator(&editor, "plain", false);
        assert!(open(&mut daemon, "plain").is_empty());
        assert_eq!(
            logged(&log).lines().last().unwrap(),
            worktree.to_string_lossy()
        );
    }

    #[test]
    fn an_editor_that_exits_immediately_affects_nothing() {
        let (mut daemon, _temp, _worktree) = orchestrator("/bin/true", "plain", false);
        assert!(open(&mut daemon, "plain").is_empty());
        // The session surface still answers after a fast exit.
        let events = daemon.handle_request(Request::ListSessions);
        assert!(matches!(events.first(), Some(Event::Sessions(_))));
    }

    #[test]
    fn a_missing_editor_program_reports_instead_of_killing_the_loop() {
        let (mut daemon, _temp, _worktree) =
            orchestrator("/nonexistent-editor-xyz", "plain", false);
        let events = open(&mut daemon, "plain");
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Failed { message, .. } if message.contains("could not launch")
        )));
        // The loop is alive: the same client gets answers afterwards.
        assert!(matches!(
            open(&mut daemon, "plain").first(),
            Some(Event::Failed { .. })
        ));
    }

    #[test]
    fn no_editor_anywhere_is_reported_something_a_user_can_act_on() {
        let (mut daemon, _temp, _worktree) = orchestrator("", "plain", false);
        let events = open(&mut daemon, "plain");
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Failed { message, .. }
                if message.contains("config.lua") && message.contains("$EDITOR")
        )));
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("groved-capability-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn the_handshake_capability_follows_the_path_template() {
        let temp = temp_dir();
        let movable = Store::load_at(&temp.join("state"), &temp, "trees/{repo}");
        let store_session_scoped = Store::load_at(
            &temp.join("state2"),
            &temp,
            "{session}/{repo}/{branch_slug}",
        );
        let fetch = FetchPolicy::new(2, Duration::from_secs(60));
        let runtime = DaemonRuntime::load_source(
            "local grove = require('grove'); grove.setup({})",
            "capability-config",
        )
        .runtime;
        let movable = SessionOrchestrator::new(
            movable,
            Vec::new(),
            temp.clone(),
            TerminalManager::new(PathBuf::from("/bin/sh"), temp.clone(), 100),
            fetch,
            runtime,
        );
        let runtime = DaemonRuntime::load_source(
            "local grove = require('grove'); grove.setup({})",
            "capability-config-2",
        )
        .runtime;
        let session_scoped = SessionOrchestrator::new(
            store_session_scoped,
            Vec::new(),
            temp.clone(),
            TerminalManager::new(PathBuf::from("/bin/sh"), temp.clone(), 100),
            FetchPolicy::new(2, Duration::from_secs(60)),
            runtime,
        );
        assert!(movable.ownership_movable());
        // §2.4: a worktree's location then depends on the session that made
        // it, so ownership cannot change hands — and the client learns that
        // from the handshake instead of from a refused key.
        assert!(!session_scoped.ownership_movable());
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("groved-snapshot-{label}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
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

    fn owned(branch: &str) -> OwnedWorktree {
        OwnedWorktree {
            repo: RepoId("repo".into()),
            branch: branch.into(),
        }
    }

    #[test]
    fn snapshot_save_and_restore_cover_the_manual_flow() {
        let temp = temp_dir("flow");
        let clone = temp.join("repo");
        fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "Grove Test"]);
        git(&clone, &["config", "user.email", "grove@example.test"]);
        fs::write(clone.join("tracked"), "base\n").unwrap();
        git(&clone, &["add", "tracked"]);
        git(&clone, &["commit", "-qm", "base"]);
        let gone_path = temp.join("gone");
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-qb",
                "gone",
                gone_path.to_str().unwrap(),
            ],
        );
        let kept_path = temp.join("kept");
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-qb",
                "kept",
                kept_path.to_str().unwrap(),
            ],
        );

        // A plain template: the snapshot flow needs ownership to move, and
        // the {session} variant would refuse it (§2.4).
        let template = "trees/{repo}/{branch_slug}";
        let mut store = Store::load_at(&temp.join("state"), &temp, template);
        let saved_at = store.snapshot_path(&SessionId("snap".into()));
        let session = SessionId("snap".into());
        store.create(session.clone(), "snap".to_string()).unwrap();
        store.add_member(&session, RepoId("repo".into())).unwrap();
        store
            .adopt(&session, owned("gone"), &gone_path, &clone)
            .unwrap();
        store
            .adopt(&session, owned("kept"), &kept_path, &clone)
            .unwrap();
        // The removable worktree goes away after the save, so the restore
        // must report it and still restore the rest.
        let mut daemon = SessionOrchestrator::new(
            store,
            vec![Repository {
                id: RepoId("repo".into()),
                name: "repo".into(),
                path: clone.clone(),
                base_branch: None,
            }],
            temp.clone(),
            TerminalManager::new(PathBuf::from("/bin/sh"), temp.clone(), 100),
            FetchPolicy::new(2, Duration::from_secs(60)),
            DaemonRuntime::load_source(
                "local grove = require('grove'); grove.setup({})",
                "snapshot-config",
            )
            .runtime,
        );
        let opened = daemon.open(&session).unwrap();
        assert_eq!(opened.terminals.len(), 2);
        assert!(!saved_at.exists(), "nothing writes a snapshot on its own");
        let events = daemon.handle_request(Request::SaveSnapshot(session.clone()));
        assert!(saved_at.exists(), "only an explicit SaveSnapshot writes");
        assert!(matches!(events.first(), Some(Event::Sessions(_))));

        // A session with no snapshot at all: the typed report, not a panic,
        // and not a ghost of a restore.
        fs::remove_file(&saved_at).unwrap();
        let events = daemon.handle_request(Request::RestoreSnapshot(session.clone()));
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::Failed { message, .. } if message.contains("no snapshot exists")
            )),
            "a missing snapshot must be reported: {events:?}"
        );

        // End the session: its worktrees go, its live terminals die — then
        // re-create the session and save again, break one worktree, restore.
        let events = daemon.handle_request(Request::EndSession(session.clone()));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::SessionEnded(_)))
        );
        assert!(!gone_path.exists());
        assert!(!kept_path.exists());

        // Re-create the session and its worktrees so the save has content.
        // The first orchestrator consumed the store; load what persisted.
        let mut store = Store::load_at(&temp.join("state"), &temp, template);
        let recreated_gone = temp.join("gone2");
        // The branch survives the end; the worktree is re-cut from it.
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-q",
                recreated_gone.to_str().unwrap(),
                "gone",
            ],
        );
        fs::create_dir_all(&kept_path).unwrap();
        git(
            &clone,
            &["worktree", "add", "-q", kept_path.to_str().unwrap(), "kept"],
        );
        store.create(session.clone(), "snap2".to_string()).unwrap();
        store.add_member(&session, RepoId("repo".into())).unwrap();
        store
            .adopt(&session, owned("gone"), &recreated_gone, &clone)
            .unwrap();
        store
            .adopt(&session, owned("kept"), &kept_path, &clone)
            .unwrap();
        let mut daemon = SessionOrchestrator::new(
            store,
            vec![Repository {
                id: RepoId("repo".into()),
                name: "repo".into(),
                path: clone.clone(),
                base_branch: None,
            }],
            temp.clone(),
            TerminalManager::new(PathBuf::from("/bin/sh"), temp.clone(), 100),
            FetchPolicy::new(2, Duration::from_secs(60)),
            DaemonRuntime::load_source(
                "local grove = require('grove'); grove.setup({})",
                "snapshot-config-2",
            )
            .runtime,
        );
        let opened = daemon.open(&session).unwrap();
        assert_eq!(opened.terminals.len(), 2);
        let events = daemon.handle_request(Request::SaveSnapshot(session.clone()));
        assert!(matches!(events.first(), Some(Event::Sessions(_))));
        assert!(saved_at.exists());

        // One recorded worktree disappears before the restore.
        fs::remove_dir_all(&recreated_gone).unwrap();
        let events = daemon.handle_request(Request::RestoreSnapshot(session.clone()));
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::Failed { context, message }
                    if context == "restore snapshot"
                        && message.contains("no longer exists")
                        && message.contains("gone")
            )),
            "the missing worktree must be reported: {events:?}"
        );
        // The rest restored: one terminal at the kept checkout, tracked by
        // the session.
        let Some(Event::Sessions(rows)) = events
            .iter()
            .find(|event| matches!(event, Event::Sessions(_)))
        else {
            panic!("expected Sessions")
        };
        let row = rows.iter().find(|row| row.id == session).unwrap();
        assert_eq!(row.terminals, 2, "kept restored; gone was reported");
        assert_eq!(
            daemon.store().owner(&owned("kept")),
            Some(&session),
            "restore must not disturb ownership"
        );

        // Restoring twice does not duplicate terminals: the second run
        // reports the already-covered checkouts instead of spawning twins.
        let events = daemon.handle_request(Request::RestoreSnapshot(session.clone()));
        let Some(Event::Sessions(rows)) = events
            .iter()
            .find(|event| matches!(event, Event::Sessions(_)))
        else {
            panic!("expected Sessions")
        };
        let row = rows.iter().find(|row| row.id == session).unwrap();
        assert_eq!(
            row.terminals, 2,
            "no duplicate terminals on the second restore"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::Failed { context, message }
                    if context == "restore snapshot" && message.contains("already exists")
            )),
            "the second restore reports the covered rows: {events:?}"
        );
    }

    #[test]
    fn restoring_into_the_reboot_case_spawns_joins_and_detaches() {
        // §6's reboot case: the daemon restarted, so every session is closed,
        // no pty survived, and the grouping survived. A restore must bring
        // the recorded terminals back as live ones — spawned, joined to the
        // session, and the state moved off Closed — without anyone having
        // opened first.
        let temp = temp_dir("reboot");
        let clone = temp.join("repo");
        fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "Grove Test"]);
        git(&clone, &["config", "user.email", "grove@example.test"]);
        fs::write(clone.join("tracked"), "base\n").unwrap();
        git(&clone, &["add", "tracked"]);
        git(&clone, &["commit", "-qm", "base"]);
        let kept_path = temp.join("kept");
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-qb",
                "kept",
                kept_path.to_str().unwrap(),
            ],
        );

        let template = "trees/{repo}/{branch_slug}";
        let mut store = Store::load_at(&temp.join("state"), &temp, template);
        let session = SessionId("reboot".into());
        store.create(session.clone(), "reboot".to_string()).unwrap();
        store.add_member(&session, RepoId("repo".into())).unwrap();
        store
            .adopt(&session, owned("kept"), &kept_path, &clone)
            .unwrap();
        let spawn_log = temp.join("spawns.log");
        let mut daemon = SessionOrchestrator::new(
            store,
            vec![Repository {
                id: RepoId("repo".into()),
                name: "repo".into(),
                path: clone.clone(),
                base_branch: None,
            }],
            temp.clone(),
            TerminalManager::new(PathBuf::from("/bin/sh"), temp.clone(), 100),
            FetchPolicy::new(2, Duration::from_secs(60)),
            DaemonRuntime::load_source(
                &format!(
                    "local grove = require('grove')\n\
                     grove.on('terminal_spawned', function(t)\n\
                       local file = assert(io.open({log:?}, 'a'))\n\
                       file:write(t.repo .. ':' .. t.branch .. '\\n')\n\
                       file:close()\n\
                     end)",
                    log = spawn_log.to_string_lossy()
                ),
                "reboot-config",
            )
            .runtime,
        );
        let opened = daemon.open(&session).unwrap();
        assert_eq!(opened.terminals.len(), 1);
        // The save's answer is the picker rows; the empty check belongs to
        // the spawn flow, where nothing else can be riding along.
        assert!(matches!(
            daemon
                .handle_request(Request::SaveSnapshot(session.clone()))
                .first(),
            Some(Event::Sessions(_))
        ));

        // The reboot: a fresh orchestrator over the persisted state. The
        // store's loader closed every session and dropped every pty.
        let mut rebooted = SessionOrchestrator::new(
            Store::load_at(&temp.join("state"), &temp, template),
            vec![Repository {
                id: RepoId("repo".into()),
                name: "repo".into(),
                path: clone.clone(),
                base_branch: None,
            }],
            temp.clone(),
            TerminalManager::new(PathBuf::from("/bin/sh"), temp.clone(), 100),
            FetchPolicy::new(2, Duration::from_secs(60)),
            DaemonRuntime::load_source(
                "local grove = require('grove'); grove.setup({})",
                "reboot-config-2",
            )
            .runtime,
        );
        // Nothing is live: the reboot's session is closed, as §6 promises.
        let events = rebooted.handle_request(Request::ListSessions);
        let Some(Event::Sessions(rows)) = events.first() else {
            panic!("expected Sessions")
        };
        let row = rows.iter().find(|row| row.id == session).unwrap();
        assert_eq!(row.state, SessionState::Closed);
        assert_eq!(row.terminals, 0);

        let events = rebooted.handle_request(Request::RestoreSnapshot(session.clone()));
        let Some(Event::Sessions(rows)) = events
            .iter()
            .find(|event| matches!(event, Event::Sessions(_)))
        else {
            panic!("expected Sessions")
        };
        let row = rows.iter().find(|row| row.id == session).unwrap();
        // Restored, not opened: the terminal count comes from the restore.
        assert_eq!(
            row.terminals, 1,
            "the restore spawned the recorded terminal"
        );
        assert_eq!(
            row.state,
            SessionState::Detached,
            "live terminals contradict Closed (§2.3)"
        );
        // The spawn hook saw it, with the (repo, branch) identity.
        assert_eq!(fs::read_to_string(&spawn_log).unwrap(), "repo:kept\n");
        // The join is real: closing the session kills what the restore made.
        let closed = rebooted.close(&session).unwrap();
        assert_eq!(
            closed.killed.len(),
            1,
            "close kills what the restore joined"
        );
        // A killed terminal is gone, not merely dead.
        assert!(matches!(
            rebooted.terminals().is_alive(closed.killed[0]),
            Err(TerminalError::Missing(_))
        ));
    }
}
