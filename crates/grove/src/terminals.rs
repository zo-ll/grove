//! The terminal pane (SPEC §4.1) — a real pty, not a preview.
//!
//! The daemon owns the process; this module owns the grid. Per SPEC §8 the
//! client runs its own `vt100` over the live byte stream, so the pane stays
//! current between refreshes rather than waiting to be told what it looks like.
//!
//! # Why there are two histories
//!
//! `grove-proto`'s reassembly contract is the subtle part, and getting it wrong
//! is invisible until someone scrolls up. On attach the daemon sends the
//! visible grid first so the pane can paint at once, then streams older history
//! behind it in chunks — while live output keeps arriving the whole time.
//!
//! Appending all of that to one buffer in arrival order **misorders history**:
//! live output can scroll lines off the top of the grid before backfill
//! finishes, and those lines are *newer* than every backfill chunk. So backfill
//! accumulates separately, ordered by `seq`, and is joined in front of the
//! parser's own scrollback when `done` arrives.
//!
//! # What "switching worktrees" must not do
//!
//! The pane follows the WORKTREES cursor, and moving that cursor must not
//! disturb either pty. Grove detaches from one and attaches to the next; the
//! processes keep running, and the grid for a terminal already seen is kept so
//! returning to it paints immediately instead of re-fetching history.

use std::collections::HashMap;

use grove_proto::{
    Attach, Cell, Color as WireColor, Request, Screen as WireScreen, ScrollbackRequest, TerminalId,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use tui_term::widget::PseudoTerminal;

use crate::theme::{Role, Theme};

/// How much history to ask for. `config.scrollback` is the daemon's own cap,
/// so this asks for everything it kept rather than second-guessing it.
const WANTED: ScrollbackRequest = ScrollbackRequest::All;

/// Lines moved per scroll keystroke. Three, because one is tedious and a
/// whole page loses the line the user was reading.
pub const SCROLL_STEP: isize = 3;

/// One terminal's grid and history.
struct Pty {
    parser: vt100::Parser,
    /// Backfill chunks in `seq` order, all older than the screen. Held apart
    /// from the parser's scrollback until `done`, because they must end up
    /// *before* anything live output has scrolled off since.
    backfill: Vec<String>,
    /// The next `seq` expected. A gap means a chunk was lost, and joining
    /// anyway would silently reorder history.
    next_seq: u32,
    joined: bool,
}

impl Pty {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            // The parser keeps its own scrollback for lines that leave the
            // grid while we are watching.
            parser: vt100::Parser::new(rows, cols, 10_000),
            backfill: Vec::new(),
            next_seq: 0,
            joined: false,
        }
    }
}

/// Every terminal the pane has seen, and which one it is showing.
#[derive(Default)]
pub struct Terminals {
    ptys: HashMap<TerminalId, Pty>,
    showing: Option<TerminalId>,
    rows: u16,
    cols: u16,
}

impl Terminals {
    /// Show `terminal`, detaching from whatever was showing.
    ///
    /// Returns the requests to send, in order. Detach comes first so the
    /// daemon stops streaming a grid nobody is looking at before it starts
    /// streaming the next — switching quickly along a list of worktrees must
    /// not leave a trail of attached terminals feeding output into the void.
    pub fn show(&mut self, terminal: Option<TerminalId>, rows: u16, cols: u16) -> Vec<Request> {
        self.rows = rows.max(1);
        self.cols = cols.max(1);
        if self.showing == terminal {
            return Vec::new();
        }
        let mut requests = Vec::new();
        if let Some(previous) = self.showing.take() {
            requests.push(Request::DetachTerminal(previous));
        }
        let Some(next) = terminal else {
            return requests;
        };
        self.showing = Some(next);

        // A grid we have seen before is kept, so returning to it paints at
        // once. Only ask for history the first time.
        let known = self.ptys.contains_key(&next);
        self.ptys
            .entry(next)
            .or_insert_with(|| Pty::new(self.rows, self.cols));
        requests.push(Request::AttachTerminal(Attach {
            terminal: next,
            scrollback: if known {
                ScrollbackRequest::None
            } else {
                WANTED
            },
            rows: self.rows,
            cols: self.cols,
        }));
        requests
    }

