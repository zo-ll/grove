//! Scenarios: the worlds the TUI can be launched into.
//!
//! Every screen needs states that are impractical to conjure from real
//! repositories — a worktree both dirty *and* owned by another session, a prune
//! list with one row per disqualifying reason, a session member with no
//! worktrees at all. Producing those for real means arranging several git
//! repositories into a specific shape and then keeping them that way.
//!
//! So they are written down instead. A scenario is data, and the fake daemon
//! answers every request from it, which makes the TUI exercisable with no git
//! and no daemon, and makes rendered frames comparable across runs.

use std::collections::BTreeMap;

use grove_domain::{Ownership, RepoId, SessionId, SessionState};
use grove_proto::{
    Attrs, Cell, Color, DiffFile, DiffLine, PruneBlocker, PruneCandidate, PruneState, RepoRow,
    Screen, SessionRow, TerminalId, TerminalRow, TerminalTarget, WorktreeRef, WorktreeRow,
};

/// One world the TUI can be launched into.
pub struct Scenario {
    pub name: &'static str,
    /// What this scenario exists to exercise, so a screen's fixture can be
    /// found by what it covers rather than by guessing from its name.
    pub covers: &'static str,
    pub sessions: Vec<SessionRow>,
    pub repos: Vec<RepoRow>,
    /// Worktrees per repo, including ones this session does not own and the
    /// clone — the WORKTREES pane lists all of them.
    pub worktrees: BTreeMap<String, Vec<WorktreeRow>>,
    pub prune: Vec<PruneCandidate>,
    pub terminals: Vec<TerminalRow>,
    /// Lines each terminal emits, one per tick, cycling. Enough to prove the
    /// pane updates without pretending to be a real pty.
    pub output: BTreeMap<u64, Vec<String>>,
    pub diff_files: Vec<DiffFile>,
    pub diff_hunks: Vec<DiffLine>,
}

fn repo(id: &str, worktrees: u32, dirty: bool, member: bool) -> RepoRow {
    RepoRow {
        repo: RepoId(id.into()),
        name: id.into(),
        base_branch: "origin/main".into(),
        base_from_origin_head: true,
        worktrees,
        dirty,
        member,
    }
}

#[allow(clippy::too_many_arguments)]
fn wt(
    repo: &str,
    branch: &str,
    ownership: Ownership,
    ahead: u64,
    behind: u64,
    dirty_files: u32,
    terminal: Option<u64>,
    foreground: Option<&str>,
    stale: bool,
) -> WorktreeRow {
    WorktreeRow {
        worktree: WorktreeRef {
            repo: RepoId(repo.into()),
            branch: branch.into(),
        },
        detached: false,
        ownership,
        ahead,
        behind,
        dirty_files,
        age: 7200,
        size: 286_000_000,
        terminal: terminal.map(TerminalId),
        foreground: foreground.map(str::to_string),
        stale,
    }
}

