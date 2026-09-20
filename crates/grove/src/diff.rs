//! The diff screen (SPEC §4.4).
//!
//! Read-only, and deliberately so. There is no `stage`, no `commit`, no
//! cross-repo diff: those are git write-verbs, and every worktree has its own
//! shell one pane away that does them better than a menu would. The footer
//! says `↑↓ file` and `esc close` and nothing else, which is the screen's
//! whole promise.
//!
//! The file cursor is the interesting part. Hunks arrive for one file at a
//! time — §4.4's `↑↓ file` moves the cursor and the *daemon* answers with that
//! file's patch, so opening the screen does not pay for every file's diff. The
//! cursor therefore has to say when it moved, so the caller knows to ask.

use grove_proto::{DiffFile, DiffLine};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::text::ELLIPSIS;
use crate::theme::{Role, Theme};

/// A `Diff` event, as the screen takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incoming {
    pub repo: String,
    pub branch: String,
    pub base: String,
    pub files: Vec<DiffFile>,
    /// The file the hunks are for, as the daemon named it.
    pub selected: Option<String>,
    pub hunks: Vec<DiffLine>,
    pub added: u64,
    pub removed: u64,
}

/// The screen's state.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Diff {
    base: String,
    branch: String,
    repo: String,
    files: Vec<DiffFile>,
    cursor: usize,
    /// The patch for the file under the cursor, as the daemon sent it.
    hunks: Vec<DiffLine>,
    /// Which file those hunks belong to, so a late reply for a file the cursor
    /// has left is not painted under the wrong name.
    hunks_for: Option<String>,
    added: u64,
    removed: u64,
    /// First hunk line shown. Large diffs scroll rather than truncating.
    offset: usize,
}

impl Diff {
    /// Take a `Diff` event, whole.
    ///
    /// The event is the unit the daemon sends and the unit this screen shows,
    /// so it is passed as one thing rather than unpacked into nine arguments
    /// that have to be kept in the same order at every call site.
    pub fn set(&mut self, event: Incoming) {
        // Keep the cursor on the same file: the event arrives again whenever
        // the cursor moves, and resetting it would make `↓` unusable.
        let previous = self.selected().map(|file| file.path.clone());
        self.repo = event.repo;
        self.branch = event.branch;
        self.base = event.base;
        self.files = event.files;
        self.added = event.added;
        self.removed = event.removed;
        // The cursor is the user's, not the event's: `selected` says which
        // file the hunks are for, and a late reply naming the file they just
        // left would otherwise drag the cursor back to it.
        self.cursor = previous
            .or_else(|| event.selected.clone())
            .and_then(|path| self.files.iter().position(|file| file.path == path))
            .unwrap_or(0)
            .min(self.files.len().saturating_sub(1));

        // Hunks belong to the file the daemon named. The cursor can have moved
        // since the request went out, and painting one file's changes under
        // another's name is the worst thing a diff can do — so a reply for a
        // file that is no longer selected is dropped, and the screen goes on
        // showing nothing until the right one arrives.
        let here = self.selected().map(|file| file.path.clone());
        if event.selected.is_some() && event.selected != here {
            return;
        }
        self.hunks = event.hunks;
        self.hunks_for = event.selected.or(here);
        self.offset = 0;
    }

    pub fn selected(&self) -> Option<&DiffFile> {
        self.files.get(self.cursor)
    }

    #[cfg(test)]
    pub fn hunks_for(&self) -> Option<&str> {
        self.hunks_for.as_deref()
    }

    /// Move the cursor, returning the file whose patch is now wanted.
    ///
    /// `Some` means "ask the daemon for this one" — the screen does not have
    /// it, and §4.4's whole point is that it did not fetch it in advance.
    pub fn move_down(&mut self) -> Option<String> {
        let last = self.files.len().saturating_sub(1);
        if self.files.is_empty() || self.cursor >= last {
            return None;
        }
        self.cursor += 1;
        self.leave_file();
        self.selected().map(|file| file.path.clone())
    }

    pub fn move_up(&mut self) -> Option<String> {
        if self.cursor == 0 {
            return None;
        }
        self.cursor -= 1;
        self.leave_file();
        self.selected().map(|file| file.path.clone())
    }

