//! The chrome every overlay shares (SPEC §4.2-§4.5).
//!
//! In the mock the palette, the session picker, the prune list, the diff and
//! the end-session confirm are all one shape: the dash dimmed behind, and a
//! box of at most 98 columns centred over it with an accent border, a header
//! line, its rows, and a footer of the keys that work there.
//!
//! That shape lives here rather than in each screen, because five copies of it
//! is five places for it to drift — which is what happened to the pane
//! headers before this.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

use crate::statusbar;
use crate::theme::{Ink, Theme};

/// The widest the mock lets an overlay be.
const IDEAL: u16 = 98;

/// Columns between the border and the contents — the mock's `padding:0 22px`.
const PAD: u16 = 2;

/// The parts of an overlay its screen fills in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parts {
    /// One line: `❯ query` and whatever count sits on the right.
    pub header: Rect,
    /// The rows.
    pub body: Rect,
}

impl Parts {
    /// The parts of a plain rectangle, laid out as the chrome lays them.
    ///
    /// For a caller that has already made the room — the screens' own tests,
    /// which are about what a row says rather than about the box around it.
    #[cfg(test)]
    pub fn of(area: Rect) -> Self {
        Self {
            header: Rect {
                height: 1.min(area.height),
                ..area
            },
            body: Rect {
                y: area.y + 2,
                height: area.height.saturating_sub(2),
                ..area
            },
        }
    }
}

/// Where everything in an overlay is, without drawing it.
///
/// [`render`] draws from this, and the mouse resolves clicks against it, so
/// a click on `enter resume` is a click on the columns `enter resume` was
/// painted in — not on a second guess at where they probably are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The box, border included. A click outside it closes the overlay.
    pub frame: Rect,
    pub parts: Parts,
    /// Each footer hint and the columns it was drawn in, left to right, with
    /// `esc` last because it is drawn at the right.
    pub buttons: Vec<(Rect, statusbar::Hint)>,
}

/// Lay an overlay out: a box of at most [`IDEAL`] columns, centred, as tall as
/// `rows` of content need.
pub fn layout(area: Rect, rows: u16, hints: Vec<statusbar::Hint>) -> Layout {
    // Two borders, the header and the blank under it, and the footer with
    // its own blank line above: six rows that are not content.
    let wanted = rows.saturating_add(6);
    let height = wanted.min(area.height);
    let width = IDEAL.min(area.width);
    let frame = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let within = Block::default().borders(Borders::ALL).inner(frame);
    let inner = Rect {
        x: within.x + PAD,
        y: within.y,
        width: within.width.saturating_sub(PAD * 2),
        height: within.height,
    };
    if inner.width == 0 || inner.height == 0 {
        return Layout {
            frame,
            parts: Parts {
                header: inner,
                body: inner,
            },
            buttons: Vec::new(),
        };
    }
    Layout {
        frame,
        parts: Parts {
            header: Rect { height: 1, ..inner },
            // The header, the blank line under it, and the footer with its
            // own blank line above are all spoken for.
            body: Rect {
                y: inner.y + 2,
                height: inner.height.saturating_sub(4),
                ..inner
            },
        },
        buttons: buttons(inner, hints),
    }
}

/// Draw the chrome and return the areas inside it.
///
/// `rows` is how many lines of content the screen has to show. The box is
/// `height:fit-content` in the mock, so an overlay with three rows in it is
/// three rows tall rather than a tall box with a lot of nothing under the
/// last one.
pub fn render(
    buf: &mut Buffer,
    area: Rect,
    border: Style,
    rows: u16,
    hints: Vec<statusbar::Hint>,
    theme: &Theme,
) -> Parts {
    dim_behind(buf, area, theme);
    let laid = layout(area, rows, hints);
    Clear.render(laid.frame, buf);
    Block::default()
        .borders(Borders::ALL)
        .border_type(theme.border())
        .border_style(border)
        .render(laid.frame, buf);
    for (at, hint) in &laid.buttons {
        Paragraph::new(Line::from(vec![
            Span::styled(hint.keys.clone(), theme.style(crate::theme::Role::Accent)),
            Span::styled(format!(" {}", hint.label), theme.ink_style(Ink::Subtext)),
        ]))
        .render(*at, buf);
    }
    laid.parts
}

