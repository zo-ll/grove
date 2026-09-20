//! The REPOS pane (SPEC §4.1).
//!
//! The session's **member** repos, not the workspace's. Membership is explicit
//! and a member may hold no worktrees at all — that is the normal state of
//! "this task touches this repo, I have not branched yet", and it has to look
//! like a state rather than like an error or an empty row.
//!
//! Three columns: a dot for worktree presence and dirtiness, the repo name,
//! and the number of branched worktrees with the clone excluded. The count is
//! `·` rather than `0` for the same reason: zero is a number the user did
//! something to reach, and `·` reads as "nothing here yet".

use grove_proto::RepoRow;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::theme::{Role, Theme};

/// A repo that holds at least one branched worktree.
const DOT_PRESENT: &str = "●";
/// A member with no worktrees yet — explicit membership, nothing branched.
const DOT_EMPTY: &str = "○";
/// The count column when there is nothing to count.
const COUNT_NONE: &str = "·";
/// What a truncated name ends with. One column, so the budget arithmetic
/// stays honest.
const ELLIPSIS: &str = "…";

/// The pane's own state: which repos it shows and where the cursor is.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Repos {
    rows: Vec<RepoRow>,
    cursor: usize,
}

impl Repos {
    /// Replace the rows, keeping the cursor on the same repo where possible.
    ///
    /// The daemon re-sends the list for reasons that have nothing to do with
    /// the user — another session took a worktree, a scan finished — and a
    /// cursor that jumps to the top when that happens moves the WORKTREES pane
    /// out from under someone mid-keystroke.
    pub fn set(&mut self, rows: Vec<RepoRow>) {
        let previous = self.selected().map(|row| row.repo.clone());
        // Only members: §4.1 is explicit that this pane is the session's, not
        // the workspace's, and the workspace list belongs to the palette.
        self.rows = rows.into_iter().filter(|row| row.member).collect();
        self.cursor = previous
            .and_then(|repo| self.rows.iter().position(|row| row.repo == repo))
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
    }

    /// The rows as the pane holds them, for tests to assert against.
    ///
    /// Production code reads `selected()` — the pane draws itself, and the
    /// WORKTREES pane in #20 follows the selection, not the list. An accessor
    /// nothing calls is an invitation to reach past the pane and render its
    /// rows somewhere else.
    #[cfg(test)]
    pub fn rows(&self) -> &[RepoRow] {
        &self.rows
    }

    /// The selected repo, which is what the WORKTREES pane follows.
    pub fn selected(&self) -> Option<&RepoRow> {
        self.rows.get(self.cursor)
    }