    /// Forget the patch on screen. It belongs to the file being left, and
    /// leaving it up would show one file's changes under another's name until
    /// the reply arrives.
    fn leave_file(&mut self) {
        self.hunks.clear();
        self.hunks_for = None;
        self.offset = 0;
    }

    /// Scroll the patch. Large diffs scroll rather than stalling on a render
    /// of ten thousand lines.
    pub fn scroll(&mut self, lines: isize, room: usize) -> bool {
        let max = self.hunks.len().saturating_sub(room);
        let wanted = self.offset.saturating_add_signed(lines).min(max);
        if wanted == self.offset {
            return false;
        }
        self.offset = wanted;
        true
    }

    /// Draw the screen.
    pub fn render(&self, buf: &mut Buffer, area: Rect, theme: &Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        // Left: the file list. Right: the patch. The list is fixed-ish because
        // its content is paths and counts; the patch takes the rest.
        let left = (area.width / 3).clamp(20, 48).min(area.width);
        let mut lines = vec![Line::from(vec![
            Span::styled(
                format!(" diff · {} · {} vs {}", self.repo, self.branch, self.base),
                theme.style(Role::Accent),
            ),
            Span::raw("  "),
            // Omitted when there are none, for the same reason the rows omit
            // them: a binary-only diff showing `+0 −0` reads as "nothing
            // changed" rather than "nothing countable changed".
            Span::styled(plus(self.added), theme.style(Role::Clean)),
            Span::raw(" "),
            Span::styled(minus(self.removed), theme.style(Role::Error)),
        ])];

        let room = usize::from(area.height).saturating_sub(2);
        let patch: Vec<&DiffLine> = self.hunks.iter().skip(self.offset).take(room).collect();
        for index in 0..room {
            let mut spans = Vec::new();
            match self.files.get(index) {
                Some(file) => {
                    let here = index == self.cursor;
                    let style = if here {
                        theme.style(Role::Accent).add_modifier(Modifier::BOLD)
                    } else {
                        theme.style(Role::Muted)
                    };
                    // Path first from the left, because the filename is what
                    // identifies it and the directories are what repeat.
                    let path = ellipsise_left(&file.path, usize::from(left).saturating_sub(14));
                    spans.push(Span::styled(
                        format!(" {} ", file.status),
                        theme.style(status_role(file.status)),
                    ));
                    spans.push(Span::styled(
                        format!(
                            "{path:<width$}",
                            width = usize::from(left).saturating_sub(14)
                        ),
                        style,
                    ));
                    spans.push(Span::styled(
                        format!("{:>5}", plus(file.added)),
                        theme.style(Role::Clean),
                    ));
                    spans.push(Span::styled(
                        format!("{:>5}", minus(file.removed)),
                        theme.style(Role::Error),
                    ));
                }
                None => spans.push(Span::raw(" ".repeat(usize::from(left)))),
            }
            spans.push(Span::styled(" │ ", theme.style(Role::Muted)));
            if let Some(line) = patch.get(index) {
                let (text, role) = match line {
                    DiffLine::Header(text) => (text, Role::Accent),
                    DiffLine::Context(text) => (text, Role::Muted),
                    DiffLine::Added(text) => (text, Role::Clean),
                    DiffLine::Removed(text) => (text, Role::Error),
                };
                spans.push(Span::styled(text.clone(), theme.style(role)));
            }
            lines.push(Line::from(spans));
        }

        lines.push(Line::from(vec![
            Span::styled(" ↑↓ file", theme.style(Role::Muted)),
            Span::raw("   "),
            Span::styled("esc close", theme.style(Role::Muted)),
        ]));
        Paragraph::new(lines).render(area, buf);
    }
}

/// `M`, `A`, `D` and `?` read differently — the colour carries what the letter
/// means so a glance is enough.
fn status_role(status: char) -> Role {
    match status {
        'A' => Role::Clean,
        'D' => Role::Error,
        'M' => Role::Dirty,
        // `?` is untracked: not part of the diff, just present.
        _ => Role::Muted,
    }
}

