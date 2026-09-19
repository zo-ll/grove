//! Session transitions that coordinate persistent state, Git and PTYs.

use crate::fetch::{FetchPolicy, FetchResult, FetchStatus, RefFreshness};
use crate::terminal::{TerminalError, TerminalKey, TerminalManager};
use grove_domain::{Ownership, RepoId, SessionId, SessionState};
use grove_git::{RemoveOptions, Repository, SizeTask, Tracking, Workspace};
use grove_lua::{DaemonRuntime, HookReport, LifecycleEvent, LifecyclePayload, WorktreePathContext};
use grove_proto::{
    Attach, Event, RepoRow, Request, ScrollbackRequest, SessionRow, TerminalId, TerminalRow,
    TerminalTarget, WorktreeRef, WorktreeRow,
};
use grove_state::{OwnedWorktree, Store};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
/// Lines per scrollback chunk. One chunk per line would flood the wire with
/// headers; all history in one chunk would pay a 10,000-line buffer at once.
const SCROLLBACK_CHUNK: usize = 200;
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
    terminals: TerminalManager,
    fetch: FetchPolicy,
    runtime: DaemonRuntime,
    live: HashMap<SessionId, Vec<LiveTerminal>>,
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
            terminals,
            fetch,
            runtime,
            live: HashMap::new(),
            hook_reports: Vec::new(),
            notified_exits: HashSet::new(),
            unknown_overrides,
            pending_sizes: HashMap::new(),
            known_sizes: HashMap::new(),
            failed_sizes: HashSet::new(),
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
        self.drain_sizes();
        let mut events = self.drain_unknown_overrides();
        let context = format!("{request:?}");
        events.extend(match self.apply_request(request) {
            Ok(events) => events,
            Err(error) => vec![Event::Failed {
                context,
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
            let terminal = self.live_worktree_terminal(&repository.id, checkout.branch.as_deref());
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

    /// Attaches to a terminal: resize to the pane's size first, then screen,
    /// then backfill chunks, and the live output stream the socket layer
    /// forwards from now on.
    ///
    /// The ordering is the protocol's reassembly contract: the screen arrives
    /// first and carries exactly the visible grid; history follows as chunks
    /// strictly older than it; live output may interleave the moment the
    /// screen is sent — it is never withheld for backfill, and a client that
    /// has painted must not go stale while history loads.
    pub fn attach_terminal(
        &mut self,
        attach: Attach,
    ) -> Result<(Vec<Event>, mpsc::Receiver<Vec<u8>>), OrchestrationError> {
        // The pane's size at attach time: resizing before the screen means the
        // client never paints a wrongly-sized grid, and vt100 reflows.
        self.terminals
            .resize(attach.terminal, attach.rows, attach.cols)?;
        let attachment = self.terminals.attach(attach.terminal)?;
        let snapshot = &attachment.snapshot;
        let mut events = vec![Event::TerminalScreen {
            terminal: attach.terminal,
            screen: snapshot.screen.clone(),
        }];
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
        let mut sent = 0_usize;
        let total = lines.len();
        loop {
            let end = (sent + SCROLLBACK_CHUNK).min(total);
            let seq = u32::try_from(sent / SCROLLBACK_CHUNK).unwrap_or(u32::MAX);
            events.push(Event::TerminalScrollback {
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
        Ok((events, attachment.output))
    }

    /// The scratch shell spawns with no worktree (§4.5); a worktree target
    /// spawns one terminal for that checkout. A worktree owned by a session
    /// joins that session's live set, so closing the session kills it.
    fn spawn_terminal(
        &mut self,
        target: &TerminalTarget,
    ) -> Result<TerminalId, OrchestrationError> {
        match target {
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
        let mut rows = Vec::new();
        for (id, key, alive) in self.terminals.list() {
            if !alive {
                continue;
            }
            let target = match &key {
                TerminalKey::Scratch => TerminalTarget::Scratch { cwd: None },
                TerminalKey::Worktree(path) => match self.worktree_target(path) {
                    Some(target) => target,
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

    /// Names a checkout path on the wire, or `None` when git no longer knows
    /// the checkout (removed while its terminal was alive).
    fn worktree_target(&self, path: &Path) -> Option<TerminalTarget> {
        for repository in self.repositories.values() {
            let Ok(checkouts) = grove_git::worktrees(&repository.path) else {
                // A repo git cannot answer for contributes no names; the
                // same degradation the pane rows apply.
                continue;
            };
            if let Some(checkout) = checkouts
                .into_iter()
                .find(|checkout| checkout.path == *path)
            {
                return Some(TerminalTarget::Worktree(WorktreeRef {
                    repo: repository.id.clone(),
                    branch: checkout.branch.unwrap_or_default(),
                }));
            }
        }
        None
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
                // The override wins for its repo; otherwise git answers with
                // the branch origin advertises as the default.
                let base = self
                    .runtime
                    .repo_override(&repository.name)
                    .and_then(|rule| rule.base.clone())
                    .or_else(|| repository.base_branch.clone())
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
    fn live_worktree_terminal(&self, repo: &RepoId, branch: Option<&str>) -> Option<TerminalId> {
        let branch = branch?;
        self.live.values().flatten().find_map(|terminal| {
            (terminal.worktree.repo == *repo
                && terminal.worktree.branch == branch
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
fn expand_home(path: &Path) -> PathBuf {
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
        let (events, output) = daemon
            .attach_terminal(Attach {
                terminal: id,
                scrollback: ScrollbackRequest::All,
                rows: 10,
                cols: 40,
            })
            .unwrap();
        assert!(matches!(events[0], Event::TerminalScreen { .. }));
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
        let (events, _) = daemon
            .attach_terminal(Attach {
                terminal: id,
                scrollback: ScrollbackRequest::Lines(5),
                rows: 12,
                cols: 50,
            })
            .unwrap();
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
        // The shell itself is the foreground process of a fresh pane.
        assert!(worktree_row.foreground.is_some());

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
