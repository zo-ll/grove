//! All git access. Backend lane only — the TUI never links this crate.
//!
//! The read and write operations Grove performs are enumerated in `SPEC.md` §7
//! and that list is closed: if git cannot answer something, Grove does not
//! display it.
//!
//! Implemented in issues #4 and #5.
//!
//! Discovery treats both `.git` directories and `.git` files as repositories.
//! The latter means checked-out submodules are returned as repositories in their
//! own right.  Finding a repository does not stop the walk, so independently
//! nested repositories are returned too; only the repository's `.git` metadata
//! itself is pruned from the walk.

use grove_domain::RepoId;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("workspace path {path} could not be resolved: {source}")]
    WorkspacePath { path: PathBuf, source: io::Error },
    #[error("invalid ignore glob {pattern:?}: {message}")]
    InvalidIgnore { pattern: String, message: String },
    #[error("could not walk {path}: {source}")]
    Walk { path: PathBuf, source: io::Error },
    #[error("git {operation} failed in {path}: {message}")]
    Git {
        operation: &'static str,
        path: PathBuf,
        message: String,
    },
    #[error("git returned malformed {operation} output in {path}: {message}")]
    MalformedGitOutput {
        operation: &'static str,
        path: PathBuf,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    pub id: RepoId,
    pub name: String,
    pub path: PathBuf,
    /// `None` when `origin` is absent or has no advertised default branch.
    pub base_branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// `None` for a detached HEAD.
    pub branch: Option<String>,
    pub head: String,
    pub is_clone: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AheadBehind {
    pub ahead: u64,
    pub behind: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tracking {
    Tracked(AheadBehind),
    NoUpstream,
    UpstreamGone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffFile {
    pub path: PathBuf,
    /// Binary files have no line counts.
    pub added: Option<u64>,
    pub deleted: Option<u64>,
    pub patch: Vec<u8>,
}

/// In-memory view of repositories under one scan root.
pub struct Workspace {
    root: PathBuf,
    ignores: Vec<String>,
    repos: Vec<Repository>,
}

impl Workspace {
    pub fn discover(root: impl AsRef<Path>, ignores: Vec<String>) -> Result<Self, Error> {
        let root = fs::canonicalize(root.as_ref()).map_err(|source| Error::WorkspacePath {
            path: root.as_ref().to_owned(),
            source,
        })?;
        let mut workspace = Self {
            root,
            ignores,
            repos: Vec::new(),
        };
        workspace.refresh()?;
        Ok(workspace)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn repositories(&self) -> &[Repository] {
        &self.repos
    }

    /// Re-runs discovery. No repository facts are persisted to disk.
    pub fn refresh(&mut self) -> Result<&[Repository], Error> {
        let matcher = ignore_matcher(&self.root, &self.ignores)?;
        let mut paths = Vec::new();
        let mut seen = HashSet::new();
        walk_for_repos(&self.root, &matcher, &mut seen, &mut paths)?;
        paths.sort();
        self.repos = paths
            .into_iter()
            .map(|path| repository(&self.root, path))
            .collect::<Result<_, _>>()?;
        Ok(&self.repos)
    }
}

fn ignore_matcher(root: &Path, patterns: &[String]) -> Result<Gitignore, Error> {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        builder
            .add_line(None, pattern)
            .map_err(|err| Error::InvalidIgnore {
                pattern: pattern.clone(),
                message: err.to_string(),
            })?;
    }
    builder.build().map_err(|err| Error::InvalidIgnore {
        pattern: "<combined ignore globs>".into(),
        message: err.to_string(),
    })
}

fn walk_for_repos(
    dir: &Path,
    matcher: &Gitignore,
    seen: &mut HashSet<PathBuf>,
    repos: &mut Vec<PathBuf>,
) -> Result<(), Error> {
    let entries = fs::read_dir(dir).map_err(|source| Error::Walk {
        path: dir.to_owned(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::Walk {
            path: dir.to_owned(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| Error::Walk {
            path: path.clone(),
            source,
        })?;
        let is_dir = file_type.is_dir();
        if entry.file_name() == OsStr::new(".git") {
            if seen.insert(dir.to_owned()) {
                repos.push(dir.to_owned());
            }
            continue;
        }
        if matcher
            .matched_path_or_any_parents(&path, is_dir)
            .is_ignore()
        {
            continue;
        }
        // Do not follow symlinks: a workspace walk must remain beneath its root.
        if is_dir {
            walk_for_repos(&path, matcher, seen, repos)?;
        }
    }
    Ok(())
}

fn repository(root: &Path, path: PathBuf) -> Result<Repository, Error> {
    let relative = path.strip_prefix(root).unwrap_or(&path);
    let id = if relative.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/")
    };
    let name = path
        .file_name()
        .unwrap_or_else(|| path.as_os_str())
        .to_string_lossy()
        .into_owned();
    Ok(Repository {
        id: RepoId(id),
        name,
        base_branch: base_branch(&path)?,
        path,
    })
}

pub fn base_branch(repo: &Path) -> Result<Option<String>, Error> {
    let output = git_output(repo, &["symbolic-ref", "refs/remotes/origin/HEAD"])?;
    if !output.status.success() {
        return Ok(None);
    }
    let reference = text_output("base branch", repo, &output.stdout)?;
    Ok(reference
        .strip_prefix("refs/remotes/")
        .map(str::to_owned)
        .or(Some(reference)))
}

pub fn worktrees(repo: &Path) -> Result<Vec<Worktree>, Error> {
    let output = git_success(repo, "worktree list", &["worktree", "list", "--porcelain"])?;
    parse_worktrees(repo, &output)
}

fn parse_worktrees(repo: &Path, bytes: &[u8]) -> Result<Vec<Worktree>, Error> {
    let text = std::str::from_utf8(bytes).map_err(|err| malformed("worktree list", repo, err))?;
    let clone_path = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_owned());
    let mut result = Vec::new();
    for record in text
        .split("\n\n")
        .filter(|record| !record.trim().is_empty())
    {
        let mut path = None;
        let mut head = None;
        let mut branch = None;
        for line in record.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(value));
            } else if let Some(value) = line.strip_prefix("HEAD ") {
                head = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
                branch = Some(value.to_owned());
            }
        }
        let path = path.ok_or_else(|| Error::MalformedGitOutput {
            operation: "worktree list",
            path: repo.to_owned(),
            message: "record has no worktree path".into(),
        })?;
        let head = head.ok_or_else(|| Error::MalformedGitOutput {
            operation: "worktree list",
            path: repo.to_owned(),
            message: format!("{} has no HEAD", path.display()),
        })?;
        let comparable = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        result.push(Worktree {
            is_clone: comparable == clone_path,
            path,
            branch,
            head,
        });
    }
    Ok(result)
}

pub fn is_dirty(worktree: &Path) -> Result<bool, Error> {
    Ok(!git_success(worktree, "status", &["status", "--porcelain"])?.is_empty())
}

pub fn ahead_behind(worktree: &Path) -> Result<Tracking, Error> {
    let output = git_output(
        worktree,
        &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return if stderr.contains("no upstream configured") || stderr.contains("no upstream branch")
        {
            Ok(Tracking::NoUpstream)
        } else if stderr.contains("unknown revision")
            || stderr.contains("ambiguous argument")
            || stderr.contains("bad revision")
        {
            Ok(Tracking::UpstreamGone)
        } else {
            Err(git_failure("ahead/behind", worktree, &output))
        };
    }
    let counts = text_output("ahead/behind", worktree, &output.stdout)?;
    let mut fields = counts.split_whitespace();
    let behind = parse_count("ahead/behind", worktree, fields.next())?;
    let ahead = parse_count("ahead/behind", worktree, fields.next())?;
    if fields.next().is_some() {
        return Err(Error::MalformedGitOutput {
            operation: "ahead/behind",
            path: worktree.to_owned(),
            message: counts,
        });
    }
    Ok(Tracking::Tracked(AheadBehind { ahead, behind }))
}

pub fn is_merged(repo: &Path, branch: &str, base: &str) -> Result<bool, Error> {
    let output = git_output(repo, &["merge-base", "--is-ancestor", branch, base])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure("merge check", repo, &output)),
    }
}

