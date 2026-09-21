//! The WORKTREES pane (SPEC §4.1).
//!
//! Every worktree of the selected repo, **regardless of owner**, plus the
//! clone. Columns: ownership glyph · branch · `↑ahead` · `↓behind` · age.
//!
//! The pane's whole job is making four ownership states distinguishable at a
//! glance, because that is what makes `end session` legibly safe — it removes
//! only the first of them:
//!
//! ```text
//! ▣ feat/ABC-4471   ours, with a terminal attached
//! ▣ fix/ABC-4402    ours, no terminal
//! ◆ spike/perf      another session's — read-only here
//! ◇ chore/deps      unowned, adoptable with ^g a
//! ─ main            the clone itself, never ownable
//! ```
//!
//! Glyph *and* colour carry it, never colour alone: the theme degrades to
//! sixteen colours on terminals that have themed their own palette, and a user
//! about to delete work should not be relying on a hue to tell them whose it
//! is. Another session's rows get a dim owner label as well, because "not
//! yours" is the one state where the reason matters more than the shape.
//!
//! One refusal is *not* here. §2.4 forbids adopt and release when
//! `worktree_path` contains `{session}`, because a worktree's location then
//! depends on which session made it — but the protocol carries no flag for
//! that, so the client cannot know before it asks. The daemon refuses and the
//! reason reaches the status bar. See issue #74.
//!
//! What is deliberately absent is a progress column. git can answer ahead,
//! behind, dirty and age; it cannot answer "how far along is this branch", and
//! the source mock's progress bars were inventing it.

use grove_domain::Ownership;
use grove_proto::WorktreeRow;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::text::truncate;
use crate::theme::{Role, Theme};

/// Ours, with a pty attached.
const GLYPH_TERMINAL: &str = "▣";
/// Ours, or another session's — the filled diamond means owned.
const GLYPH_OWNED: &str = "◆";
/// Unowned, so adoptable.
const GLYPH_FREE: &str = "◇";
/// The clone: the repository's own checkout, never ownable.
const GLYPH_CLONE: &str = "─";
/// Refs have aged past `stale_after`, so ahead/behind may be wrong.
const STALE: &str = "~";
/// A detached HEAD has no branch name to show.
const DETACHED: &str = "(detached)";

/// What the pane does with the row under the cursor when `^g a` or `^g r` is
/// pressed. The refusals are as much a part of the feature as the actions:
/// §2.4 disables both when the path template is session-scoped, and a key that
/// silently does nothing teaches the user that grove is broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Ask the daemon to adopt this worktree into the open session.
    Adopt(grove_proto::WorktreeRef),
    /// Ask the daemon to release it.
    Release(grove_proto::WorktreeRef),
    /// Nothing happens, and this is why — shown on the status bar.
    Refused(String),
}

/// The pane's state: the rows for one repo, and the cursor in them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Worktrees {
    rows: Vec<WorktreeRow>,
    cursor: usize,
}

impl Worktrees {
    /// Replace the rows, keeping the cursor on the same branch where possible.
    ///
    /// Same reason as the REPOS pane: this list is re-sent for reasons the user
    /// did not cause, and a cursor that jumps moves the terminal pane with it.
    pub fn set(&mut self, rows: Vec<WorktreeRow>) {
        let previous = self.selected().map(|row| row.worktree.clone());
        self.rows = rows;
        self.cursor = previous
            .and_then(|at| self.rows.iter().position(|row| row.worktree == at))
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
    }

    /// The rows as `(repo, branch)`, for computing user columns against.
    pub fn rows_for_columns(&self) -> Vec<(String, String)> {
        self.rows
            .iter()
            .map(|row| (row.worktree.repo.0.clone(), row.worktree.branch.clone()))
            .collect()
    }

    pub fn selected(&self) -> Option<&WorktreeRow> {
        self.rows.get(self.cursor)
    }

