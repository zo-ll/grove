//! The help overlay (SPEC §3, issue #30).
//!
//! Generated from the keymap, never written out. A help screen that lists a
//! key the keymap no longer binds is worse than none: the user presses it,
//! nothing happens, and now the rest of the screen is suspect too. Everything
//! here comes from `keymap::bindings`, so a binding that moves moves here with
//! it.
//!
//! It leads with the prefix rule, because that is the one non-obvious thing
//! about grove and the one a new user gets wrong: the terminal panes take
//! every key, `^g` is how you address grove instead, and lists and overlays
//! take keys directly because there is no pty to compete with.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::keymap::{Screen, bindings, label_of};
use crate::theme::{Role, Theme};

/// The overlay's own state: which screen it is explaining, and how far down.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Help {
    /// First line shown. The overlay scrolls rather than truncating, because
    /// a terminal short enough to cut the list is exactly the one whose user
    /// needs the bottom of it.
    offset: usize,
}

impl Help {
    pub fn open(&mut self) {
        self.offset = 0;
    }

    /// Scroll, clamped to the content. Returns whether anything moved.
    pub fn scroll(&mut self, lines: isize, total: usize, room: usize) -> bool {
        let max = total.saturating_sub(room);
        let wanted = self.offset.saturating_add_signed(lines).min(max);
        if wanted == self.offset {
            return false;
        }
        self.offset = wanted;
        true
    }

    /// Everything the overlay would show for `screen`, before scrolling.
    ///
    /// Built here rather than inside `render` so the tests can read it and the
    /// scroll can measure it.
    pub fn lines(screen: Screen, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::styled(
                format!(" keys · {}", name(screen)),
                theme.style(Role::Accent).add_modifier(Modifier::BOLD),
            ),
            Line::from(""),
        ];

        // The prefix rule first, in the words that make it a rule rather than
        // a list of exceptions.
        for line in prefix_rule(screen) {
            lines.push(Line::styled(format!(" {line}"), theme.style(Role::Muted)));
        }
        lines.push(Line::from(""));

        let mut seen: Vec<&str> = Vec::new();
        for binding in bindings(screen) {
            if seen.contains(&binding.label) {
                continue;
            }
            seen.push(binding.label);
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {:<12}", label_of(binding)),
                    theme.style(Role::Accent),
                ),
                Span::styled(binding.label, theme.style(Role::Clean)),
            ]));
        }
        lines
    }

    /// Draw, from wherever the scroll has reached.
    pub fn render(&self, buf: &mut Buffer, area: Rect, screen: Screen, theme: &Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let all = Self::lines(screen, theme);
        let room = usize::from(area.height);
        let shown: Vec<Line<'static>> = all.into_iter().skip(self.offset).take(room).collect();
        Paragraph::new(shown).render(area, buf);
    }
}

/// How the prefix rule applies to this screen, which is not the same
/// everywhere — and the difference is the thing worth explaining.
fn prefix_rule(screen: Screen) -> Vec<&'static str> {
    if screen.is_pty_screen() || screen.pty_can_hold_focus() {
        vec![
            "a focused terminal takes every key it is given.",
            "^g is how you address grove instead: press it, then the key.",
            "^g ^g sends a literal ^g to the program.",
        ]
    } else {
        vec![
            "this screen is not a terminal, so keys reach it directly.",
            "no prefix here — ^g belongs to the panes behind.",
        ]
    }
}

fn name(screen: Screen) -> &'static str {
    match screen {
        Screen::Dash => "dash",
        Screen::Palette => "palette",
        Screen::Picker => "sessions",
        Screen::Prune => "prune",
        Screen::Diff => "diff",
        Screen::Shell => "scratch shell",
        Screen::EndSession => "end session",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::resolve(&grove_lua::TuiConfig::default(), crate::theme::Depth::True).0
    }

    fn text(screen: Screen) -> String {
        Help::lines(screen, &theme())
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
    fn every_binding_of_the_screen_appears() {
        // Acceptance: derived from the keymap, never a hardcoded list. Asserted
        // against the keymap itself, so a binding added there shows up here
        // without anyone remembering to add it.
        for screen in [Screen::Dash, Screen::Palette, Screen::Picker, Screen::Prune] {
            let shown = text(screen);
            for binding in bindings(screen) {
                assert!(
                    shown.contains(binding.label),
                    "{:?} does not mention {:?}:\n{shown}",
                    screen,
                    binding.label
                );
            }
        }
    }

    #[test]
    fn the_help_is_about_the_screen_you_are_on() {
        // Acceptance: context-sensitive. The dash's pane keys mean nothing in
        // the palette, and listing them there is how a help screen teaches the
        // wrong thing.
        let dash = text(Screen::Dash);
        let palette = text(Screen::Palette);
        assert!(dash.contains("adopt"), "{dash}");
        assert!(!palette.contains("adopt"), "{palette}");
        assert!(palette.contains("run"), "{palette}");
    }

    #[test]
    fn the_prefix_rule_is_explained_and_differs_where_it_applies() {
        // Acceptance: the one non-obvious thing about grove. On a screen with
        // a pty it is the rule; on an overlay the useful thing to say is that
        // it does not apply.
        let dash = text(Screen::Dash);
        assert!(dash.contains("^g is how you address grove"), "{dash}");
        assert!(dash.contains("^g ^g"), "{dash}");

        let picker = text(Screen::Picker);
        assert!(picker.contains("keys reach it directly"), "{picker}");
        assert!(!picker.contains("^g ^g"), "{picker}");
    }

    #[test]
    fn the_scratch_shell_gets_the_terminal_rule() {
        // It is a pty outright, so the prefix is the only way back.
        let shell = text(Screen::Shell);
        assert!(shell.contains("takes every key"), "{shell}");
    }

    #[test]
    fn a_short_terminal_scrolls_rather_than_hiding_the_rest() {
        // Acceptance. The terminal short enough to cut the list is exactly the
        // one whose user needs the bottom of it.
        let total = Help::lines(Screen::Dash, &theme()).len();
        let room = 6;
        assert!(total > room, "the dash has more keys than six lines");

        let mut help = Help::default();
        assert!(help.scroll(3, total, room));
        assert_eq!(help.offset, 3);
        // And it stops at the end rather than scrolling into blank space.
        assert!(help.scroll(100, total, room));
        assert_eq!(help.offset, total - room);
        assert!(!help.scroll(100, total, room), "already at the bottom");
        assert!(help.scroll(-100, total, room));
        assert_eq!(help.offset, 0);
    }

    #[test]
    fn the_key_column_shows_the_prefix_where_the_binding_has_one() {
        let dash = text(Screen::Dash);
        assert!(dash.contains("^g a"), "prefixed: {dash}");
        assert!(dash.contains("↑"), "unprefixed arrows: {dash}");
    }
}
