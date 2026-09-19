//! Wire protocol between `grove` (TUI) and `groved` (daemon).
//!
//! Every message crossing the unix socket is defined here, along with framing
//! and version negotiation. The protocol is versioned from the first commit so
//! a stale daemon is detected rather than misparsed.
//!
//! See `SPEC.md` §8. Implements issue #2.
//!
//! # What a newly-attached client receives
//!
//! A pty keeps producing output while nobody is watching it. When a client
//! attaches, three things could happen, and the choice shapes both the daemon
//! and the terminal pane:
//!
//! - **Screen only** — cheapest, but you reattach with no history to scroll.
//! - **Full scrollback replay** — complete, but attaching to a 10,000-line
//!   buffer pays for all of it before anything is drawn.
//! - **Screen, then backfill** — what this protocol does.
//!
//! [`Event::TerminalScreen`] arrives first and carries exactly the visible
//! grid, so the pane paints immediately. History then streams behind it as
//! [`Event::TerminalScrollback`] chunks, each tagged with a sequence number and
//! a `done` flag. Live [`Event::TerminalOutput`] deltas may interleave from the
//! moment the screen is sent — they are never withheld waiting for backfill to
//! finish, because a client that has painted should not go stale while history
//! loads.
//!
//! ## The reassembly contract
//!
//! The interleaving is what makes the paint instant, and it is also the one
//! thing a client can get wrong. Appending everything to a single buffer in
//! arrival order **misorders history**: live deltas can scroll lines off the
//! grid before backfill finishes, and those lines are newer than every backfill
//! chunk.
//!
//! A correct client keeps two buffers and joins them when `done` arrives:
//!
//! - backfill chunks, ordered by `seq`, all strictly **older** than the screen;
//! - lines scrolled off the grid by live deltas, all strictly **newer**.
//!
//! Three obligations make that work, two of them on the daemon:
//!
//! - **The screen and the backfill must be disjoint.** The daemon snapshots
//!   scrollback excluding the viewport it just sent. The protocol cannot
//!   enforce this; a daemon that overlaps them makes the client render lines
//!   twice with no way to detect it.
//! - **Chunks arrive oldest-first**: `seq` 0 is the oldest chunk, and the final
//!   chunk carries `done`. A chunk therefore renders as a contiguous block,
//!   appended in arrival order, with no buffering or prepending.
//! - **Lines within a chunk are oldest-first** too, for the same reason.
//! - **Chunks partition the snapshot**: no line appears in two chunks. Like the
//!   screen/backfill disjointness above, the protocol cannot enforce this — a
//!   daemon that re-reads overlapping ranges at chunk boundaries duplicates
//!   lines with no way for the client to notice.
//! - **An empty backfill is still terminated**: a pty with no history yields one
//!   empty chunk with `seq` 0 and `done` set, never zero chunks. A client
//!   waiting to join at `done` would otherwise wait forever, unable to tell
//!   "finished" from "still coming".
//!
//! # Wire format
//!
//! Frames are a 4-byte big-endian length prefix followed by that many bytes of
//! JSON. JSON is chosen for debuggability over a compact binary codec; terminal
//! output is the hot path and may justify revisiting that, which would be a
//! protocol version bump rather than a compatible change.

use std::io::{self, Read, Write};

use grove_domain::{RepoId, Session, SessionId};
use serde::{Deserialize, Serialize};

/// Incremented for any change an older peer cannot decode: a changed field
/// type, a removed variant, a different wire codec — or a new variant.
///
/// Variants are named rather than positional in this codec, so declaration
/// order is irrelevant and *any* addition breaks an older peer. The failure is
/// clean rather than silent: framing is length-delimited, so an unknown variant
/// is a decode error on one frame and the stream stays aligned.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest frame the reader will accept, to bound memory on a hostile or
/// confused peer. Terminal output is chunked well below this.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Names a worktree without describing it.
///
/// Requests identify; they never assert. A request carrying a whole
/// [`grove_domain::Worktree`] would let a client state a worktree's ahead/behind
/// counts, dirty flag — and, the day that type gains an ownership field, its
/// owner. The daemon is the sole writer of all of it (see `grove-domain`'s
/// `Ownership` docs), so the wire carries only enough to name the thing and the
/// daemon looks up the rest from its own state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorktreeRef {
    pub repo: RepoId,
    pub branch: String,
}

/// Identifies one pty for the lifetime of the daemon. Not stable across
/// daemon restarts — a reattaching client re-reads the session to learn
/// current ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TerminalId(pub u64);

