//! The session picker (SPEC §4.3).
//!
//! Three states and three verbs, and the whole screen exists so the difference
//! between them is visible before one is pressed:
//!
//! - `d` **detach** — the terminals keep running; you are just looking
//!   elsewhere.
//! - `c` **close** — the terminals die, the worktrees stay on disk.
//! - `X` **end** — the worktrees are removed. The only destructive act in
//!   grove, and the only key here that does not act: it routes to the confirm
//!   (#29), because §2 makes ending the thing you have to mean.
//!
//! Resuming replaces the open session — only one is open at a time (§2.2). The
//! outgoing one becomes detached, except when it is the unnamed launch session
//! with nothing in it, which is dropped silently: grove starts with an empty
//! session every time, and keeping a stack of them would make the picker a
//! list of accidents.

use grove_domain::{SessionId, SessionState};
use grove_proto::SessionRow;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::text::truncate;
use crate::theme::{Role, Theme};

/// Open here and now.
const GLYPH_ATTACHED: &str = "●";
/// Terminals alive, being looked at elsewhere.
const GLYPH_DETACHED: &str = "◐";
/// Terminals gone, worktrees still on disk.
const GLYPH_CLOSED: &str = "○";

/// What the picker decided a keystroke means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Replace the open session with this one.
    Resume(SessionId),
    Detach(SessionId),
    Close(SessionId),
    /// Ask for confirmation. `X` never acts from here.
    ConfirmEnd(SessionId),
    /// Nothing happens, and this is why.
    Refused(String),
}

/// The picker's state.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Sessions {
    rows: Vec<SessionRow>,
    cursor: usize,
}

impl Sessions {
    /// Take the daemon's list, keeping the cursor on the same session.
    pub fn set(&mut self, rows: Vec<SessionRow>) {
        let previous = self.selected().map(|row| row.id.clone());
        self.rows = rows;
        self.cursor = previous
            .and_then(|id| self.rows.iter().position(|row| row.id == id))
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
    }

    /// A session by the name a script would know it as.
    pub fn by_name(&self, name: &str) -> Option<&SessionRow> {
        self.rows.iter().find(|row| row.name == name)
    }

    pub fn selected(&self) -> Option<&SessionRow> {
        self.rows.get(self.cursor)
    }

    #[cfg(test)]
    pub fn rows(&self) -> &[SessionRow] {
        &self.rows
    }

    /// The session with this id, for naming the one that is open.
    pub fn by_id(&self, id: &grove_domain::SessionId) -> Option<&SessionRow> {
        self.rows.iter().find(|row| &row.id == id)
    }

    pub fn move_down(&mut self) -> bool {
        let last = self.rows.len().saturating_sub(1);
        if self.rows.is_empty() || self.cursor >= last {
            return false;
        }
        self.cursor += 1;
        true
    }

