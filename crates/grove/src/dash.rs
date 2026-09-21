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

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::text;

use crate::keymap::{Focus, Screen};
use crate::theme::{Role, Theme};

/// A pane is identified by the focus it would hold.
///
/// Deliberately not a second enum: "the WORKTREES pane" and "focus on
/// WORKTREES" are the same three things, and two types for one set drift the
/// moment a fourth pane appears.
pub type Pane = Focus;

/// How a pane takes width, taken from the mock's flex line.
///
/// `Grove TUI v6` sizes the dash with CSS flex, and the numbers there are
/// already in characters — `flex:0 1 31ch` for REPOS, `flex:1 1 58ch` for
/// WORKTREES, `flex:1 1 44ch` for the terminal — so the bases and the grow
/// flags carry over to a terminal grid unchanged.
///
/// The floors do not come from the mock. Its `min-width`s (21, 34, 28) total
/// more than an 80-column terminal has once the gaps are paid for, which would
/// drop the terminal pane on the commonest terminal there is. The mock never
/// renders that narrow — below 760px it overflows rather than reflowing — so
/// it has no opinion to be faithful to, and these stay where they were.
struct Flex {
    /// The width the pane asks for.
    basis: u16,
    /// The width below which it is all border and no content, so not drawn.
    min: u16,
    /// Whether spare width is this pane's to take. REPOS is `flex:0`: a list
    /// of repository names does not get better with sixty columns.
    grow: bool,
}

const REPOS: Flex = Flex {
    basis: 31,
    min: 14,
    grow: false,
};
const WORKTREES: Flex = Flex {
    basis: 58,
    min: 26,
    grow: true,
};
const TERMINAL: Flex = Flex {
    basis: 44,
    min: 24,
    grow: true,
};

/// The gap the mock leaves between panes.
///
/// The panes are separate boxes with `gap:16px` between them — a little over
/// one character at the mock's font size — not cells of one frame. One column
/// is the terminal's nearest whole equivalent, and it is what keeps two
/// adjacent borders from reading as a single doubled line.
const GAP: u16 = 1;

fn flex(pane: Pane) -> Flex {
    match pane {
        Focus::Repos => REPOS,
        Focus::Worktrees => WORKTREES,
        Focus::Terminal => TERMINAL,
    }
}