/// How much history a client wants when it attaches to a terminal.
///
/// See the module documentation for the ordering the daemon must honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollbackRequest {
    /// Screen and live output only. No backfill.
    None,
    /// At most this many lines of history — the most recent `n`, delivered
    /// oldest-first like every other chunk.
    Lines(u32),
    /// Everything the daemon retains.
    All,
}

/// What the client asks for when attaching to a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attach {
    pub terminal: TerminalId,
    pub scrollback: ScrollbackRequest,
    /// The pane's size at attach time, so the daemon can resize before
    /// sending the screen and the client never paints a wrongly-sized grid.
    pub rows: u16,
    pub cols: u16,
}

/// Client to daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Must be the first message. The daemon replies [`Event::Welcome`] or
    /// [`Event::VersionMismatch`] and closes.
    Hello {
        version: u32,
    },

    ListSessions,
    /// Open a stored session, replacing whatever is open. The outgoing session
    /// becomes detached, or is dropped if it is an unnamed launch session with
    /// no worktrees.
    OpenSession(SessionId),
    DetachSession(SessionId),
    CloseSession(SessionId),
    /// The only destructive request. Removes every worktree the session owns,
    /// and nothing else.
    EndSession(SessionId),

    AddMember {
        session: SessionId,
        repo: RepoId,
    },
    RemoveMember {
        session: SessionId,
        repo: RepoId,
    },
    /// Create one worktree per named repo, all on `branch`.
    NewWorktrees {
        session: SessionId,
        branch: String,
        repos: Vec<RepoId>,
    },
    AdoptWorktree {
        session: SessionId,
        worktree: WorktreeRef,
    },
    ReleaseWorktree {
        session: SessionId,
        worktree: WorktreeRef,
    },

    SpawnTerminal {
        worktree: WorktreeRef,
    },
    KillTerminal(TerminalId),
    ResizeTerminal {
        terminal: TerminalId,
        rows: u16,
        cols: u16,
    },
    /// Raw stdin for the pty. Not interpreted.
    Input {
        terminal: TerminalId,
        bytes: Vec<u8>,
    },
    AttachTerminal(Attach),
    DetachTerminal(TerminalId),

    /// Fetch member repos of a session. Never happens on a timer.
    ///
    /// This extends SPEC §8's list, which does not name a fetch request. It is
    /// a read, and it belongs to "session list and state" only generously; the
    /// extension is recorded here rather than left implicit.
    Fetch {
        session: SessionId,
        repo: Option<RepoId>,
    },
    /// Manual only, as in tmux-resurrect.
    SaveSnapshot(SessionId),
    RestoreSnapshot(SessionId),
}

/// A colour, as a terminal can express one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Color {
    /// The terminal's own default for this position.
    #[default]
    Default,
    /// One of the 256 indexed colours.
    Indexed(u8),
    /// Truecolor, per SPEC §9.
    Rgb(u8, u8, u8),
}

/// Character attributes carried alongside a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Attrs {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub dim: bool,
    pub strikethrough: bool,
}

/// One cell of the grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    /// The grapheme occupying this cell. Empty for the continuation column of a
    /// double-width character.
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

/// The visible grid, already interpreted by the daemon's vt100 parser.
///
/// Cells carry colour and attributes rather than plain text: SPEC §4.1 renders
/// coloured test output in the terminal pane and §9 promises truecolor. Plain
/// strings here would make the attach-time paint monochrome while live
/// [`Event::TerminalOutput`] deltas — raw pty bytes, SGR sequences and all —
/// rendered in colour, so a reattach would visibly lose styling until the next
/// redraw.
///
/// Note the division of labour, which is easy to get backwards: this type is an
/// attach-time *shortcut* so the pane can paint without replaying history. The
/// client still runs its own vt100 over [`Event::TerminalOutput`] to keep the
/// grid current, which is why SPEC §8 places the parser on the TUI side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    pub rows: u16,
    pub cols: u16,
    /// Row-major, top to bottom, each row exactly `cols` cells.
    ///
    /// That is a daemon obligation the protocol cannot enforce, so a client
    /// indexes defensively rather than trusting `rows`/`cols` to match.
    pub cells: Vec<Vec<Cell>>,
    /// Cursor position as (row, col), or `None` when hidden.
    pub cursor: Option<(u16, u16)>,
}

