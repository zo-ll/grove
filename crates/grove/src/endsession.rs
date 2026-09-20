//! The end-session confirm (SPEC §4.6).
//!
//! The only destructive screen in grove, and the model for how a destructive
//! confirm should read: per-repo consequences first, the aggregate second, the
//! loss warning last, and a confirm that has to be meant.
//!
//! Two rules do the work.
//!
//! **Only worktrees this session owns are listed, and only those are touched.**
//! Another session's are read-only (§2.4) and the clone is the repository
//! itself; neither appears here, because a list that shows them invites the
//! belief that they are about to go.
//!
//! **Ownership is relative to the open session.** `Ownership::Ours` means "the
//! session grove currently has open", so ending a *detached* session means
//! looking for `Other(that session)` instead — the same worktree is `Ours` or
//! `Other` depending on where you are standing. Getting that backwards would
//! either list nothing or list someone else's work.

use grove_domain::{Ownership, RepoId, SessionId};
use grove_proto::WorktreeRow;
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::text::truncate;
use crate::theme::{Ink, Role, Theme};

/// What the confirm is about, while it is open.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EndSession {
    session: Option<SessionId>,
    name: String,
    /// Repos whose worktrees have not arrived yet. The confirm does not draw
    /// its figures until this is empty: a total that grows while someone is
    /// reading it is worse than a moment's wait before a destructive act.
    awaiting: Vec<RepoId>,
    rows: Vec<WorktreeRow>,
}

impl EndSession {
    /// Begin a confirm for `session`, whose members are `repos`.
    ///
    /// Returns the repos to ask about. The worktrees are fetched per repo,
    /// because that is how the protocol answers, and only what comes back is
    /// ever shown — the figures on this screen have to be the daemon's.
    pub fn begin(&mut self, session: SessionId, name: String, repos: Vec<RepoId>) -> Vec<RepoId> {
        self.session = Some(session);
        self.name = name;
        self.awaiting = repos.clone();
        self.rows.clear();
        repos
    }

    /// Take one repo's worktrees, keeping only the ones this session owns.
    ///
    /// `open` is the session grove currently has open, which is what
    /// `Ownership::Ours` is relative to.
    pub fn take(&mut self, repo: &RepoId, rows: &[WorktreeRow], open: Option<&SessionId>) {
        let Some(ending) = self.session.clone() else {
            return;
        };
        self.awaiting.retain(|waiting| waiting != repo);
        self.rows.extend(
            rows.iter()
                .filter(|row| owned_by(row, &ending, open))
                .cloned(),
        );
    }

    /// Whether every repo has answered.
    pub fn ready(&self) -> bool {
        self.session.is_some() && self.awaiting.is_empty()
    }

    pub fn session(&self) -> Option<&SessionId> {
        self.session.as_ref()
    }

    #[cfg(test)]
    pub fn rows(&self) -> &[WorktreeRow] {
        &self.rows
    }

    /// Terminals, worktrees, bytes, uncommitted files.
    ///
    /// Summed from the rows on screen rather than from anything else, so the
    /// aggregate and the list cannot disagree — a total that does not match
    /// what is above it is the specific way a confirm lies.
    pub fn totals(&self) -> Totals {
        Totals {
            terminals: self
                .rows
                .iter()
                .filter(|row| row.terminal.is_some())
                .count(),
            worktrees: self.rows.len(),
            bytes: self.rows.iter().map(|row| row.size).sum(),
            dirty_files: self.rows.iter().map(|row| u64::from(row.dirty_files)).sum(),
        }
    }

    /// Close without doing anything.
    pub fn cancel(&mut self) {
        self.session = None;
        self.rows.clear();
        self.awaiting.clear();
    }

    /// Draw the confirm.
    /// The keys this screen offers, as the overlay's footer shows them.
    ///
    /// Straight from the keymap: `enter remove everything` and `esc cancel`
    /// are what END binds, and nothing here rewords them.
    pub fn footer(&self) -> Vec<crate::statusbar::Hint> {
        crate::statusbar::hints(crate::keymap::Screen::EndSession)
    }

    /// Rows of content, so the overlay can be that tall.
    pub fn height(&self) -> u16 {
        if !self.ready() {
            return 1;
        }
        // The per-repo rows, a blank, the summary, and the warning when there
        // is one — the mock's `endRows` block and the two lines under it.
        let warning = usize::from(self.totals().dirty_files > 0);
        u16::try_from(self.rows.len() + 2 + warning).unwrap_or(u16::MAX)
    }