/// The width below which a pane is not worth drawing at all.
fn floor(pane: Pane) -> u16 {
    flex(pane).min
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

    /// The visibility the empty state draws with.
    ///
    /// §4.1 draws two panes there rather than three: nothing is selected, so
    /// there is no terminal to show, and the guidance needs the width more
    /// than an empty box does. REPOS stays as the user left it.
    ///
    /// WORKTREES is forced on, which is what keeps the at-least-one rule true
    /// for a derived set. The toggles enforce that invariant as the user
    /// presses them, and deriving a *different* set behind their back walked
    /// straight past it: with no repos, hiding REPOS and then WORKTREES is two
    /// legal toggles — the terminal is still visible, so neither is refused —
    /// and hiding the terminal here left a screen with nothing on it at all.
    /// The guidance is also the only thing left to read in this state, so the
    /// pane that holds it is not one to lose.
    pub fn for_guidance(self) -> Self {
        let mut panes = self;
        panes.set(Focus::Worktrees, true);
        panes.set(Focus::Terminal, false);
        panes
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
            // The gaps are part of what has to fit: three panes at their floor
            // and no room for the two columns between them is not three panes.
            let needed: u16 = showing.iter().map(|pane| floor(*pane)).sum::<u16>()
                + GAP * (showing.len() as u16 - 1);
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

    fn set(&mut self, pane: Pane, rect: Option<Rect>) {
        match pane {
            Focus::Repos => self.repos = rect,
            Focus::Worktrees => self.worktrees = rect,
            Focus::Terminal => self.terminal = rect,
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
        // Past this pane and over the gap: the next box starts a column clear
        // of this one, as the mock's `gap:16px` leaves it.
        x = x.saturating_add(width).saturating_add(GAP);
    }
    areas
}

/// Column widths for the visible panes, left to right.
///
/// This is CSS flex, because the mock is: every pane starts at its basis, the
/// spare goes to the panes that grow, and a shortfall is taken from each in
/// proportion to how much width it asked for, never below its floor. The old
/// hand-rolled version took the shortfall from one pane at a time in a fixed
/// order, which is a different layout at every width but the one it was
/// checked at.
fn widths_for(total: u16, showing: &[Pane]) -> Vec<u16> {
    if showing.is_empty() {
        return Vec::new();
    }
    let gaps = GAP * (showing.len() as u16 - 1);
    let available = total.saturating_sub(gaps);
    let flexes: Vec<Flex> = showing.iter().map(|pane| flex(*pane)).collect();
    let mut widths: Vec<u16> = flexes.iter().map(|f| f.basis).collect();
    let claimed: u16 = widths.iter().copied().sum();

    if claimed <= available {
        let mut spare = available - claimed;
        let growers: Vec<usize> = flexes
            .iter()
            .enumerate()
            .filter(|(_, f)| f.grow)
            .map(|(i, _)| i)
            .collect();
        // Nothing here grows — REPOS alone, in practice. It still takes the
        // width rather than leaving a gap at the right edge, because a pane
        // that does not fill the dash looks like a pane that failed to draw.
        let growers = if growers.is_empty() {
            vec![widths.len() - 1]
        } else {
            growers
        };
        let each = spare / growers.len() as u16;
        for &i in &growers {
            widths[i] = widths[i].saturating_add(each);
            spare -= each;
        }
        // The odd column goes to the last grower — the terminal when it is
        // showing — so the panes end flush with the right edge.
        if let Some(&last) = growers.last() {
            widths[last] = widths[last].saturating_add(spare);
        }
        return widths;
    }

    // Too narrow for every basis. Shrink in proportion to what each asked for,
    // clamped at its floor, repeating because clamping one pane hands its
    // share back to the others.
    let mut shortfall = claimed - available;
    while shortfall > 0 {
        let shrinkable: Vec<usize> = (0..widths.len())
            .filter(|&i| widths[i] > flexes[i].min)
            .collect();
        if shrinkable.is_empty() {
            break;
        }
        let basis_sum: u32 = shrinkable.iter().map(|&i| u32::from(flexes[i].basis)).sum();
        let mut taken = 0u16;
        for &i in &shrinkable {
            if taken == shortfall {
                break;
            }
            // Rounded up, or a shortfall smaller than the pane count makes no
            // progress and the loop never ends.
            let share = (u32::from(shortfall) * u32::from(flexes[i].basis)).div_ceil(basis_sum);
            let give = (share as u16)
                .min(widths[i] - flexes[i].min)
                .min(shortfall - taken);
            widths[i] -= give;
            taken += give;
        }
        if taken == 0 {
            break;
        }
        shortfall -= taken;
    }
    if shortfall > 0 {
        // Only reachable with a single pane left, since `drawable` has already
        // dropped any pane whose floor does not fit. It takes the area it has
        // rather than the floor it wants: one cramped pane beats none.
        let last = widths.len() - 1;
        widths[last] = widths[last].saturating_sub(shortfall);
    }
    debug_assert!(
        widths.iter().copied().sum::<u16>() + gaps <= total,
        "the panes and their gaps must never claim more columns than the dash has"
    );
    widths
}

/// The label the mock puts at the top of each pane.
///
/// The terminal's is not a label at all — it is the selection, `repo · branch`
/// — so it is passed in. A pane headed `TERMINAL` tells you what you are
/// already looking at; the mock's tells you which worktree's shell it is,
/// which is the only question the header can answer.
fn label(pane: Pane, terminal: &str) -> &str {
    match pane {
        Focus::Repos => "REPOS",
        Focus::Worktrees => "WORKTREES",
        Focus::Terminal => terminal,
    }
}

/// Which dash list the arrows are driving, if any.
///
/// Three conditions, and each one has already been a bug once. The screen must
/// be the dash, because an overlay owns the keyboard while it is open and
/// focus still holds whichever pane was selected behind it — the same mistake
/// as the keymap's, one layer up: without this, `↓` in the session picker
/// scrolls the REPOS list nobody can see. The pane must be one that holds a
/// list, since the terminal takes keys rather than a cursor. And it must be
/// drawable at the current width, because a pane the dash dropped is not on
/// screen however the toggles are set.
pub fn list_under_arrows(screen: Screen, focus: Pane, panes: Panes, width: u16) -> Option<Pane> {
    if screen != Screen::Dash {
        return None;
    }
    if !panes.drawable(width).visible(focus) {
        return None;
    }
    match focus {
        Focus::Repos | Focus::Worktrees => Some(focus),
        // The terminal is a pty: its keys go to the program, not to a cursor.
        Focus::Terminal => None,
    }
}

/// Draw the frame and return the area inside each pane's border.
///
/// The frame is this module's; what goes in the areas it returns belongs to
/// the panes themselves — #19, #20 and #21. Returning the inner rects rather
/// than taking the contents as an argument keeps that split honest: this
/// module never learns what a repo is.
///
/// The focused pane is drawn in the accent colour and bold, everything else
/// muted. That is the whole focus indicator, and it is a colour *and* a weight
/// because colour alone disappears on a terminal that has themed its palette.
pub fn render(
    buf: &mut Buffer,
    area: Rect,
    panes: Panes,
    focus: Pane,
    theme: &Theme,
    terminal: &str,
) -> Areas {
    let areas = split(area, panes);
    let mut inner = Areas {
        repos: None,
        worktrees: None,
        terminal: None,
    };
    for pane in [Focus::Repos, Focus::Worktrees, Focus::Terminal] {
        let Some(rect) = areas.of(pane) else {
            continue;
        };
        let focused = pane == focus;
        // The mock's focus ring is the border colour and nothing else: accent
        // when focused, the frame grey when not. Bold as well, because colour
        // alone disappears on a terminal that has themed its palette.
        let border = if focused {
            theme.style(Role::Accent).add_modifier(Modifier::BOLD)
        } else {
            theme.frame_style()
        };
        let block = pane_block(theme).border_style(border);
        let within = block.inner(rect);
        block.render(rect, buf);
        inner.set(
            pane,
            header(buf, within, label(pane, terminal), focused, theme),
        );
    }
    inner
}

/// Write a pane's header and return what is left for its contents.
///
/// The header is a line *inside* the pane rather than a word let into the top
/// border, which is how the mock draws it and why the panes there have a
/// title, a gap, and then their rows. Returns `None` when the pane is too
/// short to have contents once the header has taken its line: a pane showing
/// only a header is not showing anything.
fn header(
    buf: &mut Buffer,
    within: Rect,
    label: &str,
    focused: bool,
    theme: &Theme,
) -> Option<Rect> {
    if within.width == 0 || within.height == 0 {
        return None;
    }
    let pad = theme.padding();
    let style = if focused {
        theme.style(Role::Accent)
    } else {
        theme.style(Role::Muted)
    };
    let room = within.width.saturating_sub(pad * 2);
    let line = Line::from(Span::styled(text::truncate(label, room as usize), style));
    let at = Rect {
        x: within.x + pad,
        y: within.y,
        width: room,
        height: 1,
    };
    Paragraph::new(line).render(at, buf);
    contents(within, theme)
}

/// Where a pane's rows go, given the area inside its border.
///
/// Pure, and the only place this arithmetic lives: drawing uses it to place
/// the rows, and the mouse uses it to find which row was clicked. Two copies
/// would agree until the day one was edited, and then a click would select
/// the row above the one under the pointer.
fn contents(within: Rect, theme: &Theme) -> Option<Rect> {
    if within.width == 0 || within.height == 0 {
        return None;
    }
    let pad = theme.padding();
    let room = within.width.saturating_sub(pad * 2);
    // The mock puts space under the header before the first row — 8px of
    // padding, which is a blank line here. Compact spends neither that line
    // nor the side columns.
    let gap = if pad == 0 { 0 } else { 1 };
    let taken = 1 + gap;
    let height = within.height.checked_sub(taken)?;
    if height == 0 {
        return None;
    }
    Some(Rect {
        x: within.x + pad,
        y: within.y + taken,
        width: room,
        height,
    })
}

/// The frame of each pane and the area its rows occupy, without drawing.
///
/// What [`render`] would produce for the same arguments, for a caller that
/// needs to know where things are rather than to put them there — the mouse.
pub fn layout(area: Rect, panes: Panes, theme: &Theme) -> (Areas, Areas) {
    let frames = split(area, panes);
    let mut rows = Areas {
        repos: None,
        worktrees: None,
        terminal: None,
    };
    for pane in [Focus::Repos, Focus::Worktrees, Focus::Terminal] {
        if let Some(rect) = frames.of(pane) {
            rows.set(pane, contents(pane_block(theme).inner(rect), theme));
        }
    }
    (frames, rows)
}

/// A pane's border, as drawn. Only its shape matters to [`layout`]; the
/// colours are [`render`]'s business.
fn pane_block(theme: &Theme) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(theme.border())
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
    fn the_panes_tile_the_width_with_one_gap_between_them() {
        // The mock separates the panes by `gap:16px` rather than sharing one
        // frame, so exactly one column stands between them — no more, and
        // never none, which would read as a single doubled border.
        for width in [80u16, 100, 120, 160, 200, 240] {
            let area = Rect { width, ..NARROW };
            let areas = split(area, Panes::default());
            let repos = areas.repos.expect("visible");
            let worktrees = areas.worktrees.expect("visible");
            let terminal = areas.terminal.expect("visible");
            assert_eq!(repos.x, 0);
            assert_eq!(
                worktrees.x,
                repos.x + repos.width + GAP,
                "one column between REPOS and WORKTREES"
            );
            assert_eq!(
                terminal.x,
                worktrees.x + worktrees.width + GAP,
                "one column between WORKTREES and the terminal"
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
        assert!(areas.repos.expect("visible").width >= REPOS.min);
        assert!(areas.worktrees.expect("visible").width >= WORKTREES.min);
        assert!(areas.terminal.expect("visible").width >= TERMINAL.min);
    }

    #[test]
    fn extra_width_is_shared_by_the_two_panes_that_grow() {
        // The mock's flex line: REPOS is `flex:0`, the other two are `flex:1`.
        // So spare width is split between WORKTREES and the terminal rather
        // than all landing on the terminal, and a repo list is never stretched
        // across eighty columns of whitespace.
        // Both wide enough that nothing is shrinking: REPOS is `flex:0 1`, so
        // it does give columns back when the dash is too narrow for every
        // basis — it just never takes any when there are spare.
        let narrow = split(Rect { width: 140, ..WIDE }, Panes::default());
        let wide = split(WIDE, Panes::default());
        assert_eq!(
            narrow.repos.expect("visible").width,
            wide.repos.expect("visible").width,
            "REPOS must not grow"
        );
        let grew_worktrees =
            wide.worktrees.expect("visible").width - narrow.worktrees.expect("visible").width;
        let grew_terminal =
            wide.terminal.expect("visible").width - narrow.terminal.expect("visible").width;
        assert!(grew_worktrees > 0 && grew_terminal > 0, "both must grow");
        assert!(
            grew_worktrees.abs_diff(grew_terminal) <= 1,
            "evenly, give or take the odd column: {grew_worktrees} vs {grew_terminal}"
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
        // One column wider than the two floors, because the gap between them
        // is part of what has to fit — at 40 even two panes do not.
        let tiny = Rect {
            width: 41,
            height: 10,
            ..NARROW
        };
        assert!(
            split(Rect { width: 40, ..tiny }, Panes::default())
                .worktrees
                .is_none(),
            "a column short of two floors plus their gap is one pane, not two"
        );
        let areas = split(tiny, Panes::default());
        assert!(
            areas.terminal.is_none(),
            "a pane that cannot be drawn is not on screen"
        );
        let repos = areas.repos.expect("visible");
        let worktrees = areas.worktrees.expect("visible");
        assert_eq!(repos.x, 0);
        assert_eq!(worktrees.x, repos.x + repos.width + GAP);
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
    fn corner(buf: &Buffer, rect: Rect) -> ratatui::style::Style {
        buf[(rect.x, rect.y)].style()
    }

    #[test]
    fn the_focused_pane_is_unmistakable() {
        // The acceptance criterion, asserted on what is actually painted. Both
        // signals are checked because colour alone disappears on a terminal
        // that themes its palette, and weight alone is subtle at a glance.
        let theme = theme();
        let mut buf = Buffer::empty(NARROW);
        let _ = render(
            &mut buf,
            NARROW,
            Panes::default(),
            Focus::Worktrees,
            &theme,
            "billing-service · feat/ABC-4471",
        );

        let areas = split(NARROW, Panes::default());
        let focused = corner(&buf, areas.worktrees.expect("visible"));
        let unfocused = corner(&buf, areas.repos.expect("visible"));

        assert_eq!(focused.fg, Some(theme.color(Role::Accent)));
        assert!(
            focused.add_modifier.contains(Modifier::BOLD),
            "the focused pane must differ by weight as well as colour"
        );
        assert_eq!(
            unfocused.fg,
            theme.frame_style().fg,
            "an unfocused pane recedes to the frame grey, as it does in the mock"
        );
        assert_ne!(
            unfocused.fg,
            Some(theme.color(Role::Accent)),
            "and is unmistakably not the focused one"
        );
        assert!(!unfocused.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn moving_focus_moves_the_indicator() {
        // Guards the pairing rather than a single frame: it would be possible
        // to draw one pane accented always and pass the test above.
        let theme = theme();
        let areas = split(NARROW, Panes::default());
        for focus in [Focus::Repos, Focus::Worktrees, Focus::Terminal] {
            let mut buf = Buffer::empty(NARROW);
            let _ = render(
                &mut buf,
                NARROW,
                Panes::default(),
                focus,
                &theme,
                "billing-service · feat/ABC-4471",
            );
            for pane in [Focus::Repos, Focus::Worktrees, Focus::Terminal] {
                let style = corner(&buf, areas.of(pane).expect("visible"));
                let expected = if pane == focus {
                    Some(theme.color(Role::Accent))
                } else {
                    theme.frame_style().fg
                };
                assert_eq!(style.fg, expected, "{pane:?} while focus is {focus:?}");
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
        let mut buf = Buffer::empty(NARROW);
        let _ = render(
            &mut buf,
            NARROW,
            panes,
            Focus::Worktrees,
            &theme,
            "billing-service · feat/ABC-4471",
        );

        let worktrees = split(NARROW, panes).worktrees.expect("visible");
        assert_eq!(worktrees.x, 0, "WORKTREES takes the left edge");
        // The mock heads a pane on the line below its top border, not in it.
        let painted: String = (0..NARROW.width)
            .map(|x| buf[(x, 1)].symbol().to_string())
            .collect();
        assert!(
            !painted.contains("REPOS"),
            "a hidden pane must not be painted: {painted}"
        );
        assert!(
            painted.contains("WORKTREES"),
            "the pane is headed inside itself: {painted}"
        );
    }

    #[test]
    fn an_overlay_takes_the_arrows_away_from_the_dash_lists() {
        // The review's medium, and the same class as the keymap's M4 one layer
        // up: focus still holds a dash pane while an overlay is open, so
        // asking focus alone scrolls a list behind the picker.
        for screen in [
            Screen::Palette,
            Screen::Picker,
            Screen::Diff,
            Screen::Shell,
            Screen::EndSession,
        ] {
            assert_eq!(
                list_under_arrows(screen, Focus::Repos, Panes::default(), 200),
                None,
                "{screen:?} owns the keyboard while it is open"
            );
        }
        assert_eq!(
            list_under_arrows(Screen::Dash, Focus::Repos, Panes::default(), 200),
            Some(Focus::Repos)
        );
    }

    #[test]
    fn the_terminal_pane_has_no_cursor_for_the_arrows_to_move() {
        // It is a pty: `↓` belongs to the program inside it.
        assert_eq!(
            list_under_arrows(Screen::Dash, Focus::Terminal, Panes::default(), 200),
            None
        );
    }

    #[test]
    fn a_pane_that_is_not_drawn_does_not_take_the_arrows() {
        // Whether by toggle or by width — both are "not on screen", and the
        // arrows must not drive either.
        let mut hidden = Panes::default();
        assert!(hidden.toggle(Focus::Repos));
        assert_eq!(
            list_under_arrows(Screen::Dash, Focus::Repos, hidden, 200),
            None,
            "a hidden pane"
        );
        assert_eq!(
            list_under_arrows(Screen::Dash, Focus::Terminal, Panes::default(), 40),
            None,
            "a pane too narrow to draw"
        );
    }

    #[test]
    fn the_empty_layout_never_draws_nothing() {
        // The review's medium. The toggles refuse to hide the last pane, but
        // a derived set does not go through them: with no repos, `^g 1` then
        // `^g 2` are both legal — the terminal is still visible — and the
        // empty state hid the terminal, leaving a blank screen.
        for hidden in [
            vec![],
            vec![Focus::Repos],
            vec![Focus::Worktrees],
            vec![Focus::Terminal],
            vec![Focus::Repos, Focus::Worktrees],
            vec![Focus::Repos, Focus::Terminal],
            vec![Focus::Worktrees, Focus::Terminal],
        ] {
            let mut panes = Panes::default();
            for pane in &hidden {
                let _ = panes.toggle(*pane);
            }
            let drawn = panes.for_guidance();
            assert!(
                drawn.count() >= 1,
                "hiding {hidden:?} left the empty dash with no panes"
            );
            assert!(
                drawn.visible(Focus::Worktrees),
                "the guidance has nowhere to go with {hidden:?} hidden"
            );
        }
    }

    #[test]
    fn the_empty_layout_leaves_the_users_own_toggles_alone() {
        // It decides what is drawn in this state, not what the user asked
        // for: REPOS stays hidden if they hid it, and everything comes back
        // when the dash fills.
        let mut panes = Panes::default();
        assert!(panes.toggle(Focus::Repos));
        let drawn = panes.for_guidance();
        assert!(!drawn.visible(Focus::Repos));
        assert!(
            !panes.visible(Focus::Repos) && panes.visible(Focus::Terminal),
            "the user's own set is untouched"
        );
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
