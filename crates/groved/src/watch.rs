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
const MAX_DEBOUNCE: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Debounce {
    first: Option<Instant>,
    deadline: Option<Instant>,
}

impl Debounce {
    fn observe(&mut self, now: Instant) {
        let first = *self.first.get_or_insert(now);
        self.deadline = Some((now + DEBOUNCE).min(first + MAX_DEBOUNCE));
    }

    fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    fn clear(&mut self) {
        self.first = None;
        self.deadline = None;
    }
}

pub(crate) fn tolerate_start<T>(result: notify::Result<T>) -> Option<T> {
    match result {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            eprintln!("groved: worktree watcher disabled: {error}");
            None
        }
    }
}

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

    pub(crate) fn send(&self, event: Event) {
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
                for directory in &paths.worktree_dirs {
                    roots.insert(directory.clone(), RecursiveMode::NonRecursive);
                }
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
        reconcile(&mut watcher, &mut roots, &plan);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut watcher = watcher;
            let mut pending = HashSet::new();
            let mut debounce = Debounce::default();
            while !stop_worker.load(Ordering::Acquire) {
                // Check before receiving: a continuously non-empty notify
                // queue must not starve the maximum deadline by preventing a
                // receive timeout from ever occurring.
                if debounce.deadline().is_some_and(|at| Instant::now() >= at) {
                    if let Ok(mut service) = service.lock() {
                        let changed = !pending.is_empty();
                        for repo in pending.drain() {
                            if let Ok(event) = service.worktrees_event(&repo) {
                                updates.send(event);
                            }
                        }
                        // The REPOS pane's dot is a repo row's `dirty`, and a
                        // commit moves its count: one list per flush, after
                        // the worktree rows it summarises (#157).
                        if changed && let Ok(event) = service.repos_event() {
                            updates.send(event);
                        }
                        plan = service.watch_plan();
                        reconcile(&mut watcher, &mut roots, &plan);
                    }
                    debounce.clear();
                    continue;
                }
                let timeout = debounce
                    .deadline()
                    .map(|at: Instant| at.saturating_duration_since(Instant::now()))
                    .unwrap_or(Duration::from_millis(100));
                match raw_receiver.recv_timeout(timeout) {
                    Ok(Ok(event)) if !matches!(event.kind, EventKind::Access(_)) => {
                        pending.extend(plan.affected(&event.paths));
                        if !pending.is_empty() {
                            debounce.observe(Instant::now());
                        }
                    }
                    Ok(Ok(_)) | Ok(Err(_)) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
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
) {
    let wanted = plan.roots();
    let mut failures = Vec::new();
    for path in current
        .keys()
        .filter(|path| !wanted.contains_key(*path))
        .cloned()
        .collect::<Vec<_>>()
    {
        if let Err(error) = watcher.unwatch(&path) {
            failures.push((path.clone(), error));
        }
        current.remove(&path);
    }
    for (path, mode) in &wanted {
        if current.get(path) != Some(mode) {
            if current.contains_key(path)
                && let Err(error) = watcher.unwatch(path)
            {
                failures.push((path.clone(), error));
            }
            match watcher.watch(path, *mode) {
                Ok(()) => {
                    current.insert(path.clone(), *mode);
                }
                Err(error) => {
                    current.remove(path);
                    failures.push((path.clone(), error));
                }
            }
        }
    }
    if let Some((path, error)) = failures.first() {
        eprintln!(
            "groved: {} worktree watch roots could not be reconciled; first was {}: {error}",
            failures.len(),
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watcher_start_failure_disables_pushes_instead_of_failing_the_daemon() {
        let result = tolerate_start::<()>(Err(notify::Error::generic("watch limit reached")));
        assert!(result.is_none());
    }

    #[test]
    fn continuous_events_cannot_postpone_a_push_past_the_maximum() {
        let first = Instant::now();
        let mut debounce = Debounce::default();
        debounce.observe(first);
        for millis in (100..=1_500).step_by(100) {
            debounce.observe(first + Duration::from_millis(millis));
        }
        assert_eq!(debounce.deadline(), Some(first + MAX_DEBOUNCE));
    }
}