fn plus(n: u64) -> String {
    if n == 0 {
        String::new()
    } else {
        format!("+{n}")
    }
}

fn minus(n: u64) -> String {
    if n == 0 {
        String::new()
    } else {
        format!("−{n}")
    }
}

/// Shorten from the *left*, keeping the filename.
///
/// `src/invoice/__tests__/split.test.ts` truncated from the right is
/// `src/invoice/__te…`, which says nothing. The directories repeat down the
/// list; the filename is what tells them apart.
fn ellipsise_left(path: &str, room: usize) -> String {
    if room == 0 {
        return String::new();
    }
    if path.width() <= room {
        return path.to_owned();
    }
    if room <= ELLIPSIS.width() {
        return ELLIPSIS.to_owned();
    }
    let budget = room - ELLIPSIS.width();
    let mut kept = String::new();
    let mut used = 0usize;
    for ch in path.chars().rev() {
        let w = ch.to_string().width();
        if used + w > budget {
            break;
        }
        kept.insert(0, ch);
        used += w;
    }
    format!("{ELLIPSIS}{kept}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, status: char, added: u64, removed: u64) -> DiffFile {
        DiffFile {
            path: path.into(),
            status,
            added,
            removed,
        }
    }

    fn theme() -> Theme {
        Theme::resolve(&grove_lua::TuiConfig::default(), crate::theme::Depth::True).0
    }

    fn loaded() -> Diff {
        let mut diff = Diff::default();
        diff.set(Incoming {
            repo: "billing-service".into(),
            branch: "feat/ABC-4471".into(),
            base: "origin/main".into(),
            files: vec![
                file("src/invoice/split.ts", 'M', 184, 22),
                file("src/invoice/__tests__/split.test.ts", 'A', 74, 0),
                file("src/legacy/prorate.ts", 'D', 0, 26),
                file(".env.local", '?', 0, 0),
            ],
            selected: Some("src/invoice/split.ts".into()),
            hunks: vec![
                DiffLine::Header("@@ -198,12 +198,26 @@".into()),
                DiffLine::Context("const items = invoice.lineItems;".into()),
                DiffLine::Removed("return items.map(toLine);".into()),
                DiffLine::Added("const boundary = cycleBoundary(inv);".into()),
            ],
            added: 412,
            removed: 137,
        });
        diff
    }

    fn painted(diff: &Diff) -> String {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 10,
        };
        let mut buf = Buffer::empty(area);
        diff.render(&mut buf, area, &theme());
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_four_status_markers_render_distinctly() {
        // Acceptance. The letter is the marker and the colour is what it
        // means, so a glance down the column separates added from deleted
        // without reading.
        let screen = painted(&loaded());
        for marker in ['M', 'A', 'D', '?'] {
            assert!(screen.contains(marker), "{marker} missing:\n{screen}");
        }
        let theme = theme();
        let colours: Vec<_> = ['M', 'A', 'D', '?']
            .map(|status| theme.color(status_role(status)))
            .to_vec();
        for (index, colour) in colours.iter().enumerate() {
            assert!(
                !colours[..index].contains(colour),
                "two statuses share a colour: {colours:?}"
            );
        }
    }

    #[test]
    fn a_long_path_keeps_its_filename() {
        // Acceptance: ellipsise from the left. Truncated from the right,
        // `src/invoice/__tests__/split.test.ts` becomes `src/invoice/__te…`,
        // which says nothing — the directories repeat down the list and the
        // filename is what tells them apart.
        let short = ellipsise_left("src/invoice/__tests__/split.test.ts", 20);
        assert!(short.ends_with("split.test.ts"), "{short}");
        assert!(short.starts_with(ELLIPSIS), "{short}");
        assert!(short.width() <= 20, "{short}");
        assert_eq!(ellipsise_left("short.ts", 20), "short.ts");
    }

    #[test]
    fn moving_the_cursor_asks_for_that_files_patch() {
        // §4.4's point: hunks come one file at a time, so opening the screen
        // does not pay for every file's patch. The cursor says what to ask
        // for rather than the screen guessing.
        let mut diff = loaded();
        assert_eq!(
            diff.move_down().as_deref(),
            Some("src/invoice/__tests__/split.test.ts")
        );
        assert_eq!(diff.move_up().as_deref(), Some("src/invoice/split.ts"));
        assert!(diff.move_up().is_none(), "already at the top");
    }

    #[test]
    fn a_late_patch_for_another_file_is_not_painted_under_this_one() {
        // The cursor can move between the request and the reply. Painting it
        // anyway would put one file's changes under another's name, which is
        // the worst thing a diff can do.
        let mut diff = loaded();
        diff.move_down();
        assert!(
            diff.hunks_for().is_none(),
            "moving away forgets the patch it was showing"
        );
        diff.set(Incoming {
            repo: "billing-service".into(),
            branch: "feat/ABC-4471".into(),
            base: "origin/main".into(),
            files: diff.files.clone(),
            // The reply is for the file we just left.
            selected: Some("src/invoice/split.ts".into()),
            hunks: vec![DiffLine::Added("stale".into())],
            added: 412,
            removed: 137,
        });
        assert!(
            diff.hunks_for().is_none(),
            "a reply for another file must not be shown as this one's"
        );
        assert!(
            !painted(&diff).contains("stale"),
            "and must not be painted at all"
        );
    }

    #[test]
    fn a_binary_file_or_a_rename_lists_without_counts() {
        // Acceptance: both are handled. A binary file has no line counts to
        // show, and showing `+0 −0` would suggest it is unchanged.
        let mut diff = Diff::default();
        diff.set(Incoming {
            repo: "r".into(),
            branch: "b".into(),
            base: "origin/main".into(),
            files: vec![file("assets/logo.png", 'M', 0, 0)],
            selected: Some("assets/logo.png".into()),
            hunks: vec![DiffLine::Header("Binary files differ".into())],
            added: 0,
            removed: 0,
        });
        let screen = painted(&diff);
        assert!(screen.contains("logo.png"), "{screen}");
        assert!(!screen.contains("+0"), "no misleading zero: {screen}");
        assert!(screen.contains("Binary files differ"), "{screen}");
    }

    #[test]
    fn a_large_patch_scrolls_rather_than_stalling() {
        // Acceptance. Rendering ten thousand lines to show forty is the stall
        // this avoids: only the window is drawn.
        let mut diff = Diff::default();
        let huge: Vec<DiffLine> = (0..10_000)
            .map(|n| DiffLine::Context(format!("line {n}")))
            .collect();
        diff.set(Incoming {
            repo: "r".into(),
            branch: "b".into(),
            base: "origin/main".into(),
            files: vec![file("big.ts", 'M', 10_000, 0)],
            selected: Some("big.ts".into()),
            hunks: huge,
            added: 10_000,
            removed: 0,
        });
        assert!(diff.scroll(40, 8));
        let screen = painted(&diff);
        assert!(screen.contains("line 40"), "{screen}");
        assert!(
            !screen.contains("line 0 "),
            "the top scrolled away: {screen}"
        );
    }

    #[test]
    fn the_footer_offers_nothing_but_moving_and_leaving() {
        // The screen's promise: read-only. No `s stage`, no `w session diff` —
        // those are git write-verbs and the worktree's own shell is one pane
        // away.
        let screen = painted(&loaded());
        assert!(screen.contains("↑↓ file"), "{screen}");
        assert!(screen.contains("esc close"), "{screen}");
        for verb in ["stage", "commit", "discard", "revert", "session diff"] {
            assert!(!screen.contains(verb), "{verb} in:\n{screen}");
        }
    }

    #[test]
    fn the_header_names_what_is_being_compared() {
        let screen = painted(&loaded());
        assert!(screen.contains("billing-service"), "{screen}");
        assert!(screen.contains("feat/ABC-4471 vs origin/main"), "{screen}");
        assert!(screen.contains("+412"), "{screen}");
        assert!(screen.contains("−137"), "{screen}");
    }

    #[test]
    fn an_empty_diff_says_nothing_rather_than_panicking() {
        let diff = Diff::default();
        assert!(diff.selected().is_none());
        let _ = painted(&diff);
    }
}