    /// The terminal currently on screen.
    pub fn showing(&self) -> Option<TerminalId> {
        self.showing
    }

    /// Seed the grid from the attach-time snapshot.
    ///
    /// The snapshot is replayed into the parser as bytes rather than kept
    /// beside it, so there is one grid rather than two that can disagree — and
    /// every live delta that follows applies to the state the daemon described.
    pub fn screen(&mut self, terminal: TerminalId, screen: &WireScreen) {
        let rows = screen.rows.max(1);
        let cols = screen.cols.max(1);
        let pty = self
            .ptys
            .entry(terminal)
            .or_insert_with(|| Pty::new(rows, cols));
        pty.parser.screen_mut().set_size(rows, cols);
        pty.parser.process(&encode(screen));
    }

    /// Take a backfill chunk.
    ///
    /// Out-of-order chunks are refused rather than appended: a gap means a
    /// chunk was lost, and joining anyway puts history in the wrong order with
    /// nothing on screen to say so.
    pub fn scrollback(&mut self, terminal: TerminalId, seq: u32, lines: &[String], done: bool) {
        let Some(pty) = self.ptys.get_mut(&terminal) else {
            return;
        };
        if pty.joined || seq != pty.next_seq {
            return;
        }
        pty.backfill.extend(lines.iter().cloned());
        pty.next_seq = seq.saturating_add(1);
        if done {
            pty.joined = true;
        }
    }

    /// Live output.
    pub fn output(&mut self, terminal: TerminalId, bytes: &[u8]) {
        if let Some(pty) = self.ptys.get_mut(&terminal) {
            pty.parser.process(bytes);
        }
    }

    /// Forget a terminal that has exited, so a reused id cannot inherit its
    /// grid.
    pub fn exited(&mut self, terminal: TerminalId) {
        self.ptys.remove(&terminal);
        if self.showing == Some(terminal) {
            self.showing = None;
        }
    }

    /// Resize the shown terminal, returning the request that tells the daemon.
    ///
    /// The local parser is resized too rather than waiting for the daemon's
    /// reply: the next frame draws from it, and a grid still at the old size
    /// shows a torn screen until output happens to arrive.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Option<Request> {
        self.rows = rows.max(1);
        self.cols = cols.max(1);
        let terminal = self.showing?;
        if let Some(pty) = self.ptys.get_mut(&terminal) {
            pty.parser.screen_mut().set_size(self.rows, self.cols);
        }
        Some(Request::ResizeTerminal {
            terminal,
            rows: self.rows,
            cols: self.cols,
        })
    }

    /// Keystrokes for the program inside the pty.
    pub fn input(&self, bytes: Vec<u8>) -> Option<Request> {
        Some(Request::Input {
            terminal: self.showing?,
            bytes,
        })
    }

    /// Scroll grove's view of the history. Returns whether anything moved.
    pub fn scroll(&mut self, lines: isize) -> bool {
        let Some(pty) = self.showing.and_then(|id| self.ptys.get_mut(&id)) else {
            return false;
        };
        let at = pty.parser.screen().scrollback();
        let wanted = at.saturating_add_signed(lines);
        if wanted == at {
            return false;
        }
        pty.parser.screen_mut().set_scrollback(wanted);
        pty.parser.screen().scrollback() != at
    }

    /// History older than the live grid, oldest first.
    ///
    /// Backfill first, then whatever live output has scrolled off since —
    /// which is the whole reason the two are kept apart.
    #[cfg(test)]
    pub fn history(&self, terminal: TerminalId) -> Vec<String> {
        self.ptys
            .get(&terminal)
            .map(|pty| pty.backfill.clone())
            .unwrap_or_default()
    }

    /// Draw the pane's contents into `area`, already inside the border.
    pub fn render(&self, buf: &mut Buffer, area: Rect, theme: &Theme, has_terminal: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let Some(pty) = self.showing.and_then(|id| self.ptys.get(&id)) else {
            // Nothing attached. A worktree can legitimately have no pty — an
            // adopted one, or any of them after the daemon restarted — so the
            // pane says how to get one rather than sitting blank.
            let message = if has_terminal {
                "attaching…"
            } else {
                "no terminal here — ^g enter opens one"
            };
            Paragraph::new(Line::styled(message, theme.style(Role::Muted))).render(area, buf);
            return;
        };
        PseudoTerminal::new(pty.parser.screen()).render(area, buf);
    }
}