/// Daemon to client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Welcome {
        version: u32,
    },
    /// Sent instead of `Welcome` when the versions differ; the daemon closes
    /// the connection immediately afterwards. Carries both numbers so the
    /// client can say which side is stale.
    VersionMismatch {
        daemon: u32,
        client: u32,
    },

    Sessions(Vec<Session>),
    /// A session's state changed, for any reason including another client.
    SessionChanged(Session),
    SessionEnded(SessionId),

    /// The visible grid, sent first on attach so the pane can paint at once.
    TerminalScreen {
        terminal: TerminalId,
        screen: Screen,
    },
    /// History, streamed after the screen, strictly older than it.
    ///
    /// `seq` starts at 0 for the oldest chunk and increases; `done` marks the
    /// final one; lines within a chunk are oldest-first; no line appears in two
    /// chunks; and an empty backfill is one empty chunk with `done` set rather
    /// than no chunks at all. A client must keep
    /// these separate from lines that live output scrolls off the grid — see
    /// the module documentation's reassembly contract, because merging them in
    /// arrival order silently misorders history.
    TerminalScrollback {
        terminal: TerminalId,
        seq: u32,
        lines: Vec<String>,
        done: bool,
    },
    /// Live output. May interleave with scrollback chunks.
    TerminalOutput {
        terminal: TerminalId,
        bytes: Vec<u8>,
    },
    TerminalExited {
        terminal: TerminalId,
        status: Option<i32>,
    },

    /// A request failed. `context` names the request that caused it.
    Failed {
        context: String,
        message: String,
    },
}

/// Framing and transport errors. Protocol-level failures are [`Event::Failed`]
/// instead — an error the peer reported is not an error reading the wire.
#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    /// The peer announced a frame larger than [`MAX_FRAME_BYTES`].
    TooLarge(u32),
    /// A value serialized to more bytes than a `u32` length prefix can express.
    /// Distinct from [`Self::TooLarge`], which is a real announced size.
    Unrepresentable(usize),
    /// The frame arrived intact but its contents did not parse.
    Malformed(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::TooLarge(n) => {
                write!(
                    f,
                    "frame of {n} bytes exceeds the {MAX_FRAME_BYTES} byte limit"
                )
            }
            Self::Unrepresentable(n) => {
                write!(
                    f,
                    "value serialized to {n} bytes, which no length prefix can express"
                )
            }
            Self::Malformed(e) => write!(f, "malformed frame: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Write one length-prefixed frame.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> Result<(), FrameError> {
    let body = serde_json::to_vec(value).map_err(|e| FrameError::Malformed(e.to_string()))?;
    let len = u32::try_from(body.len()).map_err(|_| FrameError::Unrepresentable(body.len()))?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    // One write, not two: terminal output is the hot path this module's
    // documentation names, and a torn prefix is the worst way to desynchronise
    // a length-delimited stream.
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

/// Read one length-prefixed frame. Handles partial reads; `read_exact` loops
/// until the whole frame has arrived or the peer goes away.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<T, FrameError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| FrameError::Malformed(e.to_string()))
}

/// Outcome of the opening handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handshake {
    Agreed,
    /// Versions differ. The caller reports which side is stale and stops; it
    /// does not attempt to speak the other version.
    Mismatch {
        daemon: u32,
        client: u32,
    },
    /// The peer sent something before greeting. Distinct from [`Self::Mismatch`]
    /// so the daemon can say "you did not greet me" rather than reporting a
    /// fabricated version 0, which is indistinguishable from a genuinely
    /// ancient peer and misleads whoever reads the error.
    NotHello,
}

/// Client side of the handshake: check the daemon's reply.
///
/// The daemon refuses a mismatched client, but nothing forces a client to
/// check the daemon in turn, so this exists to make the symmetric check the
/// easy thing to do.
pub fn accept_welcome(event: &Event) -> Handshake {
    match event {
        Event::Welcome { version } if *version == PROTOCOL_VERSION => Handshake::Agreed,
        Event::Welcome { version } => Handshake::Mismatch {
            daemon: *version,
            client: PROTOCOL_VERSION,
        },
        Event::VersionMismatch { daemon, client } => Handshake::Mismatch {
            daemon: *daemon,
            client: *client,
        },
        _ => Handshake::NotHello,
    }
}

