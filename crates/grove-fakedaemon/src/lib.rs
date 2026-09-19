//! A scripted `grove-proto` server serving deterministic fixtures.
//!
//! This exists so the UI lane can build and test every screen with no real
//! daemon and no git repositories on the machine, in states that are
//! impractical to conjure for real — see [`scenario`].
//!
//! It is the project's test harness, not a throwaway stub. Two properties make
//! it one: every answer is a pure function of the scenario, so a rendered frame
//! is comparable across runs; and it speaks the real protocol over a real
//! socket, so it exercises framing and the handshake rather than bypassing them.
//!
//! Implements issue #13.

pub mod scenario;

use std::io::{self, Read, Write};

use grove_proto::{
    Event, Handshake, PROTOCOL_VERSION, Request, TerminalId, accept_hello, read_frame, write_frame,
};
use scenario::Scenario;

/// Answer one request from a scenario.
///
/// A pure function: same scenario and request, same events, every time. That is
/// what makes snapshot-testing a rendered frame meaningful.
pub fn respond(s: &Scenario, req: &Request) -> Vec<Event> {
    match req {
        Request::Hello { version } => vec![if *version == PROTOCOL_VERSION {
            Event::Welcome {
                version: PROTOCOL_VERSION,
            }
        } else {
            Event::VersionMismatch {
                daemon: PROTOCOL_VERSION,
                client: *version,
            }
        }],

        Request::ListSessions => vec![Event::Sessions(s.sessions.clone())],
        Request::ListRepos | Request::Scan => vec![Event::Repos(s.repos.clone())],
        Request::ListTerminals => vec![Event::Terminals(s.terminals.clone())],

        Request::ListWorktrees(repo) => vec![Event::Worktrees {
            repo: repo.clone(),
            rows: s.worktrees.get(&repo.0).cloned().unwrap_or_default(),
        }],

        Request::ListPruneCandidates => vec![Event::PruneCandidates(s.prune.clone())],

        // Honours the real rule rather than removing whatever it is handed: a
        // worktree a live session owns is refused, everything else the user
        // deliberately ticked goes through, and the outcome is per-row.
        Request::Prune(selection) => {
            let mut removed = Vec::new();
            let mut failed = Vec::new();
            let mut reclaimed = 0;
            for want in selection {
                match s.prune.iter().find(|c| &c.worktree == want) {
                    Some(c) if c.blockers.iter().any(is_owned) => failed.push((
                        want.clone(),
                        "a live session owns this worktree; end that session instead".to_string(),
                    )),
                    Some(c) => {
                        reclaimed += c.size;
                        removed.push(want.clone());
                    }
                    None => failed.push((want.clone(), "not a prune candidate".to_string())),
                }
            }
            vec![Event::Pruned {
                removed,
                failed,
                reclaimed,
            }]
        }

        Request::DiffWorktree { worktree, file } => vec![Event::Diff {
            worktree: worktree.clone(),
            base: "origin/main".into(),
            files: s.diff_files.clone(),
            selected: file
                .clone()
                .or_else(|| s.diff_files.first().map(|f| f.path.clone())),
            hunks: s.diff_hunks.clone(),
            added: s.diff_files.iter().map(|f| f.added).sum(),
            removed: s.diff_files.iter().map(|f| f.removed).sum(),
        }],

        // Answering with the id is the whole point: without it a client cannot
        // attach to what it just created.
        Request::SpawnTerminal(target) => {
            let next = s.terminals.iter().map(|t| t.terminal.0).max().unwrap_or(0) + 1;
            vec![Event::TerminalSpawned {
                target: target.clone(),
                terminal: TerminalId(next),
            }]
        }

        Request::AttachTerminal(a) => {
            let lines = s.output.get(&a.terminal.0).cloned().unwrap_or_default();
            vec![
                Event::TerminalScreen {
                    terminal: a.terminal,
                    screen: scenario::screen_for(&lines, a.rows.min(24), a.cols.min(120)),
                },
                // An empty backfill is one empty chunk with `done`, never zero
                // chunks — a client joining at `done` would otherwise wait
                // forever. The protocol says so; the fake daemon must obey it
                // or it teaches clients to handle a case that cannot happen.
                Event::TerminalScrollback {
                    terminal: a.terminal,
                    seq: 0,
                    lines: Vec::new(),
                    done: true,
                },
            ]
        }

        // Everything a screen can ask for that this harness need not model, but
        // must still answer: silence would look like a hang.
        Request::OpenSession(_)
        | Request::DetachSession(_)
        | Request::CloseSession(_)
        | Request::EndSession(_)
        | Request::SessionNew { .. }
        | Request::SessionRename { .. }
        | Request::AddMember { .. }
        | Request::RemoveMember { .. }
        | Request::NewWorktrees { .. }
        | Request::AdoptWorktree { .. }
        | Request::ReleaseWorktree { .. }
        | Request::Fetch { .. }
        | Request::SaveSnapshot(_)
        | Request::RestoreSnapshot(_) => vec![Event::Sessions(s.sessions.clone())],

        Request::KillTerminal(_)
        | Request::ResizeTerminal { .. }
        | Request::Input { .. }
        | Request::DetachTerminal(_)
        | Request::OpenEditor(_) => Vec::new(),
    }
}

