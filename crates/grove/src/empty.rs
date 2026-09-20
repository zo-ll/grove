//! What the dash says when there is nothing in it (SPEC §4.1).
//!
//! A fresh workspace must not render three blank boxes. The panes are still
//! there — the frame is what tells you where things will appear — but the
//! WORKTREES pane holds numbered guidance instead of an empty list.
//!
//! The two empty states are different problems and get different copy. A
//! workspace with no repositories at all needs a scan; a workspace full of
//! repositories none of which this session has adopted needs `add`. Telling
//! the second user to scan would send them looking for a fault that is not
//! there.
//!
//! The keys come from the keymap rather than from strings here, because
//! guidance that names the wrong key is worse than no guidance: the user
//! presses it, nothing happens, and now they distrust the rest of the screen.

use ratatui::text::{Line, Span};

use crate::keymap::{Action, key_label};
use crate::theme::{Ink, Role, Theme};

/// Why the dash has nothing to show.
///
/// Each variant names the thing that is missing, so `Empty::Session` reads as
/// "empty for want of a session". They were `Repos`, `Session` and
/// `Members` until there were three of them and the common prefix became
/// noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Empty {
    /// No repositories under the workspace at all.
    Repos,
    /// Repositories exist and nothing is open to put them in. §2.3: a session
    /// is explicit, and a fresh workspace has none.
    Session,
    /// A session is open, but holds none of the workspace's repositories.
    Members,
}

impl Empty {
    /// Which empty state the dash is in, if any.
    ///
    /// `workspace` counts every repository the daemon found; `members` counts
    /// the ones this session holds. A session with members is not empty even
    /// when the selected repo has nothing branched — that is the REPOS pane's
    /// `·`, a state rather than an absence.
    pub fn of(workspace: usize, members: usize, session: bool) -> Option<Self> {
        match (workspace, session, members) {
            (0, _, _) => Some(Self::Repos),
            // Before membership, because `add` refuses without somewhere to
            // add to. Telling a user to add repos to a session that does not
            // exist is a step that cannot be taken, and the first run was
            // doing exactly that: `^g / add` answered "no open session to add
            // in" to someone following the screen's own instructions.
            (_, false, _) => Some(Self::Session),
            (_, true, 0) => Some(Self::Members),
            _ => None,
        }
    }

