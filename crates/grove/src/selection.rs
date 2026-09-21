//! Selecting text with the mouse (#111).
//!
//! grove captures the mouse, which takes the terminal's own selection away.
//! This gives it back inside grove: a drag within a pane highlights what it
//! crosses, and letting go copies it. Shift+drag is untouched — every common
//! terminal hands that to its own selection before grove ever sees it.
//!
//! A selection belongs to the pane it started in and never leaves it. Text
//! running from a list into the terminal beside it is not something anyone
//! means to copy; clamping to the pane is what makes a sloppy drag still
//! select what was meant.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthStr;

/// A selection in progress, or one finished and still shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// The pane's content, which the selection is clamped to.
    pub area: Rect,
    /// Where the button went down.
    pub anchor: Position,
    /// Where the pointer is now, or was when the button came up.
    pub head: Position,
    /// Whether the pointer has moved with the button held. A press and
    /// release in one place is a click, not a selection.
    pub dragging: bool,
}

impl Selection {
    pub fn start(area: Rect, at: Position) -> Self {
        Self {
            area,
            anchor: at,
            head: at,
            dragging: false,
        }
    }

    /// Follow the pointer, held to the pane. Returns whether anything moved.
    pub fn drag_to(&mut self, at: Position) -> bool {
        let clamped = Position {
            x: at
                .x
                .clamp(self.area.left(), self.area.right().saturating_sub(1)),
            y: at
                .y
                .clamp(self.area.top(), self.area.bottom().saturating_sub(1)),
        };
        let moved = clamped != self.head;
        self.head = clamped;
        self.dragging |= moved;
        moved
    }

    /// Whether there is anything selected at all.
    pub fn is_empty(&self) -> bool {
        !self.dragging || self.anchor == self.head
    }

    /// The selected rows, as (row, first column, last column), in reading
    /// order. A selection runs like text does: from where it started to the
    /// end of that line, whole lines between, and up to where it ended — not
    /// a rectangle, which is what nobody means when they drag across a
    /// paragraph.
    fn rows(&self) -> Vec<(u16, u16, u16)> {
        if self.is_empty() {
            return Vec::new();
        }
        let (a, h) = (self.anchor, self.head);
        let (from, to) = if (a.y, a.x) <= (h.y, h.x) {
            (a, h)
        } else {
            (h, a)
        };
        let last = self.area.right().saturating_sub(1);
        (from.y..=to.y)
            .map(|y| {
                let first = if y == from.y {
                    from.x
                } else {
                    self.area.left()
                };
                let end = if y == to.y { to.x } else { last };
                (y, first, end)
            })
            .collect()
    }

    /// Show it, over whatever the frame drew.
    pub fn highlight(&self, buf: &mut Buffer) {
        for (y, first, last) in self.rows() {
            for x in first..=last {
                if buf.area.contains(Position { x, y }) {
                    let cell = &mut buf[(x, y)];
                    cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }

    /// The text under it, read from a frame drawn the same way the screen
    /// was — so what is copied is what was highlighted, character for
    /// character.
    pub fn text(&self, buf: &Buffer) -> String {
        let mut lines = Vec::new();
        for (y, first, last) in self.rows() {
            let mut line = String::new();
            let mut x = first;
            while x <= last {
                if !buf.area.contains(Position { x, y }) {
                    break;
                }
                let symbol = buf[(x, y)].symbol();
                line.push_str(symbol);
                // A wide character covers the cell after it, which holds
                // a placeholder; copying that would put a space inside
                // every CJK word.
                x += u16::try_from(symbol.width().max(1)).unwrap_or(1);
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Line;
    use ratatui::widgets::{Paragraph, Widget};

    const AREA: Rect = Rect {
        x: 2,
        y: 1,
        width: 10,
        height: 3,
    };

    fn screen(lines: &[&str]) -> Buffer {
        let mut buf = Buffer::empty(Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 5,
        });
        let text: Vec<Line> = lines.iter().map(|l| Line::raw(*l)).collect();
        Paragraph::new(text).render(AREA, &mut buf);
        buf
    }

    fn dragged(from: (u16, u16), to: (u16, u16)) -> Selection {
        let mut s = Selection::start(
            AREA,
            Position {
                x: from.0,
                y: from.1,
            },
        );
        s.drag_to(Position { x: to.0, y: to.1 });
        s
    }

    #[test]
    fn a_click_is_not_a_selection() {
        let s = Selection::start(AREA, Position { x: 3, y: 1 });
        assert!(s.is_empty());
        assert_eq!(s.text(&screen(&["hello"])), "");
    }

    #[test]
    fn it_runs_like_text_not_like_a_rectangle() {
        let buf = screen(&["alpha one", "bravo two", "charlie"]);
        // From "one" on the first line to "bravo" on the second.
        let s = dragged((8, 1), (6, 2));
        assert_eq!(s.text(&buf), "one\nbravo");
    }

    #[test]
    fn dragging_backwards_selects_the_same_text() {
        let buf = screen(&["alpha one", "bravo two", "charlie"]);
        assert_eq!(
            dragged((6, 2), (8, 1)).text(&buf),
            dragged((8, 1), (6, 2)).text(&buf)
        );
    }

    #[test]
    fn it_never_leaves_the_pane_it_started_in() {
        // A drag far out to the right and below stops at the pane's edge.
        let buf = screen(&["alpha", "bravo", "charlie"]);
        let s = dragged((2, 1), (40, 40));
        assert_eq!(s.head, Position { x: 11, y: 3 });
        assert_eq!(s.text(&buf), "alpha\nbravo\ncharlie");
    }

    #[test]
    fn a_wide_character_is_copied_once_without_a_gap() {
        let buf = screen(&["日本語 ok"]);
        assert_eq!(dragged((2, 1), (11, 1)).text(&buf), "日本語 ok");
    }

    #[test]
    fn what_is_highlighted_is_what_is_copied() {
        let buf = screen(&["alpha one", "bravo two"]);
        let s = dragged((8, 1), (6, 2));
        let mut shown = buf.clone();
        s.highlight(&mut shown);
        let reversed: String = (0..20u16)
            .flat_map(|x| (0..5u16).map(move |y| (x, y)))
            .filter(|&(x, y)| {
                shown[(x, y)]
                    .style()
                    .add_modifier
                    .contains(Modifier::REVERSED)
            })
            .count()
            .to_string();
        // "one" to the end of line one (3 + 1 trailing cell to the edge),
        // then "bravo" up to and including column 6.
        assert_eq!(reversed, (4 + 5).to_string());
    }
}
