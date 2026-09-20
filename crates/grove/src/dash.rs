//! The dash's frame: REPOS │ WORKTREES │ terminal (SPEC §4.1).
//!
//! This module owns where the three panes are and which of them you can see.
//! What is *inside* them is #19, #20 and #21; the empty state is #22.
//!
//! Two rules do most of the work here, and both exist because the obvious
//! implementation gets them wrong:
//!
//! - **At least one pane is always visible.** `^g 1/2/3` toggle panes, and
//!   three independent booleans let a user hide all three and face a blank
//!   screen with no indication of which key brings anything back.
//! - **Focus only ever lands on something you can see.** Cycling focus through
//!   a hidden pane leaves the arrow keys driving a list that is not on screen,
//!   and hiding the focused pane has to move focus rather than orphan it.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::keymap::Focus;
use crate::theme::{Role, Theme};

/// A pane is identified by the focus it would hold.
///
/// Deliberately not a second enum: "the WORKTREES pane" and "focus on
/// WORKTREES" are the same three things, and two types for one set drift the
/// moment a fourth pane appears.
pub type Pane = Focus;

/// Smallest widths worth drawing. Below these a pane is all border and no
/// content, which is worse than the pane being absent.
const REPOS_IDEAL: u16 = 18;
const REPOS_MIN: u16 = 14;
const WORKTREES_MIN: u16 = 26;
const TERMINAL_MIN: u16 = 24;

/// The width below which a pane is not worth drawing at all.
fn floor(pane: Pane) -> u16 {
    match pane {
        Focus::Repos => REPOS_MIN,
        Focus::Worktrees => WORKTREES_MIN,
        Focus::Terminal => TERMINAL_MIN,
    }
}

/// Which panes the user can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    repos: bool,
    worktrees: bool,
    terminal: bool,
}

impl Default for Panes {
    fn default() -> Self {
        Self {
            repos: true,
            worktrees: true,
            terminal: true,
        }
    }
}

impl Panes {
    pub fn visible(self, pane: Pane) -> bool {
        match pane {
            Focus::Repos => self.repos,
            Focus::Worktrees => self.worktrees,
            Focus::Terminal => self.terminal,
        }
    }

    fn set(&mut self, pane: Pane, visible: bool) {
        match pane {
            Focus::Repos => self.repos = visible,
            Focus::Worktrees => self.worktrees = visible,
            Focus::Terminal => self.terminal = visible,
        }
    }

    /// How many panes are on screen.
    pub fn count(self) -> usize {
        [Focus::Repos, Focus::Worktrees, Focus::Terminal]
            .into_iter()
            .filter(|pane| self.visible(*pane))
            .count()
    }

    /// Show or hide a pane, refusing to hide the last one.
    ///
    /// Returns whether anything changed, so the caller can tell a refusal from
    /// a toggle rather than redrawing an identical screen.
    #[must_use]
    pub fn toggle(&mut self, pane: Pane) -> bool {
        if self.visible(pane) && self.count() == 1 {
            // The refusal is the feature: hiding the last pane leaves a blank
            // frame and no hint about which key undoes it.
            return false;
        }
        self.set(pane, !self.visible(pane));
        true
    }

    /// The pane `^g 1`, `^g 2` and `^g 3` address.
    ///
    /// `None` for anything else, because the binding table carries a number
    /// and nothing stops a fourth being added there before a fourth pane
    /// exists here.
    pub fn addressed(index: u8) -> Option<Pane> {
        match index {
            1 => Some(Focus::Repos),
            2 => Some(Focus::Worktrees),
            3 => Some(Focus::Terminal),
            _ => None,
        }
    }

    /// The next visible pane after `from`, cycling.
    ///
    /// Falls back to `from` only when nothing else is visible, which with the
    /// last-pane rule means `from` is the one pane on screen.
    pub fn next_visible(self, from: Pane) -> Pane {
        let mut candidate = from.next();
        for _ in 0..2 {
            if self.visible(candidate) {
                return candidate;
            }
            candidate = candidate.next();
        }
        from
    }