    /// The headline, which names the situation rather than the remedy.
    pub fn headline(self) -> &'static str {
        match self {
            Self::Repos => "grove is empty",
            Self::Session => "no session open",
            Self::Members => "this session has no repos",
        }
    }

    /// The steps, in order, each with the key that performs it.
    ///
    /// A workspace with repositories already has step one behind it, so that
    /// user is not told to do it again.
    fn steps(self) -> &'static [(&'static str, Action, &'static str)] {
        match self {
            Self::Repos => &[
                ("point grove at your clones", Action::OpenPalette, "scan"),
                ("start a session", Action::OpenPalette, "session new <name>"),
                (
                    "add repos to this session",
                    Action::OpenPalette,
                    "add <repo>",
                ),
                ("name a branch", Action::PrefillNew, "new <branch>"),
            ],
            Self::Session => &[
                ("start a session", Action::OpenPalette, "session new <name>"),
                (
                    "add repos to this session",
                    Action::OpenPalette,
                    "add <repo>",
                ),
                ("name a branch", Action::PrefillNew, "new <branch>"),
            ],
            Self::Members => &[
                (
                    "add repos to this session",
                    Action::OpenPalette,
                    "add <repo>",
                ),
                ("name a branch", Action::PrefillNew, "new <branch>"),
            ],
        }
    }

    /// The guidance, ready to draw.
    pub fn lines(self, theme: &Theme) -> Vec<Line<'static>> {
        // The mock's weights: the situation is stated quietly, the steps read
        // as ordinary text, and the only thing in the accent colour is the key
        // you are being asked to press. Guidance that shouts competes with the
        // one part of itself that is actionable — and the step text was green,
        // which in this palette means "clean" and meant nothing here.
        let mut lines = vec![
            Line::styled(self.headline(), theme.ink_style(Ink::Subtext)),
            Line::from(""),
        ];
        for (index, (what, action, command)) in self.steps().iter().enumerate() {
            lines.push(Line::from(vec![
                Span::styled(format!("{}  ", index + 1), theme.ink_style(Ink::Faint)),
                Span::styled(*what, theme.ink_style(Ink::Subtext)),
            ]));
            lines.push(Line::from(vec![
                Span::raw("   "),
                // The key as the keymap spells it today. If the binding moves,
                // this moves with it.
                Span::styled(key_label(*action), theme.style(Role::Accent)),
                Span::styled(format!("  {command}"), theme.ink_style(Ink::Faint)),
            ]));
            lines.push(Line::from(""));
        }
        lines
    }

    /// What the REPOS pane says while it has nothing to list.
    ///
    /// Short enough for that pane, which is the narrowest on screen and does
    /// not grow: §4.1 calls it fixed-ish, so copy that needs more room than it
    /// has is copy that will be read with its end cut off.
    pub fn repos_note(self) -> &'static str {
        match self {
            Self::Session => "no session",
            Self::Repos => "no repos found",
            Self::Members => "none in session",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::resolve(&grove_lua::TuiConfig::default(), crate::theme::Depth::True).0
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn an_empty_workspace_is_told_to_scan() {
        assert_eq!(Empty::of(0, 0, false), Some(Empty::Repos));
        let rendered = text(&Empty::Repos.lines(&theme()));
        assert!(rendered.contains("grove is empty"), "{rendered}");
        assert!(rendered.contains("scan"), "{rendered}");
    }

    #[test]
    fn a_session_with_no_members_is_not_told_to_scan() {
        // The repos are already there. Telling this user to scan sends them
        // looking for a fault that is not there.
        assert_eq!(Empty::of(4, 0, true), Some(Empty::Members));
        let rendered = text(&Empty::Members.lines(&theme()));
        assert!(rendered.contains("no repos"), "{rendered}");
        assert!(
            !rendered.contains("scan"),
            "step one is already behind this user: {rendered}"
        );
        assert!(rendered.contains("add <repo>"), "{rendered}");
    }

    #[test]
    fn a_workspace_with_no_session_is_told_to_start_one_first() {
        // The first-run dead end: the screen said "add repos to this session"
        // when there was no session, `^g /  add` answered "no open session to
        // add in", and the user had followed the instructions exactly. §2.3
        // makes a session explicit, so the guidance has to.
        assert_eq!(Empty::of(4, 0, false), Some(Empty::Session));
        let rendered = text(&Empty::Session.lines(&theme()));
        let start = rendered.find("session new").expect("step one is a session");
        let add = rendered.find("add <repo>").expect("then repos");
        assert!(start < add, "in that order: {rendered}");
        assert!(
            !rendered.contains("scan"),
            "the repos are already found: {rendered}"
        );
    }

    #[test]
    fn an_empty_workspace_is_told_to_scan_before_anything_else() {
        let rendered = text(&Empty::Repos.lines(&theme()));
        let scan = rendered.find("scan").expect("scan");
        let start = rendered.find("session new").expect("then a session");
        assert!(scan < start, "{rendered}");
    }

    #[test]
    fn a_session_with_members_is_not_empty() {
        // Even when the selected repo has nothing branched — that is the
        // REPOS pane's `·`, a state rather than an absence.
        assert_eq!(Empty::of(4, 1, true), None);
        assert_eq!(Empty::of(1, 1, true), None);
    }

    #[test]
    fn the_keys_are_the_ones_the_keymap_binds() {
        // Guidance naming a key that does nothing is worse than no guidance:
        // the user presses it, nothing happens, and the rest of the screen is
        // now suspect. Asserted against the keymap rather than a literal.
        let rendered = text(&Empty::Repos.lines(&theme()));
        assert!(
            rendered.contains(&key_label(Action::OpenPalette)),
            "the palette's real key must appear: {rendered}"
        );
        assert!(
            rendered.contains(&key_label(Action::PrefillNew)),
            "and so must new's: {rendered}"
        );
    }

    #[test]
    fn the_steps_are_numbered_in_order() {
        let rendered = text(&Empty::Repos.lines(&theme()));
        let one = rendered.find('1').expect("a first step");
        let two = rendered.find('2').expect("a second");
        let three = rendered.find('3').expect("a third");
        assert!(one < two && two < three, "{rendered}");
    }

    #[test]
    fn no_key_is_spelled_out_in_this_module() {
        // The test above compares the guidance against `key_label`, so it
        // cannot fail when a binding moves — both sides move together. This is
        // the half that can: a literal key written here would survive the
        // binding changing underneath it, and the user would be told to press
        // something that does nothing.
        // Assembled rather than written, so this test's own source does not
        // trip the check it performs.
        let prefix = format!("{}g", '^');
        let source = include_str!("empty.rs");
        let offenders: Vec<&str> = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| line.contains(&prefix))
            .collect();
        assert!(
            offenders.is_empty(),
            "keys come from the keymap, not from strings here:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn each_state_says_something_different_in_the_repos_pane() {
        assert_ne!(Empty::Repos.repos_note(), Empty::Members.repos_note());
    }
}