pub fn diff(worktree: &Path, base: &str) -> Result<Vec<DiffFile>, Error> {
    let range = format!("{base}...HEAD");
    let numstat = git_success(
        worktree,
        "diff numstat",
        &["diff", &range, "--numstat", "--no-renames", "-z"],
    )?;
    let mut files = Vec::new();
    for row in numstat
        .split(|byte| *byte == 0)
        .filter(|row| !row.is_empty())
    {
        let mut columns = row.splitn(3, |byte| *byte == b'\t');
        let added = columns.next();
        let deleted = columns.next();
        let path = columns.next().ok_or_else(|| Error::MalformedGitOutput {
            operation: "diff numstat",
            path: worktree.to_owned(),
            message: "numstat row has fewer than three columns".into(),
        })?;
        let file_path = PathBuf::from(String::from_utf8_lossy(path).into_owned());
        let patch = git_success_os(
            worktree,
            "diff patch",
            [
                OsStr::new("diff"),
                OsStr::new(&range),
                OsStr::new("--no-renames"),
                OsStr::new("--"),
                file_path.as_os_str(),
            ],
        )?;
        files.push(DiffFile {
            path: file_path,
            added: parse_numstat(added),
            deleted: parse_numstat(deleted),
            patch,
        });
    }
    Ok(files)
}