    /// Draw the confirm into the overlay's parts.
    pub fn render(&self, buf: &mut Buffer, parts: crate::overlay::Parts, theme: &Theme) {
        let (header, body) = (parts.header, parts.body);
        if body.width == 0 || body.height == 0 {
            return;
        }
        let totals = self.totals();
        // The header is the error colour throughout, as the mock has it: this
        // is the one screen that destroys work, and it says so before the
        // first row is read.
        let counts = format!(
            "{} · {} · {}",
            plural(totals.terminals, "terminal"),
            plural(totals.worktrees, "worktree"),
            bytes(totals.bytes)
        );
        let spent = "end session ".len() + self.name.chars().count().min(20);
        let room = usize::from(header.width).saturating_sub(spent + counts.chars().count());
        Line::from(vec![
            Span::styled("end session ", theme.style(Role::Error)),
            Span::styled(
                truncate(&self.name, 20),
                theme.ink_style(Ink::Text).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(room)),
            Span::styled(counts, theme.ink_style(Ink::Subtext)),
        ])
        .render(header, buf);

        if !self.ready() {
            Line::styled(
                "counting what would be removed…",
                theme.ink_style(Ink::Subtext),
            )
            .render(body, buf);
            return;
        }

        let mut lines = Vec::new();
        // Per repo first: what is about to happen to each, in its own words.
        let room = usize::from(body.height).saturating_sub(3);
        for row in self.rows.iter().take(room) {
            let (glyph, role) = if row.dirty_files > 0 {
                ("◆", Role::Dirty)
            } else if row.terminal.is_some() {
                ("◐", Role::Accent)
            } else {
                ("●", Role::Clean)
            };
            let mut detail = String::new();
            if row.ahead > 0 {
                detail.push_str(&format!("↑{} pushed · ", row.ahead));
            }
            detail.push_str(&if row.dirty_files > 0 {
                format!("{} files uncommitted", row.dirty_files)
            } else {
                "clean".into()
            });
            if let Some(command) = &row.foreground {
                detail.push_str(&format!(" · {command} running"));
            }
            detail.push_str(&format!(" · {}", bytes(row.size)));

            let warning = if row.dirty_files > 0 {
                "will be lost"
            } else if row.foreground.is_some() {
                "terminal busy"
            } else {
                ""
            };
            let detail_room = usize::from(body.width).saturating_sub(20 + 4 + warning.len() + 2);
            lines.push(Line::from(vec![
                Span::styled(format!("{glyph} "), theme.style(role)),
                Span::styled(
                    format!("{:<20}", truncate(&row.worktree.repo.0, 20)),
                    theme.ink_style(Ink::Text),
                ),
                Span::styled(
                    format!("{:<detail_room$}", truncate(&detail, detail_room)),
                    theme.ink_style(Ink::Subtext),
                ),
                Span::styled(warning, theme.style(role)),
            ]));
        }

        lines.push(Line::raw(""));
        // Then the aggregate, in one sentence.
        lines.push(Line::styled(
            format!(
                "closes {} and removes all {} · {} reclaimed",
                plural(totals.terminals, "terminal"),
                plural(totals.worktrees, "worktree"),
                bytes(totals.bytes)
            ),
            theme.ink_style(Ink::Subtext),
        ));
        // Then the loss, last, and only when there is some.
        if totals.dirty_files > 0 {
            lines.push(Line::styled(
                format!(
                    "{} uncommitted in {} will be lost",
                    plural(
                        usize::try_from(totals.dirty_files).unwrap_or(usize::MAX),
                        "file"
                    ),
                    self.dirty_repos().join(", ")
                ),
                theme.style(Role::Error).add_modifier(Modifier::BOLD),
            ));
        }
        Paragraph::new(lines).render(body, buf);
    }

    /// The repos with uncommitted work, named so the warning is specific.
    fn dirty_repos(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|row| row.dirty_files > 0)
            .map(|row| row.worktree.repo.0.clone())
            .collect()
    }
}

/// What ending the session costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    pub terminals: usize,
    pub worktrees: usize,
    pub bytes: u64,
    pub dirty_files: u64,
}