/// The scenario the mock in SPEC §4.1 draws, plus the states it does not.
///
/// Deliberately includes, because no other screen state is as easy to get
/// wrong: a worktree that is dirty *and* owned by another session (read-only
/// but visibly unsaved), a member repo with zero worktrees, a clone that can
/// never be owned, and a row with stale refs.
pub fn invoice_split() -> Scenario {
    let mut worktrees = BTreeMap::new();
    worktrees.insert(
        "billing-service".to_string(),
        vec![
            wt(
                "billing-service",
                "feat/ABC-4471-invoice-split",
                Ownership::Ours,
                4,
                2,
                9,
                Some(1),
                Some("pnpm test"),
                false,
            ),
            // Dirty AND owned elsewhere: read-only to this session, yet holding
            // uncommitted work. The pane must show both facts at once.
            wt(
                "billing-service",
                "fix/ABC-4402-retry-jitter",
                Ownership::Other(SessionId("retry-jitter".into())),
                1,
                0,
                3,
                None,
                None,
                false,
            ),
            // Nobody's: the only kind that can be adopted.
            wt(
                "billing-service",
                "spike/perf",
                Ownership::Unowned,
                0,
                0,
                0,
                None,
                None,
                true,
            ),
            // The clone. Never ownable, never removed by end-session.
            wt(
                "billing-service",
                "main",
                Ownership::Clone,
                0,
                11,
                0,
                Some(2),
                Some("zsh"),
                false,
            ),
        ],
    );
    worktrees.insert(
        "web-app".to_string(),
        vec![wt(
            "web-app",
            "feat/ABC-4471-invoice-split",
            Ownership::Ours,
            2,
            2,
            0,
            Some(3),
            Some("vite dev"),
            false,
        )],
    );
    // A member with no worktrees at all — "this task touches this repo, I have
    // not branched yet". Normal, and easy to render as an error by accident.
    worktrees.insert("search-index".to_string(), Vec::new());

    let mut output = BTreeMap::new();
    output.insert(
        1,
        vec![
            " PASS  src/invoice/split.test.ts (18)".into(),
            " FAIL  src/invoice/proration.test.ts".into(),
            "   expected 4 line items, received 3".into(),
        ],
    );
    output.insert(2, vec!["$ ".into()]);
    output.insert(
        3,
        vec![
            "  VITE v7.0.4  ready in 412 ms".into(),
            "  ➜  Local:   http://localhost:5173/".into(),
        ],
    );

    Scenario {
        name: "invoice-split",
        covers: "dash with every ownership state, a zero-worktree member, stale refs, live terminals",
        sessions: vec![
            SessionRow {
                id: SessionId("invoice-split".into()),
                name: "invoice split".into(),
                members: vec![
                    "billing-service".into(),
                    "web-app".into(),
                    "search-index".into(),
                ],
                state: SessionState::Attached,
                terminals: 3,
                since: 7200,
                size: 794_000_000,
            },
            SessionRow {
                id: SessionId("retry-jitter".into()),
                name: "retry jitter".into(),
                members: vec!["billing-service".into(), "api-gateway".into()],
                state: SessionState::Detached,
                terminals: 1,
                since: 172_800,
                size: 286_000_000,
            },
            // Closed: terminals dead, worktrees still on disk, size reclaimable.
            SessionRow {
                id: SessionId("tax-codes".into()),
                name: "tax codes".into(),
                members: vec!["billing-service".into(), "web-app".into(), "sdk-js".into()],
                state: SessionState::Closed,
                terminals: 0,
                since: 1_900_800,
                size: 794_000_000,
            },
        ],
        repos: vec![
            repo("billing-service", 4, true, true),
            repo("web-app", 1, false, true),
            repo("search-index", 0, false, true),
            repo("api-gateway", 2, false, false),
            repo("design-system", 1, false, false),
        ],
        worktrees,
        // One row per disqualifying reason, plus safe rows of each kind, so the
        // picker's pre-selection rule is exercised in full.
        prune: vec![
            PruneCandidate {
                worktree: WorktreeRef {
                    repo: RepoId("web-app".into()),
                    branch: "feat/ABC-3980-tax-codes".into(),
                },
                state: PruneState::Merged,
                size: 412_000_000,
                blockers: vec![],
            },
            PruneCandidate {
                worktree: WorktreeRef {
                    repo: RepoId("api-gateway".into()),
                    branch: "spike/grpc-transport".into(),
                },
                state: PruneState::UpstreamGone,
                size: 188_000_000,
                blockers: vec![],
            },
            PruneCandidate {
                worktree: WorktreeRef {
                    repo: RepoId("sdk-js".into()),
                    branch: "feat/ABC-3980-tax-codes".into(),
                },
                state: PruneState::Merged,
                size: 96_000_000,
                blockers: vec![PruneBlocker::Dirty { files: 2 }],
            },
            PruneCandidate {
                worktree: WorktreeRef {
                    repo: RepoId("web-app".into()),
                    branch: "wip/checkout-redesign".into(),
                },
                state: PruneState::Neither,
                size: 404_000_000,
                blockers: vec![
                    PruneBlocker::Unmerged,
                    PruneBlocker::Unpushed { commits: 6 },
                    PruneBlocker::Dirty { files: 31 },
                ],
            },
            PruneCandidate {
                worktree: WorktreeRef {
                    repo: RepoId("billing-service".into()),
                    branch: "feat/ABC-4471-invoice-split".into(),
                },
                state: PruneState::Neither,
                size: 286_000_000,
                blockers: vec![PruneBlocker::Owned {
                    session: SessionId("invoice-split".into()),
                    name: "invoice split".into(),
                }],
            },
        ],
        terminals: vec![
            TerminalRow {
                terminal: TerminalId(1),
                target: TerminalTarget::Worktree(WorktreeRef {
                    repo: RepoId("billing-service".into()),
                    branch: "feat/ABC-4471-invoice-split".into(),
                }),
                foreground: Some("pnpm test".into()),
            },
            // A scratch shell already running, so reattach has one to rediscover.
            TerminalRow {
                terminal: TerminalId(9),
                target: TerminalTarget::Scratch { cwd: None },
                foreground: None,
            },
        ],
        output,
        diff_files: vec![
            DiffFile {
                path: "src/invoice/split.ts".into(),
                status: 'M',
                added: 184,
                removed: 22,
            },
            DiffFile {
                path: "src/invoice/__tests__/split.test.ts".into(),
                status: 'A',
                added: 74,
                removed: 0,
            },
            DiffFile {
                path: "src/legacy/prorate.ts".into(),
                status: 'D',
                added: 0,
                removed: 26,
            },
            DiffFile {
                path: ".env.local".into(),
                status: '?',
                added: 0,
                removed: 0,
            },
        ],
        diff_hunks: vec![
            DiffLine::Header("@@ -198,12 +198,26 @@ export function split(".into()),
            DiffLine::Context("  const items = invoice.lineItems;".into()),
            DiffLine::Removed("  return items.map(toLine);".into()),
            DiffLine::Added("  const boundary = cycleBoundary(invoice);".into()),
            DiffLine::Added("  return [...before.map(toLine)];".into()),
        ],
    }
}