fn parse_numstat(value: Option<&[u8]>) -> Option<u64> {
    let value = value?;
    if value == b"-" {
        None
    } else {
        std::str::from_utf8(value).ok()?.parse().ok()
    }
}

/// A cancellable filesystem-size calculation performed on a worker thread.
pub struct SizeTask {
    cancelled: Arc<AtomicBool>,
    receiver: mpsc::Receiver<Result<u64, io::Error>>,
}

impl SizeTask {
    /// Requests cancellation. The worker checks between every directory entry.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn try_result(&self) -> Result<Option<u64>, SizeResultError> {
        match self.receiver.try_recv() {
            Ok(Ok(size)) => Ok(Some(size)),
            Ok(Err(err)) => Err(SizeResultError::Io(err)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) if self.cancelled.load(Ordering::Relaxed) => {
                Err(SizeResultError::Cancelled)
            }
            Err(mpsc::TryRecvError::Disconnected) => Err(SizeResultError::WorkerStopped),
        }
    }
}

impl Drop for SizeTask {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Debug, Error)]
pub enum SizeResultError {
    #[error("size walk was cancelled")]
    Cancelled,
    #[error("size worker stopped without a result")]
    WorkerStopped,
    #[error("size walk failed: {0}")]
    Io(#[source] io::Error),
}

pub fn worktree_size(path: impl Into<PathBuf>) -> SizeTask {
    let path = path.into();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        if let Some(result) = directory_size(&path, &worker_cancelled) {
            let _ = sender.send(result);
        }
    });
    SizeTask {
        cancelled,
        receiver,
    }
}

fn directory_size(path: &Path, cancelled: &AtomicBool) -> Option<Result<u64, io::Error>> {
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) => return Some(Err(err)),
    };
    if !metadata.is_dir() {
        return Some(Ok(metadata.len()));
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) => return Some(Err(err)),
    };
    let mut total = 0_u64;
    for entry in entries {
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => return Some(Err(err)),
        };
        match directory_size(&entry.path(), cancelled)? {
            Ok(size) => total = total.saturating_add(size),
            Err(err) => return Some(Err(err)),
        }
    }
    Some(Ok(total))
}

fn git_output(path: &Path, args: &[&str]) -> Result<Output, Error> {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .map_err(|err| Error::Git {
            operation: "spawn",
            path: path.to_owned(),
            message: err.to_string(),
        })
}

fn git_success(path: &Path, operation: &'static str, args: &[&str]) -> Result<Vec<u8>, Error> {
    let output = git_output(path, args)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(git_failure(operation, path, &output))
    }
}

fn git_success_os<'a>(
    path: &Path,
    operation: &'static str,
    args: impl IntoIterator<Item = &'a OsStr>,
) -> Result<Vec<u8>, Error> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .map_err(|err| Error::Git {
            operation,
            path: path.to_owned(),
            message: err.to_string(),
        })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(git_failure(operation, path, &output))
    }
}

