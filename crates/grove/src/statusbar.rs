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

use crate::keymap::{Binding, Focus, Screen, bindings};
use crate::theme::{Role, Theme};

/// Render a binding the way the bar labels it: `^g s sessions`.
fn hint(b: &Binding) -> String {
    let key = match b.key {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "shift-tab".into(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        other => format!("{other:?}").to_lowercase(),
    };
    if b.prefixed {
        format!("^g {key} {}", b.label)
    } else {
        format!("{key} {}", b.label)
    }
}

/// Hints for a screen, most important first, deduplicated by label so the
/// three pane-toggle keys read as one `^g 1-3 hide` rather than three entries.
pub fn hints(screen: Screen) -> Vec<String> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for b in bindings(screen) {
        if seen.contains(&b.label) {
            continue;
        }
        seen.push(b.label);
        out.push(hint(b));
    }
    out
}

/// Build the bar, dropping the lowest-priority hints when the terminal is too
/// narrow rather than wrapping or overflowing.
pub fn render(
    screen: Screen,
    focus: Focus,
    context: &str,
    session: &str,
    width: u16,
    theme: &Theme,
    note: Option<&str>,
) -> Line<'static> {
    let session_span = format!(" {session} ");
    let mut budget = width as usize;
    budget = budget.saturating_sub(session_span.len() + context.len() + 3);

    let mut spans: Vec<Span<'static>> = Vec::new();

    // A config grove could not read outranks the hints: the user changed
    // something and needs to know it did not take. It takes the left of the
    // bar and the hints give way, which is what the width budget is for.
    if let Some(note) = note {
        spans.push(Span::styled(
            format!("config: {note}  "),
            theme.style(Role::Error),
        ));
        budget = budget.saturating_sub(note.len() + 10);
    }

    for h in hints(screen) {
        if h.len() + 2 > budget {
            break;
        }
        budget -= h.len() + 2;
        spans.push(Span::styled(h, theme.style(Role::Muted)));
        spans.push(Span::raw("  "));
    }

    // Focus is part of the context, since the same arrow key means different
    // things depending on it — the bar should say which pane it will move.
    let where_ = match focus {
        Focus::Repos => "repos",
        Focus::Worktrees => "worktrees",
        Focus::Terminal => "terminal",
    };
    spans.push(Span::styled(
        format!("[{where_}] "),
        theme.style(Role::Muted).add_modifier(Modifier::DIM),
    ));
    spans.push(Span::raw(context.to_string()));
    spans.push(Span::styled(
        session_span,
        theme.style(Role::Accent).add_modifier(Modifier::BOLD),
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

    #[test]
    fn hints_come_from_the_keymap_not_a_hardcoded_list() {
        // The acceptance criterion for #16: move a binding and the bar follows.
        let dash = hints(Screen::Dash);
        assert!(dash.iter().any(|h| h.contains("sessions")));
        assert!(
            dash.iter().any(|h| h.starts_with("^g s")),
            "a prefixed binding must render with its prefix: {dash:?}"
        );
        assert!(
            dash.iter()
                .any(|h| h.starts_with('↑') || h.starts_with('↓')),
            "an unprefixed binding must render without one: {dash:?}"
        );
    }

    #[test]
    fn overlay_hints_carry_no_prefix() {
        // The picker is not a pty, so `^g` there would be noise the user has to
        // learn and then unlearn.
        for h in hints(Screen::Picker) {
            if h.contains("resume") || h.contains("detach") || h.contains("back") {
                assert!(!h.starts_with("^g"), "overlay hint wrongly prefixed: {h}");
            }
        }
    }

    #[test]
    fn a_narrow_bar_drops_hints_rather_than_overflowing() {
        let theme = theme();
        let wide = render(
            Screen::Dash,
            Focus::Worktrees,
            "ctx",
            "session",
            200,
            &theme,
            None,
        );
        let narrow = render(
            Screen::Dash,
            Focus::Worktrees,
            "ctx",
            "session",
            40,
            &theme,
            None,
        );
        assert!(
            narrow.spans.len() < wide.spans.len(),
            "a narrow terminal must shed hints"
        );
        let width: usize = narrow.spans.iter().map(|s| s.content.len()).sum();
        assert!(width <= 40, "rendered {width} columns into 40");
    }

    #[test]
    fn the_bar_says_which_pane_the_arrows_will_move() {
        // Focus changes what the same key does, so the bar has to show it.
        let theme = theme();
        let a = render(Screen::Dash, Focus::Repos, "", "s", 200, &theme, None);
        let b = render(Screen::Dash, Focus::Terminal, "", "s", 200, &theme, None);
        let text = |l: &Line| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert!(text(&a).contains("[repos]"));
        assert!(text(&b).contains("[terminal]"));
    }

    #[test]
    fn a_config_problem_is_shown_rather_than_swallowed() {
        // SPEC §9: a broken config reports and keeps running. A silent
        // fallback is the bad outcome — the user changed a setting, saw no
        // change, and has nothing to go on.
        let theme = theme();
        let bar = render(
            Screen::Dash,
            Focus::Worktrees,
            "",
            "s",
            200,
            &theme,
            Some("theme.accent is not a colour: \"peach\""),
        );
        let text: String = bar.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("theme.accent"), "bar said: {text}");
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
        let bar = render(Screen::Dash, Focus::Worktrees, "", "s", 200, &theme, None);
        let fg: Vec<_> = bar.spans.iter().filter_map(|s| s.style.fg).collect();
        assert!(
            fg.contains(&theme.color(Role::Muted)),
            "hints must be muted by the theme: {fg:?}"
        );
        assert!(
            fg.contains(&theme.color(Role::Accent)),
            "the session name must be the theme's accent: {fg:?}"
        );
    }
}
