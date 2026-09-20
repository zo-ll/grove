//! The prune picker (SPEC §5).
//!
//! The one screen where a mistake destroys work, so the rule it follows is
//! narrow and the reasons it shows are specific.
//!
//! **The pre-selection is the daemon's.** A row arrives with `blockers` empty
//! or not, and empty is the only thing that pre-checks it. The client does not
//! re-derive "merged and clean and nothing unpushed and unowned" from the
//! parts — the daemon fetched first so "merged" means merged against the
//! remote, and a second implementation of the rule would be a second answer to
//! "is this safe to delete".
//!
//! Everything else is listed with its reason and left unchecked, and can still
//! be ticked deliberately. One thing cannot: a worktree a live session owns is
//! refused at prune time (§2.4), because ending that session is how its
//! worktrees are released.
//!
//! `a` checks all **safe** rows, never all rows. A key that selects everything
//! on this screen is a key that deletes someone's uncommitted work, and it is
//! one keystroke from `enter`.
//!
//! There is no "open PR" state. grove has no forge integration (§11), and a
//! column that claimed to know would be inventing it.

use grove_proto::{PruneBlocker, PruneCandidate, PruneState};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::text::truncate;
use crate::theme::{Role, Theme};

/// One row of the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub candidate: PruneCandidate,
    pub checked: bool,
}

impl Row {
    /// Whether the daemon judged this row safe.
    ///
    /// A single question with a single answer: no blockers. Asking it of the
    /// blockers rather than of the state means a row is safe exactly when the
    /// daemon says it is.
    pub fn safe(&self) -> bool {
        self.candidate.blockers.is_empty()
    }

    /// Whether prune would be refused for it however it is ticked.
    ///
    /// Ownership is the one blocker the user cannot override here: §2.4 makes
    /// another session's worktrees read-only, and ending that session is how
    /// they are released.
    pub fn owned_elsewhere(&self) -> bool {
        self.candidate
            .blockers
            .iter()
            .any(|blocker| matches!(blocker, PruneBlocker::Owned { .. }))
    }
}

/// The picker's state.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Prune {
    rows: Vec<Row>,
    cursor: usize,
}

impl Prune {
    /// Take the daemon's candidates, pre-checking exactly what it judged safe.
    pub fn set(&mut self, candidates: Vec<PruneCandidate>) {
        self.rows = candidates
            .into_iter()
            .map(|candidate| Row {
                checked: candidate.blockers.is_empty(),
                candidate,
            })
            .collect();
        self.cursor = 0;
    }

    /// The rows as the picker holds them. The screen draws itself, so this is
    /// for tests asserting what the daemon's verdict became.
    #[cfg(test)]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn checked(&self) -> Vec<&Row> {
        self.rows.iter().filter(|row| row.checked).collect()
    }

    pub fn count(&self) -> usize {
        self.rows.iter().filter(|row| row.checked).count()
    }

    /// Bytes the checked rows would reclaim, for the live total.
    pub fn reclaimable(&self) -> u64 {
        self.rows
            .iter()
            .filter(|row| row.checked)
            .map(|row| row.candidate.size)
            .sum()
    }

    /// Flip the row under the cursor.
    ///
    /// A blocked row can still be ticked — the user may know something grove
    /// does not — except one owned by a live session, which prune refuses
    /// anyway. Offering a checkbox that cannot be honoured is a promise the
    /// screen cannot keep.
    pub fn toggle(&mut self) -> bool {
        match self.rows.get_mut(self.cursor) {
            Some(row) if row.owned_elsewhere() => false,
            Some(row) => {
                row.checked = !row.checked;
                true
            }
            None => false,
        }
    }

    /// Check every safe row, and only those.
    pub fn select_safe(&mut self) -> bool {
        let mut changed = false;
        for row in &mut self.rows {
            let safe = row.candidate.blockers.is_empty();
            if safe && !row.checked {
                row.checked = true;
                changed = true;
            }
        }
        changed
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

    /// Draw the picker into `area`.
    pub fn render(&self, buf: &mut Buffer, area: Rect, theme: &Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let mut lines = vec![Line::from(vec![
            Span::styled("❯ prune", theme.style(Role::Accent)),
            Span::raw("   "),
            Span::styled(
                format!("{} selected · {}", self.count(), bytes(self.reclaimable())),
                theme.style(Role::Muted),
            ),
        ])];

        if self.rows.is_empty() {
            lines.push(Line::styled("  nothing to prune", theme.style(Role::Muted)));
        }

        let room = usize::from(area.height).saturating_sub(2);
        for (index, row) in self.rows.iter().take(room).enumerate() {
            let here = index == self.cursor;
            let mark = if row.checked { "[x]" } else { "[ ]" };
            let style = if here {
                theme.style(Role::Accent).add_modifier(Modifier::BOLD)
            } else if row.checked {
                theme.style(Role::Clean)
            } else {
                theme.style(Role::Muted)
            };
            let branch = truncate(&row.candidate.worktree.branch, 22);
            let repo = truncate(&row.candidate.worktree.repo.0, 16);
            lines.push(Line::from(vec![
                Span::styled(if here { "❯" } else { " " }, theme.style(Role::Accent)),
                Span::styled(mark, style),
                Span::raw(" "),
                Span::styled(format!("{repo:<16} "), style),
                Span::styled(format!("{branch:<22} "), style),
                // The reason, in its own colour: safe rows read as safe and
                // blocked ones read as the thing that blocks them.
                Span::styled(
                    format!("{:<22}", reason(row)),
                    if row.safe() {
                        theme.style(Role::Clean)
                    } else if row.owned_elsewhere() {
                        theme.style(Role::Error)
                    } else {
                        theme.style(Role::Dirty)
                    },
                ),
                Span::styled(bytes(row.candidate.size), theme.style(Role::Muted)),
            ]));
        }

        lines.push(Line::styled(
            format!("  space toggle · a all safe · enter prune {}", self.count()),
            theme.style(Role::Muted),
        ));
        Paragraph::new(lines).render(area, buf);
    }
}