/// Nothing at all: no repos, no sessions. The dash's empty state (SPEC §4.1).
///
/// Its own scenario because "renders correctly with no data" is the case most
/// often left untested and most often seen first, on a brand-new workspace.
pub fn empty() -> Scenario {
    Scenario {
        name: "empty",
        covers: "the dash empty state on a fresh workspace",
        sessions: Vec::new(),
        repos: Vec::new(),
        worktrees: BTreeMap::new(),
        prune: Vec::new(),
        terminals: Vec::new(),
        output: BTreeMap::new(),
        diff_files: Vec::new(),
        diff_hunks: Vec::new(),
    }
}

/// A terminal with a long-running foreground process, for the "terminal busy"
/// warning the end-session confirm shows before removing anything (SPEC §4.6).
pub fn busy() -> Scenario {
    let mut s = invoice_split();
    s.name = "busy";
    s.covers = "end-session confirm with a busy terminal and uncommitted work";
    if let Some(rows) = s.worktrees.get_mut("web-app") {
        for r in rows {
            r.foreground = Some("pnpm install".into());
            r.dirty_files = 12;
        }
    }
    s
}

/// Every scenario, by name.
pub fn all() -> Vec<Scenario> {
    vec![invoice_split(), empty(), busy()]
}

pub fn by_name(name: &str) -> Option<Scenario> {
    all().into_iter().find(|s| s.name == name)
}