/// The bytes a keystroke sends to a pty.
///
/// The pane forwards keys rather than interpreting them, so this is a
/// translation and not a keymap: what the program inside does with `^c` is its
/// business. Unknown keys produce nothing rather than a guess — a stray byte
/// in a terminal is worse than a key that did not arrive.
pub fn encode_key(key: ratatui::crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut bytes = match key.code {
        KeyCode::Char(c) if ctrl => {
            // ^a is 0x01 and so on up to ^z; the control characters outside
            // that run have their own spellings on the wire.
            let upper = c.to_ascii_uppercase();
            match upper {
                'A'..='Z' => vec![upper as u8 - b'A' + 1],
                '@' | ' ' => vec![0],
                '[' => vec![0x1b],
                '\\' => vec![0x1c],
                ']' => vec![0x1d],
                '^' => vec![0x1e],
                '_' | '?' => vec![0x1f],
                _ => return None,
            }
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::F(n @ 1..=4) => vec![0x1b, b'O', b'P' + (n - 1)],
        _ => return None,
    };
    if alt {
        // Meta is an escape prefix, which is what every terminal emulator
        // sends and what readline expects.
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// Re-encode an attach-time snapshot as the bytes that would have produced it.
///
/// The alternative — holding the snapshot beside the parser and drawing
/// whichever looks fresher — is two sources of truth for one grid, and they
/// disagree the moment a delta arrives.
fn encode(screen: &WireScreen) -> Vec<u8> {
    // Clear, home, and reset attributes: the snapshot is a complete picture,
    // not an edit to whatever was there.
    let mut out = b"\x1b[2J\x1b[H\x1b[0m".to_vec();
    for (index, row) in screen.cells.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b"\r\n");
        }
        let mut current = Style::default();
        for cell in row {
            let wanted = Style::of(cell);
            if wanted != current {
                out.extend_from_slice(wanted.sgr().as_bytes());
                current = wanted;
            }
            // An empty cell is the continuation column of a double-width
            // character, which the preceding grapheme already covers.
            if !cell.text.is_empty() {
                out.extend_from_slice(cell.text.as_bytes());
            }
        }
    }
    out.extend_from_slice(b"\x1b[0m");

    // Put the cursor where the daemon says it is. Without this it ends up
    // wherever the last row left it, and the first live byte after a reattach
    // lands in the wrong place — which looks like corruption rather than like
    // a bug in the snapshot.
    match screen.cursor {
        // vt100 counts from 1 in escape sequences and from 0 on the wire.
        Some((row, col)) => {
            out.extend_from_slice(
                format!("\x1b[{};{}H", row.saturating_add(1), col.saturating_add(1)).as_bytes(),
            );
            out.extend_from_slice(b"\x1b[?25h");
        }
        // A hidden cursor is a state the program chose; the snapshot carries
        // it, so honour it rather than showing one it hid.
        None => out.extend_from_slice(b"\x1b[?25l"),
    }
    out
}

/// The parts of a cell that map to an SGR sequence.
#[derive(Default, PartialEq, Eq, Clone, Copy)]
struct Style {
    fg: Ansi,
    bg: Ansi,
    bold: bool,
    italic: bool,
    underline: bool,
    reverse: bool,
    dim: bool,
}

/// A colour in the form SGR wants it.
#[derive(Default, PartialEq, Eq, Clone, Copy)]
enum Ansi {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl From<&WireColor> for Ansi {
    fn from(colour: &WireColor) -> Self {
        match colour {
            WireColor::Default => Self::Default,
            WireColor::Indexed(i) => Self::Indexed(*i),
            WireColor::Rgb(r, g, b) => Self::Rgb(*r, *g, *b),
        }
    }
}

impl Style {
    fn of(cell: &Cell) -> Self {
        Self {
            fg: Ansi::from(&cell.fg),
            bg: Ansi::from(&cell.bg),
            bold: cell.attrs.bold,
            italic: cell.attrs.italic,
            underline: cell.attrs.underline,
            reverse: cell.attrs.reverse,
            dim: cell.attrs.dim,
        }
    }