/// Why a row is or is not safe, in the words §5 uses.
///
/// Every blocker gets its own wording. "Not safe" would be true of all of them
/// and useful for none: a dirty worktree wants committing, an unmerged one
/// wants a decision, and an owned one wants a different session closed.
fn reason(row: &Row) -> String {
    if let Some(blocker) = row.candidate.blockers.first() {
        return match blocker {
            PruneBlocker::FetchFailed => "fetch failed".into(),
            PruneBlocker::Unmerged => "unmerged".into(),
            PruneBlocker::Dirty { files } => format!("{files} files"),
            PruneBlocker::Unpushed { commits } => format!("↑{commits}"),
            PruneBlocker::Owned { name, .. } => format!("● owned by {name}"),
        };
    }
    match row.candidate.state {
        PruneState::Merged => "merged    clean".into(),
        PruneState::UpstreamGone => "gone      clean".into(),
        // Safe with no state is not a shape the daemon produces, and guessing
        // would put a confident word on a row nobody vouched for.
        PruneState::Neither => "safe".into(),
    }
}

/// Bytes as §5 writes them.
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
    use grove_domain::{RepoId, SessionId};
    use grove_proto::WorktreeRef;

    fn candidate(repo: &str, branch: &str, blockers: Vec<PruneBlocker>) -> PruneCandidate {
        PruneCandidate {
            worktree: WorktreeRef {
                repo: RepoId(repo.into()),
                branch: branch.into(),
            },
            state: if blockers.is_empty() {
                PruneState::Merged
            } else {
                PruneState::Neither
            },
            size: 412 * 1024 * 1024,
            blockers,
        }
    }

    fn theme() -> Theme {
        Theme::resolve(&grove_lua::TuiConfig::default(), crate::theme::Depth::True).0
    }

    fn listing() -> Vec<PruneCandidate> {
        vec![
            candidate("web-app", "feat/ABC-3980", vec![]),
            candidate(
                "sdk-js",
                "feat/ABC-3980",
                vec![PruneBlocker::Dirty { files: 2 }],
            ),
            candidate(
                "web-app",
                "wip/checkout",
                vec![PruneBlocker::Unpushed { commits: 6 }],
            ),
            candidate(
                "design-system",
                "feat/ABC-4110",
                vec![PruneBlocker::Unmerged],
            ),
            candidate(
                "billing-svc",
                "feat/ABC-4471",
                vec![PruneBlocker::Owned {
                    session: SessionId("s".into()),
                    name: "invoice split".into(),
                }],
            ),
        ]
    }

    #[test]
    fn only_what_the_daemon_judged_safe_is_pre_checked() {
        // The acceptance criterion. The rule — merged or gone, clean, nothing
        // unpushed, unowned — lives in the daemon, which fetched first so
        // "merged" means merged against the remote. Re-deriving it here would
        // be a second answer to "is this safe to delete".
        let mut prune = Prune::default();
        prune.set(listing());
        assert_eq!(prune.count(), 1);
        assert_eq!(
            prune.checked()[0].candidate.worktree.repo.0,
            "web-app",
            "the one row with no blockers"
        );
    }

    #[test]
    fn a_row_is_safe_exactly_when_it_has_no_blockers() {
        // Not when its state looks right: a merged-but-dirty row is not safe,
        // and the blockers are what say so.
        let mut prune = Prune::default();
        prune.set(vec![PruneCandidate {
            state: PruneState::Merged,
            ..candidate("x", "y", vec![PruneBlocker::Dirty { files: 9 }])
        }]);
        assert!(!prune.rows()[0].safe());
        assert_eq!(prune.count(), 0, "merged is not enough");
    }

    #[test]
    fn every_disqualifying_reason_reads_differently() {
        // Acceptance: each renders distinctly. "Not safe" would be true of
        // all of them and useful for none — a dirty worktree wants committing,
        // an unmerged one wants a decision, an owned one wants another session
        // closed.
        let mut prune = Prune::default();
        prune.set(listing());
        let reasons: Vec<String> = prune.rows().iter().map(reason).collect();
        assert_eq!(reasons[1], "2 files");
        assert_eq!(reasons[2], "↑6");
        assert_eq!(reasons[3], "unmerged");
        assert!(reasons[4].contains("invoice split"));
        let unique: std::collections::HashSet<&String> = reasons.iter().collect();
        assert_eq!(unique.len(), reasons.len(), "{reasons:?}");
    }

    #[test]
    fn a_failed_fetch_says_so_rather_than_claiming_unmerged() {
        // The verdict could not be made, which is a different thing from
        // "not merged" — and it is the daemon's own wording.
        let mut prune = Prune::default();
        prune.set(vec![candidate("a", "b", vec![PruneBlocker::FetchFailed])]);
        assert_eq!(reason(&prune.rows()[0]), "fetch failed");
        assert_eq!(prune.count(), 0);
    }

    #[test]
    fn a_selects_every_safe_row_and_no_others() {
        // The acceptance criterion that matters most: a key that selects
        // everything here deletes someone's uncommitted work, and it is one
        // keystroke from `enter`.
        let mut prune = Prune::default();
        let mut rows = listing();
        rows.push(candidate("another", "feat/safe", vec![]));
        prune.set(rows);

        // Safe rows arrive checked, so `a` is what brings them back after the
        // user has unchecked some — and it must bring back only those.
        assert!(prune.toggle(), "uncheck the first safe row");
        assert_eq!(prune.count(), 1);
        assert!(prune.move_down(), "onto the dirty row");
        assert!(prune.toggle(), "tick a blocked row deliberately");
        assert_eq!(prune.count(), 2);

        assert!(prune.select_safe());
        assert_eq!(
            prune.count(),
            3,
            "both safe rows, plus the one ticked by hand"
        );
        let blocked_checked = prune.checked().iter().filter(|row| !row.safe()).count();
        assert_eq!(
            blocked_checked, 1,
            "a added no blocked row of its own — only the one the user ticked"
        );

        // And with nothing to change it says so, rather than costing a frame.
        assert!(!prune.select_safe());
    }

    #[test]
    fn a_blocked_row_can_still_be_ticked_deliberately() {
        // §5: listed with the reason visible, unchecked, but selectable. The
        // user may know something grove does not.
        let mut prune = Prune::default();
        prune.set(listing());
        assert!(prune.move_down(), "onto the dirty row");
        assert!(prune.toggle());
        assert_eq!(prune.count(), 2);
    }

    #[test]
    fn a_worktree_another_session_owns_cannot_be_ticked_at_all() {
        // §2.4 makes it read-only and prune refuses it anyway. A checkbox
        // that cannot be honoured is a promise the screen cannot keep.
        let mut prune = Prune::default();
        prune.set(listing());
        for _ in 0..4 {
            assert!(prune.move_down());
        }
        assert!(prune.rows()[4].owned_elsewhere());
        assert!(!prune.toggle(), "ownership is not the user's to override");
        assert_eq!(prune.count(), 1);
    }

    #[test]
    fn the_total_follows_what_is_checked() {
        // Acceptance: the running total updates live, because "1.0 GB" is the
        // only part of this screen that says how much is at stake.
        let mut prune = Prune::default();
        prune.set(listing());
        let one = prune.reclaimable();
        assert!(one > 0);
        assert!(prune.move_down());
        assert!(prune.toggle());
        assert!(
            prune.reclaimable() > one,
            "ticking a row adds its size to the total"
        );
    }

    #[test]
    fn there_is_no_forge_state() {
        // Acceptance, and §11's cut: grove has no forge integration, so a
        // column claiming to know about pull requests would be inventing it.
        let mut prune = Prune::default();
        prune.set(listing());
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 10,
        };
        let mut buf = Buffer::empty(area);
        prune.render(&mut buf, area, &theme());
        let painted: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        for invented in ["pull request", "open pr", " pr ", "review", "approved"] {
            assert!(!painted.contains(invented), "{invented:?} in:\n{painted}");
        }
    }

    #[test]
    fn the_footer_says_what_enter_will_remove() {
        let mut prune = Prune::default();
        prune.set(listing());
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 10,
        };
        let mut buf = Buffer::empty(area);
        prune.render(&mut buf, area, &theme());
        let painted: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(painted.contains("enter prune 1"), "{painted}");
        assert!(painted.contains("a all safe"), "{painted}");
        assert!(painted.contains("1 selected"), "{painted}");
    }

    #[test]
    fn sizes_read_the_way_the_spec_writes_them() {
        assert_eq!(bytes(0), "—");
        assert_eq!(bytes(412 * 1024 * 1024), "412 MB");
        assert_eq!(bytes(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn an_empty_listing_says_so() {
        let prune = Prune::default();
        assert!(prune.is_empty());
        assert_eq!(prune.count(), 0);
        assert_eq!(prune.reclaimable(), 0);
    }
}