/// Dim whatever the overlay is covering.
///
/// The mock lays the overlay over a 66%-opaque wash of the base colour, so the
/// dash is still there and plainly not what you are typing into. There is no
/// alpha in a terminal, so the cells keep their characters and lose their
/// colour — the same effect by the only means available, and better than
/// blanking the screen, which throws away the context the overlay was opened
/// from.
fn dim_behind(buf: &mut Buffer, area: Rect, theme: &Theme) {
    let washed = Style::default()
        .fg(theme.ink(Ink::Frame))
        .bg(theme.ink(Ink::Backdrop));
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_style(washed);
        }
    }
}

/// Where each footer hint goes along the bottom of the box.
///
/// Taken from the keymap by way of the status bar, so an overlay's footer and
/// the bar under it can never disagree about what `enter` does. `esc` goes to
/// the right, on its own, as the mock puts it. Hints that do not fit are left
/// off rather than clipped: half a hint is a key with no verb.
fn buttons(inner: Rect, hints: Vec<statusbar::Hint>) -> Vec<(Rect, statusbar::Hint)> {
    if inner.height < 2 {
        return Vec::new();
    }
    let row = inner.y + inner.height - 1;
    let (escape, rest): (Vec<_>, Vec<_>) = hints.into_iter().partition(|h| h.keys == "esc");
    let escape_width = escape.first().map_or(0, statusbar::Hint::width);
    let room = usize::from(inner.width).saturating_sub(escape_width);

    let mut out = Vec::new();
    let mut spent = 0usize;
    for hint in rest {
        let width = hint.width();
        if spent + width > room {
            break;
        }
        out.push((
            Rect {
                x: inner.x + spent as u16,
                y: row,
                width: width as u16,
                height: 1,
            },
            hint,
        ));
        // Two columns between hints, as the mock spaces them.
        spent += width + 2;
    }
    if let Some(escape) = escape.into_iter().next() {
        let width = escape.width() as u16;
        out.push((
            Rect {
                x: inner.right().saturating_sub(width),
                y: row,
                width,
                height: 1,
            },
            escape,
        ));
    }
    out
}

/// The title line every overlay opens with: `❯ ` and what is being typed.
///
/// The mock gives the prompt the accent colour and the count on the right the
/// muted one, on every one of the five screens, so it is written once.
pub fn header(prompt: &str, typed: &str, count: &str, width: u16, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{prompt} "),
        theme.style(crate::theme::Role::Accent),
    )];
    let mut spent = prompt.chars().count() + 1;
    spans.push(Span::styled(
        typed.to_string(),
        theme.ink_style(Ink::Text).add_modifier(Modifier::BOLD),
    ));
    spent += typed.chars().count();
    let room = (width as usize).saturating_sub(spent + count.chars().count());
    spans.push(Span::raw(" ".repeat(room)));
    spans.push(Span::styled(
        count.to_string(),
        theme.ink_style(Ink::Subtext),
    ));
    Line::from(spans)
}