    /// The escape sequence that sets exactly this style.
    ///
    /// Always starts from a reset, so a style is never the previous cell's
    /// with additions — an attribute that is off here must turn off.
    fn sgr(self) -> String {
        let mut parts = vec!["0".to_string()];
        if self.bold {
            parts.push("1".into());
        }
        if self.dim {
            parts.push("2".into());
        }
        if self.italic {
            parts.push("3".into());
        }
        if self.underline {
            parts.push("4".into());
        }
        if self.reverse {
            parts.push("7".into());
        }
        match self.fg {
            Ansi::Default => {}
            Ansi::Indexed(i) => parts.push(format!("38;5;{i}")),
            Ansi::Rgb(r, g, b) => parts.push(format!("38;2;{r};{g};{b}")),
        }
        match self.bg {
            Ansi::Default => {}
            Ansi::Indexed(i) => parts.push(format!("48;5;{i}")),
            Ansi::Rgb(r, g, b) => parts.push(format!("48;2;{r};{g};{b}")),
        }
        format!("\x1b[{}m", parts.join(";"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_proto::{Attrs, Color};

    fn id(n: u64) -> TerminalId {
        TerminalId(n)
    }

    fn cell(text: &str) -> Cell {
        Cell {
            text: text.into(),
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::default(),
        }
    }

    fn screen_of(rows: u16, cols: u16, text: &str) -> WireScreen {
        let mut first: Vec<Cell> = text.chars().map(|c| cell(&c.to_string())).collect();
        first.resize(usize::from(cols), cell(" "));
        let mut cells = vec![first];
        cells.resize(usize::from(rows), vec![cell(" "); usize::from(cols)]);
        WireScreen {
            rows,
            cols,
            cells,
            cursor: None,
        }
    }

    fn grid(terminals: &Terminals, terminal: TerminalId) -> String {
        terminals
            .ptys
            .get(&terminal)
            .map(|pty| pty.parser.screen().contents())
            .unwrap_or_default()
    }

    #[test]
    fn the_attach_snapshot_paints_the_grid() {
        // The snapshot exists so the pane can paint before history arrives.
        let mut terminals = Terminals::default();
        terminals.screen(id(1), &screen_of(2, 8, "hello"));
        assert!(grid(&terminals, id(1)).contains("hello"));
    }

    #[test]
    fn live_output_applies_on_top_of_the_snapshot() {
        // One grid, not two: the parser holds the snapshot, so a delta that
        // edits it lands where the daemon said it would.
        let mut terminals = Terminals::default();
        let mut screen = screen_of(2, 8, "abc");
        screen.cursor = Some((0, 3));
        terminals.screen(id(1), &screen);
        terminals.output(id(1), b"XYZ");
        let contents = grid(&terminals, id(1));
        assert!(contents.contains("abcXYZ"), "{contents:?}");
    }

    #[test]
    fn styling_survives_the_snapshot() {
        // The snapshot carries colour, and re-encoding has to preserve it or a
        // reattach visibly loses styling until the next full redraw.
        let mut screen = screen_of(1, 4, "");
        screen.cells[0][0] = Cell {
            text: "X".into(),
            fg: Color::Indexed(9),
            bg: Color::Default,
            attrs: Attrs {
                bold: true,
                ..Attrs::default()
            },
        };
        let mut terminals = Terminals::default();
        terminals.screen(id(1), &screen);
        let pty = terminals.ptys.get(&id(1)).expect("a pty");
        let painted = pty.parser.screen().cell(0, 0).expect("a cell");
        assert_eq!(painted.contents(), "X");
        assert!(painted.bold(), "bold must survive the snapshot");
        assert_eq!(painted.fgcolor(), vt100::Color::Idx(9));
    }

    #[test]
    fn an_attribute_that_is_off_turns_off() {
        // Each style starts from a reset, so a bold cell followed by a plain
        // one does not leave the rest of the row bold.
        let mut screen = screen_of(1, 2, "");
        screen.cells[0][0] = Cell {
            text: "B".into(),
            attrs: Attrs {
                bold: true,
                ..Attrs::default()
            },
            ..cell("")
        };
        screen.cells[0][1] = cell("p");
        let mut terminals = Terminals::default();
        terminals.screen(id(1), &screen);
        let pty = terminals.ptys.get(&id(1)).expect("a pty");
        assert!(pty.parser.screen().cell(0, 0).expect("a cell").bold());
        assert!(
            !pty.parser.screen().cell(0, 1).expect("a cell").bold(),
            "bold must not bleed into the next cell"
        );
    }

    #[test]
    fn switching_detaches_before_it_attaches() {
        // Moving quickly down a list of worktrees must not leave a trail of
        // attached terminals streaming output nobody is watching.
        let mut terminals = Terminals::default();
        let first = terminals.show(Some(id(1)), 24, 80);
        assert!(matches!(first.as_slice(), [Request::AttachTerminal(_)]));

        let second = terminals.show(Some(id(2)), 24, 80);
        match second.as_slice() {
            [
                Request::DetachTerminal(gone),
                Request::AttachTerminal(attach),
            ] => {
                assert_eq!(*gone, id(1));
                assert_eq!(attach.terminal, id(2));
            }
            other => panic!("expected a detach then an attach, got {other:?}"),
        }
    }

    #[test]
    fn showing_the_same_terminal_again_asks_for_nothing() {
        // Redrawing, or a refresh that does not change the selection, must not
        // re-attach — it would re-stream the history every frame.
        let mut terminals = Terminals::default();
        terminals.show(Some(id(1)), 24, 80);
        assert!(terminals.show(Some(id(1)), 24, 80).is_empty());
    }

    #[test]
    fn returning_to_a_terminal_does_not_refetch_its_history() {
        // The grid is kept, so coming back paints at once. Asking for the
        // whole scrollback again would re-send it on every pass down a list.
        let mut terminals = Terminals::default();
        terminals.show(Some(id(1)), 24, 80);
        terminals.show(Some(id(2)), 24, 80);
        let back = terminals.show(Some(id(1)), 24, 80);
        match back.as_slice() {
            [Request::DetachTerminal(_), Request::AttachTerminal(attach)] => {
                assert_eq!(
                    attach.scrollback,
                    ScrollbackRequest::None,
                    "history we already have must not be re-sent"
                );
            }
            other => panic!("expected a detach then an attach, got {other:?}"),
        }
    }

    #[test]
    fn selecting_a_worktree_without_a_terminal_only_detaches() {
        let mut terminals = Terminals::default();
        terminals.show(Some(id(1)), 24, 80);
        let requests = terminals.show(None, 24, 80);
        assert!(matches!(requests.as_slice(), [Request::DetachTerminal(_)]));
        assert!(terminals.showing().is_none());
    }

    #[test]
    fn backfill_arrives_oldest_first_and_joins_at_done() {
        let mut terminals = Terminals::default();
        terminals.show(Some(id(1)), 24, 80);
        terminals.scrollback(id(1), 0, &["oldest".into(), "older".into()], false);
        terminals.scrollback(id(1), 1, &["old".into()], true);
        assert_eq!(terminals.history(id(1)), vec!["oldest", "older", "old"]);
    }

    #[test]
    fn a_chunk_out_of_order_is_refused_rather_than_reordering_history() {
        // A gap means a chunk was lost. Appending anyway puts history in the
        // wrong order with nothing on screen to say so.
        let mut terminals = Terminals::default();
        terminals.show(Some(id(1)), 24, 80);
        terminals.scrollback(id(1), 0, &["first".into()], false);
        terminals.scrollback(id(1), 2, &["skipped ahead".into()], false);
        assert_eq!(
            terminals.history(id(1)),
            vec!["first"],
            "a chunk after a gap must not be appended"
        );
    }

    #[test]
    fn nothing_is_appended_after_the_join() {
        // `done` means done; a late chunk arriving afterwards belongs to a
        // backfill that is already finished.
        let mut terminals = Terminals::default();
        terminals.show(Some(id(1)), 24, 80);
        terminals.scrollback(id(1), 0, &["all of it".into()], true);
        terminals.scrollback(id(1), 1, &["too late".into()], false);
        assert_eq!(terminals.history(id(1)), vec!["all of it"]);
    }

    #[test]
    fn an_empty_backfill_still_terminates() {
        // The protocol says one empty chunk with `done`, never zero chunks —
        // a client waiting to join would otherwise wait forever.
        let mut terminals = Terminals::default();
        terminals.show(Some(id(1)), 24, 80);
        terminals.scrollback(id(1), 0, &[], true);
        assert!(terminals.history(id(1)).is_empty());
        assert!(terminals.ptys.get(&id(1)).expect("a pty").joined);
    }

    #[test]
    fn resizing_moves_the_local_grid_and_tells_the_daemon() {
        // The next frame draws from the local parser, so leaving it at the old
        // size shows a torn screen until output happens to arrive.
        let mut terminals = Terminals::default();
        terminals.show(Some(id(1)), 24, 80);
        let request = terminals.resize(40, 120).expect("a resize request");
        match request {
            Request::ResizeTerminal { rows, cols, .. } => {
                assert_eq!((rows, cols), (40, 120));
            }
            other => panic!("expected a resize, got {other:?}"),
        }
        let pty = terminals.ptys.get(&id(1)).expect("a pty");
        assert_eq!(pty.parser.screen().size(), (40, 120));
    }

    #[test]
    fn keystrokes_go_to_the_terminal_on_screen() {
        let mut terminals = Terminals::default();
        terminals.show(Some(id(7)), 24, 80);
        match terminals.input(b"ls\r".to_vec()) {
            Some(Request::Input { terminal, bytes }) => {
                assert_eq!(terminal, id(7));
                assert_eq!(bytes, b"ls\r");
            }
            other => panic!("expected input, got {other:?}"),
        }
    }

    #[test]
    fn there_is_nowhere_to_type_when_nothing_is_attached() {
        let terminals = Terminals::default();
        assert!(terminals.input(b"x".to_vec()).is_none());
    }

    #[test]
    fn scrolling_reaches_history_and_stops_at_its_end() {
        // Acceptance: scrollback is reachable. It is also finite, and a scroll
        // that cannot move must say so rather than costing a frame.
        let mut terminals = Terminals::default();
        terminals.show(Some(id(1)), 2, 20);
        // Five lines through a two-row grid leaves three in scrollback.
        terminals.output(id(1), b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        assert!(terminals.scroll(1), "there is history to reach");
        assert!(terminals.scroll(-10), "and a way back to the live grid");
        assert!(
            !terminals.scroll(-10),
            "already at the live grid, so nothing moved and no frame is owed"
        );
    }

    #[test]
    fn keys_reach_the_program_as_the_bytes_it_expects() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |code, mods| encode_key(KeyEvent::new(code, mods));

        assert_eq!(
            key(KeyCode::Char('a'), KeyModifiers::NONE),
            Some(b"a".to_vec())
        );
        assert_eq!(
            key(KeyCode::Enter, KeyModifiers::NONE),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            key(KeyCode::Up, KeyModifiers::NONE),
            Some(b"\x1b[A".to_vec())
        );
        // ^c is how a user stops a runaway program, and it has to arrive as
        // the byte rather than as the letter.
        assert_eq!(
            key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(vec![3])
        );
        assert_eq!(
            key(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Some(vec![4])
        );
        // Alt is an escape prefix, which is what readline expects.
        assert_eq!(
            key(KeyCode::Char('b'), KeyModifiers::ALT),
            Some(vec![0x1b, b'b'])
        );
        // A key with no pty spelling sends nothing: a stray byte is worse
        // than a keystroke that did not arrive.
        assert_eq!(key(KeyCode::F(12), KeyModifiers::NONE), None);
    }

    #[test]
    fn a_terminal_that_exits_is_forgotten() {
        // Ids are reused, and inheriting a dead terminal's grid would paint
        // one program's output under another's name.
        let mut terminals = Terminals::default();
        terminals.show(Some(id(1)), 24, 80);
        terminals.screen(id(1), &screen_of(2, 8, "gone"));
        terminals.exited(id(1));
        assert!(terminals.showing().is_none());
        assert!(grid(&terminals, id(1)).is_empty());
    }
}
