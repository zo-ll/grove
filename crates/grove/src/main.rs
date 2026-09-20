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

mod dash;
mod events;
mod keymap;
mod repos;
mod statusbar;
mod terminal;
mod theme;

use std::io::Stdout;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use dash::Panes;
use events::{Input, Inputs};
use grove_lua::{TuiConfig, TuiRuntime};
use grove_proto::{
    Event as DaemonEvent, Handshake, PROTOCOL_VERSION, Request, accept_welcome, socket_path,
};
use keymap::{Action, Focus, Routed, Router, Screen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::Event as TermEvent;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use repos::Repos;
use theme::{Depth, Role, Theme};

/// What the shell is currently able to show. Screens replace this in #18-#30;
/// until then it is enough to prove the loop, the redraw policy and the
/// teardown all behave.
/// Where grove is, what has focus, and what it looks like.
struct Ui {
    screen: Screen,
    focus: Focus,
    router: Router,
    /// Which dash panes are on screen. Focus is kept consistent with this:
    /// see `Panes::refocus`.
    panes: Panes,
    /// The session's member repos and the cursor in them, which is what the
    /// WORKTREES pane will follow in #20.
    repos: Repos,
    /// The terminal's current width, because whether a pane is on screen
    /// depends on it: below a certain width the dash drops panes it cannot
    /// draw, and focus must not cycle onto one of those.
    width: u16,
    theme: Theme,
    /// What the config could not give us, shown once rather than swallowed.
    /// SPEC §9: a broken config reports and keeps running.
    config_note: Option<String>,
}

impl Ui {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_config(&TuiConfig::default(), None, Depth::detect())
    }

    fn with_config(
        config: &TuiConfig,
        error: Option<grove_lua::ConfigError>,
        depth: Depth,
    ) -> Self {
        let (theme, bad) = Theme::resolve(config, depth);
        // One line, not one per problem: the bar has a few columns, and a user
        // who mistyped two colours needs to know that, not to read a list.
        let mut notes: Vec<String> = Vec::new();
        if let Some(error) = error {
            notes.push(error.message().to_owned());
        }
        notes.extend(bad.iter().map(ToString::to_string));
        Self {
            screen: Screen::Dash,
            focus: Focus::Worktrees,
            router: Router::new(),
            panes: Panes::default(),
            repos: Repos::default(),
            width: 80,
            theme,
            config_note: (!notes.is_empty()).then(|| notes.join("; ")),
        }
    }
}