    pub fn move_up(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    /// What `enter` means for the row under the cursor.
    pub fn resume(&self) -> Option<Intent> {
        let row = self.selected()?;
        Some(match row.state {
            // Resuming what is already open would replace it with itself and
            // detach it on the way, which reads as grove losing its place.
            SessionState::Attached => Intent::Refused(format!("{} is already open", row.name)),
            _ => Intent::Resume(row.id.clone()),
        })
    }

    /// What `d` means.
    pub fn detach(&self) -> Option<Intent> {
        let row = self.selected()?;
        Some(match row.state {
            SessionState::Attached | SessionState::Detached => Intent::Detach(row.id.clone()),
            // Its terminals are already gone; there is nothing to detach from.
            SessionState::Closed => {
                Intent::Refused(format!("{} is closed — its terminals are gone", row.name))
            }
        })
    }

    /// What `c` means.
    pub fn close(&self) -> Option<Intent> {
        let row = self.selected()?;
        Some(match row.state {
            SessionState::Closed => Intent::Refused(format!("{} is already closed", row.name)),
            _ => Intent::Close(row.id.clone()),
        })
    }

    /// What `X` means — always a question, never an act.
    pub fn end(&self) -> Option<Intent> {
        let row = self.selected()?;
        Some(Intent::ConfirmEnd(row.id.clone()))
    }

    /// Draw the picker into `area`.
    pub fn render(&self, buf: &mut Buffer, area: Rect, theme: &Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let mut lines = vec![Line::from(vec![
            Span::styled(" session ❯", theme.style(Role::Accent)),
            Span::raw("   "),
            Span::styled(
                format!("{} sessions", self.rows.len()),
                theme.style(Role::Muted),
            ),
        ])];

        if self.rows.is_empty() {
            lines.push(Line::styled(
                "  no stored sessions",
                theme.style(Role::Muted),
            ));
        }

        let room = usize::from(area.height).saturating_sub(2);
        for (index, row) in self.rows.iter().take(room).enumerate() {
            let here = index == self.cursor;
            let (glyph, glyph_role) = match row.state {
                SessionState::Attached => (GLYPH_ATTACHED, Role::Clean),
                SessionState::Detached => (GLYPH_DETACHED, Role::Dirty),
                SessionState::Closed => (GLYPH_CLOSED, Role::Muted),
            };
            let name_style = if here {
                theme.style(Role::Accent).add_modifier(Modifier::BOLD)
            } else {
                theme.style(Role::Clean)
            };
            lines.push(Line::from(vec![
                Span::styled(if here { "❯" } else { " " }, theme.style(Role::Accent)),
                Span::styled(glyph, theme.style(glyph_role)),
                Span::raw(" "),
                Span::styled(format!("{:<18}", truncate(&row.name, 18)), name_style),
                Span::styled(
                    format!("{:<34}", truncate(&members(row), 34)),
                    theme.style(Role::Muted),
                ),
                Span::styled(status(row), theme.style(Role::Muted)),
            ]));
        }

        lines.push(Line::styled(
            " enter resume   d detach   c close   X end                esc close",
            theme.style(Role::Muted),
        ));
        Paragraph::new(lines).render(area, buf);
    }
}

/// The member column: names while they fit, a count when they do not.
///
/// §4.3 shows both forms — "billing-service, web-app, sdk-js" and "3 repos" —
/// and which one is useful depends on how many there are.
fn members(row: &SessionRow) -> String {
    let joined = row.members.join(", ");
    if row.members.len() > 3 || joined.len() > 34 {
        format!("{} repos", row.members.len())
    } else {
        joined
    }
}

/// The right-hand column: what this session's state costs or holds.
///
/// A closed session shows its reclaimable size, because that is the number
/// that decides whether to end it — its terminals are already gone, so
/// "closed" alone says nothing about what it is still using.
fn status(row: &SessionRow) -> String {
    match row.state {
        SessionState::Attached => match row.terminals {
            1 => "attached · 1 terminal".into(),
            n => format!("attached · {n} terminals"),
        },
        SessionState::Detached => format!("detached {}", age(row.since)),
        SessionState::Closed => format!("closed · {}", bytes(row.size)),
    }
}

fn age(seconds: u64) -> String {
    const HOUR: u64 = 3600;
    const DAY: u64 = 24 * HOUR;
    match seconds {
        s if s < HOUR => format!("{}m", (s / 60).max(1)),
        s if s < DAY => format!("{}h", s / HOUR),
        s => format!("{}d", s / DAY),
    }
}

fn bytes(n: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    match n {
        0 => "—".into(),
        n if n >= GB => format!("{:.1} GB", n as f64 / GB as f64),
        n if n >= MB => format!("{} MB", n / MB),
        n => format!("{} KB", (n / 1024).max(1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str, state: SessionState) -> SessionRow {
        SessionRow {
            id: SessionId(name.into()),
            name: name.into(),
            members: vec!["billing-service".into(), "web-app".into()],
            state,
            terminals: match state {
                SessionState::Attached => 4,
                _ => 0,
            },
            since: 2 * 24 * 3600,
            size: 794 * 1024 * 1024,
        }
    }

    fn theme() -> Theme {
        Theme::resolve(&grove_lua::TuiConfig::default(), crate::theme::Depth::True).0
    }

    fn listing() -> Vec<SessionRow> {
        vec![
            session("invoice split", SessionState::Attached),
            session("tokens review", SessionState::Detached),
            session("tax codes", SessionState::Closed),
        ]
    }

    fn painted(sessions: &Sessions) -> String {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 8,
        };
        let mut buf = Buffer::empty(area);
        sessions.render(&mut buf, area, &theme());
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_three_states_render_distinctly() {
        // Acceptance: three states, three glyphs, and the affordances that go
        // with them. The glyph carries it rather than the colour alone — the
        // theme degrades to sixteen colours, and these three decide whether
        // pressing `c` kills a terminal.
        let mut sessions = Sessions::default();
        sessions.set(listing());
        let screen = painted(&sessions);
        assert!(screen.contains(GLYPH_ATTACHED), "{screen}");
        assert!(screen.contains(GLYPH_DETACHED), "{screen}");
        assert!(screen.contains(GLYPH_CLOSED), "{screen}");
        let glyphs = [GLYPH_ATTACHED, GLYPH_DETACHED, GLYPH_CLOSED];
        assert_eq!(
            glyphs
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3
        );
    }

    #[test]
    fn a_closed_session_shows_what_ending_it_would_reclaim() {
        // Acceptance. Its terminals are already gone, so "closed" alone says
        // nothing about what it still holds — and that number is what decides
        // whether to end it.
        let mut sessions = Sessions::default();
        sessions.set(vec![session("tax codes", SessionState::Closed)]);
        assert_eq!(status(&sessions.rows()[0]), "closed · 794 MB");
        assert!(painted(&sessions).contains("794 MB"));
    }

    #[test]
    fn an_attached_session_counts_its_terminals_in_words_that_agree() {
        let mut one = session("solo", SessionState::Attached);
        one.terminals = 1;
        assert_eq!(status(&one), "attached · 1 terminal");
        let mut many = session("busy", SessionState::Attached);
        many.terminals = 4;
        assert_eq!(status(&many), "attached · 4 terminals");
    }

    #[test]
    fn a_detached_session_says_how_long_it_has_been_away() {
        let row = session("tokens review", SessionState::Detached);
        assert_eq!(status(&row), "detached 2d");
    }

    #[test]
    fn x_asks_rather_than_acts() {
        // Acceptance, and §2's rule: ending is the only destructive act, so
        // this key routes to the confirm (#29) whatever the state.
        let mut sessions = Sessions::default();
        sessions.set(listing());
        for _ in 0..3 {
            assert!(matches!(sessions.end(), Some(Intent::ConfirmEnd(_))));
            if !sessions.move_down() {
                break;
            }
        }
    }

    #[test]
    fn resuming_the_open_session_is_refused_rather_than_replacing_it_with_itself() {
        // It would detach and reattach the same session, which reads as grove
        // losing its place.
        let mut sessions = Sessions::default();
        sessions.set(listing());
        match sessions.resume() {
            Some(Intent::Refused(why)) => assert!(why.contains("already open"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(sessions.move_down());
        assert!(matches!(sessions.resume(), Some(Intent::Resume(_))));
    }

    #[test]
    fn each_verb_is_refused_where_it_would_mean_nothing() {
        // Three verbs for three states: the combinations that do not apply
        // say so rather than sending a request the daemon will reject.
        let mut sessions = Sessions::default();
        sessions.set(vec![session("tax codes", SessionState::Closed)]);
        match sessions.detach() {
            Some(Intent::Refused(why)) => assert!(why.contains("terminals are gone"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        match sessions.close() {
            Some(Intent::Refused(why)) => assert!(why.contains("already closed"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        // But ending it is exactly what a closed session is for.
        assert!(matches!(sessions.end(), Some(Intent::ConfirmEnd(_))));
    }

    #[test]
    fn a_detached_session_can_be_detached_again_without_complaint() {
        // It is already where `d` would put it, and refusing would make the
        // key feel broken for no gain.
        let mut sessions = Sessions::default();
        sessions.set(vec![session("tokens", SessionState::Detached)]);
        assert!(matches!(sessions.detach(), Some(Intent::Detach(_))));
    }

    #[test]
    fn the_member_column_names_them_while_it_can_and_counts_them_when_it_cannot() {
        let mut few = session("a", SessionState::Closed);
        few.members = vec!["billing-service".into(), "web-app".into()];
        assert_eq!(members(&few), "billing-service, web-app");

        let mut many = session("b", SessionState::Closed);
        many.members = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        assert_eq!(members(&many), "4 repos");
    }

    #[test]
    fn the_cursor_follows_the_session_not_the_index() {
        // The list is re-sent whenever any session changes state, which is
        // often and never because of where the cursor is.
        let mut sessions = Sessions::default();
        sessions.set(listing());
        assert!(sessions.move_down());
        assert_eq!(
            sessions.selected().map(|r| r.name.as_str()),
            Some("tokens review")
        );

        let mut rows = listing();
        rows.insert(0, session("new one", SessionState::Detached));
        sessions.set(rows);
        assert_eq!(
            sessions.selected().map(|r| r.name.as_str()),
            Some("tokens review")
        );
    }

    #[test]
    fn an_empty_list_offers_nothing_rather_than_panicking() {
        let sessions = Sessions::default();
        assert!(sessions.selected().is_none());
        assert!(sessions.resume().is_none());
        assert!(sessions.end().is_none());
        assert!(painted(&sessions).contains("no stored sessions"));
    }

    #[test]
    fn the_footer_names_every_verb() {
        let mut sessions = Sessions::default();
        sessions.set(listing());
        let screen = painted(&sessions);
        for verb in ["enter resume", "d detach", "c close", "X end", "esc"] {
            assert!(screen.contains(verb), "{verb} missing from:\n{screen}");
        }
    }
}