    /// Move the cursor down, stopping at the end.
    ///
    /// Clamped rather than wrapping: this list sits beside another one, and a
    /// cursor that silently returns to the top makes "hold the down arrow"
    /// change the WORKTREES pane to something the user has already passed.
    pub fn move_down(&mut self) -> bool {
        let last = self.rows.len().saturating_sub(1);
        if self.rows.is_empty() || self.cursor >= last {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// Move the cursor up, stopping at the top.
    pub fn move_up(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    /// Draw the pane's contents into `area`, which is already inside the
    /// pane's border.
    ///
    /// An empty member list draws nothing here: SPEC §4.1's empty state is a
    /// whole-dash affair with numbered guidance, and half of it rendered in one
    /// pane would be worse than the space it fills. That is #22.
    pub fn render(&self, buf: &mut Buffer, area: Rect, theme: &Theme, focused: bool) {
        if area.width == 0 || area.height == 0 || self.rows.is_empty() {
            return;
        }
        let lines: Vec<Line> = self
            .rows
            .iter()
            .take(area.height as usize)
            .enumerate()
            .map(|(index, row)| self.line(row, index == self.cursor && focused, area.width, theme))
            .collect();
        Paragraph::new(lines).render(area, buf);
    }

    fn line(&self, row: &RepoRow, selected: bool, width: u16, theme: &Theme) -> Line<'static> {
        let dot = if row.worktrees == 0 {
            DOT_EMPTY
        } else {
            DOT_PRESENT
        };
        // Dirtiness is the dot's colour, so presence and state are one glyph
        // rather than two columns competing for a narrow pane.
        let dot_style = if row.dirty {
            theme.style(Role::Dirty)
        } else if row.worktrees == 0 {
            theme.style(Role::Muted)
        } else {
            theme.style(Role::Clean)
        };
        let count = if row.worktrees == 0 {
            COUNT_NONE.to_string()
        } else {
            row.worktrees.to_string()
        };

        // Dot, space, name, space, count — the name is what gives way when the
        // pane is narrow, because the other two are one column each and losing
        // either costs a whole signal.
        let fixed = 1 + 1 + 1 + count.chars().count();
        let room = (width as usize).saturating_sub(fixed);
        let name = truncate(&row.name, room);
        let padding = room.saturating_sub(name.chars().count());

        let name_style = if selected {
            theme.style(Role::Accent).add_modifier(Modifier::BOLD)
        } else {
            theme.style(Role::Muted)
        };

        Line::from(vec![
            Span::styled(dot.to_string(), dot_style),
            Span::raw(" "),
            Span::styled(name, name_style),
            Span::raw(" ".repeat(padding + 1)),
            Span::styled(count, theme.style(Role::Muted)),
        ])
    }
}

/// Shorten a name to `room` columns, ending in `…` when it does not fit.
///
/// Counts characters rather than display columns. Repo names come from
/// directory names, which are overwhelmingly ASCII; a wide character would
/// make this one column optimistic, and the alternative is a dependency for
/// the width table. Worth revisiting if a repo ever renders wrong.
fn truncate(name: &str, room: usize) -> String {
    if room == 0 {
        return String::new();
    }
    if name.chars().count() <= room {
        return name.to_owned();
    }
    if room == 1 {
        return ELLIPSIS.to_owned();
    }
    let kept: String = name.chars().take(room - 1).collect();
    format!("{kept}{ELLIPSIS}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_domain::RepoId;

    fn row(name: &str, worktrees: u32, dirty: bool) -> RepoRow {
        RepoRow {
            repo: RepoId(name.into()),
            name: name.into(),
            base_branch: "origin/main".into(),
            base_from_origin_head: true,
            worktrees,
            dirty,
            member: true,
        }
    }

    fn theme() -> Theme {
        Theme::resolve(&grove_lua::TuiConfig::default(), crate::theme::Depth::True).0
    }

    fn painted(repos: &Repos, width: u16, focused: bool) -> Vec<String> {
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height: 10,
        };
        let mut buf = Buffer::empty(area);
        repos.render(&mut buf, area, &theme(), focused);
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
    fn a_member_with_no_worktrees_is_a_state_not_an_absence() {
        // The acceptance criterion, and the case the pane exists to get right:
        // explicit membership means "this task touches this repo" before
        // anything is branched, so it needs a glyph of its own and a count
        // that does not read as a quantity.
        let mut repos = Repos::default();
        repos.set(vec![row("search-idx", 0, false)]);
        let lines = painted(&repos, 20, true);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with(DOT_EMPTY), "hollow dot: {}", lines[0]);
        assert!(lines[0].ends_with(COUNT_NONE), "no count: {}", lines[0]);
        assert!(!lines[0].contains('0'), "zero is a number, · is a state");
    }

    #[test]
    fn a_zero_worktree_member_is_still_selectable() {
        // It is a row like any other: the user adds a repo to the session and
        // then branches in it, which requires selecting it first.
        let mut repos = Repos::default();
        repos.set(vec![row("a", 0, false), row("b", 0, false)]);
        assert_eq!(repos.selected().map(|r| r.name.as_str()), Some("a"));
        assert!(repos.move_down());
        assert_eq!(repos.selected().map(|r| r.name.as_str()), Some("b"));
    }

    #[test]
    fn the_dot_carries_presence_and_dirtiness_together() {
        let theme = theme();
        let mut repos = Repos::default();
        repos.set(vec![
            row("clean", 2, false),
            row("dirty", 2, true),
            row("empty", 0, false),
        ]);
        let styles: Vec<_> = repos
            .rows()
            .iter()
            .map(|r| repos.line(r, false, 20, &theme).spans[0].style.fg)
            .collect();
        assert_eq!(styles[0], Some(theme.color(Role::Clean)));
        assert_eq!(styles[1], Some(theme.color(Role::Dirty)));
        assert_eq!(
            styles[2],
            Some(theme.color(Role::Muted)),
            "a member with nothing branched is not 'clean', it is 'not started'"
        );
    }

    #[test]
    fn a_long_name_ellipsises_without_widening_the_column() {
        // Acceptance: the pane is the narrowest of the three and a name that
        // overflows pushes the count off the edge, which loses a signal.
        let mut repos = Repos::default();
        repos.set(vec![row("a-very-long-repository-name-indeed", 3, false)]);
        for width in [10u16, 14, 18, 24] {
            let lines = painted(&repos, width, true);
            let rendered = &lines[0];
            assert!(
                rendered.chars().count() <= width as usize,
                "{width} columns overflowed: {rendered:?}"
            );
            assert!(
                rendered.ends_with('3'),
                "the count must survive truncation: {rendered:?}"
            );
            assert!(
                rendered.contains(ELLIPSIS),
                "expected an ellipsis: {rendered:?}"
            );
        }
    }

    #[test]
    fn a_name_that_fits_is_not_ellipsised() {
        assert_eq!(truncate("web-app", 10), "web-app");
        assert_eq!(truncate("web-app", 7), "web-app");
        assert_eq!(truncate("web-app", 6), "web-a…");
        assert_eq!(truncate("web-app", 1), ELLIPSIS);
        assert_eq!(truncate("web-app", 0), "");
    }

    #[test]
    fn only_the_sessions_members_are_listed() {
        // §4.1: this pane is the session's, not the workspace's. The workspace
        // list is the palette's job, and showing it here would make membership
        // look like something grove decided rather than something the user did.
        let mut repos = Repos::default();
        let mut outsider = row("not-a-member", 4, false);
        outsider.member = false;
        repos.set(vec![row("member", 1, false), outsider]);
        assert_eq!(repos.rows().len(), 1);
        assert_eq!(repos.selected().map(|r| r.name.as_str()), Some("member"));
    }

    #[test]
    fn refreshing_the_list_keeps_the_cursor_on_the_same_repo() {
        // The daemon re-sends for reasons the user did not cause. A cursor
        // that resets to the top moves the WORKTREES pane out from under them.
        let mut repos = Repos::default();
        repos.set(vec![
            row("a", 1, false),
            row("b", 1, false),
            row("c", 1, false),
        ]);
        assert!(repos.move_down());
        assert!(repos.move_down());
        assert_eq!(repos.selected().map(|r| r.name.as_str()), Some("c"));

        // Same repos, one of them now dirty, and a new one first.
        repos.set(vec![
            row("new", 1, false),
            row("a", 1, true),
            row("b", 1, false),
            row("c", 1, false),
        ]);
        assert_eq!(
            repos.selected().map(|r| r.name.as_str()),
            Some("c"),
            "the cursor follows the repo, not the index"
        );
    }

    #[test]
    fn a_selected_repo_that_disappears_leaves_the_cursor_in_bounds() {
        let mut repos = Repos::default();
        repos.set(vec![row("a", 1, false), row("b", 1, false)]);
        assert!(repos.move_down());
        repos.set(vec![row("a", 1, false)]);
        assert_eq!(repos.selected().map(|r| r.name.as_str()), Some("a"));
    }

    #[test]
    fn the_cursor_stops_at_the_ends_rather_than_wrapping() {
        let mut repos = Repos::default();
        repos.set(vec![row("a", 1, false), row("b", 1, false)]);
        assert!(!repos.move_up(), "already at the top");
        assert!(repos.move_down());
        assert!(!repos.move_down(), "already at the bottom");
        assert_eq!(repos.selected().map(|r| r.name.as_str()), Some("b"));
    }

    #[test]
    fn an_empty_member_list_draws_nothing_and_defers_to_the_dash() {
        // Acceptance: the empty state is a whole-dash affair with numbered
        // guidance (#22). Half of it inside one pane would be worse than the
        // space it fills.
        let repos = Repos::default();
        assert!(repos.selected().is_none());
        assert!(painted(&repos, 20, true).is_empty());
    }

    #[test]
    fn the_selection_is_only_highlighted_in_the_focused_pane() {
        // Two lists sit side by side and only one of them is being driven;
        // highlighting both makes it ambiguous which arrows move.
        let theme = theme();
        let mut repos = Repos::default();
        repos.set(vec![row("a", 1, false)]);
        let focused = repos.line(&repos.rows()[0], true, 20, &theme);
        let unfocused = repos.line(&repos.rows()[0], false, 20, &theme);
        assert_eq!(focused.spans[2].style.fg, Some(theme.color(Role::Accent)));
        assert_ne!(unfocused.spans[2].style.fg, Some(theme.color(Role::Accent)));
    }
}
