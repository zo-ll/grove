//! The Grove TUI.
//!
//! Depends on `grove-domain`, `grove-proto` and `grove-lua` only. It never
//! links a git crate, never spawns a pty, and never touches repositories on
//! disk — everything about repos arrives over the protocol. `scripts/
//! check-boundaries.sh` enforces that; terminal *rendering* crates are expected
//! here, terminal *spawning* is not.
//!
//! See `SPEC.md` §3 and §4. This is issue #14: the shell every screen mounts
//! into. Screens themselves are #18 through #30.

mod events;
mod terminal;

use std::io::Stdout;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use events::{Input, Inputs};
use grove_proto::{Event as DaemonEvent, Handshake, PROTOCOL_VERSION, Request, accept_welcome};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Alignment;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

/// What the shell is currently able to show. Screens replace this in #18-#30;
/// until then it is enough to prove the loop, the redraw policy and the
/// teardown all behave.
enum State {
    /// Connected, waiting for the first data.
    Connected { workspace: PathBuf, note: String },
    /// Could not reach a daemon, or lost it. The TUI stays up to say so: the
    /// user's terminals are still running inside a daemon it can no longer see,
    /// and exiting silently would suggest otherwise.
    Disconnected { workspace: PathBuf, reason: String },
}

fn main() -> ExitCode {
    let workspace = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    match run(workspace) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // The guard has already restored the terminal by here, so this
            // lands on a usable screen.
            eprintln!("grove: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(workspace: PathBuf) -> std::io::Result<()> {
    let inputs = Inputs::new();
    let mut state = connect(&workspace, &inputs);

    let (_guard, mut term) = terminal::Guard::new()?;

    // Redraw only on change. A dashboard of idle terminals must not spin, so
    // nothing here loops on a timer: the loop blocks until a source produces.
    let mut dirty = true;
    loop {
        if dirty {
            term.draw(|f| draw(f, &state))?;
            dirty = false;
        }

        let Some(first) = inputs.next() else {
            // Every source is gone; there is nothing left that could wake us.
            return Ok(());
        };

        // Coalesce whatever else is already queued into this same redraw. A
        // held-down key or a burst of terminal output should cost one frame,
        // not one frame each.
        let mut batch = vec![first];
        batch.extend(inputs.drain());

        for input in batch {
            match handle(input, &mut state) {
                Flow::Continue { redraw } => dirty |= redraw,
                // The reason is carried on the state so main can report it
                // after the guard has restored the terminal.
                Flow::Quit => return finish(&state),
            }
        }
    }
}

/// Report anything the user should know, once the terminal is usable again.
fn finish(state: &State) -> std::io::Result<()> {
    match state {
        State::Disconnected { reason, .. } if reason.starts_with("terminal") => {
            Err(std::io::Error::other(reason.clone()))
        }
        _ => Ok(()),
    }
}

enum Flow {
    Continue { redraw: bool },
    Quit,
}

fn handle(input: Input, state: &mut State) -> Flow {
    match input {
        // `^g q` quits per SPEC §3.2. The prefix itself is #16; until then the
        // bare chord is enough to leave without killing the terminal.
        Input::Terminal(TermEvent::Key(KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::CONTROL,
            ..
        })) => Flow::Quit,

        Input::Terminal(TermEvent::Resize(..)) => Flow::Continue { redraw: true },

        // Focus events arrive as keepalives from the reader and mean nothing to
        // the user; redrawing on them would defeat the redraw-on-change rule.
        Input::Terminal(TermEvent::FocusGained | TermEvent::FocusLost) => {
            Flow::Continue { redraw: false }
        }
        Input::Terminal(_) => Flow::Continue { redraw: false },

        Input::Daemon(ev) => {
            if let State::Connected { note, .. } = state {
                *note = describe(&ev);
            }
            Flow::Continue { redraw: true }
        }

        Input::DaemonGone(reason) => {
            let workspace = workspace_of(state);
            *state = State::Disconnected { workspace, reason };
            Flow::Continue { redraw: true }
        }

        // Without a terminal there is nothing to draw on and no way to hear the
        // user; staying up would be pretending.
        Input::TerminalGone(reason) => {
            let workspace = workspace_of(state);
            *state = State::Disconnected { workspace, reason };
            Flow::Quit
        }
    }
}

fn workspace_of(state: &State) -> PathBuf {
    match state {
        State::Connected { workspace, .. } | State::Disconnected { workspace, .. } => {
            workspace.clone()
        }
    }
}

fn describe(ev: &DaemonEvent) -> String {
    match ev {
        DaemonEvent::Welcome { version } => format!("connected, protocol v{version}"),
        DaemonEvent::Sessions(s) => format!("{} session(s)", s.len()),
        DaemonEvent::Failed { context, message } => format!("{context} failed: {message}"),
        other => format!("{other:?}"),
    }
}