    /// The previous visible pane before `from`, cycling.
    pub fn previous_visible(self, from: Pane) -> Pane {
        let mut candidate = from.previous();
        for _ in 0..2 {
            if self.visible(candidate) {
                return candidate;
            }
            candidate = candidate.previous();
        }
        from
    }

    /// The panes that actually fit, which is not always the panes the user
    /// asked for.
    ///
    /// A terminal can be narrower than three panes need. The first version
    /// handed the shortfall to the rightmost pane and let it reach zero width:
    /// the pane was still "visible", still focusable, and painted nothing — the
    /// arrows-drive-an-invisible-list bug this module exists to prevent,
    /// arrived at from the other end. Panes are dropped from the right instead,
    /// because the terminal is the pane whose absence costs least, and the
    /// leftmost always survives so something is always on screen.
    pub fn drawable(self, width: u16) -> Self {
        let mut panes = self;
        loop {
            let showing: Vec<Pane> = [Focus::Repos, Focus::Worktrees, Focus::Terminal]
                .into_iter()
                .filter(|pane| panes.visible(*pane))
                .collect();
            let Some(last) = showing.last().copied() else {
                return panes;
            };
            if showing.len() == 1 {
                return panes;
            }
            let needed: u16 = showing.iter().map(|pane| floor(*pane)).sum();
            if needed <= width {
                return panes;
            }
            panes.set(last, false);
        }
    }

    /// Where focus belongs once `focus` may have become invisible.
    ///
    /// Hiding the pane you are in has to move you somewhere you can see, or
    /// the arrows drive a list that is not on screen.
    pub fn refocus(self, focus: Pane) -> Pane {
        if self.visible(focus) {
            focus
        } else {
            self.next_visible(focus)
        }
    }
}

/// Where each visible pane goes. `None` means hidden, not zero-width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Areas {
    pub repos: Option<Rect>,
    pub worktrees: Option<Rect>,
    pub terminal: Option<Rect>,
}

impl Areas {
    /// The area of a pane, if it is on screen.
    pub fn of(&self, pane: Pane) -> Option<Rect> {
        match pane {
            Focus::Repos => self.repos,
            Focus::Worktrees => self.worktrees,
            Focus::Terminal => self.terminal,
        }
    }
}

/// Divide the dash's area between the visible panes.
///
/// Widths follow §4.1: REPOS is fixed-ish, WORKTREES flexible, the terminal
/// takes the remainder. The arithmetic is done here rather than with layout
/// constraints because the interesting part is what happens when the width
/// runs out, and a constraint solver distributes the shortfall evenly across
/// panes that do not deserve it equally — the terminal can lose columns and
/// still be a terminal; REPOS at eight columns is a border.
pub fn split(area: Rect, panes: Panes) -> Areas {
    let mut areas = Areas {
        repos: None,
        worktrees: None,
        terminal: None,
    };
    if area.width == 0 || area.height == 0 {
        return areas;
    }

    // What fits, not what was asked for — a pane too narrow to draw is not on
    // screen, and `Areas` is what the rest of the UI trusts for that.
    let panes = panes.drawable(area.width);
    let showing: Vec<Pane> = [Focus::Repos, Focus::Worktrees, Focus::Terminal]
        .into_iter()
        .filter(|pane| panes.visible(*pane))
        .collect();

    let widths = widths_for(area.width, &showing);
    let mut x = area.x;
    for (pane, width) in showing.iter().zip(widths) {
        let rect = Rect {
            x,
            y: area.y,
            width,
            height: area.height,
        };
        match pane {
            Focus::Repos => areas.repos = Some(rect),
            Focus::Worktrees => areas.worktrees = Some(rect),
            Focus::Terminal => areas.terminal = Some(rect),
        }
        x = x.saturating_add(width);
    }
    areas
}

