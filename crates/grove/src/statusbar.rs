//! The status bar along the bottom (SPEC §4.1).
//!
//! Keyhints on the left, context in the middle, session name on the right.
//!
//! The hints are **generated from the keymap**, never written out here. A
//! hand-kept list would drift from the bindings the first time one moved, and
//! the bar would then teach the wrong keys — worse than showing none.

use ratatui::crossterm::event::KeyCode;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::keymap::{Binding, Screen, bar_bindings};
use ratatui::style::Style;

use unicode_width::UnicodeWidthStr;

use crate::text;
use crate::theme::{Ink, Role, Theme};

/// How the bar spells one key.
fn key_of(b: &Binding) -> String {
    match b.key {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "shift-tab".into(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// One entry on the bar: the keys that do it, and what it is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub keys: String,
    pub label: &'static str,
}

impl Hint {
    /// The width it takes, keys and label and the space between.
    fn width(&self) -> usize {
        self.keys.chars().count() + 1 + self.label.chars().count()
    }
}

/// The bar's entries for a screen, in table order.
///
/// Consecutive bindings that share a label are folded into one entry, because
/// three of them are one idea: `^g 1-3 hide`, not `^g 1 hide  ^g 2 hide  ^g 3
/// hide`, and `↑↓ file` rather than an arrow each. The mock folds them the
/// same way, and it is the only reason the dash's bar fits in a line.
pub fn hints(screen: Screen) -> Vec<Hint> {
    let mut out: Vec<Hint> = Vec::new();
    let mut run: Vec<&'static Binding> = Vec::new();

    let flush = |run: &mut Vec<&'static Binding>, out: &mut Vec<Hint>| {
        let Some(first) = run.first() else {
            return;
        };
        let keys: Vec<String> = run.iter().map(|b| key_of(b)).collect();
        let spelled = match keys.len() {
            1 => keys[0].clone(),
            // A run of single characters is a range — `1-3` — and anything
            // else is written out, which in practice means the two arrows.
            _ if keys.iter().all(|k| k.chars().count() == 1)
                && keys
                    .windows(2)
                    .all(|pair| next_char(&pair[0]) == Some(pair[1].clone())) =>
            {
                format!("{}-{}", keys[0], keys[keys.len() - 1])
            }
            _ => keys.concat(),
        };
        out.push(Hint {
            keys: if first.prefixed {
                format!("^g {spelled}")
            } else {
                spelled
            },
            label: first.label,
        });
        run.clear();
    };

    for binding in bar_bindings(screen) {
        if let Some(last) = run.last()
            && (last.label != binding.label || last.prefixed != binding.prefixed)
        {
            flush(&mut run, &mut out);
        }
        run.push(binding);
    }
    flush(&mut run, &mut out);
    out
}

/// The character after this one, when it is a single character.
fn next_char(key: &str) -> Option<String> {
    let mut chars = key.chars();
    let only = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    char::from_u32(u32::from(only) + 1).map(String::from)
}

/// Build the bar.
///
/// The mock's is a band of its own colour under the panes: hints on the left,
/// then what is selected, a rule, and the open session. When the width runs
/// out it is the *selection* that gives way — it is `overflow:hidden` there,
/// while the hints are not — so a narrow terminal ellipsizes `repo · branch`
/// rather than quietly unteaching the keys. The session is last to go,
/// because it is the only place grove says which session you are in.
pub fn render(
    screen: Screen,
    context: &str,
    session: &str,
    width: u16,
    theme: &Theme,
    note: Option<&str>,
) -> Line<'static> {
    let ground = theme.ink(Ink::Frame);
    let on_ground = |style: Style| style.bg(ground);
    let session_span = format!(" {session} ");
    let mut left: Vec<Span<'static>> =
        vec![Span::styled(" ".to_string(), on_ground(Style::default()))];
    let mut spent = 1 + session_span.chars().count();

    // A config grove could not read outranks the hints: the user changed
    // something and needs to know it did not take.
    if let Some(note) = note {
        let said = format!("config: {note}  ");
        spent += said.chars().count();
        left.push(Span::styled(said, on_ground(theme.style(Role::Error))));
    }

    for hint in hints(screen) {
        if spent + hint.width() + 2 > width as usize {
            break;
        }
        spent += hint.width() + 2;
        left.push(Span::styled(
            hint.keys,
            on_ground(theme.style(Role::Accent)),
        ));
        left.push(Span::styled(
            format!(" {}  ", hint.label),
            on_ground(theme.ink_style(Ink::Subtext)),
        ));
    }

    // What is left over is the selection's, ellipsized into it, and the rule
    // only exists if there is something on both sides of it.
    let rule = " │ ";
    // Columns, not bytes: `│` is three bytes wide and one column, and the
    // difference between those two numbers is a bar that does not reach the
    // right edge.
    let rule_width = rule.width();
    let room = (width as usize).saturating_sub(spent);
    let mut right: Vec<Span<'static>> = Vec::new();
    let mut shown = 0;
    if !context.is_empty() && room > rule_width + 1 {
        let trimmed = text::truncate(context, room - rule_width);
        shown = trimmed.width() + rule_width;
        right.push(Span::styled(trimmed, on_ground(theme.style(Role::Muted))));
        right.push(Span::styled(
            rule.to_string(),
            on_ground(theme.ink_style(Ink::Divider)),
        ));
    }

    // Everything on the left is left-aligned and everything on the right is
    // right-aligned; the gap between them is whatever nothing claimed.
    let mut spans = left;
    spans.push(Span::styled(
        " ".repeat(room - shown),
        on_ground(Style::default()),
    ));
    spans.extend(right);
    spans.push(Span::styled(
        session_span,
        on_ground(theme.style(Role::Accent)).add_modifier(Modifier::BOLD),
    ));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Depth;
    use grove_lua::TuiConfig;

    /// The default theme, since these tests are about layout rather than
    /// colour. Truecolor so a styled span carries the configured value.
    fn theme() -> Theme {
        Theme::resolve(&TuiConfig::default(), Depth::True).0
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn hints_come_from_the_keymap_not_a_hardcoded_list() {
        // The acceptance criterion for #16: move a binding and the bar follows.
        let dash = hints(Screen::Dash);
        assert!(dash.iter().any(|h| h.label == "sessions"));
        assert!(
            dash.iter().any(|h| h.keys == "^g s"),
            "a prefixed binding must render with its prefix: {dash:?}"
        );
    }

    #[test]
    fn the_dash_bar_reads_as_the_mock_writes_it() {
        // Five entries, in this order, and nothing else — the mock's bar. The
        // dash has fourteen bindings; a bar that listed them all would be a
        // wall of keys nobody reads, and `^g ?` is where the rest live.
        let dash: Vec<String> = hints(Screen::Dash)
            .iter()
            .map(|h| format!("{} {}", h.keys, h.label))
            .collect();
        assert_eq!(
            dash,
            [
                "^g tab pane",
                "^g 1-3 hide",
                "^g s sessions",
                "^g d diff",
                "^g / palette",
            ]
        );
    }

    #[test]
    fn keys_that_share_a_label_are_folded_into_one_entry() {
        // Three pane toggles are one idea, and so are two arrows. The mock
        // folds both, and it is the only reason the bar fits on a line.
        let hide = hints(Screen::Dash)
            .into_iter()
            .find(|h| h.label == "hide")
            .expect("the pane toggles");
        assert_eq!(hide.keys, "^g 1-3", "a run of keys is a range");

        let file = hints(Screen::Diff)
            .into_iter()
            .find(|h| h.label == "file")
            .expect("the diff's arrows");
        assert_eq!(file.keys, "↑↓", "two arrows are one entry");
    }

    #[test]
    fn overlay_hints_carry_no_prefix() {
        // The picker is not a pty, so `^g` there would be noise the user has to
        // learn and then unlearn.
        for h in hints(Screen::Picker) {
            assert!(
                !h.keys.starts_with("^g"),
                "overlay hint wrongly prefixed: {h:?}"
            );
        }
    }

    #[test]
    fn a_narrow_bar_drops_hints_rather_than_overflowing() {
        let theme = theme();
        let wide = render(Screen::Dash, "ctx", "session", 200, &theme, None);
        let narrow = render(Screen::Dash, "ctx", "session", 40, &theme, None);
        assert!(
            text(&wide).chars().count() > text(&narrow).chars().count(),
            "the narrow bar must have given something up"
        );
        let width: usize = narrow.spans.iter().map(|s| s.content.chars().count()).sum();
        assert!(width <= 40, "rendered {width} columns into 40");
    }

    #[test]
    fn the_session_survives_a_bar_too_narrow_for_anything_else() {
        // It is the only place grove says which session you are in. Hints are
        // repeated in `^g ?`; this is not repeated anywhere.
        let theme = theme();
        let bar = render(
            Screen::Dash,
            "billing · feat/x",
            "session: invoice split",
            30,
            &theme,
            None,
        );
        assert!(
            text(&bar).contains("invoice split"),
            "bar said: {}",
            text(&bar)
        );
    }

    #[test]
    fn the_bar_carries_the_selection_and_the_session() {
        // The mock's right-hand side: what is selected, a rule, the session.
        let theme = theme();
        let bar = render(
            Screen::Dash,
            "billing-service · feat/ABC-4471",
            "session: invoice split",
            200,
            &theme,
            None,
        );
        let said = text(&bar);
        assert!(said.contains("billing-service · feat/ABC-4471"), "{said}");
        assert!(said.contains(" │ "), "the mock rules them apart: {said}");
        assert!(said.contains("session: invoice split"), "{said}");
        assert!(
            said.find("billing-service") < said.find("session:"),
            "selection first, then the session: {said}"
        );
    }

    #[test]
    fn a_config_problem_is_shown_rather_than_swallowed() {
        // SPEC §9: a broken config reports and keeps running. A silent
        // fallback is the bad outcome — the user changed a setting, saw no
        // change, and has nothing to go on.
        let theme = theme();
        let bar = render(
            Screen::Dash,
            "",
            "s",
            200,
            &theme,
            Some("theme.accent is not a colour: \"peach\""),
        );
        assert!(
            text(&bar).contains("theme.accent"),
            "bar said: {}",
            text(&bar)
        );
        assert!(
            bar.spans
                .iter()
                .any(|s| s.style.fg == Some(theme.color(Role::Error))),
            "a config problem must read as a problem"
        );
    }

    #[test]
    fn the_bar_takes_its_colours_from_the_theme() {
        // Not from `Style::default()`, whose foreground is None: every styled
        // span here has to carry a colour the theme chose, or a user's config
        // does nothing to the one line that is always on screen.
        let theme = theme();
        let bar = render(Screen::Dash, "ctx", "s", 200, &theme, None);
        let fg: Vec<_> = bar.spans.iter().filter_map(|s| s.style.fg).collect();
        assert!(
            fg.contains(&theme.ink(Ink::Subtext)),
            "hint labels must be the mock's subtext: {fg:?}"
        );
        assert!(
            fg.contains(&theme.color(Role::Accent)),
            "the keys and the session name must be the theme's accent: {fg:?}"
        );
    }

    #[test]
    fn the_bar_is_a_band_of_its_own_colour_across_the_width() {
        // The mock's bar is a filled bar, not a line of text on the terminal's
        // background. Every column of it carries the ground, or the fill stops
        // wherever the hints happen to end.
        let theme = theme();
        let bar = render(Screen::Dash, "ctx", "session", 120, &theme, None);
        let width: usize = bar.spans.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(width, 120, "the bar must fill the width exactly");
        assert!(
            bar.spans
                .iter()
                .all(|s| s.style.bg == Some(theme.ink(Ink::Frame))),
            "every span must sit on the bar's ground"
        );
    }
}
