//! Where a click lands (#111).
//!
//! Pure, and fed the same geometry the dash was drawn with —
//! [`crate::dash::layout`] — so a click can only ever mean the row that is
//! under the pointer. The alternative, re-deriving positions here, is two
//! copies of one piece of arithmetic, and the day they disagree a click
//! selects the row above the one that was clicked.

use ratatui::layout::{Position, Rect};

use crate::dash::{Areas, Pane};
use crate::keymap::Focus;

/// What is under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// A row of a list pane, counted from the top of the list.
    Row(Pane, usize),
    /// Inside a pane but not on one of its rows: its border, its header, the
    /// space under the last row — or anywhere in the terminal, which is not a
    /// list.
    Pane(Pane),
    /// The gap between panes, or off the dash altogether.
    Nothing,
}

/// Resolve a position against the dash as it was drawn.
pub fn hit(frames: &Areas, rows: &Areas, column: u16, row: u16) -> Hit {
    let at = Position { x: column, y: row };
    for pane in [Focus::Repos, Focus::Worktrees, Focus::Terminal] {
        let Some(frame) = frames.of(pane) else {
            continue;
        };
        if !frame.contains(at) {
            continue;
        }
        if pane != Focus::Terminal
            && let Some(list) = rows.of(pane)
            && list.contains(at)
        {
            return Hit::Row(pane, usize::from(row - list.y));
        }
        return Hit::Pane(pane);
    }
    Hit::Nothing
}

/// Whether a position is on the status bar's session name.
///
/// The name is the rightmost thing on the bar, drawn as ` {session} `, so it
/// is the last `width` columns of the bottom row. The mock makes it a control
/// — `session: invoice split  ❯ switch` — and this is the terminal's version
/// of that.
pub fn on_session_name(bar: Rect, session: &str, column: u16, row: u16) -> bool {
    let width = u16::try_from(session.chars().count() + 2).unwrap_or(u16::MAX);
    row == bar.y && column >= bar.right().saturating_sub(width) && column < bar.right()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn areas(repos: Rect, worktrees: Rect, terminal: Rect) -> Areas {
        Areas {
            repos: Some(repos),
            worktrees: Some(worktrees),
            terminal: Some(terminal),
        }
    }

    const fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn dash() -> (Areas, Areas) {
        // Three panes with a gap between each, rows starting under a header.
        let frames = areas(rect(0, 0, 20, 10), rect(21, 0, 30, 10), rect(52, 0, 28, 10));
        let rows = areas(rect(2, 3, 16, 6), rect(23, 3, 26, 6), rect(54, 3, 24, 6));
        (frames, rows)
    }

    #[test]
    fn a_click_on_a_row_names_that_row() {
        let (frames, rows) = dash();
        assert_eq!(hit(&frames, &rows, 5, 3), Hit::Row(Focus::Repos, 0));
        assert_eq!(hit(&frames, &rows, 30, 5), Hit::Row(Focus::Worktrees, 2));
    }

    #[test]
    fn a_click_on_a_header_or_border_is_the_pane_not_a_row() {
        // Clicking a pane's title should focus it, not select whatever row
        // happens to be nearest.
        let (frames, rows) = dash();
        assert_eq!(hit(&frames, &rows, 5, 1), Hit::Pane(Focus::Repos));
        assert_eq!(hit(&frames, &rows, 0, 5), Hit::Pane(Focus::Repos));
    }

    #[test]
    fn the_terminal_is_a_pane_everywhere_never_a_list() {
        let (frames, rows) = dash();
        assert_eq!(hit(&frames, &rows, 60, 5), Hit::Pane(Focus::Terminal));
    }

    #[test]
    fn the_gap_between_panes_is_nothing() {
        // The column the mock leaves between panes belongs to neither.
        let (frames, rows) = dash();
        assert_eq!(hit(&frames, &rows, 20, 5), Hit::Nothing);
        assert_eq!(hit(&frames, &rows, 51, 5), Hit::Nothing);
    }

    #[test]
    fn a_hidden_pane_catches_nothing() {
        // Acceptance: mouse events never reach a pane that is not on screen.
        let (mut frames, mut rows) = dash();
        frames.repos = None;
        rows.repos = None;
        assert_eq!(hit(&frames, &rows, 5, 3), Hit::Nothing);
    }

    #[test]
    fn the_session_name_is_the_right_end_of_the_bar() {
        let bar = rect(0, 29, 100, 1);
        let name = "session: invoice split";
        let span = u16::try_from(name.len() + 2).unwrap();
        assert!(on_session_name(bar, name, 99, 29));
        assert!(on_session_name(bar, name, 100 - span, 29));
        assert!(
            !on_session_name(bar, name, 99 - span, 29),
            "one short of it"
        );
        assert!(
            !on_session_name(bar, name, 99, 28),
            "the row above is not the bar"
        );
    }
}