/// Connect, greet, and check the daemon's reply.
///
/// A failure here is not fatal: the TUI comes up and says what went wrong,
/// because "grove exited" and "grove cannot reach its daemon" are very
/// different things to a user whose work is running inside that daemon.
fn connect(workspace: &Path, inputs: &Inputs) -> State {
    let path = socket_path(workspace);

    let stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            return State::Disconnected {
                workspace: workspace.to_path_buf(),
                reason: format!("no daemon at {}: {e}", path.display()),
            };
        }
    };
    let Ok(read_half) = stream.try_clone() else {
        return State::Disconnected {
            workspace: workspace.to_path_buf(),
            reason: "could not split the daemon connection".into(),
        };
    };

    let mut write_half = stream;
    if let Err(e) = grove_proto::write_frame(
        &mut write_half,
        &Request::Hello {
            version: PROTOCOL_VERSION,
        },
    ) {
        return State::Disconnected {
            workspace: workspace.to_path_buf(),
            reason: format!("could not greet the daemon: {e}"),
        };
    }

    // Check the daemon in turn rather than assuming it agreed — the symmetric
    // half of the handshake, which `grove-proto` provides precisely so it is
    // the easy thing to do.
    let mut reader = read_half;
    match grove_proto::read_frame::<_, DaemonEvent>(&mut reader) {
        Ok(ev) => match accept_welcome(&ev) {
            Handshake::Agreed => {
                events::spawn_daemon_reader(reader, inputs.sender());
                State::Connected {
                    workspace: workspace.to_path_buf(),
                    note: "connected".into(),
                }
            }
            Handshake::Mismatch { daemon, client } => State::Disconnected {
                workspace: workspace.to_path_buf(),
                reason: format!(
                    "protocol mismatch: the daemon speaks v{daemon}, this grove speaks v{client}"
                ),
            },
            Handshake::NotHello => State::Disconnected {
                workspace: workspace.to_path_buf(),
                reason: "the daemon sent something other than a greeting".into(),
            },
        },
        Err(e) => State::Disconnected {
            workspace: workspace.to_path_buf(),
            reason: format!("the daemon did not greet back: {e}"),
        },
    }
}

/// `$XDG_RUNTIME_DIR/grove/<workspace-hash>.sock`, per SPEC §6.
fn socket_path(workspace: &Path) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("grove").join(format!("{}.sock", hash(workspace)))
}

/// FNV-1a over the canonical path. Stable across runs and platforms, which is
/// what matters — two `grove` invocations in one workspace must agree.
fn hash(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canonical.as_os_str().as_encoded_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn draw(f: &mut ratatui::Frame, state: &State) {
    let (title, body) = match state {
        State::Connected { workspace, note } => (
            "grove",
            vec![
                Line::from(vec![
                    Span::raw("workspace  "),
                    Span::styled(
                        workspace.display().to_string(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(format!("daemon     {note}")),
                Line::from(""),
                Line::from("screens land in #18-#30"),
                Line::from("ctrl-q to quit"),
            ],
        ),
        State::Disconnected { workspace, reason } => (
            "grove — disconnected",
            vec![
                Line::from(vec![
                    Span::raw("workspace  "),
                    Span::styled(
                        workspace.display().to_string(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::from(reason.clone()),
                Line::from(""),
                // Said plainly, because the distinction matters: the daemon
                // holds the user's running work.
                Line::from("terminals in a running daemon are unaffected by this."),
                Line::from("ctrl-q to quit"),
            ],
        ),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_alignment(Alignment::Left);

    f.render_widget(
        Paragraph::new(body).block(block).wrap(Wrap { trim: false }),
        f.area(),
    );
}

#[allow(dead_code)]
type Backend = Terminal<CrosstermBackend<Stdout>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn connected() -> State {
        State::Connected {
            workspace: PathBuf::from("/w"),
            note: "connected".into(),
        }
    }

    #[test]
    fn keepalives_do_not_cause_a_redraw() {
        // The terminal reader emits a focus event to check whether anyone is
        // still listening. Redrawing on those would turn the idle dashboard
        // into a 4 Hz spinner, which is exactly what the issue forbids.
        let mut s = connected();
        match handle(Input::Terminal(TermEvent::FocusGained), &mut s) {
            Flow::Continue { redraw } => assert!(!redraw),
            Flow::Quit => panic!("keepalive must not quit"),
        }
    }

    #[test]
    fn a_resize_causes_a_redraw() {
        let mut s = connected();
        match handle(Input::Terminal(TermEvent::Resize(80, 24)), &mut s) {
            Flow::Continue { redraw } => assert!(redraw),
            Flow::Quit => panic!("resize must not quit"),
        }
    }

    #[test]
    fn losing_the_daemon_keeps_the_tui_up_and_says_so() {
        // Exiting here would imply the user's work went with it. It did not.
        let mut s = connected();
        match handle(Input::DaemonGone("socket closed".into()), &mut s) {
            Flow::Continue { redraw } => assert!(redraw),
            Flow::Quit => panic!("losing the daemon must not quit the TUI"),
        }
        match &s {
            State::Disconnected { reason, .. } => assert!(reason.contains("socket closed")),
            State::Connected { .. } => panic!("state must reflect the loss"),
        }
    }

    #[test]
    fn losing_the_terminal_does_quit() {
        // Nothing to draw on and no way to hear the user; staying up pretends.
        let mut s = connected();
        assert!(matches!(
            handle(Input::TerminalGone("eof".into()), &mut s),
            Flow::Quit
        ));
    }

    #[test]
    fn ctrl_q_quits() {
        let mut s = connected();
        let ev = TermEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(matches!(handle(Input::Terminal(ev), &mut s), Flow::Quit));
    }

    #[test]
    fn the_socket_path_is_stable_and_workspace_specific() {
        let a = PathBuf::from("/tmp");
        let b = PathBuf::from("/usr");
        assert_eq!(
            socket_path(&a),
            socket_path(&a),
            "must be stable across calls"
        );
        assert_ne!(
            socket_path(&a),
            socket_path(&b),
            "must differ per workspace"
        );
        assert!(socket_path(&a).to_string_lossy().ends_with(".sock"));
    }
}
