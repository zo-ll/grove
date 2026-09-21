use crate::session::SessionOrchestrator;
use grove_domain::RepoId;
use grove_git::GitWatchPaths;
use grove_proto::Event;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Clone, Default)]
pub(crate) struct Broadcaster {
    clients: Arc<Mutex<Vec<mpsc::SyncSender<Event>>>>,
}

impl Broadcaster {
    pub(crate) fn subscribe(&self) -> mpsc::Receiver<Event> {
        let (sender, receiver) = mpsc::sync_channel(16);
        if let Ok(mut clients) = self.clients.lock() {
            clients.push(sender);
        }
        receiver
    }

    fn send(&self, event: Event) {
        if let Ok(mut clients) = self.clients.lock() {
            clients.retain(|client| match client.try_send(event.clone()) {
                Ok(()) | Err(mpsc::TrySendError::Full(_)) => true,
                Err(mpsc::TrySendError::Disconnected(_)) => false,
            });
        }
    }
}

pub(crate) struct RepoWatch {
    pub(crate) repo: RepoId,
    pub(crate) paths: Vec<GitWatchPaths>,
}

#[derive(Default)]
pub(crate) struct WatchPlan {
    pub(crate) repos: Vec<RepoWatch>,
}

impl WatchPlan {
    fn affected(&self, paths: &[PathBuf]) -> HashSet<RepoId> {
        self.repos
            .iter()
            .filter(|repo| {
                paths
                    .iter()
                    .any(|event_path| repo.paths.iter().any(|watch| relevant(watch, event_path)))
            })
            .map(|repo| repo.repo.clone())
            .collect()
    }

    fn roots(&self) -> HashMap<PathBuf, RecursiveMode> {
        let mut roots = HashMap::new();
        for repo in &self.repos {
            for paths in &repo.paths {
                roots.insert(paths.worktree.clone(), RecursiveMode::Recursive);
                roots.insert(paths.git_dir.clone(), RecursiveMode::NonRecursive);
                roots.insert(paths.common_dir.clone(), RecursiveMode::NonRecursive);
                let refs = paths.common_dir.join("refs");
                if refs.exists() {
                    roots.insert(refs, RecursiveMode::Recursive);
                }
                let linked_worktrees = paths.common_dir.join("worktrees");
                if linked_worktrees.exists() {
                    roots.insert(linked_worktrees, RecursiveMode::Recursive);
                }
            }
        }
        roots
    }
}

fn relevant(watch: &GitWatchPaths, path: &Path) -> bool {
    if path.starts_with(&watch.worktree) {
        let dot_git = watch.worktree.join(".git");
        if path != dot_git && !path.starts_with(&watch.git_dir) {
            return true;
        }
    }

    is_git_file(path, &watch.git_dir.join("HEAD"))
        || is_git_file(path, &watch.git_dir.join("index"))
        || path.starts_with(watch.common_dir.join("refs"))
        || path.starts_with(watch.common_dir.join("worktrees"))
        || is_git_file(path, &watch.common_dir.join("packed-refs"))
}

fn is_git_file(path: &Path, target: &Path) -> bool {
    path == target || path == target.with_extension("lock")
}

pub(crate) struct WorktreeWatcher {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl WorktreeWatcher {
    pub(crate) fn start(
        service: Arc<Mutex<SessionOrchestrator>>,
        updates: Broadcaster,
    ) -> notify::Result<Self> {
        let (raw_sender, raw_receiver) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = raw_sender.send(event);
        })?;
        let mut plan = service
            .lock()
            .map(|service| service.watch_plan())
            .unwrap_or_default();
        let mut roots = HashMap::new();
        reconcile(&mut watcher, &mut roots, &plan)?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut watcher = watcher;
            let mut pending = HashSet::new();
            let mut deadline = None;
            while !stop_worker.load(Ordering::Acquire) {
                let timeout = deadline
                    .map(|at: Instant| at.saturating_duration_since(Instant::now()))
                    .unwrap_or(Duration::from_millis(100));
                match raw_receiver.recv_timeout(timeout) {
                    Ok(Ok(event)) if !matches!(event.kind, EventKind::Access(_)) => {
                        pending.extend(plan.affected(&event.paths));
                        if !pending.is_empty() {
                            deadline = Some(Instant::now() + DEBOUNCE);
                        }
                    }
                    Ok(Ok(_)) | Ok(Err(_)) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if deadline.is_some_and(|at| Instant::now() >= at) {
                            if let Ok(mut service) = service.lock() {
                                for repo in pending.drain() {
                                    if let Ok(event) = service.worktrees_event(&repo) {
                                        updates.send(event);
                                    }
                                }
                                plan = service.watch_plan();
                                let _ = reconcile(&mut watcher, &mut roots, &plan);
                            }
                            deadline = None;
                        }
                    }
                }
            }
        });
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for WorktreeWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn reconcile(
    watcher: &mut RecommendedWatcher,
    current: &mut HashMap<PathBuf, RecursiveMode>,
    plan: &WatchPlan,
) -> notify::Result<()> {
    let wanted = plan.roots();
    for path in current
        .keys()
        .filter(|path| !wanted.contains_key(*path))
        .cloned()
        .collect::<Vec<_>>()
    {
        watcher.unwatch(&path)?;
        current.remove(&path);
    }
    for (path, mode) in &wanted {
        if current.get(path) != Some(mode) {
            if current.contains_key(path) {
                watcher.unwatch(path)?;
            }
            watcher.watch(path, *mode)?;
            current.insert(path.clone(), *mode);
        }
    }
    Ok(())
}
