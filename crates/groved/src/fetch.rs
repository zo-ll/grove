//! Explicit, bounded fetch policy for session members.

use grove_domain::RepoId;
use grove_git::Repository;
use grove_state::StoredSession;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchStatus {
    Fetched,
    Failed(String),
    RepositoryMissing,
    NotMember,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchResult {
    pub repo: RepoId,
    pub status: FetchStatus,
    /// Age of the last successful fetch, including one predating this attempt.
    pub ref_age: Option<Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefFreshness {
    pub age: Option<Duration>,
    pub stale: bool,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("invalid stale_after value {value:?}; expected an integer followed by s, m, h, or d")]
pub struct StaleAfterError {
    value: String,
}

/// Fetches only when one of its methods is called and joins every worker before
/// returning. It owns no timer and starts no persistent background thread.
#[derive(Clone, Debug)]
pub struct FetchPolicy {
    max_concurrency: usize,
    stale_after: Duration,
}

impl FetchPolicy {
    pub fn new(max_concurrency: usize, stale_after: Duration) -> Self {
        Self {
            max_concurrency: max_concurrency.max(1),
            stale_after,
        }
    }

    pub fn from_config(
        max_concurrency: usize,
        config: &grove_lua::DaemonConfig,
    ) -> Result<Self, StaleAfterError> {
        Ok(Self::new(
            max_concurrency,
            parse_stale_after(&config.stale_after)?,
        ))
    }

    /// Fetches every member of the session and no other discovered repository.
    pub fn fetch_on_open(
        &self,
        session: &StoredSession,
        repositories: &[Repository],
    ) -> Vec<FetchResult> {
        self.fetch_selected(session, repositories, None, fetch_repository)
    }

    /// Explicit fetch command. `None` means all session members; `Some` means
    /// exactly that member. A non-member is rejected without invoking Git.
    pub fn fetch_on_demand(
        &self,
        session: &StoredSession,
        repositories: &[Repository],
        repo: Option<&RepoId>,
    ) -> Vec<FetchResult> {
        self.fetch_selected(session, repositories, repo, fetch_repository)
    }

    pub fn ref_freshness(&self, repository: &Repository) -> Result<RefFreshness, grove_git::Error> {
        let age = grove_git::ref_age(&repository.path)?;
        Ok(RefFreshness {
            age,
            stale: age.is_none_or(|age| age >= self.stale_after),
        })
    }

    fn fetch_selected<F>(
        &self,
        session: &StoredSession,
        repositories: &[Repository],
        requested: Option<&RepoId>,
        fetch: F,
    ) -> Vec<FetchResult>
    where
        F: Fn(&Repository) -> FetchResult + Sync,
    {
        let selected = match requested {
            Some(repo) if !session.members.contains(repo) => {
                return vec![FetchResult {
                    repo: repo.clone(),
                    status: FetchStatus::NotMember,
                    ref_age: None,
                }];
            }
            Some(repo) => vec![repo.clone()],
            None => session.members.clone(),
        };
        let by_id: HashMap<_, _> = repositories
            .iter()
            .map(|repository| (repository.id.clone(), repository))
            .collect();
        let mut immediate = Vec::new();
        let mut tasks = Vec::new();
        for (index, repo) in selected.into_iter().enumerate() {
            if let Some(repository) = by_id.get(&repo) {
                tasks.push((index, (*repository).clone()));
            } else {
                immediate.push((
                    index,
                    FetchResult {
                        repo,
                        status: FetchStatus::RepositoryMissing,
                        ref_age: None,
                    },
                ));
            }
        }
        let mut results = bounded_map(tasks, self.max_concurrency, fetch);
        results.extend(immediate);
        results.sort_by_key(|(index, _)| *index);
        results.into_iter().map(|(_, result)| result).collect()
    }
}

pub fn parse_stale_after(value: &str) -> Result<Duration, StaleAfterError> {
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let amount = number.parse::<u64>().map_err(|_| StaleAfterError {
        value: value.to_owned(),
    })?;
    let seconds = match unit {
        "s" => Some(amount),
        "m" => amount.checked_mul(60),
        "h" => amount.checked_mul(60 * 60),
        "d" => amount.checked_mul(24 * 60 * 60),
        _ => None,
    }
    .ok_or_else(|| StaleAfterError {
        value: value.to_owned(),
    })?;
    Ok(Duration::from_secs(seconds))
}

fn fetch_repository(repository: &Repository) -> FetchResult {
    let status = match grove_git::fetch_prune(&repository.path) {
        Ok(()) => FetchStatus::Fetched,
        Err(error) => FetchStatus::Failed(error.to_string()),
    };
    let ref_age = grove_git::ref_age(&repository.path).ok().flatten();
    FetchResult {
        repo: repository.id.clone(),
        status,
        ref_age,
    }
}

fn bounded_map<T, R, F>(items: Vec<(usize, T)>, limit: usize, operation: F) -> Vec<(usize, R)>
where
    T: Send,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    if items.is_empty() {
        return Vec::new();
    }
    let worker_count = limit.max(1).min(items.len());
    let queue = Arc::new(Mutex::new(VecDeque::from(items)));
    let (sender, receiver) = mpsc::channel();
    thread::scope(|scope| {
        for _ in 0..worker_count {
            let queue = Arc::clone(&queue);
            let sender = sender.clone();
            let operation = &operation;
            scope.spawn(move || {
                loop {
                    let item = queue.lock().expect("fetch queue poisoned").pop_front();
                    let Some((index, item)) = item else {
                        break;
                    };
                    if sender.send((index, operation(&item))).is_err() {
                        break;
                    }
                }
            });
        }
    });
    drop(sender);
    receiver.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_domain::{SessionId, SessionState};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn repository(id: &str) -> Repository {
        Repository {
            id: RepoId(id.into()),
            name: id.into(),
            path: PathBuf::from(format!("/{id}")),
            base_branch: Some("origin/main".into()),
        }
    }

    fn session(members: &[&str]) -> StoredSession {
        StoredSession {
            id: SessionId("session".into()),
            name: "Session".into(),
            members: members.iter().map(|id| RepoId((*id).into())).collect(),
            owned: Vec::new(),
            state: SessionState::Closed,
            state_changed_at: 0,
        }
    }

    #[test]
    fn session_open_fetches_members_only_and_surfaces_each_result() {
        let policy = FetchPolicy::new(2, Duration::from_secs(60));
        let repositories = vec![repository("one"), repository("two"), repository("outsider")];
        let results = policy.fetch_selected(
            &session(&["one", "missing", "two"]),
            &repositories,
            None,
            |repo| FetchResult {
                repo: repo.id.clone(),
                status: if repo.id.0 == "two" {
                    FetchStatus::Failed("offline".into())
                } else {
                    FetchStatus::Fetched
                },
                ref_age: None,
            },
        );
        assert_eq!(
            results
                .iter()
                .map(|result| &result.repo.0)
                .collect::<Vec<_>>(),
            ["one", "missing", "two"]
        );
        assert_eq!(results[0].status, FetchStatus::Fetched);
        assert_eq!(results[1].status, FetchStatus::RepositoryMissing);
        assert_eq!(results[2].status, FetchStatus::Failed("offline".into()));
        assert!(results.iter().all(|result| result.repo.0 != "outsider"));
    }

    #[test]
    fn on_demand_rejects_non_members_without_fetching() {
        let policy = FetchPolicy::new(2, Duration::from_secs(60));
        let calls = AtomicUsize::new(0);
        let outsider = RepoId("outsider".into());
        let results = policy.fetch_selected(
            &session(&["one"]),
            &[repository("one"), repository("outsider")],
            Some(&outsider),
            |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                unreachable!()
            },
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(results[0].status, FetchStatus::NotMember);
    }

    #[test]
    fn worker_concurrency_is_bounded() {
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let items = (0..12).map(|index| (index, index)).collect();
        let results = bounded_map(items, 3, |_| {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(10));
            active.fetch_sub(1, Ordering::SeqCst);
            now
        });
        assert_eq!(results.len(), 12);
        assert!(peak.load(Ordering::SeqCst) > 1);
        assert!(peak.load(Ordering::SeqCst) <= 3);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unknown_or_old_refs_are_stale() {
        let threshold = Duration::from_secs(60);
        for (age, stale) in [
            (None, true),
            (Some(Duration::from_secs(59)), false),
            (Some(Duration::from_secs(60)), true),
        ] {
            assert_eq!(age.is_none_or(|age| age >= threshold), stale);
        }
    }

    #[test]
    fn stale_after_config_parses_supported_units_and_rejects_bad_values() {
        assert_eq!(parse_stale_after("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_stale_after("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(
            parse_stale_after("4h").unwrap(),
            Duration::from_secs(14_400)
        );
        assert_eq!(
            parse_stale_after("2d").unwrap(),
            Duration::from_secs(172_800)
        );
        for value in ["", "4", "1.5h", "lots"] {
            assert!(parse_stale_after(value).is_err(), "accepted {value:?}");
        }
    }
}