/// `$XDG_CONFIG_HOME/grove/config.lua`, falling back to `~/.config`, per
/// SPEC §9. A missing file is not an error — it means defaults.
fn config_path() -> Option<PathBuf> {
    config_path_from(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// The lookup itself, taking its inputs rather than reading them, so it can be
/// tested without setting process-wide environment variables — which race,
/// since tests share a process.
///
/// `None` when neither variable is set, rather than a relative `.config`: that
/// path would be resolved against the working directory, so a grove started in
/// a directory someone else populated would execute their Lua. Reading nothing
/// is also the truer reading of §9 — a config grove cannot find means defaults.
fn config_path_from(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    Some(
        xdg.or_else(|| home.map(|home| home.join(".config")))?
            .join("grove")
            .join("config.lua"),
    )
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
    // Before the terminal is taken, so a config error can still be printed if
    // anything below fails outright.
    let loaded = config_path().map(TuiRuntime::load);
    let mut ui = match loaded {
        Some(loaded) => Ui::with_config(loaded.runtime.config(), loaded.error, Depth::detect()),
        // Nowhere to look, so defaults — see `config_path_from`.
        None => Ui::with_config(&TuiConfig::default(), None, Depth::detect()),
    };

    let inputs = Inputs::new();
    let mut state = connect(&workspace, &inputs);

    let (_guard, mut term) = terminal::Guard::new()?;
    // Whether a pane fits depends on the width, so start from the real one
    // rather than assuming the default until the first resize.
    if let Ok(size) = term.size() {
        ui.width = size.width;
        ui.focus = ui.panes.drawable(size.width).refocus(ui.focus);
    }

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
                // Cycling consults visibility rather than the enum's own
                // order: focus on a hidden pane means the arrows drive a list
                // that is not on screen.
                Routed::Act(Action::CycleFocus) => {
                    ui.focus = ui.panes.drawable(ui.width).next_visible(ui.focus);
                    Flow::Continue { redraw: true }
                }
                Routed::Act(Action::CycleFocusBack) => {
                    ui.focus = ui.panes.drawable(ui.width).previous_visible(ui.focus);
                    Flow::Continue { redraw: true }
                }
                // The arrows drive whichever list has focus — the point of
                // focus being functional rather than decorative.
                Routed::Act(Action::MoveDown) if ui.focus == Focus::Repos => Flow::Continue {
                    redraw: ui.repos.move_down(),
                },
                Routed::Act(Action::MoveUp) if ui.focus == Focus::Repos => Flow::Continue {
                    redraw: ui.repos.move_up(),
                },
                Routed::Act(Action::TogglePane(index)) => {
                    let Some(pane) = Panes::addressed(index) else {
                        return Flow::Continue { redraw: false };
                    };
                    // A refused toggle — the last visible pane — changes
                    // nothing, so it must not cost a frame either.
                    let changed = ui.panes.toggle(pane);
                    if changed {
                        ui.focus = ui.panes.drawable(ui.width).refocus(ui.focus);
                    }
                    Flow::Continue { redraw: changed }
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

        Input::Terminal(TermEvent::Resize(width, _)) => {
            ui.width = width;
            // A resize can take a pane off screen, so focus is rechecked here
            // rather than only when the user presses something.
            ui.focus = ui.panes.drawable(width).refocus(ui.focus);
            Flow::Continue { redraw: true }
        }

        // Focus events arrive as keepalives from the reader and mean nothing to
        // the user; redrawing on them would defeat the redraw-on-change rule.
        Input::Terminal(TermEvent::FocusGained | TermEvent::FocusLost) => {
            Flow::Continue { redraw: false }
        }
        Input::Terminal(_) => Flow::Continue { redraw: false },

        // The REPOS pane's data. Kept ahead of the catch-all below because
        // that one only narrates the event on the status line, which for a
        // list of repos would say a lot and show nothing.
        Input::Daemon(DaemonEvent::Repos(rows)) => {
            ui.repos.set(rows);
            Flow::Continue { redraw: true }
        }

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

    // The dash is a frame of panes rather than a paragraph, so it takes the
    // body whole. Everything the shell says about its own state — no daemon,
    // no connection yet — still goes through the block below, because those
    // are not screens and #22's empty state is about a workspace with nothing
    // in it, not about a daemon grove cannot reach.
    if matches!(state, State::Connected { .. }) && ui.screen == Screen::Dash {
        let inner = dash::render(f.buffer_mut(), body_area, ui.panes, ui.focus, &ui.theme);
        if let Some(area) = inner.repos {
            ui.repos
                .render(f.buffer_mut(), area, &ui.theme, ui.focus == Focus::Repos);
        }
        // #20 and #21 fill these; until then each says so in its own pane
        // rather than the dash claiming to be finished.
        for (pane, area) in [
            (Focus::Worktrees, inner.worktrees),
            (Focus::Terminal, inner.terminal),
        ] {
            if let Some(area) = area {
                f.render_widget(
                    Paragraph::new(dash::placeholder_line(pane, &ui.theme)),
                    area,
                );
            }
        }
        status_bar(f, bar_area, ui);
        return;
    }

    let (title, body) = match state {
        State::Connected { workspace, note } => (
            "grove",
            vec![
                Line::from(vec![
                    Span::styled("workspace  ", ui.theme.style(Role::Muted)),
                    Span::styled(
                        workspace.display().to_string(),
                        ui.theme.style(Role::Clean).add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("daemon     ", ui.theme.style(Role::Muted)),
                    Span::styled(note.clone(), ui.theme.style(Role::Clean)),
                ]),
                Line::from(""),
                Line::styled("screens land in #18-#30", ui.theme.style(Role::Muted)),
                Line::styled("ctrl-q to quit", ui.theme.style(Role::Muted)),
            ],
        ),
        State::Disconnected { workspace, reason } => (
            "grove — disconnected",
            vec![
                Line::from(vec![
                    Span::styled("workspace  ", ui.theme.style(Role::Muted)),
                    Span::styled(
                        workspace.display().to_string(),
                        ui.theme.style(Role::Accent).add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::styled(reason.clone(), ui.theme.style(Role::Error)),
                Line::from(""),
                // Said plainly, because the distinction matters: the daemon
                // holds the user's running work.
                Line::styled(
                    "terminals in a running daemon are unaffected by this.",
                    ui.theme.style(Role::Muted),
                ),
                Line::styled("ctrl-q to quit", ui.theme.style(Role::Muted)),
            ],
        ),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        // Corners are a theme setting, so the border type cannot be a literal
        // here — this is the one place the setting becomes visible.
        .border_type(ui.theme.border())
        .border_style(ui.theme.style(Role::Muted))
        .title(Span::styled(title, ui.theme.style(Role::Accent)))
        .title_alignment(Alignment::Left)
        .padding(Padding::horizontal(ui.theme.padding()));

    f.render_widget(
        Paragraph::new(body).block(block).wrap(Wrap { trim: false }),
        body_area,
    );

    status_bar(f, bar_area, ui);
}

/// The one row along the bottom, drawn the same way whatever is above it.
fn status_bar(f: &mut ratatui::Frame, area: Rect, ui: &Ui) {
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
            area.width,
            &ui.theme,
            ui.config_note.as_deref(),
        )),
        area,
    );
}

#[allow(dead_code)]
type Backend = Terminal<CrosstermBackend<Stdout>>;

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn connected() -> State {
        State::Connected {
            workspace: PathBuf::from("/w"),
            note: "connected".into(),
        }
    }

    #[test]
    fn a_config_that_throws_leaves_grove_usable_and_says_so() {
        // SPEC §9: a broken config must never brick grove. It falls back to
        // defaults entirely, reports, and keeps running — so the assertion is
        // both halves, the working theme and the message.
        let loaded = grove_lua::TuiRuntime::load_source("error('boom')", "broken");
        let ui = Ui::with_config(loaded.runtime.config(), loaded.error, Depth::True);
        assert_eq!(
            ui.theme,
            Theme::resolve(&TuiConfig::default(), Depth::True).0,
            "a thrown config falls back to defaults entirely"
        );
        let note = ui.config_note.expect("the failure must be reported");
        assert!(
            note.contains("boom"),
            "the note must carry the cause: {note}"
        );
    }

    #[test]
    fn a_config_that_loads_but_misspells_a_colour_reports_that_instead() {
        // A different failure from a throw: the file evaluated, so there is no
        // ConfigError — only a value this crate could not read. It must still
        // reach the user, or the setting silently does nothing.
        let loaded = grove_lua::TuiRuntime::load_source(
            "local grove = require('grove')\n\
             grove.setup({ theme = { accent = 'peach' } })",
            "typo",
        );
        assert!(loaded.error.is_none(), "the file itself is valid Lua");
        let ui = Ui::with_config(loaded.runtime.config(), loaded.error, Depth::True);
        let note = ui.config_note.expect("a bad colour must be reported");
        assert!(
            note.contains("accent"),
            "the note must name the setting: {note}"
        );
    }

    #[test]
    fn the_config_lives_where_the_spec_says() {
        assert_eq!(
            config_path_from(Some(PathBuf::from("/x")), Some(PathBuf::from("/home/u"))),
            Some(PathBuf::from("/x/grove/config.lua")),
            "XDG_CONFIG_HOME wins when it is set"
        );
        assert_eq!(
            config_path_from(None, Some(PathBuf::from("/home/u"))),
            Some(PathBuf::from("/home/u/.config/grove/config.lua"))
        );
        // Not a relative `.config`: that resolves against the working
        // directory, so a grove started in a directory someone else wrote
        // would run their Lua.
        assert_eq!(config_path_from(None, None), None);
    }

    fn key(code: KeyCode) -> Input {
        Input::Terminal(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn prefix() -> Input {
        Input::Terminal(TermEvent::Key(KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL,
        )))
    }

    #[test]
    fn hiding_a_pane_from_the_keyboard_moves_focus_off_it() {
        // End to end through the router, because the wiring is where this can
        // go wrong: the layout already refuses to strand focus, and a `handle`
        // that forgets to call `refocus` would leave the arrows driving a list
        // that is no longer drawn.
        let mut s = connected();
        let mut ui = Ui::new();
        ui.focus = Focus::Repos;

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('1')), &mut s, &mut ui);

        assert!(!ui.panes.visible(Focus::Repos), "^g 1 hides REPOS");
        assert_ne!(ui.focus, Focus::Repos, "focus must leave a hidden pane");
        assert!(ui.panes.visible(ui.focus));
    }

    #[test]
    fn refusing_to_hide_the_last_pane_costs_no_frame() {
        // A redraw that changes nothing is a frame spent to paint the same
        // pixels, and the loop's whole redraw policy is change-driven.
        let mut s = connected();
        let mut ui = Ui::new();
        for pane in ['1', '2'] {
            handle(prefix(), &mut s, &mut ui);
            handle(key(KeyCode::Char(pane)), &mut s, &mut ui);
        }
        assert_eq!(ui.panes.count(), 1);

        handle(prefix(), &mut s, &mut ui);
        match handle(key(KeyCode::Char('3')), &mut s, &mut ui) {
            Flow::Continue { redraw } => assert!(!redraw, "a refusal must not redraw"),
            Flow::Quit => panic!("toggling a pane must not quit"),
        }
        assert_eq!(ui.panes.count(), 1, "the last pane stays");
    }

    #[test]
    fn cycling_focus_skips_hidden_panes() {
        let mut s = connected();
        let mut ui = Ui::new();
        ui.focus = Focus::Repos;

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('2')), &mut s, &mut ui);
        assert!(!ui.panes.visible(Focus::Worktrees));

        ui.focus = Focus::Repos;
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Tab), &mut s, &mut ui);
        assert_eq!(
            ui.focus,
            Focus::Terminal,
            "^g tab must pass over the hidden WORKTREES pane"
        );
    }

    #[test]
    fn focus_never_cycles_onto_a_pane_too_narrow_to_draw() {
        // The review's finding, wired end to end: at 40 columns the dash drops
        // the terminal pane, so `^g tab` must pass over it exactly as it
        // passes over one the user hid.
        let mut s = connected();
        let mut ui = Ui::new();
        handle(Input::Terminal(TermEvent::Resize(40, 24)), &mut s, &mut ui);
        ui.focus = Focus::Worktrees;

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Tab), &mut s, &mut ui);
        assert_eq!(
            ui.focus,
            Focus::Repos,
            "the terminal pane does not fit at 40 columns, so focus must skip it"
        );
    }

    #[test]
    fn shrinking_the_terminal_moves_focus_off_a_pane_that_no_longer_fits() {
        // Focus can be left behind by a resize as easily as by a toggle, and
        // the resize path is the one nobody presses a key for.
        let mut s = connected();
        let mut ui = Ui::new();
        handle(Input::Terminal(TermEvent::Resize(200, 40)), &mut s, &mut ui);
        ui.focus = Focus::Terminal;

        handle(Input::Terminal(TermEvent::Resize(40, 24)), &mut s, &mut ui);
        assert_ne!(ui.focus, Focus::Terminal, "that pane is no longer drawn");
        assert!(
            ui.panes.drawable(ui.width).visible(ui.focus),
            "focus must land somewhere that fits"
        );
    }

    fn repo_row(name: &str, worktrees: u32) -> grove_proto::RepoRow {
        grove_proto::RepoRow {
            repo: grove_domain::RepoId(name.into()),
            name: name.into(),
            base_branch: "origin/main".into(),
            base_from_origin_head: true,
            worktrees,
            dirty: false,
            member: true,
        }
    }

    #[test]
    fn the_repos_event_fills_the_pane() {
        // The pane's data arrives over the protocol like everything else, and
        // before this arm existed it was narrated onto the status line — a
        // sentence about repos instead of a list of them.
        let mut s = connected();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("a", 1), repo_row("b", 0)])),
            &mut s,
            &mut ui,
        );
        assert_eq!(ui.repos.rows().len(), 2);
        assert_eq!(ui.repos.selected().map(|r| r.name.as_str()), Some("a"));
    }

    #[test]
    fn arrows_drive_the_focused_list_and_nothing_else() {
        // Focus is functional: the same key has to move the list that has it
        // and leave the others alone. The source mock drew a focus ring and
        // always moved the worktree list, which is the bug `Focus` exists to
        // prevent.
        let mut s = connected();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("a", 1), repo_row("b", 1)])),
            &mut s,
            &mut ui,
        );

        ui.focus = Focus::Repos;
        handle(key(KeyCode::Down), &mut s, &mut ui);
        assert_eq!(ui.repos.selected().map(|r| r.name.as_str()), Some("b"));

        ui.focus = Focus::Worktrees;
        handle(key(KeyCode::Up), &mut s, &mut ui);
        assert_eq!(
            ui.repos.selected().map(|r| r.name.as_str()),
            Some("b"),
            "an arrow in another pane must not move this list"
        );
    }

    #[test]
    fn an_arrow_at_the_end_of_the_list_costs_no_frame() {
        let mut s = connected();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("only", 1)])),
            &mut s,
            &mut ui,
        );
        ui.focus = Focus::Repos;
        match handle(key(KeyCode::Down), &mut s, &mut ui) {
            Flow::Continue { redraw } => {
                assert!(!redraw, "a cursor that cannot move must not redraw")
            }
            Flow::Quit => panic!("an arrow must not quit"),
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
