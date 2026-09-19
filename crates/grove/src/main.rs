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
mod keymap;
mod statusbar;
mod terminal;

use std::io::Stdout;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use events::{Input, Inputs};
use grove_proto::{
    Event as DaemonEvent, Handshake, PROTOCOL_VERSION, Request, accept_welcome, socket_path,
};
use keymap::{Action, Focus, Routed, Router, Screen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::Event as TermEvent;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

/// What the shell is currently able to show. Screens replace this in #18-#30;
/// until then it is enough to prove the loop, the redraw policy and the
/// teardown all behave.
/// Where grove is and what has focus, so routing can depend on both.
struct Ui {
    screen: Screen,
    focus: Focus,
    router: Router,
}

impl Ui {
    fn new() -> Self {
        Self {
            screen: Screen::Dash,
            focus: Focus::Worktrees,
            router: Router::new(),
        }
    }
}

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
    let mut ui = Ui::new();

    let (_guard, mut term) = terminal::Guard::new()?;

    // Redraw only on change. A dashboard of idle terminals must not spin, so
    // nothing here loops on a timer: the loop blocks until a source produces.
    let mut dirty = true;
    loop {
        if dirty {
            term.draw(|f| draw(f, &state, &ui))?;
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
            match handle(input, &mut state, &mut ui) {
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

fn handle(input: Input, state: &mut State, ui: &mut Ui) -> Flow {
    match input {
        Input::Terminal(TermEvent::Key(key)) => {
            match ui.router.route(ui.screen, ui.focus, key) {
                Routed::Act(Action::Quit) => Flow::Quit,
                Routed::Act(Action::CycleFocus) => {
                    ui.focus = ui.focus.next();
                    Flow::Continue { redraw: true }
                }
                Routed::Act(Action::CycleFocusBack) => {
                    ui.focus = ui.focus.previous();
                    Flow::Continue { redraw: true }
                }
                // Screens are #18-#30. Until they exist an action is
                // acknowledged with a redraw rather than silently dropped, so
                // the routing is visibly working.
                Routed::Act(_) => Flow::Continue { redraw: true },
                // Forwarding to the daemon is #21; the route is correct now.
                Routed::ToPty(_) => Flow::Continue { redraw: false },
                // Half a chord: the status bar shows it, so redraw.
                Routed::PrefixPending => Flow::Continue { redraw: true },
                Routed::Unbound => Flow::Continue { redraw: true },
                // Consumed by the palette in #23; routed correctly now.
                Routed::Text(_) => Flow::Continue { redraw: false },
                Routed::Ignored => Flow::Continue { redraw: false },
            }
        }

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

fn draw(f: &mut ratatui::Frame, state: &State, ui: &Ui) {
    // One row reserved at the bottom for the status bar, per SPEC §4.1.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());
    let (body_area, bar_area) = (chunks[0], chunks[1]);
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
        body_area,
    );

    let context = if ui.router.prefix_pending() {
        "^g …".to_string()
    } else {
        String::new()
    };
    f.render_widget(
        Paragraph::new(statusbar::render(
            ui.screen,
            ui.focus,
            &context,
            "no session",
            bar_area.width,
        )),
        bar_area,
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
        match handle(
            Input::Terminal(TermEvent::FocusGained),
            &mut s,
            &mut Ui::new(),
        ) {
            Flow::Continue { redraw } => assert!(!redraw),
            Flow::Quit => panic!("keepalive must not quit"),
        }
    }

    #[test]
    fn a_resize_causes_a_redraw() {
        let mut s = connected();
        match handle(
            Input::Terminal(TermEvent::Resize(80, 24)),
            &mut s,
            &mut Ui::new(),
        ) {
            Flow::Continue { redraw } => assert!(redraw),
            Flow::Quit => panic!("resize must not quit"),
        }
    }

    #[test]
    fn losing_the_daemon_keeps_the_tui_up_and_says_so() {
        // Exiting here would imply the user's work went with it. It did not.
        let mut s = connected();
        match handle(
            Input::DaemonGone("socket closed".into()),
            &mut s,
            &mut Ui::new(),
        ) {
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
            handle(Input::TerminalGone("eof".into()), &mut s, &mut Ui::new()),
            Flow::Quit
        ));
    }

    #[test]
    fn quitting_goes_through_the_prefix_like_everything_else() {
        // `^g q` per SPEC §3.2 — and a bare `q` must NOT quit, or a `q` typed
        // into a focused shell would kill grove out from under it.
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = connected();
        let mut ui = Ui::new();
        ui.focus = Focus::Terminal;

        let bare = TermEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(
            !matches!(handle(Input::Terminal(bare), &mut s, &mut ui), Flow::Quit),
            "a bare q belongs to the focused pty"
        );

        let prefix = TermEvent::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        handle(Input::Terminal(prefix), &mut s, &mut ui);
        let q = TermEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(matches!(
            handle(Input::Terminal(q), &mut s, &mut ui),
            Flow::Quit
        ));
    }

    #[test]
    fn tab_moves_focus_and_that_changes_routing() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = connected();
        let mut ui = Ui::new();
        assert_eq!(ui.focus, Focus::Worktrees);

        let prefix = TermEvent::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        handle(Input::Terminal(prefix), &mut s, &mut ui);
        let tab = TermEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        handle(Input::Terminal(tab), &mut s, &mut ui);
        assert_eq!(ui.focus, Focus::Terminal, "^g tab must move focus");
    }

    #[test]
    fn the_tui_looks_for_the_socket_the_protocol_names() {
        // Both halves have to name the same file or the TUI connects to a
        // socket nobody is listening on, and says "no daemon" while one is
        // running. There is one implementation now, in `grove-proto`; this
        // asserts the TUI is reaching it rather than a local lookalike.
        let workspace = PathBuf::from("/tmp");
        assert_eq!(
            socket_path(&workspace),
            grove_proto::socket_path(&workspace)
        );
        assert!(socket_path(&workspace).to_string_lossy().ends_with(".sock"));
    }
}