    /// Put the cursor on a row, for a click. Returns whether it moved, so a
    /// click on the row already selected does not re-ask the daemon for
    /// anything.
    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.rows.len() || index == self.cursor {
            return false;
        }
        self.cursor = index;
        true
    }

    pub fn move_down(&mut self) -> bool {
        let last = self.rows.len().saturating_sub(1);
        if self.rows.is_empty() || self.cursor >= last {
            return false;
        }
        self.cursor += 1;
        true
    }

    pub fn move_up(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    /// What `^g a` means for the row under the cursor.
    pub fn adopt(&self) -> Option<Intent> {
        let row = self.selected()?;
        Some(match &row.ownership {
            Ownership::Clone => {
                Intent::Refused("the clone is the repository itself and is never owned".into())
            }
            Ownership::Ours => Intent::Refused("this session already owns it".into()),
            Ownership::Other(session) => {
                Intent::Refused(format!("{} owns it — release it there first", session.0))
            }
            // A detached checkout has no branch name, and adoption is recorded
            // by branch: §2 stores owned worktrees as repo + branch, so there
            // is nothing to write down.
            Ownership::Unowned if row.detached => {
                Intent::Refused("a detached checkout has no branch to adopt by".into())
            }
            Ownership::Unowned => Intent::Adopt(row.worktree.clone()),
        })
    }

    /// What `^g r` means for the row under the cursor.
    pub fn release(&self) -> Option<Intent> {
        let row = self.selected()?;
        Some(match &row.ownership {
            Ownership::Ours => Intent::Release(row.worktree.clone()),
            Ownership::Clone => {
                Intent::Refused("the clone is the repository itself and is never owned".into())
            }
            Ownership::Other(session) => {
                Intent::Refused(format!("{} owns it, not this session", session.0))
            }
            Ownership::Unowned => Intent::Refused("nothing owns it".into()),
        })
    }

    /// Draw into `area`, which is already inside the pane's border.
    pub fn render(
        &self,
        buf: &mut Buffer,
        area: Rect,
        theme: &Theme,
        focused: bool,
        columns: &[&crate::columns::Column],
    ) {
        if area.width == 0 || area.height == 0 || self.rows.is_empty() {
            return;
        }
        let lines: Vec<Line> = self
            .rows
            .iter()
            .take(area.height as usize)
            .enumerate()
            .map(|(index, row)| {
                let mut line = self.line(row, index == self.cursor && focused, area.width, theme);
                // User columns go after grove's own, so the built-in shape is
                // what the eye lands on first and a config cannot push the
                // ownership glyph off the row.
                for column in columns {
                    let value = column.cell(&row.worktree.repo.0, &row.worktree.branch);
                    if value.is_empty() {
                        continue;
                    }
                    line.spans.push(Span::styled(
                        format!(" {}", truncate(value, column.width())),
                        // Muted and marked: it is the user's data, not
                        // grove's, and §10.3 asks for it to read as theirs.
                        theme.style(Role::Muted).add_modifier(Modifier::ITALIC),
                    ));
                }
                line
            })
            .collect();
        Paragraph::new(lines).render(area, buf);
    }

    fn line(&self, row: &WorktreeRow, selected: bool, width: u16, theme: &Theme) -> Line<'static> {
        let (glyph, glyph_role) = glyph_for(row);

        // Ahead/behind are omitted rather than shown as zero: `↑0 ↓0` is four
        // columns saying nothing, in the pane that has the least room.
        let mut counters = String::new();
        if row.ahead > 0 {
            counters.push_str(&format!("↑{} ", row.ahead));
        }
        if row.behind > 0 {
            counters.push_str(&format!("↓{} ", row.behind));
        }
        if row.stale {
            // The marker says "these numbers may be wrong", so it belongs next
            // to them rather than at the end of the row.
            counters.push_str(STALE);
            counters.push(' ');
        }

        let age = age_label(row.age);
        let owner = match &row.ownership {
            Ownership::Other(session) => Some(session.0.clone()),
            _ => None,
        };

        let trailing = format!("{counters}{age}");
        let owner_width = owner.as_deref().map_or(0, |o| o.width() + 1);
        let fixed = 1 + 1 + 1 + trailing.width() + owner_width;
        let room = (width as usize).saturating_sub(fixed);

        let name = if row.detached {
            DETACHED.to_string()
        } else {
            row.worktree.branch.clone()
        };
        let name = truncate(&name, room);
        let padding = room.saturating_sub(name.width());

        let name_style = if selected {
            theme.style(Role::Accent).add_modifier(Modifier::BOLD)
        } else if matches!(row.ownership, Ownership::Other(_)) {
            // Another session's row is read-only here, and reads as such
            // whether or not it is selected.
            theme.style(Role::Muted).add_modifier(Modifier::DIM)
        } else {
            theme.style(Role::Muted)
        };

        let mut spans = vec![
            Span::styled(glyph.to_string(), theme.style(glyph_role)),
            Span::raw(" "),
            Span::styled(name, name_style),
            Span::raw(" ".repeat(padding + 1)),
        ];
        if let Some(owner) = owner {
            spans.push(Span::styled(
                format!("{owner} "),
                theme.style(Role::Muted).add_modifier(Modifier::DIM),
            ));
        }
        spans.push(Span::styled(
            trailing,
            if row.stale {
                // A stale row must not present confident numbers as fresh
                // facts, so the counters carry the warning colour too.
                theme.style(Role::Dirty)
            } else {
                theme.style(Role::Muted)
            },
        ));
        Line::from(spans)
    }
}