fn git_failure(operation: &'static str, path: &Path, output: &Output) -> Error {
    Error::Git {
        operation,
        path: path.to_owned(),
        message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
}

fn text_output(operation: &'static str, path: &Path, bytes: &[u8]) -> Result<String, Error> {
    std::str::from_utf8(bytes)
        .map(str::trim)
        .map(str::to_owned)
        .map_err(|err| malformed(operation, path, err))
}

fn malformed(operation: &'static str, path: &Path, err: impl std::fmt::Display) -> Error {
    Error::MalformedGitOutput {
        operation,
        path: path.to_owned(),
        message: err.to_string(),
    }
}

fn parse_count(operation: &'static str, path: &Path, value: Option<&str>) -> Result<u64, Error> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| malformed(operation, path, "missing or invalid count"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("grove-git-{label}-{unique}"));
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

    fn repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.name", "Grove Test"]);
        git(path, &["config", "user.email", "grove@example.test"]);
        fs::write(path.join("tracked"), "one\n").unwrap();
        git(path, &["add", "tracked"]);
        git(path, &["commit", "-qm", "initial"]);
    }

    #[test]
    fn discovery_finds_nested_repos_and_honours_ignores() {
        let temp = TempDir::new("discover");
        repo(&temp.0.join("outer"));
        repo(&temp.0.join("outer/nested"));
        repo(&temp.0.join("ignored"));
        let workspace = Workspace::discover(&temp.0, vec!["ignored/".into()]).unwrap();
        let ids: Vec<_> = workspace
            .repositories()
            .iter()
            .map(|repo| repo.id.0.as_str())
            .collect();
        assert_eq!(ids, ["outer", "outer/nested"]);
    }

    #[test]
    fn discovery_recognises_git_files_used_by_submodules() {
        let temp = TempDir::new("git-file");
        let submodule = temp.0.join("submodule");
        fs::create_dir_all(&submodule).unwrap();
        fs::write(submodule.join(".git"), "gitdir: ../metadata\n").unwrap();
        let workspace = Workspace::discover(&temp.0, Vec::new()).unwrap();
        assert_eq!(workspace.repositories().len(), 1);
        assert_eq!(workspace.repositories()[0].id.0, "submodule");
    }

    #[test]
    fn discovery_of_thirty_repositories_is_startup_scale() {
        let temp = TempDir::new("thirty");
        for index in 0..30 {
            repo(&temp.0.join(format!("repo-{index:02}")));
        }
        let started = Instant::now();
        let workspace = Workspace::discover(&temp.0, Vec::new()).unwrap();
        let elapsed = started.elapsed();
        eprintln!("30-repo workspace discovery: {elapsed:?}");
        assert_eq!(workspace.repositories().len(), 30);
        assert!(
            elapsed < Duration::from_secs(10),
            "30-repo discovery took {:?}",
            elapsed
        );
    }

    #[test]
    fn worktree_listing_marks_clone_and_detached_head() {
        let temp = TempDir::new("worktrees");
        let clone = temp.0.join("clone");
        let detached = temp.0.join("detached");
        repo(&clone);
        git(
            &clone,
            &[
                "worktree",
                "add",
                "--detach",
                detached.to_str().unwrap(),
                "HEAD",
            ],
        );
        let rows = worktrees(&clone).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|row| row.is_clone && row.branch.as_deref() == Some("main"))
        );
        assert!(rows.iter().any(|row| !row.is_clone && row.branch.is_none()));
    }

    #[test]
    fn no_upstream_is_data_and_dirty_includes_untracked_files() {
        let temp = TempDir::new("status");
        repo(&temp.0);
        assert_eq!(ahead_behind(&temp.0).unwrap(), Tracking::NoUpstream);
        assert!(!is_dirty(&temp.0).unwrap());
        fs::write(temp.0.join("untracked"), "work").unwrap();
        assert!(is_dirty(&temp.0).unwrap());
    }

    #[test]
    fn diff_has_numstat_and_a_patch_per_file() {
        let temp = TempDir::new("diff");
        repo(&temp.0);
        git(&temp.0, &["branch", "base"]);
        fs::write(temp.0.join("tracked"), "one\ntwo\n").unwrap();
        git(&temp.0, &["add", "tracked"]);
        git(&temp.0, &["commit", "-qm", "change"]);
        let files = diff(&temp.0, "base").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].added, Some(1));
        assert_eq!(files[0].deleted, Some(0));
        assert!(String::from_utf8_lossy(&files[0].patch).contains("+two"));
        assert!(!is_merged(&temp.0, "main", "base").unwrap());
        assert!(is_merged(&temp.0, "base", "main").unwrap());
    }

    #[test]
    fn size_walk_returns_without_blocking_the_caller() {
        let temp = TempDir::new("size");
        fs::write(temp.0.join("bytes"), vec![0; 128]).unwrap();
        let task = worktree_size(temp.0.clone());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(size) = task.try_result().unwrap() {
                assert!(size >= 128);
                break;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
    }
}