/// Daemon side of the handshake: read the client's `Hello` and decide.
pub fn accept_hello(request: &Request) -> Handshake {
    match request {
        Request::Hello { version } if *version == PROTOCOL_VERSION => Handshake::Agreed,
        Request::Hello { version } => Handshake::Mismatch {
            daemon: PROTOCOL_VERSION,
            client: *version,
        },
        // Anything before Hello is a protocol violation, and a different one
        // from a version disagreement.
        _ => Handshake::NotHello,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_domain::SessionState;

    #[test]
    fn requests_identify_rather_than_assert() {
        // grove-domain documents that the daemon is the sole writer of
        // `Ownership`: a client may ask for a worktree to be adopted or
        // released, but may not assert who owns one. Nothing in the type system
        // enforces that, so this is the tripwire.
        //
        // To watch it fire, add a variant carrying the offending type and give
        // it an arm in the exhaustive match below — mutating an existing
        // variant's type instead breaks the test's own constructions first, and
        // the assertion never runs.
        let src = include_str!("lib.rs");
        let start = src
            .find("pub enum Request {")
            .expect("Request enum present");
        let end = start + src[start..].find("\n}\n").expect("end of Request enum");

        // Doc comments are prose about these rules and mention the type names,
        // so judge the declarations only.
        let body: String = src[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // A `Worktree` in TYPE position is preceded by `:`, `(`, `<` or `,`.
        // Variant names contain the word too (`AdoptWorktree`, `NewWorktrees`),
        // and those are preceded by an identifier character, so position is
        // what separates a declaration from a name. Checking position rather
        // than a list of spellings also catches `Option<Worktree>`,
        // `Box<Worktree>` and a `Worktree` in any tuple position.
        let mut offenders = Vec::new();
        let mut i = 0;
        while let Some(found) = body[i..].find("Worktree") {
            let at = i + found;
            let prev = body[..at].trim_end().chars().next_back();
            let in_type_position = matches!(prev, Some(':' | '(' | '<' | ','));
            if in_type_position && !body[at + "Worktree".len()..].starts_with("Ref") {
                offenders.push(at);
            }
            i = at + 1;
        }
        assert!(
            offenders.is_empty(),
            "a Request carries a whole Worktree; use WorktreeRef so requests identify rather than assert"
        );

        assert!(
            !body.contains("Ownership"),
            "a Request carries an Ownership; the daemon must remain its sole writer"
        );
    }

    fn round_trip<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let mut buf = Vec::new();
        write_frame(&mut buf, value).expect("write");
        let back: T = read_frame(&mut buf.as_slice()).expect("read");
        assert_eq!(&back, value);
    }

    #[test]
    fn every_request_round_trips() {
        // Exhaustive by construction: the match below fails to compile if a
        // variant is added, so this cannot silently drift out of date the way
        // a hand-kept list does.
        let sid = SessionId("s".into());
        let rid = RepoId("r".into());
        let wt = WorktreeRef {
            repo: rid.clone(),
            branch: "feat/x".into(),
        };
        let tid = TerminalId(1);

        let all = [
            Request::Hello {
                version: PROTOCOL_VERSION,
            },
            Request::ListSessions,
            Request::OpenSession(sid.clone()),
            Request::DetachSession(sid.clone()),
            Request::CloseSession(sid.clone()),
            Request::EndSession(sid.clone()),
            Request::AddMember {
                session: sid.clone(),
                repo: rid.clone(),
            },
            Request::RemoveMember {
                session: sid.clone(),
                repo: rid.clone(),
            },
            Request::NewWorktrees {
                session: sid.clone(),
                branch: "feat/x".into(),
                repos: vec![rid.clone()],
            },
            Request::AdoptWorktree {
                session: sid.clone(),
                worktree: wt.clone(),
            },
            Request::ReleaseWorktree {
                session: sid.clone(),
                worktree: wt.clone(),
            },
            Request::SpawnTerminal { worktree: wt },
            Request::KillTerminal(tid),
            Request::ResizeTerminal {
                terminal: tid,
                rows: 24,
                cols: 80,
            },
            Request::Input {
                terminal: tid,
                bytes: vec![0, 27, 255],
            },
            Request::AttachTerminal(Attach {
                terminal: tid,
                scrollback: ScrollbackRequest::Lines(500),
                rows: 24,
                cols: 80,
            }),
            Request::DetachTerminal(tid),
            Request::Fetch {
                session: sid.clone(),
                repo: Some(rid),
            },
            Request::SaveSnapshot(sid.clone()),
            Request::RestoreSnapshot(sid),
        ];

        for r in &all {
            round_trip(r);
            // Compile-time exhaustiveness: adding a variant breaks this match.
            match r {
                Request::Hello { .. }
                | Request::ListSessions
                | Request::OpenSession(_)
                | Request::DetachSession(_)
                | Request::CloseSession(_)
                | Request::EndSession(_)
                | Request::AddMember { .. }
                | Request::RemoveMember { .. }
                | Request::NewWorktrees { .. }
                | Request::AdoptWorktree { .. }
                | Request::ReleaseWorktree { .. }
                | Request::SpawnTerminal { .. }
                | Request::KillTerminal(_)
                | Request::ResizeTerminal { .. }
                | Request::Input { .. }
                | Request::AttachTerminal(_)
                | Request::DetachTerminal(_)
                | Request::Fetch { .. }
                | Request::SaveSnapshot(_)
                | Request::RestoreSnapshot(_) => {}
            }
        }
    }

    #[test]
    fn every_event_round_trips() {
        let session = Session {
            id: SessionId("s".into()),
            name: "invoice split".into(),
            members: vec![],
            owned: vec![],
            state: SessionState::Attached,
        };
        round_trip(&Event::Welcome {
            version: PROTOCOL_VERSION,
        });
        round_trip(&Event::VersionMismatch {
            daemon: 1,
            client: 2,
        });
        round_trip(&Event::Sessions(vec![session.clone()]));
        round_trip(&Event::SessionChanged(session));
        round_trip(&Event::TerminalScreen {
            terminal: TerminalId(1),
            screen: Screen {
                rows: 1,
                cols: 2,
                cells: vec![vec![
                    Cell {
                        text: "a".into(),
                        fg: Color::Rgb(250, 179, 135),
                        bg: Color::Default,
                        attrs: Attrs {
                            bold: true,
                            ..Attrs::default()
                        },
                    },
                    Cell {
                        text: "b".into(),
                        fg: Color::Indexed(2),
                        bg: Color::Default,
                        attrs: Attrs::default(),
                    },
                ]],
                cursor: Some((0, 1)),
            },
        });
        round_trip(&Event::TerminalScrollback {
            terminal: TerminalId(1),
            seq: 0,
            lines: vec!["old".into()],
            done: true,
        });
        round_trip(&Event::TerminalOutput {
            terminal: TerminalId(1),
            bytes: vec![27, 91, 65],
        });
        round_trip(&Event::TerminalExited {
            terminal: TerminalId(1),
            status: Some(1),
        });
        round_trip(&Event::Failed {
            context: "OpenSession".into(),
            message: "gone".into(),
        });
    }

    #[test]
    fn frames_survive_partial_reads() {
        // A reader that yields one byte at a time is the shape a socket takes
        // under load; read_exact must loop rather than truncate.
        struct Dribble<'a>(&'a [u8]);
        impl Read for Dribble<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() || buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = self.0[0];
                self.0 = &self.0[1..];
                Ok(1)
            }
        }
        let msg = Request::Input {
            terminal: TerminalId(7),
            bytes: vec![1, 2, 3],
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: Request = read_frame(&mut Dribble(&buf)).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn two_frames_in_one_buffer_read_back_in_order() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Request::ListSessions).unwrap();
        write_frame(&mut buf, &Request::KillTerminal(TerminalId(2))).unwrap();
        let mut cursor = buf.as_slice();
        let a: Request = read_frame(&mut cursor).unwrap();
        let b: Request = read_frame(&mut cursor).unwrap();
        assert_eq!(a, Request::ListSessions);
        assert_eq!(b, Request::KillTerminal(TerminalId(2)));
    }

    #[test]
    fn oversized_frame_is_refused_without_allocating() {
        let mut framed = (MAX_FRAME_BYTES + 1).to_be_bytes().to_vec();
        framed.extend_from_slice(b"{}");
        match read_frame::<_, Request>(&mut framed.as_slice()) {
            Err(FrameError::TooLarge(n)) => assert_eq!(n, MAX_FRAME_BYTES + 1),
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn malformed_body_is_reported_not_panicked() {
        let body = b"not json";
        let mut framed = (body.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(body);
        match read_frame::<_, Request>(&mut framed.as_slice()) {
            Err(FrameError::Malformed(_)) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn truncated_frame_is_an_io_error_not_a_hang() {
        let mut framed = 99u32.to_be_bytes().to_vec();
        framed.extend_from_slice(b"short");
        match read_frame::<_, Request>(&mut framed.as_slice()) {
            Err(FrameError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected Io(UnexpectedEof), got {other:?}"),
        }
    }

    #[test]
    fn handshake_agrees_only_on_an_exact_match() {
        assert_eq!(
            accept_hello(&Request::Hello {
                version: PROTOCOL_VERSION
            }),
            Handshake::Agreed
        );
        assert_eq!(
            accept_hello(&Request::Hello {
                version: PROTOCOL_VERSION + 1
            }),
            Handshake::Mismatch {
                daemon: PROTOCOL_VERSION,
                client: PROTOCOL_VERSION + 1
            }
        );
        // A peer that speaks before saying hello is not trusted into the
        // protocol on the assumption it meant well.
        // A peer that speaks before greeting is its own case, not a fabricated
        // "version 0" indistinguishable from a genuinely ancient peer.
        assert_eq!(accept_hello(&Request::ListSessions), Handshake::NotHello);
    }
}