/// A screen of canned output, for the terminal pane.
pub fn screen_for(lines: &[String], rows: u16, cols: u16) -> Screen {
    let cells = (0..rows)
        .map(|r| {
            let line = lines.get(r as usize).cloned().unwrap_or_default();
            (0..cols)
                .map(|c| {
                    let ch = line.chars().nth(c as usize).unwrap_or(' ');
                    // Colour the way a test runner does, so the pane is proved
                    // to carry attributes rather than only text.
                    let fg = if line.contains("PASS") {
                        Color::Indexed(2)
                    } else if line.contains("FAIL") {
                        Color::Indexed(1)
                    } else {
                        Color::Default
                    };
                    Cell {
                        text: ch.to_string(),
                        fg,
                        bg: Color::Default,
                        attrs: Attrs {
                            bold: line.contains("FAIL"),
                            ..Attrs::default()
                        },
                    }
                })
                .collect()
        })
        .collect();
    Screen {
        rows,
        cols,
        cells,
        cursor: Some((0, 0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scenario_names_what_it_covers() {
        for s in all() {
            assert!(!s.name.is_empty());
            assert!(
                !s.covers.is_empty(),
                "{}: a fixture nobody can identify gets duplicated instead of reused",
                s.name
            );
        }
    }

    #[test]
    fn the_main_scenario_carries_all_four_ownership_states() {
        // These are the WORKTREES pane's four rows, and the reason the fixture
        // exists: arranging them from real repos means three sessions and a
        // hand-made worktree.
        let s = invoice_split();
        let rows = &s.worktrees["billing-service"];
        let kinds: Vec<_> = rows.iter().map(|r| &r.ownership).collect();
        assert!(kinds.iter().any(|o| matches!(o, Ownership::Ours)));
        assert!(kinds.iter().any(|o| matches!(o, Ownership::Other(_))));
        assert!(kinds.iter().any(|o| matches!(o, Ownership::Unowned)));
        assert!(kinds.iter().any(|o| matches!(o, Ownership::Clone)));
    }

    #[test]
    fn a_worktree_is_dirty_and_owned_by_another_session() {
        // Named in #13 because it is the row most likely to be rendered wrong:
        // read-only to this session, yet holding uncommitted work.
        let s = invoice_split();
        assert!(
            s.worktrees["billing-service"]
                .iter()
                .any(|r| r.dirty_files > 0 && matches!(r.ownership, Ownership::Other(_))),
            "no worktree is both dirty and owned elsewhere"
        );
    }

    #[test]
    fn a_member_repo_has_no_worktrees() {
        let s = invoice_split();
        assert!(s.repos.iter().any(|r| r.member && r.worktrees == 0));
        assert!(s.worktrees.get("search-index").is_some_and(Vec::is_empty));
    }

    #[test]
    fn prune_covers_every_blocker_and_both_safe_states() {
        // The picker pre-checks only provably safe rows and shows every other
        // with its reason, so each reason needs a row to render.
        let s = invoice_split();
        let blockers: Vec<_> = s.prune.iter().flat_map(|c| c.blockers.iter()).collect();
        assert!(blockers.iter().any(|b| matches!(b, PruneBlocker::Unmerged)));
        assert!(
            blockers
                .iter()
                .any(|b| matches!(b, PruneBlocker::Dirty { .. }))
        );
        assert!(
            blockers
                .iter()
                .any(|b| matches!(b, PruneBlocker::Unpushed { .. }))
        );
        assert!(
            blockers
                .iter()
                .any(|b| matches!(b, PruneBlocker::Owned { .. }))
        );

        let safe: Vec<_> = s.prune.iter().filter(|c| c.blockers.is_empty()).collect();
        assert!(safe.iter().any(|c| c.state == PruneState::Merged));
        assert!(safe.iter().any(|c| c.state == PruneState::UpstreamGone));
    }

    #[test]
    fn sessions_cover_all_three_states() {
        let s = invoice_split();
        let states: Vec<_> = s.sessions.iter().map(|x| x.state).collect();
        assert!(states.contains(&SessionState::Attached));
        assert!(states.contains(&SessionState::Detached));
        assert!(states.contains(&SessionState::Closed));
    }

    #[test]
    fn a_scratch_terminal_exists_for_reattach_to_find() {
        // It belongs to no worktree, so nothing else can discover it.
        let s = invoice_split();
        assert!(
            s.terminals
                .iter()
                .any(|t| matches!(t.target, TerminalTarget::Scratch { .. }))
        );
    }

    #[test]
    fn the_empty_scenario_is_actually_empty() {
        let s = empty();
        assert!(s.repos.is_empty() && s.sessions.is_empty() && s.worktrees.is_empty());
    }

    #[test]
    fn canned_screens_carry_colour_not_just_text() {
        // Plain text here would let the terminal pane pass its tests while
        // rendering monochrome, which is the mismatch the protocol's styled
        // cells exist to prevent.
        let lines = vec![" PASS  ok".to_string(), " FAIL  bad".to_string()];
        let screen = screen_for(&lines, 2, 9);
        assert_eq!(screen.cells.len(), 2);
        assert!(screen.cells[0].iter().any(|c| c.fg == Color::Indexed(2)));
        assert!(screen.cells[1].iter().any(|c| c.attrs.bold));
    }

    #[test]
    fn screens_are_exactly_rows_by_cols() {
        // The protocol documents this as a daemon obligation, so the fake
        // daemon has to honour it or it teaches clients the wrong shape.
        let screen = screen_for(&["short".to_string()], 3, 20);
        assert_eq!(screen.cells.len(), 3);
        assert!(screen.cells.iter().all(|row| row.len() == 20));
    }
}