/// Column widths for the visible panes, left to right.
///
/// The order the shortfall is taken in is the whole of the degradation policy:
/// the terminal gives up columns first, then WORKTREES, and REPOS last —
/// REPOS is the narrowest and the one whose content is a fixed-width list of
/// names, so shaving it costs more than it saves.
fn widths_for(total: u16, showing: &[Pane]) -> Vec<u16> {
    let ideal = |pane: &Pane| match pane {
        Focus::Repos => REPOS_IDEAL,
        Focus::Worktrees => WORKTREES_MIN,
        Focus::Terminal => TERMINAL_MIN,
    };
    let mut widths: Vec<u16> = showing.iter().map(ideal).collect();
    let claimed: u16 = widths.iter().copied().sum();

    if claimed <= total {
        // Everything that is left goes to the rightmost flexible pane — the
        // terminal if it is showing, WORKTREES otherwise. REPOS never grows:
        // §4.1 calls it fixed-ish, and a half-empty 60-column list of repo
        // names is wasted width.
        let spare = total - claimed;
        let grow = showing
            .iter()
            .rposition(|pane| !matches!(pane, Focus::Repos))
            .or(showing.len().checked_sub(1));
        if let Some(index) = grow {
            widths[index] = widths[index].saturating_add(spare);
        }
        return widths;
    }

    // Too narrow for every pane's ideal. Take the shortfall in policy order,
    // never below a pane's floor, and if it still does not fit let the last
    // pane be clipped rather than wrapping the layout round to x = 0.
    let mut shortfall = claimed - total;
    for pane in [Focus::Terminal, Focus::Worktrees, Focus::Repos] {
        if shortfall == 0 {
            break;
        }
        let Some(index) = showing.iter().position(|p| *p == pane) else {
            continue;
        };
        let give = widths[index].saturating_sub(floor(pane)).min(shortfall);
        widths[index] -= give;
        shortfall -= give;
    }
    if shortfall > 0 {
        // Only reachable with a single pane left, since `drawable` has already
        // dropped any pane whose floor does not fit. It takes the area it has
        // rather than the floor it wants: one cramped pane beats none.
        let last = widths.len() - 1;
        widths[last] = widths[last].saturating_sub(shortfall);
    }
    debug_assert!(
        widths.iter().copied().sum::<u16>() <= total,
        "the panes must never claim more columns than the dash has"
    );
    widths
}

/// The title §4.1 puts on each pane.
///
/// The terminal's is the selected worktree once #21 fills it in; until then it
/// says what it is rather than lying about a worktree.
fn title(pane: Pane) -> &'static str {
    match pane {
        Focus::Repos => "REPOS",
        Focus::Worktrees => "WORKTREES",
        Focus::Terminal => "TERMINAL",
    }
}

/// What the pane says until the issue that fills it lands.
fn placeholder(pane: Pane) -> &'static str {
    match pane {
        Focus::Repos => "repos land in #19",
        Focus::Worktrees => "worktrees land in #20",
        Focus::Terminal => "the pty lands in #21",
    }
}