fn is_owned(b: &grove_proto::PruneBlocker) -> bool {
    matches!(b, grove_proto::PruneBlocker::Owned { .. })
}

/// Serve one connection until the peer goes away.
///
/// Refuses to proceed on a version mismatch, exactly as a real daemon must —
/// testing against a harness that is laxer than production teaches the client
/// habits that break later.
pub fn serve<S: Read + Write>(s: &Scenario, mut conn: S) -> io::Result<()> {
    let hello: Request = match read_frame(&mut conn) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    match accept_hello(&hello) {
        Handshake::Agreed => {
            let _ = write_frame(
                &mut conn,
                &Event::Welcome {
                    version: PROTOCOL_VERSION,
                },
            );
        }
        Handshake::Mismatch { daemon, client } => {
            let _ = write_frame(&mut conn, &Event::VersionMismatch { daemon, client });
            return Ok(());
        }
        Handshake::NotHello => {
            let _ = write_frame(
                &mut conn,
                &Event::Failed {
                    context: "handshake".into(),
                    message: "expected Hello".into(),
                },
            );
            return Ok(());
        }
    }

    loop {
        let req: Request = match read_frame(&mut conn) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        for ev in respond(s, &req) {
            if write_frame(&mut conn, &ev).is_err() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_domain::RepoId;
    use grove_proto::{Attach, ScrollbackRequest, TerminalTarget, WorktreeRef};

    fn s() -> Scenario {
        scenario::invoice_split()
    }

    #[test]
    fn answers_are_a_pure_function_of_the_scenario() {
        // Snapshot-testing a rendered frame is only meaningful if the same
        // request yields the same events every time.
        let a = respond(&s(), &Request::ListRepos);
        let b = respond(&s(), &Request::ListRepos);
        assert_eq!(a, b);
    }

    #[test]
    fn every_request_gets_an_answer_or_deliberate_silence() {
        // Silence that is not deliberate looks like a hang to a client, and the
        // screen that hits it is the one nobody tested.
        let sc = s();
        let wt = WorktreeRef {
            repo: RepoId("billing-service".into()),
            branch: "feat/ABC-4471-invoice-split".into(),
        };
        let answered = [
            Request::ListSessions,
            Request::ListRepos,
            Request::Scan,
            Request::ListTerminals,
            Request::ListWorktrees(RepoId("billing-service".into())),
            Request::ListPruneCandidates,
            Request::DiffWorktree {
                worktree: wt.clone(),
                file: None,
            },
            Request::SpawnTerminal(TerminalTarget::Scratch { cwd: None }),
            Request::SessionNew { name: "x".into() },
        ];
        for r in answered {
            assert!(!respond(&sc, &r).is_empty(), "{r:?} got no answer");
        }
        // These are fire-and-forget by design.
        for r in [
            Request::Input {
                terminal: TerminalId(1),
                bytes: vec![b'x'],
            },
            Request::OpenEditor(wt),
        ] {
            assert!(respond(&sc, &r).is_empty(), "{r:?} should be silent");
        }
    }

    #[test]
    fn spawning_returns_an_id_the_client_can_attach_to() {
        let sc = s();
        let evs = respond(
            &sc,
            &Request::SpawnTerminal(TerminalTarget::Scratch { cwd: None }),
        );
        let Some(Event::TerminalSpawned { terminal, .. }) = evs.first() else {
            panic!("spawn must answer with the new id: {evs:?}");
        };
        // Fresh, not colliding with one already live.
        assert!(sc.terminals.iter().all(|t| t.terminal != *terminal));
    }

    #[test]
    fn attaching_sends_a_screen_then_a_terminated_backfill() {
        // The protocol requires an empty backfill to be one empty chunk with
        // `done`, never zero chunks, or a client joining at `done` waits
        // forever. A harness that skips it teaches clients to handle a case
        // that cannot occur.
        let evs = respond(
            &s(),
            &Request::AttachTerminal(Attach {
                terminal: TerminalId(1),
                scrollback: ScrollbackRequest::All,
                rows: 4,
                cols: 40,
            }),
        );
        assert!(matches!(evs[0], Event::TerminalScreen { .. }));
        match &evs[1] {
            Event::TerminalScrollback { done, seq, .. } => {
                assert!(*done);
                assert_eq!(*seq, 0);
            }
            other => panic!("expected a terminated backfill, got {other:?}"),
        }
    }

    #[test]
    fn prune_refuses_a_live_sessions_worktree_and_reports_per_row() {
        // The user may deliberately tick an unsafe row, and the daemon does not
        // overrule that — except for a worktree a live session owns, which
        // ending that session is for.
        let sc = s();
        let owned = sc
            .prune
            .iter()
            .find(|c| c.blockers.iter().any(is_owned))
            .unwrap()
            .worktree
            .clone();
        let dirty = sc
            .prune
            .iter()
            .find(|c| !c.blockers.is_empty() && !c.blockers.iter().any(is_owned))
            .unwrap()
            .worktree
            .clone();

        let evs = respond(&sc, &Request::Prune(vec![owned.clone(), dirty.clone()]));
        match &evs[0] {
            Event::Pruned {
                removed, failed, ..
            } => {
                assert!(
                    removed.contains(&dirty),
                    "a deliberately ticked dirty row must go"
                );
                assert!(
                    failed.iter().any(|(w, _)| w == &owned),
                    "a live session's worktree must be refused"
                );
            }
            other => panic!("expected Pruned, got {other:?}"),
        }
    }

    #[test]
    fn the_handshake_is_enforced_not_waved_through() {
        // A harness laxer than production teaches the client habits that break
        // against a real daemon.
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &Request::Hello {
                version: PROTOCOL_VERSION + 1,
            },
        )
        .unwrap();
        let mut conn = io::Cursor::new(buf);
        let mut out = Vec::new();
        {
            struct Duplex<'a>(&'a mut io::Cursor<Vec<u8>>, &'a mut Vec<u8>);
            impl Read for Duplex<'_> {
                fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                    self.0.read(b)
                }
            }
            impl Write for Duplex<'_> {
                fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                    self.1.write(b)
                }
                fn flush(&mut self) -> io::Result<()> {
                    self.1.flush()
                }
            }
            serve(&s(), Duplex(&mut conn, &mut out)).unwrap();
        }
        let ev: Event = read_frame(&mut out.as_slice()).unwrap();
        assert!(matches!(ev, Event::VersionMismatch { .. }));
    }

    #[test]
    fn an_unknown_repo_yields_no_rows_rather_than_an_error() {
        // The dash asks for whatever repo the cursor is on; a repo with no
        // worktrees is a normal state, not a failure.
        let evs = respond(&s(), &Request::ListWorktrees(RepoId("nope".into())));
        match &evs[0] {
            Event::Worktrees { rows, .. } => assert!(rows.is_empty()),
            other => panic!("expected Worktrees, got {other:?}"),
        }
    }
}