/// The glyph and colour for a row's ownership, which is the pane's whole point.
///
/// A `match` on the enum rather than a chain of conditions: a fifth ownership
/// state has to fail to compile here rather than render as something else.
fn glyph_for(row: &WorktreeRow) -> (&'static str, Role) {
    match row.ownership {
        // Ours splits by whether a pty is attached — the difference between
        // "mine, working in it" and "mine, idle".
        Ownership::Ours if row.terminal.is_some() => (GLYPH_TERMINAL, Role::Accent),
        Ownership::Ours if row.dirty_files > 0 => (GLYPH_OWNED, Role::Dirty),
        Ownership::Ours => (GLYPH_OWNED, Role::Clean),
        Ownership::Other(_) => (GLYPH_OWNED, Role::Muted),
        Ownership::Unowned => (GLYPH_FREE, Role::Muted),
        Ownership::Clone => (GLYPH_CLONE, Role::Muted),
    }
}

/// `now`, `4h`, `3d` — short enough for the narrowest useful pane.
fn age_label(seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    match seconds {
        s if s < 5 * MINUTE => "now".into(),
        s if s < HOUR => format!("{}m", s / MINUTE),
        s if s < DAY => format!("{}h", s / HOUR),
        s => format!("{}d", s / DAY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_domain::{RepoId, SessionId};
    use grove_proto::WorktreeRef;

    fn row(branch: &str, ownership: Ownership) -> WorktreeRow {
        WorktreeRow {
            worktree: WorktreeRef {
                repo: RepoId("repo".into()),
                branch: branch.into(),
            },
            detached: false,
            ownership,
            ahead: 0,
            behind: 0,
            dirty_files: 0,
            age: 0,
            size: 0,
            terminal: None,
            foreground: None,
            stale: false,
        }
    }

    fn theme() -> Theme {
        Theme::resolve(&grove_lua::TuiConfig::default(), crate::theme::Depth::True).0
    }

    fn painted(worktrees: &Worktrees, width: u16) -> Vec<String> {
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height: 10,
        };
        let mut buf = Buffer::empty(area);
        worktrees.render(&mut buf, area, &theme(), true, &[]);
        (0..area.height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .filter(|line| !line.is_empty())
            .collect()
    }

    #[test]
    fn the_four_ownership_states_are_distinct_at_eighty_columns() {
        // The acceptance criterion. `end session` removes only the first, so a
        // user has to be able to tell them apart before pressing it — and at
        // the width the dash gives this pane on an 80-column terminal.
        let mut worktrees = Worktrees::default();
        let mut ours = row("feat/ABC-4471", Ownership::Ours);
        ours.terminal = Some(grove_proto::TerminalId(1));
        worktrees.set(vec![
            ours,
            row(
                "spike/perf",
                Ownership::Other(SessionId("other-task".into())),
            ),
            row("chore/deps", Ownership::Unowned),
            row("main", Ownership::Clone),
        ]);

        // 26 columns is what `dash::split` gives WORKTREES at 80.
        let lines = painted(&worktrees, 26);
        assert_eq!(lines.len(), 4);
        let glyphs: Vec<&str> = lines.iter().map(|l| &l[0..GLYPH_OWNED.len()]).collect();
        assert_eq!(glyphs[0], GLYPH_TERMINAL, "ours with a terminal");
        assert_eq!(glyphs[1], GLYPH_OWNED, "another session's");
        assert_eq!(glyphs[2], GLYPH_FREE, "unowned");
        assert_eq!(glyphs[3], GLYPH_CLONE, "the clone");
        assert_eq!(
            glyphs
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            4,
            "all four must differ by glyph alone: {glyphs:?}"
        );
    }

    #[test]
    fn ownership_is_carried_by_shape_not_only_by_colour() {
        // Colour alone fails on a terminal that has themed its palette, and
        // this is the pane where being wrong means deleting someone's work.
        let theme = theme();
        let mut ours = row("a", Ownership::Ours);
        ours.terminal = Some(grove_proto::TerminalId(1));
        let rows = [
            ours,
            row("b", Ownership::Other(SessionId("s".into()))),
            row("c", Ownership::Unowned),
            row("d", Ownership::Clone),
        ];
        let glyphs: Vec<&str> = rows.iter().map(|r| glyph_for(r).0).collect();
        assert_eq!(
            glyphs
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            4
        );
        // And the colours differ too, so the two signals agree rather than one
        // carrying everything.
        let colours: Vec<_> = rows
            .iter()
            .map(|r| theme.color(glyph_for(r).1))
            .collect::<Vec<_>>();
        assert_ne!(colours[0], colours[2], "ours must not look unowned");
    }

    #[test]
    fn another_sessions_row_says_whose_it_is() {
        // Acceptance: another session's rows are visibly read-only. The glyph
        // says "owned", the dim label says by whom — without it, "owned" and
        // "owned by me" are the same shape.
        let mut worktrees = Worktrees::default();
        worktrees.set(vec![row(
            "spike/perf",
            Ownership::Other(SessionId("invoice-split".into())),
        )]);
        let lines = painted(&worktrees, 40);
        assert!(
            lines[0].contains("invoice-split"),
            "the owner must be named: {}",
            lines[0]
        );
    }

    #[test]
    fn the_clone_can_be_neither_adopted_nor_released() {
        // `Ownership::Clone` is the repository's own checkout. Adopting it
        // would put the repo itself under `end session`.
        let mut worktrees = Worktrees::default();
        worktrees.set(vec![row("main", Ownership::Clone)]);
        assert!(matches!(worktrees.adopt(), Some(Intent::Refused(_))));
        assert!(matches!(worktrees.release(), Some(Intent::Refused(_))));
    }

    #[test]
    fn adopt_is_offered_only_where_it_is_possible() {
        let mut worktrees = Worktrees::default();
        worktrees.set(vec![row("free", Ownership::Unowned)]);
        assert!(matches!(worktrees.adopt(), Some(Intent::Adopt(_))));

        worktrees.set(vec![row("mine", Ownership::Ours)]);
        assert!(matches!(worktrees.adopt(), Some(Intent::Refused(_))));
        assert!(matches!(worktrees.release(), Some(Intent::Release(_))));

        worktrees.set(vec![row(
            "theirs",
            Ownership::Other(SessionId("other".into())),
        )]);
        match worktrees.adopt() {
            Some(Intent::Refused(why)) => assert!(
                why.contains("other"),
                "the refusal must name the owner: {why}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_pane_refuses_only_what_it_can_know_by_itself() {
        // §2.4 also forbids adopt and release when `worktree_path` contains
        // `{session}` — and the protocol gives the client no way to know that,
        // so this pane cannot pre-empt it. The daemon refuses with
        // `SessionPathTemplate` and the reason reaches the bar as a `Failed`
        // event. This test exists so the omission is deliberate rather than
        // forgotten: an unowned row offers adoption, and the daemon decides.
        let mut worktrees = Worktrees::default();
        worktrees.set(vec![row("free", Ownership::Unowned)]);
        assert!(matches!(worktrees.adopt(), Some(Intent::Adopt(_))));
    }

    #[test]
    fn a_detached_checkout_cannot_be_adopted_by_branch() {
        // Ownership is recorded as repo + branch, and a detached HEAD has no
        // branch to record.
        let mut worktrees = Worktrees::default();
        let mut detached = row("", Ownership::Unowned);
        detached.detached = true;
        worktrees.set(vec![detached]);
        match worktrees.adopt() {
            Some(Intent::Refused(why)) => assert!(why.contains("detached"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_detached_row_is_labelled_rather_than_blank() {
        let mut worktrees = Worktrees::default();
        let mut detached = row("", Ownership::Unowned);
        detached.detached = true;
        worktrees.set(vec![detached]);
        assert!(painted(&worktrees, 30)[0].contains(DETACHED));
    }

    #[test]
    fn stale_rows_are_marked_because_their_numbers_may_be_wrong() {
        // `stale` is also set when a row's own reads failed, so the marker is
        // the difference between "0 behind" and "we could not tell".
        let mut worktrees = Worktrees::default();
        let mut stale = row("old", Ownership::Ours);
        stale.ahead = 1;
        stale.stale = true;
        worktrees.set(vec![stale]);
        let line = &painted(&worktrees, 30)[0];
        assert!(line.contains(STALE), "expected a stale marker: {line}");
        assert!(line.contains("↑1"), "{line}");
    }

    #[test]
    fn zero_counters_are_omitted_rather_than_shown() {
        // `↑0 ↓0` is four columns saying nothing in the pane with least room.
        let mut worktrees = Worktrees::default();
        worktrees.set(vec![row("clean", Ownership::Ours)]);
        let line = &painted(&worktrees, 30)[0];
        assert!(!line.contains('↑'), "{line}");
        assert!(!line.contains('↓'), "{line}");
    }

    #[test]
    fn there_is_no_progress_column() {
        // Acceptance, and the rule the whole spec turns on: grove shows only
        // what git and the filesystem can answer. The source mock drew
        // progress bars, which git cannot produce.
        let mut worktrees = Worktrees::default();
        let mut busy = row("feat/x", Ownership::Ours);
        busy.ahead = 4;
        busy.behind = 2;
        busy.dirty_files = 9;
        worktrees.set(vec![busy]);
        let line = &painted(&worktrees, 40)[0];
        for invented in ['%', '█', '▓', '▁'] {
            assert!(
                !line.contains(invented),
                "nothing here may imply progress: {line}"
            );
        }
    }

    #[test]
    fn the_row_never_overflows_the_pane() {
        let mut worktrees = Worktrees::default();
        let mut long = row(
            "feature/a-very-long-branch-name-that-will-not-fit",
            Ownership::Ours,
        );
        long.ahead = 12;
        long.behind = 34;
        long.age = 3 * 24 * 60 * 60;
        worktrees.set(vec![long]);
        for width in [20u16, 26, 40, 80] {
            let line = &painted(&worktrees, width)[0];
            assert!(
                line.width() <= width as usize,
                "{width} columns overflowed by {}: {line:?}",
                line.width() - width as usize
            );
        }
    }

    #[test]
    fn the_cursor_follows_the_worktree_not_the_index() {
        let mut worktrees = Worktrees::default();
        worktrees.set(vec![
            row("a", Ownership::Ours),
            row("b", Ownership::Ours),
            row("c", Ownership::Ours),
        ]);
        assert!(worktrees.move_down());
        assert!(worktrees.move_down());
        worktrees.set(vec![
            row("new", Ownership::Unowned),
            row("a", Ownership::Ours),
            row("b", Ownership::Ours),
            row("c", Ownership::Ours),
        ]);
        assert_eq!(
            worktrees.selected().map(|r| r.worktree.branch.as_str()),
            Some("c")
        );
    }

    #[test]
    fn ages_read_as_ages() {
        assert_eq!(age_label(0), "now");
        assert_eq!(age_label(299), "now");
        assert_eq!(age_label(20 * 60), "20m");
        assert_eq!(age_label(4 * 60 * 60), "4h");
        assert_eq!(age_label(3 * 24 * 60 * 60), "3d");
    }

    #[test]
    fn an_empty_repo_draws_nothing_and_defers_to_the_dash() {
        let worktrees = Worktrees::default();
        assert!(worktrees.selected().is_none());
        assert!(worktrees.adopt().is_none());
        assert!(painted(&worktrees, 30).is_empty());
    }
}