/// Draw the frame.
///
/// The focused pane is drawn in the accent colour and bold, everything else
/// muted. That is the whole focus indicator, and it has to survive the theme
/// degrading to sixteen colours — which is why it is a colour *and* a weight
/// rather than a colour alone.
pub fn render(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    panes: Panes,
    focus: Pane,
    theme: &Theme,
) {
    let areas = split(area, panes);
    for pane in [Focus::Repos, Focus::Worktrees, Focus::Terminal] {
        let Some(rect) = areas.of(pane) else {
            continue;
        };
        let focused = pane == focus;
        let style = if focused {
            theme.style(Role::Accent).add_modifier(Modifier::BOLD)
        } else {
            theme.style(Role::Muted)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(theme.border())
            .border_style(style)
            .title(Span::styled(title(pane), style));
        let inner = block.inner(rect);
        block.render(rect, buf);
        Paragraph::new(Line::styled(placeholder(pane), theme.style(Role::Muted)))
            .render(inner, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDE: Rect = Rect {
        x: 0,
        y: 0,
        width: 200,
        height: 40,
    };
    const NARROW: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 24,
    };

    #[test]
    fn the_last_pane_cannot_be_hidden() {
        // The acceptance criterion, and the reason visibility is a type rather
        // than three booleans: a blank dash gives the user nothing to press.
        let mut panes = Panes::default();
        assert!(panes.toggle(Focus::Repos));
        assert!(panes.toggle(Focus::Worktrees));
        assert_eq!(panes.count(), 1);
        assert!(
            !panes.toggle(Focus::Terminal),
            "hiding the last visible pane must be refused"
        );
        assert!(panes.visible(Focus::Terminal));
        assert_eq!(panes.count(), 1);
    }

    #[test]
    fn a_refused_toggle_is_distinguishable_from_one_that_happened() {
        // So the caller can avoid redrawing an unchanged screen, and later say
        // why nothing moved.
        let mut panes = Panes::default();
        assert!(panes.toggle(Focus::Repos), "hiding one of three changes it");
        assert!(panes.toggle(Focus::Repos), "showing it again changes it");
    }

    #[test]
    fn focus_skips_panes_that_are_not_on_screen() {
        // Cycling onto a hidden pane leaves the arrows driving a list nobody
        // can see — the bug this whole type exists to prevent.
        let mut panes = Panes::default();
        assert!(panes.toggle(Focus::Worktrees));
        assert_eq!(panes.next_visible(Focus::Repos), Focus::Terminal);
        assert_eq!(panes.previous_visible(Focus::Terminal), Focus::Repos);
    }

    #[test]
    fn cycling_with_one_pane_stays_put() {
        let mut panes = Panes::default();
        assert!(panes.toggle(Focus::Repos));
        assert!(panes.toggle(Focus::Terminal));
        assert_eq!(panes.next_visible(Focus::Worktrees), Focus::Worktrees);
        assert_eq!(panes.previous_visible(Focus::Worktrees), Focus::Worktrees);
    }

    #[test]
    fn hiding_the_focused_pane_moves_focus_somewhere_visible() {
        let mut panes = Panes::default();
        assert!(panes.toggle(Focus::Worktrees));
        let focus = panes.refocus(Focus::Worktrees);
        assert_ne!(focus, Focus::Worktrees);
        assert!(
            panes.visible(focus),
            "focus must land on something on screen"
        );
    }

    #[test]
    fn refocus_leaves_a_visible_focus_alone() {
        let panes = Panes::default();
        assert_eq!(panes.refocus(Focus::Worktrees), Focus::Worktrees);
    }

    #[test]
    fn the_panes_tile_the_width_exactly_and_in_order() {
        // No gaps, no overlaps, no column of the dash unaccounted for — and
        // left to right in the order §4.1 draws them.
        for width in [80u16, 100, 120, 160, 200, 240] {
            let area = Rect { width, ..NARROW };
            let areas = split(area, Panes::default());
            let repos = areas.repos.expect("visible");
            let worktrees = areas.worktrees.expect("visible");
            let terminal = areas.terminal.expect("visible");
            assert_eq!(repos.x, 0);
            assert_eq!(worktrees.x, repos.x + repos.width, "gap before WORKTREES");
            assert_eq!(
                terminal.x,
                worktrees.x + worktrees.width,
                "gap before terminal"
            );
            assert_eq!(
                terminal.x + terminal.width,
                width,
                "the panes must cover the dash at {width} columns"
            );
        }
    }

    #[test]
    fn eighty_columns_gives_every_pane_something_to_draw_in() {
        // The acceptance floor. Each pane needs room for two border columns
        // plus content; a pane narrower than its floor is all frame.
        let areas = split(NARROW, Panes::default());
        assert!(areas.repos.expect("visible").width >= REPOS_MIN);
        assert!(areas.worktrees.expect("visible").width >= WORKTREES_MIN);
        assert!(areas.terminal.expect("visible").width >= TERMINAL_MIN);
    }

    #[test]
    fn extra_width_goes_to_the_terminal_not_to_repos() {
        // §4.1: REPOS is fixed-ish. A repo list stretched across 80 columns is
        // whitespace where the terminal wanted characters.
        let narrow = split(NARROW, Panes::default());
        let wide = split(WIDE, Panes::default());
        assert_eq!(
            narrow.repos.expect("visible").width,
            wide.repos.expect("visible").width,
            "REPOS must not grow with the terminal"
        );
        assert!(
            wide.terminal.expect("visible").width > narrow.terminal.expect("visible").width + 100,
            "the terminal takes the remainder"
        );
    }

    #[test]
    fn a_hidden_pane_has_no_area_and_its_width_goes_to_the_others() {
        let mut panes = Panes::default();
        assert!(panes.toggle(Focus::Repos));
        let areas = split(WIDE, panes);
        assert!(areas.repos.is_none(), "hidden means absent, not zero-width");
        let worktrees = areas.worktrees.expect("visible");
        let terminal = areas.terminal.expect("visible");
        assert_eq!(
            worktrees.x, 0,
            "the leftmost visible pane starts at the edge"
        );
        assert_eq!(terminal.x + terminal.width, WIDE.width);
    }

    #[test]
    fn one_pane_takes_the_whole_width() {
        let mut panes = Panes::default();
        assert!(panes.toggle(Focus::Repos));
        assert!(panes.toggle(Focus::Terminal));
        let areas = split(WIDE, panes);
        let worktrees = areas.worktrees.expect("visible");
        assert_eq!(worktrees.x, 0);
        assert_eq!(worktrees.width, WIDE.width);
    }

    #[test]
    fn a_terminal_too_narrow_for_three_panes_drops_one_rather_than_starving_it() {
        // The review's finding, and it was worse than reported: the shortfall
        // used to come off the rightmost pane until it hit zero width, so the
        // terminal was "visible", focusable, zero columns wide — and the
        // panes together still overflowed the area.
        let tiny = Rect {
            width: 40,
            height: 10,
            ..NARROW
        };
        let areas = split(tiny, Panes::default());
        assert!(
            areas.terminal.is_none(),
            "a pane that cannot be drawn is not on screen"
        );
        let repos = areas.repos.expect("visible");
        let worktrees = areas.worktrees.expect("visible");
        assert_eq!(repos.x, 0);
        assert_eq!(worktrees.x, repos.x + repos.width);
        assert_eq!(
            worktrees.x + worktrees.width,
            tiny.width,
            "the survivors tile the width exactly"
        );
    }

    #[test]
    fn no_pane_is_ever_zero_width() {
        // Across every width from degenerate to wide, and every combination of
        // toggles: a zero-width pane is the arrows-drive-an-invisible-list bug
        // arrived at from the layout rather than from the toggle.
        for width in 0..=120u16 {
            // Every subset, not a sample: the two I first left out were the
            // ones where the surviving pane is not the leftmost.
            for hidden in [
                vec![],
                vec![Focus::Repos],
                vec![Focus::Worktrees],
                vec![Focus::Terminal],
                vec![Focus::Repos, Focus::Worktrees],
                vec![Focus::Repos, Focus::Terminal],
                vec![Focus::Worktrees, Focus::Terminal],
                vec![Focus::Repos, Focus::Worktrees, Focus::Terminal],
            ] {
                let mut panes = Panes::default();
                for pane in hidden {
                    let _ = panes.toggle(pane);
                }
                let area = Rect {
                    width,
                    height: 10,
                    ..NARROW
                };
                let areas = split(area, panes);
                let mut covered = 0u16;
                for pane in [Focus::Repos, Focus::Worktrees, Focus::Terminal] {
                    if let Some(rect) = areas.of(pane) {
                        assert!(rect.width > 0, "{pane:?} is zero columns at width {width}");
                        covered = covered.saturating_add(rect.width);
                    }
                }
                assert!(
                    covered <= width,
                    "the panes claim {covered} columns of {width}"
                );
            }
        }
    }

    #[test]
    fn the_narrowest_useful_terminal_keeps_one_pane() {
        // Something is always on screen, however little room there is, because
        // the alternative is a blank frame with no way back.
        let sliver = Rect {
            width: 12,
            height: 8,
            ..NARROW
        };
        let areas = split(sliver, Panes::default());
        assert!(areas.repos.is_some(), "the leftmost pane survives");
        assert!(areas.worktrees.is_none());
        assert!(areas.terminal.is_none());
        assert_eq!(areas.repos.expect("visible").width, sliver.width);
    }

    #[test]
    fn a_degenerate_area_produces_no_panes_rather_than_panicking() {
        let empty = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        let areas = split(empty, Panes::default());
        assert!(areas.repos.is_none() && areas.worktrees.is_none() && areas.terminal.is_none());
    }

    fn theme() -> Theme {
        // Truecolor, so a styled cell carries the configured value and the
        // assertions are about the theme rather than about degradation.
        Theme::resolve(&grove_lua::TuiConfig::default(), crate::theme::Depth::True).0
    }

    /// The style of the cell at a pane's top-left corner — its border.
    fn corner(buf: &ratatui::buffer::Buffer, rect: Rect) -> ratatui::style::Style {
        buf[(rect.x, rect.y)].style()
    }

    #[test]
    fn the_focused_pane_is_unmistakable() {
        // The acceptance criterion, asserted on what is actually painted. Both
        // signals are checked because colour alone disappears on a terminal
        // that themes its palette, and weight alone is subtle at a glance.
        let theme = theme();
        let mut buf = ratatui::buffer::Buffer::empty(NARROW);
        render(&mut buf, NARROW, Panes::default(), Focus::Worktrees, &theme);

        let areas = split(NARROW, Panes::default());
        let focused = corner(&buf, areas.worktrees.expect("visible"));
        let unfocused = corner(&buf, areas.repos.expect("visible"));

        assert_eq!(focused.fg, Some(theme.color(Role::Accent)));
        assert!(
            focused.add_modifier.contains(Modifier::BOLD),
            "the focused pane must differ by weight as well as colour"
        );
        assert_eq!(unfocused.fg, Some(theme.color(Role::Muted)));
        assert!(!unfocused.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn moving_focus_moves_the_indicator() {
        // Guards the pairing rather than a single frame: it would be possible
        // to draw one pane accented always and pass the test above.
        let theme = theme();
        let areas = split(NARROW, Panes::default());
        for focus in [Focus::Repos, Focus::Worktrees, Focus::Terminal] {
            let mut buf = ratatui::buffer::Buffer::empty(NARROW);
            render(&mut buf, NARROW, Panes::default(), focus, &theme);
            for pane in [Focus::Repos, Focus::Worktrees, Focus::Terminal] {
                let style = corner(&buf, areas.of(pane).expect("visible"));
                let expected = if pane == focus {
                    theme.color(Role::Accent)
                } else {
                    theme.color(Role::Muted)
                };
                assert_eq!(
                    style.fg,
                    Some(expected),
                    "{pane:?} while focus is {focus:?}"
                );
            }
        }
    }

    #[test]
    fn a_hidden_pane_paints_nothing() {
        // Hidden has to mean absent on screen too, not just absent from the
        // layout — a stale border left behind would be indistinguishable from
        // a pane that failed to draw.
        let theme = theme();
        let mut panes = Panes::default();
        assert!(panes.toggle(Focus::Repos));
        let mut buf = ratatui::buffer::Buffer::empty(NARROW);
        render(&mut buf, NARROW, panes, Focus::Worktrees, &theme);

        let worktrees = split(NARROW, panes).worktrees.expect("visible");
        assert_eq!(worktrees.x, 0, "WORKTREES takes the left edge");
        let painted: String = (0..NARROW.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            !painted.contains("REPOS"),
            "a hidden pane must not be painted: {painted}"
        );
        assert!(painted.contains("WORKTREES"));
    }

    #[test]
    fn the_toggle_keys_address_the_panes_left_to_right() {
        assert_eq!(Panes::addressed(1), Some(Focus::Repos));
        assert_eq!(Panes::addressed(2), Some(Focus::Worktrees));
        assert_eq!(Panes::addressed(3), Some(Focus::Terminal));
        assert_eq!(Panes::addressed(4), None, "there is no fourth pane");
        assert_eq!(Panes::addressed(0), None);
    }
}