/// Whether `row` belongs to the session being ended.
///
/// The clone is never owned and another session's is never ours to remove, so
/// both are excluded by construction rather than by a later check.
fn owned_by(row: &WorktreeRow, ending: &SessionId, open: Option<&SessionId>) -> bool {
    match &row.ownership {
        // Relative to where we are standing: `Ours` is the open session's.
        Ownership::Ours => open == Some(ending),
        Ownership::Other(owner) => owner == ending,
        Ownership::Unowned | Ownership::Clone => false,
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
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
    use grove_proto::{TerminalId, WorktreeRef};
    use ratatui::layout::Rect;

    fn row(repo: &str, ownership: Ownership) -> WorktreeRow {
        WorktreeRow {
            worktree: WorktreeRef {
                repo: RepoId(repo.into()),
                branch: "feat/x".into(),
            },
            detached: false,
            ownership,
            ahead: 4,
            behind: 0,
            dirty_files: 0,
            age: 0,
            size: 286 * 1024 * 1024,
            terminal: None,
            foreground: None,
            stale: false,
        }
    }

    fn theme() -> Theme {
        Theme::resolve(&grove_lua::TuiConfig::default(), crate::theme::Depth::True).0
    }

    fn ours() -> SessionId {
        SessionId("invoice split".into())
    }

    fn painted(end: &EndSession) -> String {
        let area = Rect {
            x: 0,
            y: 0,
            width: 110,
            height: 12,
        };
        let mut buf = Buffer::empty(area);
        end.render(&mut buf, crate::overlay::Parts::of(area), &theme());
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
    fn only_what_the_session_owns_is_listed() {
        // The acceptance criterion. Another session's worktrees are read-only
        // and the clone is the repository itself; listing either invites the
        // belief that it is about to go.
        let mut end = EndSession::default();
        end.begin(
            ours(),
            "invoice split".into(),
            vec![RepoId("billing".into())],
        );
        end.take(
            &RepoId("billing".into()),
            &[
                row("billing", Ownership::Ours),
                row(
                    "billing",
                    Ownership::Other(SessionId("someone else".into())),
                ),
                row("billing", Ownership::Unowned),
                row("billing", Ownership::Clone),
            ],
            Some(&ours()),
        );
        assert_eq!(end.rows().len(), 1);
        assert_eq!(end.totals().worktrees, 1);
    }

    #[test]
    fn ending_a_detached_session_finds_its_worktrees_under_other() {
        // `Ours` means the *open* session. Ending a detached one means
        // looking for `Other(that session)` — the same worktree is `Ours` or
        // `Other` depending on where you are standing, and getting it
        // backwards either lists nothing or lists someone else's work.
        let elsewhere = SessionId("tokens review".into());
        let mut end = EndSession::default();
        end.begin(
            elsewhere.clone(),
            "tokens review".into(),
            vec![RepoId("web".into())],
        );
        end.take(
            &RepoId("web".into()),
            &[
                // Open session's — not the one being ended.
                row("web", Ownership::Ours),
                row("web", Ownership::Other(elsewhere.clone())),
            ],
            Some(&ours()),
        );
        assert_eq!(end.rows().len(), 1);
        assert_eq!(end.rows()[0].ownership, Ownership::Other(elsewhere));
    }

    #[test]
    fn the_figures_are_the_sum_of_what_is_listed() {
        // Acceptance: the figures match what the daemon actually removes. The
        // aggregate is summed from the rows on screen, so the total and the
        // list cannot disagree — a total that does not match what is above it
        // is the specific way a confirm lies.
        let mut end = EndSession::default();
        end.begin(
            ours(),
            "invoice split".into(),
            vec![RepoId("a".into()), RepoId("b".into())],
        );
        let mut busy = row("a", Ownership::Ours);
        busy.terminal = Some(TerminalId(1));
        busy.dirty_files = 9;
        end.take(&RepoId("a".into()), &[busy], Some(&ours()));
        end.take(
            &RepoId("b".into()),
            &[row("b", Ownership::Ours)],
            Some(&ours()),
        );

        let totals = end.totals();
        assert_eq!(totals.worktrees, 2);
        assert_eq!(totals.terminals, 1);
        assert_eq!(totals.dirty_files, 9);
        assert_eq!(totals.bytes, 2 * 286 * 1024 * 1024);
    }

    #[test]
    fn uncommitted_work_is_called_out_per_repo_and_in_aggregate() {
        // Acceptance, and §4.6's shape: the per-repo line says "will be lost"
        // and the summary names the repo and the count.
        let mut end = EndSession::default();
        end.begin(
            ours(),
            "invoice split".into(),
            vec![RepoId("billing".into())],
        );
        let mut dirty = row("billing", Ownership::Ours);
        dirty.dirty_files = 9;
        end.take(&RepoId("billing".into()), &[dirty], Some(&ours()));

        let screen = painted(&end);
        assert!(screen.contains("9 files uncommitted"), "{screen}");
        assert!(screen.contains("will be lost"), "{screen}");
        assert!(
            screen.contains("9 files uncommitted in billing"),
            "{screen}"
        );
    }

    #[test]
    fn a_clean_session_gets_no_loss_warning() {
        // Acceptance. A warning that always appears is a warning nobody reads
        // — which matters most on the screen where one of them is true.
        let mut end = EndSession::default();
        end.begin(ours(), "invoice split".into(), vec![RepoId("a".into())]);
        end.take(
            &RepoId("a".into()),
            &[row("a", Ownership::Ours)],
            Some(&ours()),
        );

        let screen = painted(&end);
        assert!(!screen.contains("will be lost"), "{screen}");
        assert!(screen.contains("clean"), "{screen}");
    }

    #[test]
    fn a_busy_terminal_is_surfaced_before_removal() {
        let mut end = EndSession::default();
        end.begin(ours(), "invoice split".into(), vec![RepoId("sdk".into())]);
        let mut busy = row("sdk", Ownership::Ours);
        busy.terminal = Some(TerminalId(3));
        busy.foreground = Some("install".into());
        end.take(&RepoId("sdk".into()), &[busy], Some(&ours()));

        let screen = painted(&end);
        assert!(screen.contains("install running"), "{screen}");
        assert!(screen.contains("terminal busy"), "{screen}");
    }

    #[test]
    fn the_figures_are_not_shown_until_every_repo_has_answered() {
        // A total that grows while someone is reading it is worse than a
        // moment's wait before a destructive act.
        let mut end = EndSession::default();
        end.begin(
            ours(),
            "invoice split".into(),
            vec![RepoId("a".into()), RepoId("b".into())],
        );
        end.take(
            &RepoId("a".into()),
            &[row("a", Ownership::Ours)],
            Some(&ours()),
        );
        assert!(!end.ready());
        assert!(painted(&end).contains("counting"), "{}", painted(&end));

        end.take(
            &RepoId("b".into()),
            &[row("b", Ownership::Ours)],
            Some(&ours()),
        );
        assert!(end.ready());
        assert!(painted(&end).contains("removes all 2 worktrees"));
    }

    #[test]
    fn the_screen_reads_consequences_then_aggregate_then_loss() {
        // §4.6's order, which is the point of the screen: what happens to
        // each repo, what it adds up to, and what cannot be undone — last,
        // where it is the final thing read before the key.
        let mut end = EndSession::default();
        end.begin(
            ours(),
            "invoice split".into(),
            vec![RepoId("billing".into())],
        );
        let mut dirty = row("billing", Ownership::Ours);
        dirty.dirty_files = 9;
        end.take(&RepoId("billing".into()), &[dirty], Some(&ours()));

        let screen = painted(&end);
        let per_repo = screen.find("9 files uncommitted").expect("per repo");
        let aggregate = screen.find("removes all").expect("aggregate");
        let loss = screen.rfind("will be lost").expect("loss");
        assert!(per_repo < aggregate, "{screen}");
        assert!(aggregate < loss, "{screen}");
        // The key itself is the last thing on the screen, below all of this,
        // in the footer the overlay draws from END's bindings.
        assert_eq!(
            end.footer().first().map(|hint| hint.label.clone()),
            Some("remove everything".to_string())
        );
    }

    #[test]
    fn cancelling_forgets_everything() {
        let mut end = EndSession::default();
        end.begin(ours(), "x".into(), vec![RepoId("a".into())]);
        end.take(
            &RepoId("a".into()),
            &[row("a", Ownership::Ours)],
            Some(&ours()),
        );
        end.cancel();
        assert!(end.session().is_none());
        assert_eq!(end.totals().worktrees, 0);
    }

    #[test]
    fn counts_agree_with_their_nouns() {
        assert_eq!(plural(1, "terminal"), "1 terminal");
        assert_eq!(plural(0, "worktree"), "0 worktrees");
        assert_eq!(plural(3, "file"), "3 files");
    }
}