/// One row of an overlay list, in the mock's two states.
///
/// Selected means the accent as a *background* with the base colour on it,
/// not an accent foreground: the mock fills the row, and a filled row is the
/// one thing on the screen that cannot be mistaken for another.
pub fn row(chosen: bool, cells: Vec<(String, Ink)>, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, (text, ink)) in cells.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                " ".to_string(),
                if chosen {
                    Style::default().bg(theme.color(crate::theme::Role::Accent))
                } else {
                    Style::default()
                },
            ));
        }
        let style = if chosen {
            Style::default()
                .bg(theme.color(crate::theme::Role::Accent))
                .fg(theme.ink(Ink::Backdrop))
        } else {
            theme.ink_style(ink)
        };
        spans.push(Span::styled(text, style));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Screen;
    use crate::statusbar;
    use crate::theme::Depth;
    use grove_lua::TuiConfig;

    fn theme() -> Theme {
        Theme::resolve(&TuiConfig::default(), Depth::True).0
    }

    const SCREEN: Rect = Rect {
        x: 0,
        y: 0,
        width: 120,
        height: 30,
    };

    fn painted(buf: &Buffer) -> String {
        (0..SCREEN.height)
            .map(|y| {
                (0..SCREEN.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn draw(rows: u16, screen: Screen) -> (Buffer, Parts) {
        let theme = theme();
        let mut buf = Buffer::empty(SCREEN);
        let parts = render(
            &mut buf,
            SCREEN,
            theme.style(crate::theme::Role::Accent),
            rows,
            statusbar::hints(screen),
            &theme,
        );
        (buf, parts)
    }

    #[test]
    fn the_box_is_the_mocks_width_and_centred() {
        let (buf, _) = draw(4, Screen::Picker);
        let top = painted(&buf)
            .lines()
            .find(|line| line.contains('╭'))
            .expect("a box")
            .to_string();
        // Columns, not bytes: the box is drawn in characters three bytes wide.
        let left = top.chars().position(|c| c == '╭').expect("left edge");
        let right = top
            .chars()
            .rev()
            .position(|c| c == '╮')
            .map(|back| top.chars().count() - 1 - back)
            .expect("right edge");
        assert_eq!(right - left + 1, IDEAL as usize, "the mock's 98 columns");
        assert_eq!(
            left,
            SCREEN.width as usize - 1 - right,
            "and the same margin either side"
        );
    }

    #[test]
    fn a_narrow_screen_gives_the_box_what_there_is() {
        // `max-width:100%` in the mock. A box wider than the terminal would
        // draw its right border off the edge, which is a box with one side.
        let theme = theme();
        let narrow = Rect {
            width: 40,
            ..SCREEN
        };
        let mut buf = Buffer::empty(narrow);
        let parts = render(
            &mut buf,
            narrow,
            theme.style(crate::theme::Role::Accent),
            3,
            statusbar::hints(Screen::Picker),
            &theme,
        );
        assert!(parts.body.right() <= narrow.right());
        assert!(
            parts.header.width > 0,
            "and still has room to say something"
        );
    }

    #[test]
    fn the_box_is_as_tall_as_its_contents() {
        // `height:fit-content`: three rows of session do not get a box with
        // ten rows of nothing under them.
        let short = draw(3, Screen::Picker).1;
        let tall = draw(9, Screen::Picker).1;
        assert_eq!(tall.body.height - short.body.height, 6);
        assert!(
            short.body.y > tall.body.y,
            "and it stays centred as it grows"
        );
    }

    #[test]
    fn the_footer_is_the_screens_keys_with_esc_on_the_right() {
        let (buf, _) = draw(4, Screen::Picker);
        let footer = painted(&buf)
            .lines()
            .filter(|line| line.contains("resume"))
            .map(str::to_string)
            .next()
            .expect("a footer");
        for verb in ["enter resume", "d detach", "c close", "X end"] {
            assert!(footer.contains(verb), "{verb} missing from: {footer}");
        }
        let escape = footer.find("esc back").expect("esc is offered");
        assert!(
            escape > footer.find("X end").expect("end"),
            "esc goes to the right, on its own: {footer}"
        );
    }

    #[test]
    fn every_overlay_screen_gets_a_footer_from_its_own_keys() {
        // The rule the bar follows, applied to the footers: no screen may
        // invent a verb, and none may go without one.
        for screen in [
            Screen::Picker,
            Screen::Prune,
            Screen::Diff,
            Screen::EndSession,
            Screen::Palette,
        ] {
            let hints = statusbar::hints(screen);
            assert!(!hints.is_empty(), "{screen:?} has no keys to show");
            let (buf, _) = draw(4, screen);
            let text = painted(&buf);
            for hint in hints {
                assert!(
                    text.contains(&hint.label),
                    "{screen:?} did not offer {}: {text}",
                    hint.label
                );
            }
        }
    }

    #[test]
    fn what_is_behind_keeps_its_characters_and_loses_its_colour() {
        // The mock lays the overlay over a wash rather than a blank: the dash
        // is still readable behind it, and plainly not what is being typed
        // into. A terminal has no alpha, so the wash is a colour change.
        let theme = theme();
        let mut buf = Buffer::empty(SCREEN);
        // Something to be behind it, at a corner the box cannot reach.
        buf[(0, 0)].set_symbol("▣");
        buf[(0, 0)].set_style(Style::default().fg(theme.color(crate::theme::Role::Clean)));
        render(
            &mut buf,
            SCREEN,
            theme.style(crate::theme::Role::Accent),
            4,
            statusbar::hints(Screen::Picker),
            &theme,
        );
        assert_eq!(buf[(0, 0)].symbol(), "▣", "the dash is still there");
        assert_eq!(
            buf[(0, 0)].style().fg,
            Some(theme.ink(Ink::Frame)),
            "and washed out"
        );
    }
}
