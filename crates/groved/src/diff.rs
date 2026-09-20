//! The diff screen's read side: the file list, the selected file's hunks,
//! per SPEC §4.4. Strictly read-only — nothing here may write to a worktree,
//! which is why every git call is a read from §7's list.

use grove_git::DiffFile as RawFile;
use grove_proto::{DiffFile, DiffLine};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DiffScreenError {
    #[error("git inspection failed: {0}")]
    Git(#[from] grove_git::Error),
}

/// One answer to `DiffWorktree`, minus the wire's worktree echo.
#[derive(Debug, PartialEq, Eq)]
pub struct DiffScreen {
    /// The branch the diff was taken against, named for the header's
    /// "vs origin/main". Empty when the repo has no base: nothing to
    /// compare against is an empty screen, not an error.
    pub base: String,
    pub files: Vec<DiffFile>,
    pub selected: Option<String>,
    pub hunks: Vec<DiffLine>,
    pub added: u64,
    pub removed: u64,
}

/// The diff screen for a checkout.
///
/// `requested` is the file cursor (§4.4's `↑↓ file` moves it and refetches):
/// the hunk pane shows that file and nothing else, so one refetch pays one
/// patch subprocess, never the whole list's. `None` selects the first file,
/// which is where the screen starts.
///
/// A file deleted between the listing and this fetch is reported in band:
/// the fresh list no longer contains it, so `selected` is `None` and the
/// hunks are empty — the client moves the cursor, and the request does not
/// fail for every other file's sake.
pub fn diff_screen(
    worktree: &Path,
    base: Option<&str>,
    requested: Option<&str>,
) -> Result<DiffScreen, DiffScreenError> {
    let Some(base) = base else {
        return Ok(DiffScreen {
            base: String::new(),
            files: Vec::new(),
            selected: None,
            hunks: Vec::new(),
            added: 0,
            removed: 0,
        });
    };
    let mut files = grove_git::diff_selected(worktree, base, None)?;
    let mut untracked = grove_git::untracked_files(worktree)?;
    untracked.sort();
    files.extend(untracked.into_iter().map(untracked_file));

    // The cursor: the requested file if the fresh list still knows it, else
    // where the screen starts — the first file. A file deleted between the
    // listing and this fetch is gone from the fresh list, so there is no
    // selection and no hunks; the client learns that in band and moves the
    // cursor, rather than the request failing for every other file's sake.
    let selection = requested
        .map(|wanted| files.iter().find(|file| file.path == Path::new(wanted)))
        .unwrap_or_else(|| files.first())
        .map(|file| file.path.clone());
    // One refetch pays one patch subprocess: the selection's, never the
    // whole list's. A file deleted between the listing and this fetch has
    // no selection, and git answers nothing for it anyway.
    if let Some(path) = &selection {
        let selected = files.iter().find(|file| &file.path == path);
        if selected.is_some_and(|file| file.added.is_some()) {
            // A worktree that stops answering between the list and the patch
            // pass degrades to an empty hunk pane for that file, like every
            // other unreadable checkout here.
            if let Ok(patch) = grove_git::diff_patch(worktree, base, path)
                && let Some(slot) = files.iter_mut().find(|listed| &listed.path == path)
            {
                slot.patch = patch;
            }
        }
    }
    let selected_file = selection
        .as_ref()
        .and_then(|path| files.iter().find(|file| &file.path == path));
    Ok(DiffScreen {
        base: base.to_owned(),
        added: files.iter().filter_map(|file| file.added).sum(),
        removed: files.iter().filter_map(|file| file.deleted).sum(),
        files: files
            .iter()
            .map(|file| DiffFile {
                path: file.path.to_string_lossy().into_owned(),
                // The wire's status set is closed: a typechange is a
                // modification of an existing path, so T rides as M, and any
                // letter the wire does not name falls to ? rather than
                // leaking an undocumented state to clients.
                status: match file.status {
                    'T' => 'M',
                    known @ ('M' | 'A' | 'D' | '?') => known,
                    _ => '?',
                },
                added: file.added.unwrap_or(0),
                removed: file.deleted.unwrap_or(0),
            })
            .collect(),
        selected: selection
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        hunks: selected_file
            .map(|file| classify(&file.patch))
            .unwrap_or_default(),
    })
}

/// The selected file's patch, pre-classified so the client never parses git.
fn classify(patch: &[u8]) -> Vec<DiffLine> {
    let mut hunks = Vec::new();
    // File-level metadata is already on the wire as the file list; it appears
    // only before the first hunk header, so skipping it there — and only
    // there — keeps an added line whose content starts `++ ` or a removed one
    // starting `-- ` from being mistaken for a file header.
    let mut in_hunks = false;
    for line in String::from_utf8_lossy(patch).lines() {
        // A hunk header — every one of them, not just the first: later hunks
        // in the same file are separators too, not content that happens to
        // start with @@.
        if line.starts_with("@@") {
            in_hunks = true;
            hunks.push(DiffLine::Header(line.to_string()));
            continue;
        }
        // File metadata appears only before the first header.
        if !in_hunks {
            continue;
        }
        // A context line keeps its leading space even when the content is
        // empty; "\ No newline" is a byte-level remark the screen does not
        // render.
        if line.starts_with("\\ ") {
            continue;
        } else if let Some(added) = line.strip_prefix('+') {
            hunks.push(DiffLine::Added(added.to_string()));
        } else if let Some(removed) = line.strip_prefix('-') {
            hunks.push(DiffLine::Removed(removed.to_string()));
        } else {
            hunks.push(DiffLine::Context(
                line.strip_prefix(' ').unwrap_or(line).to_string(),
            ));
        }
    }
    hunks
}

fn untracked_file(path: PathBuf) -> RawFile {
    RawFile {
        path,
        status: '?',
        added: None,
        deleted: None,
        patch: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("groved-diff-{label}-{unique}"));
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

    /// A checkout one base behind: modified, added and deleted files, a
    /// binary change, and one untracked file — every status the screen
    /// renders except `T`.
    fn fixture() -> (PathBuf, PathBuf) {
        let temp = temp_dir("screen");
        let base = temp.join("base");
        let worktree = temp.join("work");
        fs::create_dir_all(&base).unwrap();
        let work = worktree.clone();
        git(&base, &["init", "-q", "-b", "base"]);
        git(&base, &["config", "user.name", "Grove Test"]);
        git(&base, &["config", "user.email", "grove@example.test"]);
        fs::write(base.join("kept"), "one\n").unwrap();
        fs::write(base.join("removed-later"), "gone\n").unwrap();
        fs::write(base.join("binary"), vec![0_u8, 1, 2]).unwrap();
        fs::write(base.join("touched"), "old\n").unwrap();
        git(&base, &["add", "."]);
        git(&base, &["commit", "-qm", "base"]);
        git(
            &base,
            &["worktree", "add", "-qb", "work", work.to_str().unwrap()],
        );
        fs::write(work.join("touched"), "new\n").unwrap();
        git(&work, &["add", "touched"]);
        fs::write(work.join("added"), "a\nb\nc\n").unwrap();
        git(&work, &["add", "added"]);
        git(&work, &["rm", "-q", "removed-later"]);
        fs::write(work.join("binary"), vec![9_u8, 8, 7, 0]).unwrap();
        git(&work, &["add", "binary"]);
        git(&work, &["commit", "-qm", "all-kinds"]);
        fs::write(work.join("untracked"), "untracked\n").unwrap();
        (work, base)
    }

    #[test]
    fn screen_lists_every_status_and_selects_the_requested_file() {
        let (worktree, _) = fixture();
        let screen = diff_screen(&worktree, Some("base"), None).unwrap();
        assert_eq!(screen.base, "base");
        // Default cursor: the first file, with its hunks.
        assert_eq!(
            screen.selected.as_deref(),
            screen.files.first().map(|file| file.path.as_str())
        );
        assert!(!screen.hunks.is_empty(), "the default cursor has hunks");
        // touched, added, removed-later, binary — and the untracked row.
        assert_eq!(screen.files.len(), 5);
        assert_eq!(screen.added, 4, "1 + 3 text lines; binary counts none");
        assert_eq!(screen.removed, 2);
        let statuses: Vec<(&str, char)> = screen
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.status))
            .collect();
        assert!(statuses.contains(&("touched", 'M')));
        assert!(statuses.contains(&("added", 'A')));
        assert!(statuses.contains(&("removed-later", 'D')));
        // The binary file is listed, carries no counts, and the cursor on it
        // reports no hunks rather than emitting bytes.
        let binary = screen
            .files
            .iter()
            .find(|file| file.path == "binary")
            .unwrap();
        assert_eq!((binary.added, binary.removed), (0, 0));

        let screen = diff_screen(&worktree, Some("base"), Some("binary")).unwrap();
        assert_eq!(screen.selected.as_deref(), Some("binary"));
        assert!(screen.hunks.is_empty(), "binary reports no hunks");

        // Moving the cursor refetches only that file's hunks.
        let screen = diff_screen(&worktree, Some("base"), Some("added")).unwrap();
        assert_eq!(screen.selected.as_deref(), Some("added"));
        assert!(
            screen
                .hunks
                .contains(&DiffLine::Header("@@ -0,0 +1,3 @@".to_string()))
                || screen
                    .hunks
                    .iter()
                    .any(|line| matches!(line, DiffLine::Added(text) if text == "a"))
        );
    }

    #[test]
    fn deleted_between_list_and_fetch_is_reported_in_band() {
        let (worktree, base) = fixture();
        let requested = "removed-later";
        let (files_before, _) = {
            // The listing the client saw; then the file's change is undone on
            // the worktree's side before the cursor refetches.
            let before =
                grove_git::diff_selected(&worktree, "base", Some(Path::new(requested))).unwrap();
            (before, ())
        };
        assert!(
            files_before
                .iter()
                .any(|file| file.path == Path::new(requested))
        );
        // Recreate the base content in the worktree: the file is no longer
        // part of the diff.
        fs::copy(base.join(requested), worktree.join(requested)).unwrap();
        git(&worktree, &["add", requested]);
        git(&worktree, &["commit", "-qm", "restore the deleted file"]);
        let screen = diff_screen(&worktree, Some("base"), Some(requested)).unwrap();
        assert!(
            !screen.files.iter().any(|file| file.path == requested),
            "the fresh list must not contain the restored file"
        );
        assert_eq!(screen.selected, None);
        assert!(screen.hunks.is_empty());
    }

    #[test]
    fn a_clean_worktree_is_an_empty_list_not_an_error() {
        let temp = temp_dir("clean");
        let base = temp.join("base");
        let worktree = temp.join("work");
        fs::create_dir_all(&base).unwrap();
        git(&base, &["init", "-q", "-b", "base"]);
        git(&base, &["config", "user.name", "Grove Test"]);
        git(&base, &["config", "user.email", "grove@example.test"]);
        fs::write(base.join("same"), "same\n").unwrap();
        git(&base, &["add", "same"]);
        git(&base, &["commit", "-qm", "same"]);
        let work = worktree.clone();
        git(
            &base,
            &["worktree", "add", "-qb", "work", work.to_str().unwrap()],
        );
        let screen = diff_screen(&worktree, Some("base"), None).unwrap();
        assert!(screen.files.is_empty());
        assert_eq!(screen.selected, None);
        assert_eq!((screen.added, screen.removed), (0, 0));
    }

    #[test]
    fn a_repo_without_a_base_is_an_empty_screen() {
        let (worktree, _) = fixture();
        let screen = diff_screen(&worktree, None, Some("touched")).unwrap();
        assert_eq!(screen.base, "");
        assert!(screen.files.is_empty());
        assert_eq!(screen.selected, None);
        assert!(screen.hunks.is_empty());
    }

    #[test]
    fn every_hunk_header_is_a_header() {
        // The first fix over-gated: only the first @@ was a Header and every
        // later one fell through to Context. Two hunks in one file are the
        // ordinary case.
        let patch = b"diff --git a/x b/x\nindex aaa..bbb 100644\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n one\n-old\n+new\n@@ -10,2 +10,2 @@\n more\n-context\n+changed\n";
        assert_eq!(
            classify(patch),
            vec![
                DiffLine::Header("@@ -1,2 +1,2 @@".to_string()),
                DiffLine::Context("one".to_string()),
                DiffLine::Removed("old".to_string()),
                DiffLine::Added("new".to_string()),
                DiffLine::Header("@@ -10,2 +10,2 @@".to_string()),
                DiffLine::Context("more".to_string()),
                DiffLine::Removed("context".to_string()),
                DiffLine::Added("changed".to_string()),
            ]
        );
    }

    #[test]
    fn classify_reads_content_after_the_first_header_not_metadata() {
        // A removed line whose content starts `++ ` (raw `-++ x`) and an
        // added `-- ` (raw `+--- x`) come after the first hunk header, where
        // file metadata cannot appear — they are content, not headers to skip.
        let patch = b"diff --git a/x b/x\nindex aaa..bbb 100644\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n context\n-++ x\n+--- x\n";
        assert_eq!(
            classify(patch),
            vec![
                DiffLine::Header("@@ -1,2 +1,2 @@".to_string()),
                DiffLine::Context("context".to_string()),
                DiffLine::Removed("++ x".to_string()),
                DiffLine::Added("--- x".to_string()),
            ]
        );
    }

    #[test]
    fn hunks_classify_lines_without_making_the_client_parse() {
        let (worktree, _) = fixture();
        let screen = diff_screen(&worktree, Some("base"), Some("touched")).unwrap();
        assert_eq!(
            screen.hunks,
            vec![
                DiffLine::Header("@@ -1 +1 @@".to_string()),
                DiffLine::Removed("old".to_string()),
                DiffLine::Added("new".to_string()),
            ]
        );
    }
}
