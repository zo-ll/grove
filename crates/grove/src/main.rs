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

mod clipboard;
mod columns;
mod dash;
mod diff;
mod empty;
mod endsession;
mod events;
mod help;
mod keymap;
mod mouse;
mod overlay;
mod palette;
mod prune;
mod repos;
mod select;
mod selection;
mod sessions;
mod statusbar;
mod terminal;
mod terminals;
mod text;
mod theme;
mod userkeys;
mod worktrees;

use std::ffi::OsString;
use std::io::Stdout;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use dash::Panes;
use diff::Diff;
use endsession::EndSession;
use events::{Input, Inputs};
use grove_lua::{TuiConfig, TuiRuntime};
use grove_proto::{
    Event as DaemonEvent, Handshake, PROTOCOL_VERSION, Request, accept_welcome, socket_path,
};
use help::Help;
use keymap::{Action, Focus, Routed, Router, Screen};
use palette::{Palette, Takes};
use prune::Prune;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::Event as TermEvent;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use repos::Repos;
use sessions::Sessions;
use terminals::Terminals;
use theme::{Depth, Ink, Role, Theme};
use userkeys::UserKeys;
use worktrees::{Intent, Worktrees};

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
    /// The session's member repos and the cursor in them. The WORKTREES pane
    /// follows this selection.
    repos: Repos,
    /// Every worktree of the selected repo, whoever owns it.
    worktrees: Worktrees,
    /// The grids of every pty this pane has shown, and which one is on screen.
    terminals: Terminals,
    /// grove's command line, while it is open.
    palette: Palette,
    /// The prune picker's rows, while it is open.
    prune: Prune,
    /// Stored sessions, for the picker.
    sessions: Sessions,
    /// The user's own worktree columns, computed when the rows change.
    columns: columns::Columns,
    /// The user's own keymaps from `config.lua`, and the runtime they live in.
    keys: UserKeys,
    lua: Option<grove_lua::TuiRuntime>,
    /// The diff screen, while it is open.
    diff: Diff,
    /// The help overlay's scroll, while it is open, and the screen it is
    /// explaining — kept so closing it returns you where you were.
    help: Help,
    helping: Option<Screen>,
    /// The scratch shell's pty, once one exists. Kept so `^g i` re-attaches
    /// rather than spawning a second shell every time it is pressed.
    scratch: Option<grove_proto::TerminalId>,
    /// The end-session confirm, while it is open.
    ending: EndSession,
    /// A `new` that hit a branch already checked out somewhere else, and the
    /// worktree it collided with. §7 says the flow offers to adopt that
    /// worktree rather than refusing and leaving the user to find it.
    conflict: Option<(grove_proto::WorktreeRef, std::path::PathBuf)>,
    /// The terminal's current height, for sizing the pty. Width is `width`.
    height: u16,
    /// The last size the terminal pane actually had, kept so hiding the pane
    /// does not reflow the program inside its pty.
    last_pane_size: (u16, u16),
    /// Text being selected with the mouse, or selected and still shown.
    selection: Option<selection::Selection>,
    /// Text waiting to be put on the clipboard. The loop owns the terminal,
    /// so it is the loop that writes the request; the handler only says what.
    clipboard: Option<String>,
    /// Something to say about the last thing the user pressed — a refusal, or
    /// a failure to reach the daemon. Cleared by the next successful action.
    note: Option<String>,
    /// The open session, learned from the daemon's `Sessions` event. Adopt and
    /// release name it, because ownership is a session's and the daemon will
    /// not infer which one asked.
    session: Option<grove_domain::SessionId>,
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
    /// The pty's size: the terminal pane's inner area, or the whole terminal
    /// minus the chrome when the pane is not on screen — a pty still needs a
    /// size, and one that swings with visibility would reflow the program
    /// inside it every time the pane is toggled.
    fn terminal_pane_size(&self) -> (u16, u16) {
        // The area the rows are painted in, from the geometry the pane is
        // drawn with. This used to be its own arithmetic — a border and
        // nothing else, under a one-row bar — and when the dash grew a header
        // row, a blank line and padding, the pty stayed two rows taller and
        // two columns wider than the pane showing it.
        match dash::layout(self.body_area(), self.panes, &self.theme)
            .1
            .terminal
        {
            Some(rect) => (rect.height.max(1), rect.width.max(1)),
            // Hidden. The pty keeps the size it had rather than taking the
            // whole body: a program inside it would otherwise reflow twice
            // around a `^g 3` to glance at something else, and vim redrawing
            // itself at two different widths is a worse cost than a grid that
            // is briefly the wrong size for a pane nobody is looking at.
            None => self.last_pane_size,
        }
    }

    /// The size of the pty on screen: the scratch shell's box while it is
    /// open, the dash's terminal pane otherwise.
    ///
    /// The shell used to be attached and resized with the pane's size and
    /// then drawn in its own box — 34x53 inside a 20x92 area at 160x40, so it
    /// wrapped at half the width it had and ran fourteen rows past the bottom.
    fn shown_pty_size(&self) -> (u16, u16) {
        if self.screen == Screen::Shell {
            let body = self.body_area();
            if let Some((rows, hints)) = self.overlay_shape(body) {
                let area = shell_pty_area(overlay::layout(body, rows, hints).parts.body);
                return (area.height.max(1), area.width.max(1));
            }
        }
        self.terminal_pane_size()
    }

    /// The area the dash is drawn in: all of it but the status bar and the
    /// blank row above it. `draw` lays the frame out the same way.
    fn body_area(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height.saturating_sub(2),
        }
    }

    /// The status bar's row.
    fn bar_area(&self) -> Rect {
        Rect {
            x: 0,
            y: self.height.saturating_sub(1),
            width: self.width,
            height: 1,
        }
    }

    /// The panes as they are drawn, which is not always as they were asked
    /// for: §4.1's empty state draws two, so the guidance has the width.
    fn drawn_panes(&self) -> Panes {
        let empty = empty::Empty::of(
            self.repos.workspace_count(),
            self.repos.member_count(),
            self.session.is_some(),
        );
        match empty {
            Some(_) => self.panes.for_guidance(),
            None => self.panes,
        }
    }

    /// How many rows an overlay's contents take and what its footer offers,
    /// for the screen that is open — or `None` on the dash.
    ///
    /// Drawing and the mouse both ask this. The box's size and the footer's
    /// buttons follow from it, so if the two asked separately a click could
    /// land on a button drawn a column to the left of where it was aimed.
    fn overlay_shape(&self, body: Rect) -> Option<(u16, Vec<statusbar::Hint>)> {
        Some(match self.screen {
            Screen::Dash => return None,
            Screen::EndSession => (self.ending.height(), self.ending.footer()),
            Screen::Picker => (self.sessions.height(), self.sessions.footer()),
            Screen::Prune => (self.prune.height(), self.prune.footer()),
            Screen::Palette => (self.palette.height(), self.palette.footer()),
            Screen::Diff => (tall(body), self.diff.footer()),
            Screen::Shell => (tall(body), statusbar::hints(Screen::Shell)),
        })
    }

    /// How many lines the help has and how many fit, for the scroll.
    fn help_extent(&self) -> (usize, usize) {
        let screen = self.helping.unwrap_or(self.screen);
        (
            help::Help::lines_with(screen, &self.keys.spellings(), &self.theme).len(),
            usize::from(self.height.saturating_sub(1)).max(1),
        )
    }

    /// Record the terminal pane's size while it is on screen, for the times
    /// it is not.
    fn remember_pane_size(&mut self) {
        if let Some(rect) = dash::layout(self.body_area(), self.panes, &self.theme)
            .1
            .terminal
        {
            self.last_pane_size = (rect.height.max(1), rect.width.max(1));
        }
    }

    #[cfg(test)]
    fn new() -> Self {
        Self::with_config(&TuiConfig::default(), None, Depth::detect())
    }

    /// Take the user's keymaps, reporting anything that could not be bound or
    /// that took a key grove uses.
    fn with_lua(&mut self, runtime: grove_lua::TuiRuntime) {
        let spellings: Vec<String> = runtime
            .registrations()
            .keymaps
            .iter()
            .map(|registration| registration.key.clone())
            .collect();
        let (keys, notes) = UserKeys::load(&spellings, |chord| {
            keymap::what_uses(chord.prefixed, chord.key)
        });
        self.keys = keys;
        if !notes.is_empty() {
            // Reported at load, which is the moment the user can still connect
            // it to what they just wrote.
            let said: Vec<String> = notes.iter().map(ToString::to_string).collect();
            self.config_note = Some(match &self.config_note {
                Some(existing) => format!("{existing}; {}", said.join("; ")),
                None => said.join("; "),
            });
        }
        self.palette.with_user(
            runtime
                .registrations()
                .commands
                .iter()
                .map(|registration| registration.name.clone())
                .collect(),
        );
        self.lua = Some(runtime);
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
            worktrees: Worktrees::default(),
            terminals: Terminals::default(),
            palette: Palette::default(),
            prune: Prune::default(),
            sessions: Sessions::default(),
            columns: columns::Columns::default(),
            keys: UserKeys::default(),
            lua: None,
            diff: Diff::default(),
            help: Help::default(),
            helping: None,
            scratch: None,
            ending: EndSession::default(),
            conflict: None,
            session: None,
            note: None,
            width: 80,
            height: 24,
            last_pane_size: (22, 34),
            selection: None,
            clipboard: None,
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
    Connected {
        workspace: PathBuf,
        note: String,
        /// The write half of the daemon connection. `None` only in tests that
        /// exercise the loop without a socket.
        daemon: Option<UnixStream>,
    },
    /// Could not reach a daemon, or lost it. The TUI stays up to say so: the
    /// user's terminals are still running inside a daemon it can no longer see,
    /// and exiting silently would suggest otherwise.
    Disconnected { workspace: PathBuf, reason: String },
}

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Invocation {
    /// Open the TUI on this workspace, or the current directory.
    Run(Option<PathBuf>),
    Help,
    Version,
    /// Something that is neither a flag grove knows nor a path. Said, with
    /// the usage, rather than taken as a workspace called `-x`.
    Bad(String),
}

/// Read the arguments, without touching the daemon or the terminal.
///
/// Flags are answered before anything else happens. `grove --version` used
/// to take `--version` as the workspace path and start a daemon for it;
/// that is the first thing a new user types, so it has to be the first thing
/// that works. A path that really starts with `-` goes after `--`.
fn parse_args(mut args: impl Iterator<Item = std::ffi::OsString>) -> Invocation {
    let Some(first) = args.next() else {
        return Invocation::Run(None);
    };
    let extra = args.next();
    let invocation = match first.to_str() {
        Some("-h" | "--help") => Invocation::Help,
        Some("-V" | "--version") => Invocation::Version,
        Some("--") => match extra {
            Some(path) => return Invocation::Run(Some(PathBuf::from(path))),
            None => Invocation::Bad("`--` needs a workspace path after it".into()),
        },
        Some(flag) if flag.starts_with('-') => Invocation::Bad(format!("unknown option {flag}")),
        _ => Invocation::Run(Some(PathBuf::from(first))),
    };
    match (invocation, extra) {
        (Invocation::Bad(why), _) => Invocation::Bad(why),
        (_, Some(extra)) => {
            Invocation::Bad(format!("unexpected argument {}", extra.to_string_lossy()))
        }
        (invocation, None) => invocation,
    }
}

const USAGE: &str = "\
usage: grove [workspace]

Open grove on a directory of git clones (the current directory by default).
If no daemon is serving that workspace, grove starts the groved installed
beside it; the daemon keeps your terminals running after grove exits.

  -h, --help        this
  -V, --version     print the version

  GROVE_NO_AUTOSTART=1   do not start a daemon
  GROVE_DAEMON=path      start this daemon binary instead

Inside grove, ^g ? lists the keys for the screen you are on.";

fn main() -> ExitCode {
    let workspace = match parse_args(std::env::args_os().skip(1)) {
        Invocation::Help => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Invocation::Version => {
            println!("grove {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Invocation::Bad(why) => {
            eprintln!("grove: {why}\n\n{USAGE}");
            return ExitCode::from(2);
        }
        Invocation::Run(path) => path
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from(".")),
    };

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
        Some(loaded) => {
            let mut ui = Ui::with_config(loaded.runtime.config(), loaded.error, Depth::detect());
            ui.with_lua(loaded.runtime);
            ui
        }
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
        // And the height: the mouse resolves clicks against the layout, and a
        // layout sized by the default 24 rows would put every click on the
        // wrong row until the terminal happened to be resized.
        ui.height = size.height;
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
        // Between frames, never inside one: an escape sequence written while
        // ratatui is mid-draw would land in the middle of its output.
        if let Some(text) = ui.clipboard.take() {
            use std::io::Write;
            let backend = term.backend_mut();
            let _ = backend.write_all(clipboard::osc52(&text).as_bytes());
            let _ = backend.flush();
        }
    }
}

/// Run the command the palette has selected, or say why it cannot yet.
///
/// Only the argumentless commands that already have a request behind them run
/// today. The rest are honest about it rather than closing the palette and
/// doing nothing: a command line that swallows `enter` teaches the user that
/// grove is unreliable, which is a harder thing to unlearn than a wait.
fn run_command(chosen: Option<palette::Entry>, state: &mut State, ui: &mut Ui) -> bool {
    // Already collecting an argument: `enter` means "do it", not "choose it".
    if ui.palette.arguing().is_some() {
        return confirm_argument(state, ui);
    }
    let Some(command) = chosen else {
        // Nothing matched what was typed. The palette already says so.
        return false;
    };
    if matches!(command.takes, Takes::Argument(_)) {
        // Step into argument mode rather than running: the picker is built
        // from the repos the daemon last sent, which is why this needs the UI
        // rather than only the command.
        let repos = ui.repos.all().to_vec();
        ui.palette.argue(command, &repos);
        return true;
    }

    let request = match command.name.as_str() {
        "scan" => Some(Request::Scan),
        // The picker opens on the daemon's answer, not on the keystroke: its
        // whole point is that the safe rows are the daemon's judgement, and an
        // empty screen that fills in later would invite `a` before they
        // arrive.
        "prune" => Some(Request::ListPruneCandidates),
        "snapshot" => ui.session.clone().map(Request::SaveSnapshot),
        // `defaults` is an editor and `keys` the help overlay (#30). Each is
        // a screen rather than a request.
        _ => {
            ui.note = Some(format!("{} lands with its screen", command.name));
            return true;
        }
    };
    let Some(request) = request else {
        ui.note = Some("no open session to snapshot".into());
        return true;
    };
    match send(state, &request) {
        Ok(()) => {
            // Ran. The palette closes, because a command line that stays open
            // after running invites the same command twice.
            ui.screen = Screen::Dash;
            ui.note = None;
            true
        }
        Err(e) => {
            ui.note = Some(format!("could not reach the daemon: {e}"));
            true
        }
    }
}

/// Open the end-session confirm for `session`.
///
/// The figures come from the daemon, one repo at a time, because that is how
/// the protocol answers — so the confirm asks for every member repo and shows
/// nothing until they are all back. A screen that counts up while someone
/// reads it is the wrong screen to hurry.
fn begin_end_session(session: grove_domain::SessionId, state: &mut State, ui: &mut Ui) -> bool {
    let name = ui
        .sessions
        .selected()
        .filter(|row| row.id == session)
        .map(|row| row.name.clone())
        .unwrap_or_else(|| session.0.clone());
    // Its members, not the workspace's: ending a session touches only what it
    // owns, and asking about the rest would be asking about worktrees that can
    // never be listed here.
    let repos: Vec<grove_domain::RepoId> = ui
        .sessions
        .selected()
        .filter(|row| row.id == session)
        .map(|row| {
            ui.repos
                .all()
                .iter()
                .filter(|repo| row.members.contains(&repo.name))
                .map(|repo| repo.repo.clone())
                .collect()
        })
        .unwrap_or_default();

    let wanted = ui.ending.begin(session, name, repos);
    for repo in wanted {
        if let Err(e) = send(state, &Request::ListWorktrees(repo)) {
            ui.note = Some(format!("could not reach the daemon: {e}"));
            break;
        }
    }
    ui.screen = Screen::EndSession;
    true
}

/// Carry out what the session picker decided a key means.
///
/// `X` is the exception that shapes the rest: it never sends anything, it
/// moves to the confirm, because ending a session removes worktrees and §2
/// makes that the one act grove asks about.
fn act_on_session(intent: Option<sessions::Intent>, state: &mut State, ui: &mut Ui) -> bool {
    let Some(intent) = intent else {
        // Nothing under the cursor — an empty list, not a refusal.
        return false;
    };
    let request = match intent {
        sessions::Intent::Refused(why) => {
            ui.note = Some(why);
            return true;
        }
        sessions::Intent::ConfirmEnd(session) => {
            return begin_end_session(session, state, ui);
        }
        sessions::Intent::Resume(session) => Request::OpenSession(session),
        sessions::Intent::Detach(session) => Request::DetachSession(session),
        sessions::Intent::Close(session) => Request::CloseSession(session),
    };
    if let Err(e) = send(state, &request) {
        ui.note = Some(format!("could not reach the daemon: {e}"));
        return true;
    }
    // The daemon decides what the list looks like afterwards — resuming
    // replaces the open session and detaches the outgoing one, which is its
    // bookkeeping rather than something to mirror here.
    let _ = send(state, &Request::ListSessions);
    ui.note = None;
    if matches!(request, Request::OpenSession(_)) {
        // The dash is about to be a different session's.
        ui.screen = Screen::Dash;
        let _ = send(state, &Request::ListRepos);
    }
    true
}

/// Open the session picker, from `^g s` or a click on the bar's session name.
fn open_picker(state: &mut State, ui: &mut Ui) -> bool {
    // Ask before showing: a list of sessions that fills in underneath someone
    // already pressing `X` is the worst possible version of this screen.
    if let Err(e) = send(state, &Request::ListSessions) {
        ui.note = Some(format!("could not reach the daemon: {e}"));
    }
    ui.screen = Screen::Picker;
    true
}

/// The mouse on the dash (#111).
///
/// Everything here resolves against [`dash::layout`], the geometry the dash
/// was drawn with, so a click means whatever is painted under the pointer.
/// Only the dash for now: while an overlay is open the dash underneath is not
/// what anyone is pointing at, and the overlays' own clicks are the next piece.
fn on_mouse(event: MouseEvent, state: &mut State, ui: &mut Ui) -> Flow {
    if !matches!(state, State::Connected { .. }) {
        return Flow::Continue { redraw: false };
    }
    // Help covers the screen and is a glance, not a destination: the wheel
    // scrolls it as `^g ↑`/`^g ↓` do, and a click anywhere puts it away as
    // `^g ?` does. There is nothing in it to click on, and nothing under it
    // that should take a click meant for it.
    if ui.helping.is_some() {
        return match event.kind {
            MouseEventKind::ScrollUp => press(KeyCode::Up, true, state, ui),
            MouseEventKind::ScrollDown => press(KeyCode::Down, true, state, ui),
            MouseEventKind::Down(MouseButton::Left) => press(KeyCode::Char('?'), true, state, ui),
            _ => Flow::Continue { redraw: false },
        };
    }
    let at = ratatui::layout::Position {
        x: event.column,
        y: event.row,
    };

    // A drag that started a selection belongs to it, wherever the pointer
    // goes next — over the list beside it, over a program that wants the
    // mouse. Otherwise the selection would stop the moment the pointer
    // crossed into vim's pane.
    if let Some(selection) = ui.selection.as_mut() {
        match event.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                return Flow::Continue {
                    redraw: selection.drag_to(at),
                };
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let finished = *selection;
                if finished.is_empty() {
                    // It was a click. The click has already done its work.
                    ui.selection = None;
                } else {
                    let text = selected_text(state, ui, &finished);
                    if let Some(shown) = ui.selection.as_mut() {
                        shown.dragging = true;
                    }
                    if !text.is_empty() {
                        ui.clipboard = Some(text);
                    }
                }
                return Flow::Continue { redraw: true };
            }
            _ => {}
        }
    }

    let starts = matches!(event.kind, MouseEventKind::Down(MouseButton::Left));
    // Decided before the click is handled: the click may close an overlay,
    // and the selection belongs to the screen that was pressed on.
    let area = if starts {
        selectable_area(ui, at)
    } else {
        None
    };
    let had = ui.selection.take().is_some() && starts;
    let screen = ui.screen;
    let flow = on_mouse_action(event, state, ui);
    if let Some(area) = area
        && ui.screen == screen
    {
        ui.selection = Some(selection::Selection::start(area, at));
    }
    match flow {
        Flow::Continue { redraw } => Flow::Continue {
            redraw: redraw || had,
        },
        Flow::Quit => Flow::Quit,
    }
}

/// Where a drag starting at `at` would select, if anywhere.
///
/// The dash's panes, the scratch shell and the diff — the places with text a
/// person might want to copy. Not over a program that took the mouse (that
/// drag is the program's), and not over the pick lists, whose rows act on a
/// press.
fn selectable_area(ui: &Ui, at: ratatui::layout::Position) -> Option<Rect> {
    let body = ui.body_area();
    let pty_took_it = ui.terminals.wants_mouse().is_some();
    if ui.screen == Screen::Dash {
        let (_, rows) = dash::layout(body, ui.drawn_panes(), &ui.theme);
        let terminal = rows.terminal.filter(|_| !pty_took_it);
        return [rows.repos, rows.worktrees, terminal]
            .into_iter()
            .flatten()
            .find(|area| area.contains(at));
    }
    let (count, hints) = ui.overlay_shape(body)?;
    let laid = overlay::layout(body, count, hints);
    let area = match ui.screen {
        Screen::Shell if !pty_took_it => shell_pty_area(laid.parts.body),
        Screen::Diff => laid.parts.body,
        _ => return None,
    };
    area.contains(at).then_some(area)
}

/// What a mouse event does, selection aside.
fn on_mouse_action(event: MouseEvent, state: &mut State, ui: &mut Ui) -> Flow {
    if ui.screen != Screen::Dash {
        return on_overlay_mouse(event, state, ui);
    }
    Flow::Continue {
        redraw: on_dash_mouse(event, state, ui),
    }
}

/// The mouse on the dash itself: its panes, rows and the bar's session name.
fn on_dash_mouse(event: MouseEvent, state: &mut State, ui: &mut Ui) -> bool {
    let (column, row) = (event.column, event.row);
    let (frames, rows) = dash::layout(ui.body_area(), ui.drawn_panes(), &ui.theme);
    let under = mouse::hit(&frames, &rows, column, row);

    // A program in the terminal pane that asked for the mouse gets it — a
    // click positions vim's cursor, the wheel scrolls htop — and a click
    // still gives the pane the keyboard, as it would anywhere else.
    if let Some(pty) = rows.terminal
        && to_pty(&event, pty, state, ui)
    {
        if matches!(event.kind, MouseEventKind::Down(_)) && ui.focus != Focus::Terminal {
            ui.focus = Focus::Terminal;
            return true;
        }
        return false;
    }

    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if mouse::on_session_name(ui.bar_area(), &ui.session_name(), column, row) {
                return open_picker(state, ui);
            }
            match under {
                mouse::Hit::Row(Focus::Repos, index) => {
                    ui.focus = Focus::Repos;
                    if ui.repos.select(index) {
                        request_worktrees(state, ui);
                    }
                    true
                }
                mouse::Hit::Row(Focus::Worktrees, index) => {
                    ui.focus = Focus::Worktrees;
                    if ui.worktrees.select(index) {
                        follow_selection(state, ui);
                    }
                    true
                }
                mouse::Hit::Row(pane, _) | mouse::Hit::Pane(pane) => {
                    let moved = ui.focus != pane;
                    ui.focus = pane;
                    moved
                }
                // A click on nothing does nothing: no focus change, no jump.
                mouse::Hit::Nothing => false,
            }
        }
        // The wheel scrolls whatever is under the pointer, focused or not —
        // it is pointing, not typing, and pointing does not need focus first.
        kind @ (MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) => {
            let up = kind == MouseEventKind::ScrollUp;
            let pane = match under {
                mouse::Hit::Row(pane, _) | mouse::Hit::Pane(pane) => pane,
                mouse::Hit::Nothing => return false,
            };
            match pane {
                Focus::Repos => {
                    let moved = if up {
                        ui.repos.move_up()
                    } else {
                        ui.repos.move_down()
                    };
                    if moved {
                        request_worktrees(state, ui);
                    }
                    moved
                }
                Focus::Worktrees => {
                    let moved = if up {
                        ui.worktrees.move_up()
                    } else {
                        ui.worktrees.move_down()
                    };
                    if moved {
                        follow_selection(state, ui);
                    }
                    moved
                }
                // grove's scrollback, as `^g ↑` and `^g ↓` scroll it.
                Focus::Terminal => ui.terminals.scroll(if up {
                    terminals::SCROLL_STEP
                } else {
                    -terminals::SCROLL_STEP
                }),
            }
        }
        _ => false,
    }
}

/// Where the scratch shell's pty is drawn inside its overlay: the body, less
/// the note under it that says `^g` is the way out. The mouse forwards clicks
/// relative to this, so it is the one place that knows it.
fn shell_pty_area(body: Rect) -> Rect {
    Rect {
        height: body.height.saturating_sub(2),
        ..body
    }
}

/// Hand a mouse event to the program in the pty drawn at `area`, if the
/// pointer is over it and the program asked for the mouse.
///
/// Returns whether the program has the mouse there — in which case grove does
/// nothing else with the event, even one the program's mode does not report,
/// since two things answering one click is how a click in vim also scrolls
/// grove. The program asks with `\e[?1000h` and its relatives; the parser
/// remembers; [`terminals::encode_mouse`] speaks whichever encoding it chose.
fn to_pty(event: &MouseEvent, area: Rect, state: &mut State, ui: &mut Ui) -> bool {
    let at = ratatui::layout::Position {
        x: event.column,
        y: event.row,
    };
    if !area.contains(at) {
        return false;
    }
    let Some((mode, encoding)) = ui.terminals.wants_mouse() else {
        return false;
    };
    let encoded = terminals::encode_mouse(event, at.x - area.x, at.y - area.y, mode, encoding);
    if let Some(request) = encoded.and_then(|bytes| ui.terminals.input(bytes))
        && let Err(e) = send(state, &request)
    {
        ui.note = Some(format!("could not reach the daemon: {e}"));
    }
    true
}

/// Press a key as if it had been typed, prefix and all.
///
/// Every mouse action on an overlay comes down to this, so a click can only
/// ever do what some key already does — through the same routing, the same
/// guards, the same requests. A mouse layer with its own idea of what
/// "resume" means would be a second implementation of every screen.
fn press(code: KeyCode, prefixed: bool, state: &mut State, ui: &mut Ui) -> Flow {
    let mut redraw = false;
    if prefixed {
        match handle(
            Input::Terminal(TermEvent::Key(KeyEvent::new(
                KeyCode::Char('g'),
                KeyModifiers::CONTROL,
            ))),
            state,
            ui,
        ) {
            Flow::Quit => return Flow::Quit,
            Flow::Continue { redraw: r } => redraw |= r,
        }
    }
    match handle(
        Input::Terminal(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))),
        state,
        ui,
    ) {
        Flow::Quit => Flow::Quit,
        Flow::Continue { redraw: r } => Flow::Continue {
            redraw: redraw || r,
        },
    }
}

/// What a click on a row of an overlay's list does, beyond moving there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnClick {
    /// Pick lists: the first click selects, a click on the row already
    /// selected presses `enter` — the session picker's resume, the palette's
    /// run.
    ConfirmOnSecond,
    /// Tick lists — prune, and the palette's repo picker: every click
    /// toggles, as `space` does and as the mock's rows do.
    Toggle,
    /// The diff's files: selecting is all there is.
    Select,
}

/// An overlay's list, as it is drawn: where it is, and where its cursor is.
#[derive(Debug, Clone, Copy)]
struct ClickList {
    /// The rows that are painted, one per item from the first.
    area: Rect,
    cursor: usize,
    on_click: OnClick,
}

impl Ui {
    /// The clickable list on the overlay that is open, if it has one.
    fn overlay_list(&self, body: Rect) -> Option<ClickList> {
        let list = |top: u16, count: usize, width: u16, cursor: usize, on_click| {
            let painted = u16::try_from(count)
                .unwrap_or(u16::MAX)
                .min(body.bottom().saturating_sub(top));
            ClickList {
                area: Rect {
                    x: body.x,
                    y: top,
                    width,
                    height: painted,
                },
                cursor,
                on_click,
            }
        };
        match self.screen {
            Screen::Palette if self.palette.arguing().is_none() => {
                let (cursor, count) = self.palette.cursor();
                Some(list(
                    body.y,
                    count,
                    body.width,
                    cursor,
                    OnClick::ConfirmOnSecond,
                ))
            }
            Screen::Palette => {
                // A command that takes a name has no rows to click.
                let select = self.palette.select()?;
                Some(list(
                    body.y + self.palette.list_offset(),
                    select.rows().len(),
                    body.width,
                    select.cursor(),
                    OnClick::Toggle,
                ))
            }
            Screen::Picker => {
                let (cursor, count) = self.sessions.cursor();
                Some(list(
                    body.y,
                    count,
                    body.width,
                    cursor,
                    OnClick::ConfirmOnSecond,
                ))
            }
            Screen::Prune => {
                let (cursor, count) = self.prune.cursor();
                Some(list(body.y, count, body.width, cursor, OnClick::Toggle))
            }
            Screen::Diff => {
                let (cursor, count) = self.diff.cursor();
                Some(list(
                    body.y,
                    count,
                    diff::list_width(body.width),
                    cursor,
                    OnClick::Select,
                ))
            }
            Screen::Dash | Screen::EndSession | Screen::Shell => None,
        }
    }
}

/// The mouse over an overlay (#111).
///
/// A click outside the box closes it, the way that screen closes; a click on
/// a footer hint presses its key; a click on a row walks the cursor there
/// with the arrows and then does what that list does with a click; the wheel
/// presses the arrows. Everything is a key in the end — see [`press`].
fn on_overlay_mouse(event: MouseEvent, state: &mut State, ui: &mut Ui) -> Flow {
    let quiet = Flow::Continue { redraw: false };
    let body = ui.body_area();
    let Some((rows, hints)) = ui.overlay_shape(body) else {
        return quiet;
    };
    let laid = overlay::layout(body, rows, hints);
    let at = ratatui::layout::Position {
        x: event.column,
        y: event.row,
    };
    if ui.screen == Screen::Shell && to_pty(&event, shell_pty_area(laid.parts.body), state, ui) {
        return quiet;
    }

    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if !laid.frame.contains(at) {
                // Out of the box: close it. `esc` everywhere but the scratch
                // shell, where `esc` belongs to the program inside it and
                // `^g i` is the way out.
                return if ui.screen == Screen::Shell {
                    press(KeyCode::Char('i'), true, state, ui)
                } else {
                    press(KeyCode::Esc, false, state, ui)
                };
            }
            if let Some((_, hint)) = laid.buttons.iter().find(|(area, _)| area.contains(at)) {
                return press(hint.key, hint.prefixed, state, ui);
            }
            let Some(list) = ui.overlay_list(laid.parts.body) else {
                return quiet;
            };
            if !list.area.contains(at) {
                return quiet;
            }
            click_row(list, usize::from(at.y - list.area.y), state, ui)
        }
        kind @ (MouseEventKind::ScrollUp | MouseEventKind::ScrollDown)
            if laid.frame.contains(at) =>
        {
            let key = if kind == MouseEventKind::ScrollUp {
                KeyCode::Up
            } else {
                KeyCode::Down
            };
            match ui.screen {
                // Its program did not ask for the mouse (that was handled
                // above), so the wheel scrolls grove's scrollback, as it does
                // over the dash's terminal pane.
                Screen::Shell => Flow::Continue {
                    redraw: ui.terminals.scroll(if key == KeyCode::Up {
                        terminals::SCROLL_STEP
                    } else {
                        -terminals::SCROLL_STEP
                    }),
                },
                // Over the patch, scroll the patch (`^g ↑`/`^g ↓`); over the
                // file list, move between files.
                Screen::Diff
                    if at.x >= laid.parts.body.x + diff::list_width(laid.parts.body.width) =>
                {
                    press(key, true, state, ui)
                }
                _ => press(key, false, state, ui),
            }
        }
        _ => quiet,
    }
}

/// Walk an overlay's cursor to the clicked row, then act as that list acts.
///
/// Walked with the arrows rather than set directly, so whatever moving the
/// cursor does — the diff asking for the file's patch, say — happens exactly
/// as it would from the keyboard.
fn click_row(list: ClickList, index: usize, state: &mut State, ui: &mut Ui) -> Flow {
    let (key, steps) = if index >= list.cursor {
        (KeyCode::Down, index - list.cursor)
    } else {
        (KeyCode::Up, list.cursor - index)
    };
    let mut redraw = false;
    for _ in 0..steps {
        match press(key, false, state, ui) {
            Flow::Quit => return Flow::Quit,
            Flow::Continue { redraw: r } => redraw |= r,
        }
    }
    let follow = match list.on_click {
        OnClick::Toggle => Some(KeyCode::Char(' ')),
        OnClick::ConfirmOnSecond if steps == 0 => Some(KeyCode::Enter),
        OnClick::ConfirmOnSecond | OnClick::Select => None,
    };
    match follow {
        Some(key) => match press(key, false, state, ui) {
            Flow::Quit => Flow::Quit,
            Flow::Continue { redraw: r } => Flow::Continue {
                redraw: redraw || r,
            },
        },
        None => Flow::Continue { redraw },
    }
}

/// Carry out a command that has its argument.
///
/// Every one of these names the open session, because membership and
/// worktrees belong to a session rather than to the workspace. Checking a
/// non-member in `new`'s picker adds it first — §4.2 says picking one is how
/// it becomes a member, so the two requests go together rather than leaving
/// the user to do it twice.
fn confirm_argument(state: &mut State, ui: &mut Ui) -> bool {
    let Some(command) = ui.palette.arguing().cloned() else {
        return false;
    };
    // A user's command is its own implementation: grove hands over what was
    // typed and gets out of the way.
    if let Some(index) = command.user {
        let argument = ui.palette.argument().unwrap_or_default().to_string();
        return run_user_command(index, &argument, state, ui);
    }
    let argument = ui.palette.argument().unwrap_or_default().to_string();

    let picked: Vec<select::Row> = ui
        .palette
        .select()
        .map(|select| select.checked().into_iter().cloned().collect())
        .unwrap_or_default();

    let mut requests: Vec<Request> = Vec::new();

    // `session new` is how you get a session, so it is answered before one is
    // looked for. Requiring an open session to run it left a fresh workspace
    // with no way in: the dash said to start a session, the palette took the
    // name, and `enter` answered "no open session to session new in" — a
    // sentence assembled from a template, about the one command that does not
    // need what it was asking for.
    if command.name == "session new" {
        if argument.trim().is_empty() {
            ui.note = Some("name the session first".into());
            return true;
        }
        requests.push(Request::SessionNew {
            name: argument.trim().to_string(),
        });
        return dispatch(requests, state, ui);
    }

    // Everything else belongs to a session: membership and worktrees are a
    // session's, not the workspace's.
    let Some(session) = ui.session.clone() else {
        ui.note = Some(format!("{} needs an open session", command.name));
        return true;
    };

    match command.name.as_str() {
        "new" => {
            if argument.trim().is_empty() {
                ui.note = Some("name the branch first".into());
                return true;
            }
            if picked.is_empty() {
                ui.note = Some("no repos checked — space toggles a row".into());
                return true;
            }
            // A checked non-member joins the session before it gets a
            // worktree, because a worktree belongs to a member.
            for row in picked.iter().filter(|row| !row.member) {
                requests.push(Request::AddMember {
                    session: session.clone(),
                    repo: row.repo.clone(),
                });
            }
            requests.push(Request::NewWorktrees {
                session: session.clone(),
                branch: argument.trim().to_string(),
                repos: picked.iter().map(|row| row.repo.clone()).collect(),
            });
        }
        "add" | "remove" => {
            if picked.is_empty() {
                ui.note = Some("nothing checked — space toggles a row".into());
                return true;
            }
            for row in &picked {
                requests.push(if command.name == "add" {
                    Request::AddMember {
                        session: session.clone(),
                        repo: row.repo.clone(),
                    }
                } else {
                    Request::RemoveMember {
                        session: session.clone(),
                        repo: row.repo.clone(),
                    }
                });
            }
        }
        "session rename" => requests.push(Request::SessionRename {
            session: session.clone(),
            name: argument.trim().to_string(),
        }),
        other => {
            // `open` needs the session picker (#26) and `fetch` its own
            // argument list. Said rather than silently doing nothing.
            ui.note = Some(format!("{other} lands with its screen"));
            return true;
        }
    }

    dispatch(requests, state, ui)
}

/// Send what a confirmed command asked for, and go back to the dash.
///
/// Shared so that `session new`, which is answered before the session lookup,
/// finishes the same way as everything after it.
fn dispatch(requests: Vec<Request>, state: &mut State, ui: &mut Ui) -> bool {
    for request in &requests {
        if let Err(e) = send(state, request) {
            ui.note = Some(format!("could not reach the daemon: {e}"));
            return true;
        }
    }
    // The daemon owns what happens next; ask for the rows that will show it.
    let _ = send(state, &Request::ListRepos);
    ui.screen = Screen::Dash;
    ui.note = None;
    true
}

/// Carry out what the WORKTREES pane decided `^g a` or `^g r` means.
///
/// A refusal is as much an outcome as a request: it goes on the status bar,
/// because a key that does nothing at all reads as grove being broken rather
/// than as grove protecting something.
fn act_on_worktree(intent: Option<Intent>, state: &mut State, ui: &mut Ui) -> bool {
    let Some(intent) = intent else {
        // Nothing under the cursor — an empty pane, not a refusal.
        return false;
    };
    let request = match intent {
        Intent::Refused(why) => {
            ui.note = Some(why);
            return true;
        }
        Intent::Adopt(worktree) => ui
            .session
            .clone()
            .map(|session| Request::AdoptWorktree { session, worktree }),
        Intent::Release(worktree) => ui
            .session
            .clone()
            .map(|session| Request::ReleaseWorktree { session, worktree }),
    };
    let Some(request) = request else {
        // Ownership belongs to a session, so with none open there is nothing
        // to adopt *into*. Said rather than silently dropped.
        ui.note = Some("no open session to adopt into".into());
        return true;
    };
    match send(state, &request) {
        Ok(()) => {
            // Ownership is the daemon's to write, so the row does not change
            // here. Asking again is what makes the answer arrive: the daemon
            // replies to `ListWorktrees`, it does not push a repo's rows
            // unprompted.
            ui.note = None;
            request_worktrees(state, ui);
            false
        }
        Err(e) => {
            ui.note = Some(format!("could not reach the daemon: {e}"));
            true
        }
    }
}

/// The questions the dash cannot draw without.
///
/// Sent the moment the handshake succeeds. The daemon answers what it is
/// asked; it does not volunteer a workspace, so without this the panes stay
/// empty for as long as grove is open — which is how the dash shipped until
/// review caught it. Worktrees are not here: they are asked for per repo, once
/// there is a selection to ask about.
fn ask_for_the_dash<W: std::io::Write>(daemon: &mut W) -> Result<(), grove_proto::FrameError> {
    grove_proto::write_frame(daemon, &Request::ListSessions)?;
    grove_proto::write_frame(daemon, &Request::ListRepos)
}

/// Follow the WORKTREES cursor with the terminal pane.
///
/// Called wherever the selection can change — an arrow, a refreshed list, a
/// terminal exiting. Attaching is not free, so `Terminals::show` returns
/// nothing when the selection did not actually move.
fn follow_selection(state: &mut State, ui: &mut Ui) {
    let terminal = ui.worktrees.selected().and_then(|row| row.terminal);
    ui.remember_pane_size();
    let (rows, cols) = ui.terminal_pane_size();
    for request in ui.terminals.show(terminal, rows, cols) {
        if let Err(e) = send(state, &request) {
            ui.note = Some(format!("could not reach the daemon: {e}"));
            return;
        }
    }
}

/// How long a user callback may run before grove takes the screen back.
///
/// Short enough that a mistake does not read as a freeze, long enough that a
/// callback shelling out to `gh` has a chance — §10.5 asks for a bound, not a
/// particular one.
const USER_CALLBACK_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// Recompute the user's columns for the rows now on screen.
///
/// Off the render path deliberately: a callback shelling out to `gh` takes as
/// long as the network does, and a frame that waits for it is a pane that has
/// stopped answering arrow keys. The arrival of a new worktree list is the
/// invalidation point, because the cells describe those rows and nothing else.
fn refresh_user_columns(ui: &mut Ui) {
    let Some(runtime) = ui.lua.take() else {
        return;
    };
    let rows = ui.worktrees.rows_for_columns();
    ui.columns.refresh(&runtime, &rows);
    if let Some(note) = ui.columns.take_note() {
        ui.note = Some(note);
    }
    ui.lua = Some(runtime);
}

/// Run a user's palette command, containing whatever it does.
///
/// Same containment as a keymap's: bounded in time, and a throw takes the
/// command out of the palette rather than failing again next time it is run.
fn run_user_command(index: usize, argument: &str, state: &mut State, ui: &mut Ui) -> bool {
    let Some(runtime) = ui.lua.as_ref() else {
        return false;
    };
    runtime.set_context(lua_context(ui));
    let outcome = runtime.call_command(index, argument, USER_CALLBACK_BUDGET);
    perform_asks(state, ui);
    match outcome {
        Ok(()) => {
            ui.screen = Screen::Dash;
            ui.note = None;
            true
        }
        Err(grove_lua::CallError::Timeout) => {
            ui.note = Some("that command ran too long and was stopped".into());
            true
        }
        Err(grove_lua::CallError::Failed(why)) => {
            ui.note = Some(format!("command failed: {why}"));
            true
        }
    }
}

/// Perform what a callback asked for (§10.4's stateful helpers).
///
/// The helpers record rather than act, because `grove-lua` is shared and may
/// not reach the protocol — so this is where intent becomes requests. Asks are
/// performed in the order they were made, because a script that creates a
/// worktree and then opens the palette meant that order.
fn perform_asks(state: &mut State, ui: &mut Ui) {
    let Some(runtime) = ui.lua.as_ref() else {
        return;
    };
    for ask in runtime.take_asks() {
        match ask {
            grove_lua::Ask::NewWorktree { repo, branch } => {
                let Some(session) = ui.session.clone() else {
                    ui.note = Some("no open session to create a worktree in".into());
                    continue;
                };
                let request = Request::NewWorktrees {
                    session,
                    branch,
                    repos: vec![grove_domain::RepoId(repo)],
                };
                if let Err(e) = send(state, &request) {
                    ui.note = Some(format!("could not reach the daemon: {e}"));
                }
            }
            grove_lua::Ask::OpenSession { name } => {
                // By name, because that is what a script knows; the id is
                // grove's business and the picker already holds the mapping.
                match ui
                    .sessions
                    .by_name(&name)
                    .map(|row| Request::OpenSession(row.id.clone()))
                {
                    Some(request) => {
                        if let Err(e) = send(state, &request) {
                            ui.note = Some(format!("could not reach the daemon: {e}"));
                        }
                    }
                    None => ui.note = Some(format!("no session called {name:?}")),
                }
            }
            grove_lua::Ask::Send { text } => match ui.terminals.input(text.into_bytes()) {
                Some(request) => {
                    if let Err(e) = send(state, &request) {
                        ui.note = Some(format!("could not reach the daemon: {e}"));
                    }
                }
                None => ui.note = Some("no terminal to send to".into()),
            },
            grove_lua::Ask::Palette { prefill } => {
                // Purely local: the palette is grove's own screen.
                ui.screen = Screen::Palette;
                ui.palette.open();
                for c in prefill.chars() {
                    ui.palette.push(c);
                }
            }
        }
    }
}

/// What the VM should know about where the user is, before a callback runs.
fn lua_context(ui: &Ui) -> grove_lua::Context {
    grove_lua::Context {
        current_repo: ui.repos.selected().map(|row| row.name.clone()),
    }
}

/// Run a user keymap's callback, containing whatever it does.
fn run_user_key(index: usize, state: &mut State, ui: &mut Ui) -> Flow {
    let Some(runtime) = ui.lua.as_ref() else {
        return Flow::Continue { redraw: false };
    };
    runtime.set_context(lua_context(ui));
    let outcome = runtime.call_keymap(index, USER_CALLBACK_BUDGET);
    // Whatever it managed to record before it finished — or was stopped —
    // still happened as far as the script is concerned.
    perform_asks(state, ui);
    match outcome {
        Ok(()) => Flow::Continue { redraw: true },
        Err(grove_lua::CallError::Timeout) => {
            // Reported, but not disabled: a timeout may be a slow command
            // rather than a broken one, and taking the key away for the rest
            // of the session would be grove deciding that.
            ui.note = Some("that keymap ran too long and was stopped".into());
            Flow::Continue { redraw: true }
        }
        Err(grove_lua::CallError::Failed(why)) => {
            // §10.5: a throwing callback disables that registration rather
            // than failing again on every keystroke.
            ui.keys.disable(index);
            ui.note = Some(format!("keymap disabled after it failed: {why}"));
            Flow::Continue { redraw: true }
        }
    }
}

/// Ask the daemon for one file's patch, after the diff cursor moved.
///
/// `None` means the cursor did not move, so there is nothing to fetch and no
/// frame to spend.
fn ask_for_file(file: Option<String>, state: &mut State, ui: &mut Ui) -> Flow {
    let Some(file) = file else {
        return Flow::Continue { redraw: false };
    };
    let Some(worktree) = ui.worktrees.selected().map(|row| row.worktree.clone()) else {
        return Flow::Continue { redraw: true };
    };
    let request = Request::DiffWorktree {
        worktree,
        file: Some(file),
    };
    if let Err(e) = send(state, &request) {
        ui.note = Some(format!("could not reach the daemon: {e}"));
    }
    Flow::Continue { redraw: true }
}

/// Ask for the selected repo's worktrees.
///
/// Called whenever the selection changes, because the daemon sends worktrees
/// for a named repo rather than pushing every repo's at once. A failure is
/// left to the status bar: the pane keeps the rows it has, which are stale but
/// labelled with the repo they came from.
fn request_worktrees(state: &mut State, ui: &mut Ui) {
    let Some(repo) = ui.repos.selected().map(|row| row.repo.clone()) else {
        return;
    };
    if let Err(e) = send(state, &Request::ListWorktrees(repo)) {
        ui.note = Some(format!("could not reach the daemon: {e}"));
    }
}

/// Send a request on the daemon connection.
fn send(state: &mut State, request: &Request) -> Result<(), String> {
    match state {
        State::Connected {
            daemon: Some(socket),
            ..
        } => grove_proto::write_frame(socket, request).map_err(|e| e.to_string()),
        _ => Err("not connected to a daemon".into()),
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
    // A key or a resize ends a selection: the key is the user moving on, and
    // after a resize the highlighted cells are no longer the same text.
    let cleared = matches!(
        input,
        Input::Terminal(TermEvent::Key(_) | TermEvent::Resize(..))
    ) && ui.selection.take().is_some();
    match handle_input(input, state, ui) {
        Flow::Continue { redraw } => Flow::Continue {
            redraw: redraw || cleared,
        },
        Flow::Quit => Flow::Quit,
    }
}

fn handle_input(input: Input, state: &mut State, ui: &mut Ui) -> Flow {
    match input {
        Input::Terminal(TermEvent::Key(key)) => {
            let routed = ui.router.route(ui.screen, ui.focus, key);
            // A user binding is consulted before grove's own handling, which
            // is what "merges over" means — and only for keys the router has
            // not already claimed as text or pty input.
            if let Routed::Act(_) | Routed::Unbound = routed
                && let Some(index) =
                    ui.keys
                        .bound(matches!(routed, Routed::Unbound), key.code, key.modifiers)
            {
                return run_user_key(index, state, ui);
            }
            match routed {
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
                // Which list the arrows drive is a question about the screen
                // and the layout, not only about focus — see
                // `dash::list_under_arrows`.
                Routed::Act(Action::MoveDown)
                    if dash::list_under_arrows(ui.screen, ui.focus, ui.panes, ui.width)
                        == Some(Focus::Repos) =>
                {
                    let moved = ui.repos.move_down();
                    if moved {
                        request_worktrees(state, ui);
                    }
                    Flow::Continue { redraw: moved }
                }
                Routed::Act(Action::MoveUp)
                    if dash::list_under_arrows(ui.screen, ui.focus, ui.panes, ui.width)
                        == Some(Focus::Repos) =>
                {
                    let moved = ui.repos.move_up();
                    if moved {
                        request_worktrees(state, ui);
                    }
                    Flow::Continue { redraw: moved }
                }
                Routed::Act(Action::MoveDown)
                    if dash::list_under_arrows(ui.screen, ui.focus, ui.panes, ui.width)
                        == Some(Focus::Worktrees) =>
                {
                    let moved = ui.worktrees.move_down();
                    if moved {
                        follow_selection(state, ui);
                    }
                    Flow::Continue { redraw: moved }
                }
                Routed::Act(Action::MoveUp)
                    if dash::list_under_arrows(ui.screen, ui.focus, ui.panes, ui.width)
                        == Some(Focus::Worktrees) =>
                {
                    let moved = ui.worktrees.move_up();
                    if moved {
                        follow_selection(state, ui);
                    }
                    Flow::Continue { redraw: moved }
                }
                // grove's own scrolling, prefixed because the pty has the
                // unprefixed arrows.
                // Help is drawn over whatever screen is underneath, so it
                // takes the scroll keys while it is open — the pane behind is
                // not the thing being read.
                Routed::Act(Action::ScrollUp)
                    if ui.screen == Screen::Dash && ui.helping.is_none() =>
                {
                    Flow::Continue {
                        redraw: ui.terminals.scroll(terminals::SCROLL_STEP),
                    }
                }
                Routed::Act(Action::ScrollDown)
                    if ui.screen == Screen::Dash && ui.helping.is_none() =>
                {
                    Flow::Continue {
                        redraw: ui.terminals.scroll(-terminals::SCROLL_STEP),
                    }
                }
                Routed::Act(Action::SpawnTerminal) if ui.screen == Screen::Dash => {
                    let target = ui
                        .worktrees
                        .selected()
                        .filter(|row| row.terminal.is_none())
                        .map(|row| grove_proto::TerminalTarget::Worktree(row.worktree.clone()));
                    match target {
                        Some(target) => {
                            let request = Request::SpawnTerminal(target);
                            if let Err(e) = send(state, &request) {
                                ui.note = Some(format!("could not reach the daemon: {e}"));
                            }
                            Flow::Continue { redraw: true }
                        }
                        // Already has one, or there is no row: nothing to do,
                        // and nothing worth saying either.
                        None => Flow::Continue { redraw: false },
                    }
                }
                // Adopt and release act on the row under the WORKTREES cursor,
                // and say why when they cannot — a key that silently does
                // nothing reads as grove being broken.
                Routed::Act(Action::Adopt) if ui.screen == Screen::Dash => {
                    // An offered conflict outranks the cursor: the user was
                    // just told this key takes over the worktree that blocked
                    // them, and the WORKTREES cursor is wherever it happened
                    // to be.
                    let intent = match ui.conflict.take() {
                        Some((worktree, _)) => Some(worktrees::Intent::Adopt(worktree)),
                        None => ui.worktrees.adopt(),
                    };
                    Flow::Continue {
                        redraw: act_on_worktree(intent, state, ui),
                    }
                }
                Routed::Act(Action::Release) if ui.screen == Screen::Dash => {
                    let intent = ui.worktrees.release();
                    Flow::Continue {
                        redraw: act_on_worktree(intent, state, ui),
                    }
                }
                Routed::Act(Action::Cancel) if ui.screen == Screen::EndSession => {
                    // The safe default. Nothing has been sent, so there is
                    // nothing to undo.
                    ui.ending.cancel();
                    ui.screen = Screen::Picker;
                    Flow::Continue { redraw: true }
                }
                Routed::Act(Action::Confirm) if ui.screen == Screen::EndSession => {
                    if !ui.ending.ready() {
                        // Still counting. Confirming a total that is not
                        // finished is confirming something nobody has read.
                        ui.note = Some("still counting what would be removed".into());
                        return Flow::Continue { redraw: true };
                    }
                    let Some(session) = ui.ending.session().cloned() else {
                        return Flow::Continue { redraw: false };
                    };
                    if let Err(e) = send(state, &Request::EndSession(session)) {
                        ui.note = Some(format!("could not reach the daemon: {e}"));
                        return Flow::Continue { redraw: true };
                    }
                    let _ = send(state, &Request::ListSessions);
                    ui.ending.cancel();
                    ui.screen = Screen::Dash;
                    Flow::Continue { redraw: true }
                }
                Routed::Act(Action::Help) => {
                    // It explains the screen you were on, not itself, and
                    // returns you there — `^g ?` is a glance, not a
                    // destination.
                    match ui.helping.take() {
                        Some(previous) => ui.screen = previous,
                        None => {
                            ui.helping = Some(ui.screen);
                            ui.help.open();
                        }
                    }
                    Flow::Continue { redraw: true }
                }
                Routed::Act(Action::ScrollUp) if ui.helping.is_some() => {
                    let (total, room) = ui.help_extent();
                    Flow::Continue {
                        redraw: ui.help.scroll(-3, total, room),
                    }
                }
                Routed::Act(Action::ScrollDown) if ui.helping.is_some() => {
                    let (total, room) = ui.help_extent();
                    Flow::Continue {
                        redraw: ui.help.scroll(3, total, room),
                    }
                }
                Routed::Act(Action::OpenDiff) => {
                    // The selected worktree's, against its base. Nothing is
                    // shown until the daemon answers: an empty diff that fills
                    // in looks like a clean tree.
                    match ui.worktrees.selected().map(|row| row.worktree.clone()) {
                        Some(worktree) => {
                            let request = Request::DiffWorktree {
                                worktree,
                                file: None,
                            };
                            if let Err(e) = send(state, &request) {
                                ui.note = Some(format!("could not reach the daemon: {e}"));
                            }
                            ui.screen = Screen::Diff;
                        }
                        None => ui.note = Some("no worktree selected".into()),
                    }
                    Flow::Continue { redraw: true }
                }
                Routed::Act(Action::Cancel) if ui.screen == Screen::Diff => {
                    ui.screen = Screen::Dash;
                    Flow::Continue { redraw: true }
                }
                Routed::Act(Action::MoveDown) if ui.screen == Screen::Diff => {
                    ask_for_file(ui.diff.move_down(), state, ui)
                }
                Routed::Act(Action::MoveUp) if ui.screen == Screen::Diff => {
                    ask_for_file(ui.diff.move_up(), state, ui)
                }
                Routed::Act(Action::ScrollDown) if ui.screen == Screen::Diff => Flow::Continue {
                    redraw: ui.diff.scroll(3, usize::from(ui.height).max(1)),
                },
                Routed::Act(Action::ScrollUp) if ui.screen == Screen::Diff => Flow::Continue {
                    redraw: ui.diff.scroll(-3, usize::from(ui.height).max(1)),
                },
                Routed::Act(Action::OpenShell) => {
                    if ui.screen == Screen::Shell {
                        // `^g i` is the way out as well as the way in: `esc`
                        // belongs to the program inside the pty.
                        ui.screen = Screen::Dash;
                        // Back to whatever the dash was showing.
                        follow_selection(state, ui);
                        return Flow::Continue { redraw: true };
                    }
                    ui.screen = Screen::Shell;
                    match ui.scratch {
                        // It survives closing and reopening: the daemon kept
                        // the pty, so grove re-attaches rather than spawning
                        // a second shell in the same place.
                        Some(terminal) => {
                            let (rows, cols) = ui.shown_pty_size();
                            for request in ui.terminals.show(Some(terminal), rows, cols) {
                                if let Err(e) = send(state, &request) {
                                    ui.note = Some(format!("could not reach the daemon: {e}"));
                                    break;
                                }
                            }
                        }
                        None => {
                            // `cwd: None` means the daemon's own
                            // `config.scratch_cwd`, which is where this shell
                            // is supposed to open — not the worktree root, and
                            // not wherever grove was started.
                            let request =
                                Request::SpawnTerminal(grove_proto::TerminalTarget::Scratch {
                                    cwd: None,
                                });
                            if let Err(e) = send(state, &request) {
                                ui.note = Some(format!("could not reach the daemon: {e}"));
                            }
                        }
                    }
                    Flow::Continue { redraw: true }
                }
                Routed::Act(Action::OpenPicker) => Flow::Continue {
                    redraw: open_picker(state, ui),
                },
                Routed::Act(Action::MoveDown) if ui.screen == Screen::Picker => Flow::Continue {
                    redraw: ui.sessions.move_down(),
                },
                Routed::Act(Action::MoveUp) if ui.screen == Screen::Picker => Flow::Continue {
                    redraw: ui.sessions.move_up(),
                },
                Routed::Act(Action::Cancel) if ui.screen == Screen::Picker => {
                    ui.screen = Screen::Dash;
                    Flow::Continue { redraw: true }
                }
                Routed::Act(Action::Confirm) if ui.screen == Screen::Picker => {
                    let intent = ui.sessions.resume();
                    Flow::Continue {
                        redraw: act_on_session(intent, state, ui),
                    }
                }
                Routed::Act(Action::Detach) if ui.screen == Screen::Picker => {
                    let intent = ui.sessions.detach();
                    Flow::Continue {
                        redraw: act_on_session(intent, state, ui),
                    }
                }
                Routed::Act(Action::Close) if ui.screen == Screen::Picker => {
                    let intent = ui.sessions.close();
                    Flow::Continue {
                        redraw: act_on_session(intent, state, ui),
                    }
                }
                Routed::Act(Action::EndSession) if ui.screen == Screen::Picker => {
                    let intent = ui.sessions.end();
                    Flow::Continue {
                        redraw: act_on_session(intent, state, ui),
                    }
                }
                Routed::Act(Action::OpenPalette) => {
                    ui.screen = Screen::Palette;
                    ui.palette.open();
                    Flow::Continue { redraw: true }
                }
                Routed::Act(Action::MoveDown) if ui.screen == Screen::Palette => Flow::Continue {
                    redraw: match ui.palette.select_mut() {
                        Some(select) => select.move_down(),
                        None => ui.palette.move_down(),
                    },
                },
                Routed::Act(Action::MoveUp) if ui.screen == Screen::Palette => Flow::Continue {
                    redraw: match ui.palette.select_mut() {
                        Some(select) => select.move_up(),
                        None => ui.palette.move_up(),
                    },
                },
                Routed::Act(Action::Toggle) if ui.screen == Screen::Prune => Flow::Continue {
                    redraw: ui.prune.toggle(),
                },
                Routed::Act(Action::SelectSafe) if ui.screen == Screen::Prune => Flow::Continue {
                    redraw: ui.prune.select_safe(),
                },
                Routed::Act(Action::MoveDown) if ui.screen == Screen::Prune => Flow::Continue {
                    redraw: ui.prune.move_down(),
                },
                Routed::Act(Action::MoveUp) if ui.screen == Screen::Prune => Flow::Continue {
                    redraw: ui.prune.move_up(),
                },
                Routed::Act(Action::Cancel) if ui.screen == Screen::Prune => {
                    // Nothing has been removed, so there is nothing to undo.
                    ui.screen = Screen::Dash;
                    Flow::Continue { redraw: true }
                }
                Routed::Act(Action::Confirm) if ui.screen == Screen::Prune => {
                    let worktrees: Vec<grove_proto::WorktreeRef> = ui
                        .prune
                        .checked()
                        .iter()
                        .map(|row| row.candidate.worktree.clone())
                        .collect();
                    if worktrees.is_empty() {
                        ui.note = Some("nothing checked — space toggles a row".into());
                        return Flow::Continue { redraw: true };
                    }
                    if let Err(e) = send(state, &Request::Prune(worktrees)) {
                        ui.note = Some(format!("could not reach the daemon: {e}"));
                    }
                    // The screen stays until the daemon says what happened:
                    // this is the destructive one, and closing on send would
                    // show success before there is any.
                    Flow::Continue { redraw: true }
                }
                Routed::Act(Action::Toggle) if ui.screen == Screen::Palette => Flow::Continue {
                    redraw: match ui.palette.select_mut() {
                        Some(select) => select.toggle(),
                        // No picker open, so space is just a character — a
                        // session name can contain one.
                        None => {
                            ui.palette.push(' ');
                            true
                        }
                    },
                },
                Routed::Act(Action::Erase) if ui.screen == Screen::Palette => Flow::Continue {
                    redraw: ui.palette.backspace(),
                },
                Routed::Act(Action::Cancel) if ui.screen == Screen::Palette => {
                    // From an argument, `esc` steps back to the command list
                    // rather than closing: the user is one keystroke from what
                    // they wanted. From the list it closes, with nothing to
                    // undo because nothing has run.
                    if !ui.palette.unargue() {
                        ui.screen = Screen::Dash;
                    }
                    Flow::Continue { redraw: true }
                }
                Routed::Act(Action::Confirm) if ui.screen == Screen::Palette => {
                    let chosen = ui.palette.selected();
                    Flow::Continue {
                        redraw: run_command(chosen, state, ui),
                    }
                }
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
                // The pane is a real terminal: keys go to the program, not to
                // grove. `^g` is how grove is addressed instead, which the
                // router has already applied by the time this arrives.
                Routed::ToPty(key) => {
                    let Some(bytes) = terminals::encode_key(key) else {
                        return Flow::Continue { redraw: false };
                    };
                    let Some(request) = ui.terminals.input(bytes) else {
                        return Flow::Continue { redraw: false };
                    };
                    if let Err(e) = send(state, &request) {
                        ui.note = Some(format!("could not reach the daemon: {e}"));
                        return Flow::Continue { redraw: true };
                    }
                    // The grid changes when the program answers, not when the
                    // key is sent.
                    Flow::Continue { redraw: false }
                }
                // Half a chord: the status bar shows it, so redraw.
                Routed::PrefixPending => Flow::Continue { redraw: true },
                Routed::Unbound => Flow::Continue { redraw: true },
                // Consumed by the palette in #23; routed correctly now.
                Routed::Text(c) if ui.screen == Screen::Palette => {
                    ui.palette.push(c);
                    Flow::Continue { redraw: true }
                }
                Routed::Text(_) => Flow::Continue { redraw: false },
                Routed::Ignored => Flow::Continue { redraw: false },
            }
        }

        Input::Terminal(TermEvent::Resize(width, height)) => {
            ui.width = width;
            ui.height = height;
            ui.remember_pane_size();
            let (rows, cols) = ui.shown_pty_size();
            if let Some(request) = ui.terminals.resize(rows, cols)
                && let Err(e) = send(state, &request)
            {
                ui.note = Some(format!("could not reach the daemon: {e}"));
            }
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
        Input::Terminal(TermEvent::Mouse(event)) => on_mouse(event, state, ui),
        Input::Terminal(_) => Flow::Continue { redraw: false },

        // The REPOS pane's data. Kept ahead of the catch-all below because
        // that one only narrates the event on the status line, which for a
        // list of repos would say a lot and show nothing.
        // One session's state moved. Ownership requests name the open session,
        // so a stale answer here adopts into the wrong one — which is worse
        // than the status line this event used to produce.
        Input::Daemon(DaemonEvent::SessionChanged(row)) => {
            // Keep the row, not just the id: the bar names the open session,
            // the picker lists it, and both need more than an id to do it.
            ui.sessions.upsert(row.clone());
            match row.state {
                grove_domain::SessionState::Attached => ui.session = Some(row.id.clone()),
                // The session we were in stopped being open. Forgetting it is
                // the point: adopt has nothing to adopt into until the daemon
                // names another.
                _ if ui.session.as_ref() == Some(&row.id) => ui.session = None,
                _ => {}
            }
            Flow::Continue { redraw: true }
        }

        // Which session is open decides what adopt and release name. The
        // daemon is the authority on it; the TUI only remembers the answer.
        Input::Daemon(DaemonEvent::Sessions(rows)) => {
            ui.session = rows
                .iter()
                .find(|row| row.state == grove_domain::SessionState::Attached)
                .map(|row| row.id.clone());
            ui.sessions.set(rows);
            Flow::Continue { redraw: true }
        }

        // The pty `^g enter` asked for. Without this the terminal exists and
        // the pane still says there is none, until something else happens to
        // refresh the worktree rows.
        // The branch is already checked out somewhere. Refusing and stopping
        // there would leave the user to go and find it; §7 says offer to take
        // it over instead, and `^g a` is already the key for adopting.
        Input::Daemon(DaemonEvent::BranchCheckedOutElsewhere {
            branch,
            existing,
            existing_path,
            ..
        }) => {
            ui.note = Some(format!(
                "{branch} is already checked out at {} — ^g a adopts it",
                existing_path.display()
            ));
            ui.conflict = Some((existing, existing_path));
            // The palette has nothing left to do, and leaving it open over the
            // message would hide the worktree the offer is about.
            ui.screen = Screen::Dash;
            Flow::Continue { redraw: true }
        }

        // The answer to `prune`. Opening on this rather than on the keystroke
        // means the screen is never a blank list that fills in underneath a
        // user already pressing `a`.
        Input::Daemon(DaemonEvent::PruneCandidates(candidates)) => {
            ui.prune.set(candidates);
            ui.screen = Screen::Prune;
            Flow::Continue { redraw: true }
        }

        // What prune actually did. `failed` is per row, because a prune that
        // removed four of five and said "ok" would be a lie about the fifth.
        Input::Daemon(DaemonEvent::Pruned {
            removed,
            failed,
            reclaimed,
        }) => {
            ui.screen = Screen::Dash;
            ui.note = Some(if failed.is_empty() {
                format!("pruned {} · {} reclaimed", removed.len(), bytes(reclaimed))
            } else {
                let (worktree, why) = &failed[0];
                format!(
                    "pruned {} of {}; {} failed: {why}",
                    removed.len(),
                    removed.len() + failed.len(),
                    worktree.branch
                )
            });
            Flow::Continue { redraw: true }
        }

        // The scratch shell grove asked for. Remembered so reopening the
        // overlay re-attaches rather than spawning another shell beside it.
        Input::Daemon(DaemonEvent::TerminalSpawned {
            target: grove_proto::TerminalTarget::Scratch { .. },
            terminal,
        }) => {
            ui.scratch = Some(terminal);
            let (rows, cols) = ui.shown_pty_size();
            for request in ui.terminals.show(Some(terminal), rows, cols) {
                if let Err(e) = send(state, &request) {
                    ui.note = Some(format!("could not reach the daemon: {e}"));
                    break;
                }
            }
            Flow::Continue { redraw: true }
        }

        Input::Daemon(DaemonEvent::TerminalSpawned { terminal, .. }) => {
            let (rows, cols) = ui.terminal_pane_size();
            for request in ui.terminals.show(Some(terminal), rows, cols) {
                if let Err(e) = send(state, &request) {
                    ui.note = Some(format!("could not reach the daemon: {e}"));
                    break;
                }
            }
            // The row still says it has no terminal, and that is the daemon's
            // to correct — ask rather than edit the row here.
            request_worktrees(state, ui);
            Flow::Continue { redraw: true }
        }

        Input::Daemon(DaemonEvent::TerminalScreen { terminal, screen }) => {
            ui.terminals.screen(terminal, &screen);
            Flow::Continue { redraw: true }
        }

        Input::Daemon(DaemonEvent::TerminalScrollback {
            terminal,
            seq,
            lines,
            done,
        }) => {
            ui.terminals.scrollback(terminal, seq, &lines, done);
            // History lands behind the grid, so nothing on screen moves until
            // the user scrolls into it.
            Flow::Continue { redraw: false }
        }

        Input::Daemon(DaemonEvent::TerminalOutput { terminal, bytes }) => {
            ui.terminals.output(terminal, &bytes);
            // Only the pane on screen is worth a frame; the others are kept
            // current in their own grids for when they are shown again.
            Flow::Continue {
                redraw: ui.terminals.showing() == Some(terminal),
            }
        }

        Input::Daemon(DaemonEvent::TerminalExited { terminal, .. }) => {
            ui.terminals.exited(terminal);
            if ui.scratch == Some(terminal) {
                // Forgotten here as well as in `Terminals`, or `^g i` would
                // re-attach to a dead id for the rest of the session instead
                // of opening a new shell.
                ui.scratch = None;
            }
            Flow::Continue { redraw: true }
        }

        Input::Daemon(DaemonEvent::Diff {
            worktree,
            base,
            files,
            selected,
            hunks,
            added,
            removed,
        }) => {
            ui.diff.set(diff::Incoming {
                repo: worktree.repo.0.clone(),
                branch: worktree.branch.clone(),
                base,
                files,
                selected,
                hunks,
                added,
                removed,
            });
            Flow::Continue { redraw: true }
        }

        Input::Daemon(DaemonEvent::Repos(rows)) => {
            ui.repos.set(rows);
            // The WORKTREES pane follows this selection, and the daemon sends
            // worktrees only when asked for a repo by name.
            request_worktrees(state, ui);
            Flow::Continue { redraw: true }
        }

        // Rows for one repo. The cursor may have moved since the request went
        // out, so rows for a repo we are no longer showing are dropped rather
        // than painted under the wrong heading.
        Input::Daemon(DaemonEvent::Worktrees { repo, rows }) => {
            // While the confirm is counting, these rows are its answer rather
            // than the dash's — and it wants every member repo, not only the
            // one the cursor is on.
            if ui.screen == Screen::EndSession {
                ui.ending.take(&repo, &rows, ui.session.as_ref());
                return Flow::Continue { redraw: true };
            }
            let showing = ui.repos.selected().map(|row| row.repo.clone());
            if showing.as_ref() != Some(&repo) {
                return Flow::Continue { redraw: false };
            }
            ui.worktrees.set(rows);
            refresh_user_columns(ui);
            follow_selection(state, ui);
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
        DaemonEvent::Welcome {
            version,
            ownership_movable,
        } => format!("connected, protocol v{version}, movable {ownership_movable}"),
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
/// How long the TUI waits for a daemon it started to take the socket.
///
/// Generous, because the wait is once per workspace per boot and a machine
/// under load is not a broken one. `groved` binds before it walks the
/// workspace, so in practice this is a few milliseconds even for a directory
/// full of repositories.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);

/// The daemon binary to start, given where this grove is and what the
/// environment asks for.
///
/// Beside grove first. The installer puts the pair in one directory, and the
/// two halves speak a versioned protocol to each other — a `$PATH` that finds
/// some other `groved` would pair an upgraded TUI with a stale daemon, which
/// fails later and less clearly than this. `GROVE_DAEMON` overrides
/// everything, which is also what lets this be tested without a real daemon.
fn daemon_binary(current_exe: Option<&Path>, explicit: Option<OsString>) -> PathBuf {
    if let Some(explicit) = explicit {
        return PathBuf::from(explicit);
    }
    if let Some(beside) = current_exe
        .and_then(Path::parent)
        .map(|dir| dir.join("groved"))
        .filter(|beside| beside.is_file())
    {
        return beside;
    }
    // Nothing beside grove — a `cargo run` or a half-installed pair. Let exec
    // search $PATH and report what it finds, or does not.
    PathBuf::from("groved")
}

/// Start the daemon and wait for it to take the socket.
///
/// grove does not link the daemon: that boundary is what the whole crate graph
/// is arranged around, and `scripts/check-boundaries.sh` enforces it. Starting
/// the *process* is not a breach of it. Nothing about git or a pty enters this
/// address space, the two halves still meet over the socket and nothing else,
/// and the daemon still outlives the TUI that started it.
fn start_daemon(binary: &Path, workspace: &Path, socket: &Path) -> Result<UnixStream, String> {
    let log = socket.with_extension("log");
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // The daemon outlives this process, so its output cannot go to a terminal
    // that will be gone — and it certainly cannot go to one about to enter raw
    // mode. A file beside the socket has exactly the right lifetime: both are
    // per-workspace, and both go when the machine reboots.
    let out = std::fs::File::create(&log).map_err(|e| {
        format!(
            "could not open {} for the daemon's output: {e}",
            log.display()
        )
    })?;
    let errs = out
        .try_clone()
        .map_err(|e| format!("could not open {} twice: {e}", log.display()))?;

    let mut child = Command::new(binary)
        .arg("--workspace")
        .arg(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(errs))
        // Its own process group. ^C in the terminal running the TUI goes to
        // the foreground group, and the daemon holding every session's shells
        // must not be in it — quitting grove is not meant to kill your work.
        .process_group(0)
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", binary.display()))?;

    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    loop {
        if let Ok(stream) = UnixStream::connect(socket) {
            return Ok(stream);
        }
        // A daemon that has already exited will never bind. It may have exited
        // precisely because another grove won the race and it found a live
        // socket, so the socket is tried once more before the log is believed.
        if let Ok(Some(status)) = child.try_wait() {
            if let Ok(stream) = UnixStream::connect(socket) {
                return Ok(stream);
            }
            return Err(match last_words(&log) {
                Some(said) => format!("the daemon exited ({status}): {said}"),
                None => format!(
                    "the daemon exited ({status}) without saying why; see {}",
                    log.display()
                ),
            });
        }
        if Instant::now() >= deadline {
            // Deliberately not killed: it may be a slow start rather than a
            // stuck one, and killing it would turn "wait a moment and try
            // again" into "your daemon keeps dying".
            return Err(format!(
                "the daemon has not taken {} after {}s; it may still be starting — see {}",
                socket.display(),
                DAEMON_START_TIMEOUT.as_secs(),
                log.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The tail of the daemon's log, for an error message that says why.
///
/// Bounded, because this ends up in a one-line note under a dashboard: a
/// daemon that failed after printing a megabyte must not take the screen.
fn last_words(log: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log).ok()?;
    let said = text
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("; ");
    let said = said.trim();
    if said.is_empty() {
        return None;
    }
    Some(said.chars().take(400).collect())
}

/// Reach the daemon for this workspace, starting one if nobody has.
///
/// Typing `grove` is a reasonable way to say "run grove". Making it a
/// two-command ritual — start the daemon, then the TUI — is a thing the
/// program knows how to do for you, so it does. Setting `GROVE_NO_AUTOSTART`
/// keeps the old behaviour, for anyone running the daemon under a supervisor
/// that would rather grove did not.
fn reach_daemon(workspace: &Path, socket: &Path) -> Result<UnixStream, String> {
    let refused = match UnixStream::connect(socket) {
        Ok(stream) => return Ok(stream),
        Err(e) => e,
    };
    if std::env::var_os("GROVE_NO_AUTOSTART").is_some_and(|v| !v.is_empty()) {
        return Err(format!(
            "no daemon at {}: {refused} (GROVE_NO_AUTOSTART is set, so grove did not start one)",
            socket.display()
        ));
    }
    let binary = daemon_binary(
        std::env::current_exe().ok().as_deref(),
        std::env::var_os("GROVE_DAEMON"),
    );
    // Before the terminal is taken, and the only line grove prints on a normal
    // start: a first run builds nothing but does have to walk the workspace,
    // and a silent pause looks like a hang.
    eprintln!("grove: starting {}", binary.display());
    start_daemon(&binary, workspace, socket)
}

fn connect(workspace: &Path, inputs: &Inputs) -> State {
    let path = socket_path(workspace);

    let stream = match reach_daemon(workspace, &path) {
        Ok(s) => s,
        Err(reason) => {
            return State::Disconnected {
                workspace: workspace.to_path_buf(),
                reason,
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
            Handshake::Agreed {
                ownership_movable: _,
            } => {
                // The capability reaches the pane in #20; the connection
                // carries it and nothing here consumes it yet.
                events::spawn_daemon_reader(reader, inputs.sender());
                // The daemon answers questions; it does not volunteer the
                // dash. Without these two the panes stay empty forever, which
                // is exactly how this shipped until review caught it.
                let mut daemon = write_half;
                match ask_for_the_dash(&mut daemon) {
                    Ok(()) => State::Connected {
                        workspace: workspace.to_path_buf(),
                        note: "connected".into(),
                        // Kept so the TUI can ask for things rather than only
                        // listen. Everything grove does to a repo is a request
                        // on this half.
                        daemon: Some(daemon),
                    },
                    Err(e) => State::Disconnected {
                        workspace: workspace.to_path_buf(),
                        reason: format!("could not ask the daemon for the dash: {e}"),
                    },
                }
            }
            Handshake::Mismatch { daemon, client } => State::Disconnected {
                workspace: workspace.to_path_buf(),
                reason: format!(
                    "protocol mismatch: the daemon speaks v{daemon}, this grove speaks v{client}; \
                     the daemon outlives the TUI, so an upgraded grove meets an old groved — \
                     stop that daemon for this workspace and start again"
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
    draw_screen(f, state, ui);
    if let Some(selection) = &ui.selection {
        selection.highlight(f.buffer_mut());
    }
}

/// What a selection is read from: the frame, drawn again off screen exactly
/// as it was drawn on it, so what is copied is what was highlighted.
fn selected_text(state: &State, ui: &Ui, selection: &selection::Selection) -> String {
    // A test backend cannot fail to start: its error type is uninhabited.
    let Ok(mut offscreen) =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(ui.width, ui.height));
    if offscreen.draw(|f| draw_screen(f, state, ui)).is_err() {
        return String::new();
    }
    selection.text(offscreen.backend().buffer())
}

fn draw_screen(f: &mut ratatui::Frame, state: &State, ui: &Ui) {
    // Two rows at the bottom: the status bar, and a blank one above it. The
    // mock sets the bar off from the panes with `margin-top:12px` rather than
    // butting it against them, and a row is what that is here.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());
    let (body_area, bar_area) = (chunks[0], chunks[2]);

    // Help comes before every screen's own branch: it explains whichever one
    // is underneath, so it has to win the draw whatever that screen is.
    if let Some(explaining) = ui.helping
        && matches!(state, State::Connected { .. })
    {
        let user = ui.keys.spellings();
        ui.help
            .render(f.buffer_mut(), body_area, explaining, &user, &ui.theme);
        status_bar(f, bar_area, ui);
        return;
    }

    // The dash is a frame of panes rather than a paragraph, so it takes the
    // body whole. Everything the shell says about its own state — no daemon,
    // no connection yet — still goes through the block below, because those
    // are not screens and #22's empty state is about a workspace with nothing
    // in it, not about a daemon grove cannot reach.
    if matches!(state, State::Connected { .. }) && ui.screen == Screen::Dash {
        draw_dash(f, body_area, ui);
        status_bar(f, bar_area, ui);
        return;
    }

    if matches!(state, State::Connected { .. }) && ui.screen == Screen::Diff {
        draw_dash(f, body_area, ui);
        let parts = overlay(f, body_area, ui, ui.theme.style(Role::Accent));
        ui.diff.render(f.buffer_mut(), parts, &ui.theme);
        status_bar(f, bar_area, ui);
        return;
    }

    if matches!(state, State::Connected { .. }) && ui.screen == Screen::Shell {
        // An overlay like the rest, not a full-screen box: §4.5 gives the
        // scratch shell the screen rather than a corner of one, and the mock
        // does that by making it the tallest of the overlays rather than by
        // taking the frame.
        draw_dash(f, body_area, ui);
        let parts = overlay(f, body_area, ui, ui.theme.style(Role::Accent));
        // The mock heads it with what it is and where it is: a shell, attached
        // to nothing, in the directory the daemon started it in.
        f.render_widget(
            Paragraph::new(overlay::header(
                "shell",
                "scratch · not attached to a worktree",
                "",
                parts.header.width,
                &ui.theme,
            )),
            parts.header,
        );
        // §3.1's rule, said out loud on the one screen where it bites: every
        // key here goes to the shell, so the way back has to be written down.
        // The mock says it in the same place the picker says its warning.
        let note = Rect {
            y: parts.body.bottom().saturating_sub(1),
            height: 1,
            ..parts.body
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("keys go to the shell — ", ui.theme.ink_style(Ink::Faint)),
                Span::styled("^g", ui.theme.style(Role::Accent)),
                Span::styled(" returns to grove", ui.theme.ink_style(Ink::Faint)),
            ])),
            note,
        );
        ui.terminals.render(
            f.buffer_mut(),
            shell_pty_area(parts.body),
            &ui.theme,
            ui.scratch.is_some(),
        );
        status_bar(f, bar_area, ui);
        return;
    }

    if matches!(state, State::Connected { .. }) && ui.screen == Screen::EndSession {
        draw_dash(f, body_area, ui);
        // The one overlay that is not accent-bordered: it is about to remove
        // work, and the mock gives it the error colour for exactly that
        // reason — the border is the warning, before a word is read.
        let parts = overlay(f, body_area, ui, ui.theme.style(Role::Error));
        ui.ending.render(f.buffer_mut(), parts, &ui.theme);
        status_bar(f, bar_area, ui);
        return;
    }

    if matches!(state, State::Connected { .. }) && ui.screen == Screen::Picker {
        draw_dash(f, body_area, ui);
        let parts = overlay(f, body_area, ui, ui.theme.style(Role::Accent));
        ui.sessions.render(f.buffer_mut(), parts, &ui.theme);
        status_bar(f, bar_area, ui);
        return;
    }

    if matches!(state, State::Connected { .. }) && ui.screen == Screen::Prune {
        draw_dash(f, body_area, ui);
        let parts = overlay(f, body_area, ui, ui.theme.style(Role::Accent));
        ui.prune.render(f.buffer_mut(), parts, &ui.theme);
        status_bar(f, bar_area, ui);
        return;
    }

    if matches!(state, State::Connected { .. }) && ui.screen == Screen::Palette {
        draw_dash(f, body_area, ui);
        // Over the dash rather than beside it: §4.2 calls it grove's command
        // line, and a command line that moves the screen under it makes the
        // thing you were looking at harder to act on.
        let parts = overlay(f, body_area, ui, ui.theme.style(Role::Accent));
        ui.palette.render(f.buffer_mut(), parts, &ui.theme);
        status_bar(f, bar_area, ui);
        return;
    }

    let (title, body) = match state {
        State::Connected {
            workspace, note, ..
        } => (
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

/// Bytes, for the line that says how much prune reclaimed.
fn bytes(n: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    match n {
        n if n >= GB => format!("{:.1} GB", n as f64 / GB as f64),
        n if n >= MB => format!("{} MB", n / MB),
        n => format!("{} KB", (n / 1024).max(1)),
    }
}

/// Frame an overlay that owns the body, and hand back the inside.
///
/// The palette, the pickers and the confirm are all lists that need width:
/// drawing them into the WORKTREES pane fitted them into twenty-odd columns
/// and truncated every summary, which is how the palette shipped until a test
/// painted it at a real size.
/// Rows for an overlay that wants to be tall rather than to fit its contents.
///
/// The diff and the scratch shell are the two whose contents are unbounded —
/// a patch and a pty — so the mock gives them a fixed box, three-quarters of
/// the window, rather than `fit-content`. The rest stays visible around it,
/// which is the point of their being overlays at all.
fn tall(body: Rect) -> u16 {
    (body.height * 3 / 4).saturating_sub(6).max(1)
}

/// Draw the three panes and their contents.
///
/// Separate from `draw` because the overlays sit *over* it: the mock washes
/// the dash out behind a palette rather than replacing it, so the dash has to
/// be painted first and then dimmed.
fn draw_dash(f: &mut ratatui::Frame, body_area: Rect, ui: &Ui) {
    let empty = empty::Empty::of(
        ui.repos.workspace_count(),
        ui.repos.member_count(),
        ui.session.is_some(),
    );
    // §4.1's empty state draws two panes, not three: nothing is selected,
    // so there is no terminal to show, and the guidance needs the width
    // more than an empty box does. The user's own toggles are untouched —
    // this is what is drawn, not what they asked for. The mouse asks the same
    // question, so the answer lives on `Ui`.
    let panes = ui.drawn_panes();
    let selection = ui.selection();
    let inner = dash::render(
        f.buffer_mut(),
        body_area,
        panes,
        ui.focus,
        &ui.theme,
        &selection,
    );
    if let Some(area) = inner.repos {
        ui.repos.render(
            f.buffer_mut(),
            area,
            &ui.theme,
            ui.focus == Focus::Repos,
            ui.session.is_some(),
        );
    }
    if let Some(area) = inner.worktrees {
        // A dash with nothing in it re-homes §4.1's numbered guidance
        // here, rather than rendering three blank boxes and leaving the
        // user to guess which key starts anything.
        match empty {
            Some(state) => {
                f.render_widget(Paragraph::new(state.lines(&ui.theme)), area);
            }
            None => {
                let user = ui.columns.live();
                ui.worktrees.render(
                    f.buffer_mut(),
                    area,
                    &ui.theme,
                    ui.focus == Focus::Worktrees,
                    &user,
                );
            }
        }
    }
    if let Some(area) = inner.terminal {
        let has_terminal = ui
            .worktrees
            .selected()
            .is_some_and(|row| row.terminal.is_some());
        ui.terminals
            .render(f.buffer_mut(), area, &ui.theme, has_terminal);
    }
}

fn overlay(
    f: &mut ratatui::Frame,
    area: Rect,
    ui: &Ui,
    border: ratatui::style::Style,
) -> overlay::Parts {
    let (rows, hints) = ui
        .overlay_shape(area)
        .expect("an overlay is only drawn while its screen is open");
    overlay::render(f.buffer_mut(), area, border, rows, hints, &ui.theme)
}

impl Ui {
    /// The open session, as the mock's bar names it.
    ///
    /// The id is the fallback rather than the answer: it is what the daemon
    /// told us we are in, and a session usually has a name the user chose.
    fn session_name(&self) -> String {
        let Some(open) = self.session.as_ref() else {
            return "no session".to_string();
        };
        match self.sessions.by_id(open) {
            Some(row) => format!("session: {}", row.name),
            None => format!("session: {}", open.0),
        }
    }

    /// What the terminal pane is showing, as the mock heads it: `repo · branch`.
    ///
    /// The same string the status bar puts on its right, because they are
    /// answering the same question and two ways of phrasing it would drift.
    fn selection(&self) -> String {
        match (self.repos.selected(), self.worktrees.selected()) {
            (Some(repo), Some(worktree)) => format!("{} · {}", repo.name, worktree.worktree.branch),
            // A repo with the cursor on it but nothing under that cursor: the
            // pane is honest about which half it has.
            (Some(repo), None) => format!("{} · no worktrees", repo.name),
            _ => "no worktree selected".to_string(),
        }
    }
}

/// The one row along the bottom, drawn the same way whatever is above it.
fn status_bar(f: &mut ratatui::Frame, area: Rect, ui: &Ui) {
    // A pending prefix outranks the selection: it is about the key just
    // pressed and it disappears on the next one, while the selection is still
    // there in the pane header above.
    let context = if ui.router.prefix_pending() {
        "^g …".to_string()
    } else {
        ui.selection()
    };
    // A refusal outranks the config note: it is about the key just pressed,
    // and the config note has been true since startup.
    let note = ui
        .note
        .as_deref()
        .map(statusbar::Note::Refused)
        .or_else(|| ui.config_note.as_deref().map(statusbar::Note::Config));
    f.render_widget(
        Paragraph::new(statusbar::render(
            ui.screen,
            &context,
            &ui.session_name(),
            area.width,
            &ui.theme,
            note,
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
            daemon: None,
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
    fn an_arrow_in_an_overlay_leaves_the_dash_lists_alone() {
        // End to end through `handle`, because the gate is only worth having
        // where the key actually arrives: with the picker open, focus still
        // says REPOS, and the list must not move behind it.
        let mut s = connected();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("a", 1), repo_row("b", 1)])),
            &mut s,
            &mut ui,
        );
        ui.focus = Focus::Repos;
        ui.screen = Screen::Prune;

        handle(key(KeyCode::Down), &mut s, &mut ui);
        assert_eq!(
            ui.repos.selected().map(|r| r.name.as_str()),
            Some("a"),
            "an overlay owns the keyboard; the list behind it must not scroll"
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

    fn worktree_row(branch: &str, ownership: grove_domain::Ownership) -> grove_proto::WorktreeRow {
        grove_proto::WorktreeRow {
            worktree: grove_proto::WorktreeRef {
                repo: grove_domain::RepoId("repo".into()),
                branch: branch.into(),
            },
            detached: false,
            ownership,
            ahead: 0,
            behind: 0,
            dirty_files: 0,
            age: 0,
            size: 0,
            terminal: None,
            foreground: None,
            stale: false,
        }
    }

    #[test]
    fn a_refused_adopt_says_why_on_the_status_bar() {
        // A key that silently does nothing reads as grove being broken. The
        // clone can never be adopted, and the user has to learn that from the
        // key rather than from the spec.
        let mut s = connected();
        let mut ui = Ui::new();
        // The pane only takes rows for the repo it is showing, so the
        // selection has to exist before they arrive.
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("repo", 1)])),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("repo".into()),
                rows: vec![worktree_row("main", grove_domain::Ownership::Clone)],
            }),
            &mut s,
            &mut ui,
        );
        ui.focus = Focus::Worktrees;

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('a')), &mut s, &mut ui);
        let note = ui.note.expect("a refusal must be reported");
        assert!(note.contains("clone"), "{note}");
    }

    #[test]
    fn adopting_without_an_open_session_says_so_rather_than_silently_failing() {
        // Ownership belongs to a session, so with none open there is nothing
        // to adopt *into*.
        let mut s = connected();
        let mut ui = Ui::new();
        // The pane only takes rows for the repo it is showing, so the
        // selection has to exist before they arrive.
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("repo", 1)])),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("repo".into()),
                rows: vec![worktree_row("free", grove_domain::Ownership::Unowned)],
            }),
            &mut s,
            &mut ui,
        );
        ui.focus = Focus::Worktrees;
        assert!(ui.session.is_none());

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('a')), &mut s, &mut ui);
        let note = ui.note.expect("must say why nothing happened");
        assert!(note.contains("session"), "{note}");
    }

    #[test]
    fn the_open_session_comes_from_the_daemon() {
        let mut s = connected();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Sessions(vec![
                grove_proto::SessionRow {
                    id: grove_domain::SessionId("closed-one".into()),
                    name: "old".into(),
                    members: vec![],
                    state: grove_domain::SessionState::Closed,
                    terminals: 0,
                    since: 0,
                    size: 0,
                },
                grove_proto::SessionRow {
                    id: grove_domain::SessionId("open-one".into()),
                    name: "current".into(),
                    members: vec![],
                    state: grove_domain::SessionState::Attached,
                    terminals: 2,
                    since: 0,
                    size: 0,
                },
            ])),
            &mut s,
            &mut ui,
        );
        assert_eq!(
            ui.session.as_ref().map(|s| s.0.as_str()),
            Some("open-one"),
            "the attached session is the open one"
        );
    }

    #[test]
    fn arrows_drive_the_worktrees_list_when_it_has_focus() {
        let mut s = connected();
        let mut ui = Ui::new();
        // The pane only takes rows for the repo it is showing, so the
        // selection has to exist before they arrive.
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("repo", 1)])),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("repo".into()),
                rows: vec![
                    worktree_row("a", grove_domain::Ownership::Ours),
                    worktree_row("b", grove_domain::Ownership::Ours),
                ],
            }),
            &mut s,
            &mut ui,
        );
        ui.focus = Focus::Worktrees;
        handle(key(KeyCode::Down), &mut s, &mut ui);
        assert_eq!(
            ui.worktrees.selected().map(|r| r.worktree.branch.as_str()),
            Some("b")
        );

        ui.focus = Focus::Repos;
        handle(key(KeyCode::Up), &mut s, &mut ui);
        assert_eq!(
            ui.worktrees.selected().map(|r| r.worktree.branch.as_str()),
            Some("b"),
            "an arrow in another pane must not move this list"
        );
    }

    /// A connected state whose daemon half is a socket the test can read.
    fn wired() -> (State, UnixStream) {
        let (ours, theirs) = UnixStream::pair().expect("a socket pair");
        (
            State::Connected {
                workspace: PathBuf::from("/w"),
                note: "connected".into(),
                daemon: Some(ours),
            },
            theirs,
        )
    }

    /// Everything the TUI has sent so far.
    fn sent(socket: &mut UnixStream) -> Vec<Request> {
        socket
            .set_nonblocking(true)
            .expect("a socket that can be drained");
        let mut out = Vec::new();
        while let Ok(request) = grove_proto::read_frame::<_, Request>(socket) {
            out.push(request);
        }
        out
    }

    #[test]
    fn the_dash_asks_for_its_own_data() {
        // The review's high: the panes filled only from injected events, so a
        // real grove drew an empty dash forever. The daemon answers questions
        // and volunteers nothing.
        let (_ours, mut theirs) = UnixStream::pair().expect("a socket pair");
        let mut writer = _ours;
        ask_for_the_dash(&mut writer).expect("the requests go out");
        let asked = sent(&mut theirs);
        assert!(
            asked.contains(&Request::ListSessions),
            "the open session decides what adopt names: {asked:?}"
        );
        assert!(
            asked.contains(&Request::ListRepos),
            "the REPOS pane has nothing without this: {asked:?}"
        );
    }

    #[test]
    fn moving_the_repo_cursor_asks_for_that_repos_worktrees() {
        // The WORKTREES pane follows the REPOS selection, and the daemon sends
        // worktrees for a named repo rather than all of them at once.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("a", 1), repo_row("b", 1)])),
            &mut s,
            &mut ui,
        );
        let first = sent(&mut theirs);
        assert!(
            first.contains(&Request::ListWorktrees(grove_domain::RepoId("a".into()))),
            "the first repo's worktrees are asked for as soon as there is a selection: {first:?}"
        );

        ui.focus = Focus::Repos;
        handle(key(KeyCode::Down), &mut s, &mut ui);
        let second = sent(&mut theirs);
        assert!(
            second.contains(&Request::ListWorktrees(grove_domain::RepoId("b".into()))),
            "moving the cursor must ask about the repo now under it: {second:?}"
        );
    }

    #[test]
    fn worktrees_for_a_repo_we_are_no_longer_showing_are_dropped() {
        // The cursor can move between the request going out and the rows
        // coming back. Painting them would label one repo's worktrees with
        // another's name.
        let (mut s, _theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("a", 1)])),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("somewhere-else".into()),
                rows: vec![worktree_row("stale", grove_domain::Ownership::Ours)],
            }),
            &mut s,
            &mut ui,
        );
        assert!(
            ui.worktrees.selected().is_none(),
            "rows for another repo must not be painted under this one"
        );
    }

    #[test]
    fn a_session_that_stops_being_open_is_forgotten() {
        // Ownership requests name the open session, so a stale answer adopts
        // into the wrong one.
        let mut s = connected();
        let mut ui = Ui::new();
        let mut row = grove_proto::SessionRow {
            id: grove_domain::SessionId("s1".into()),
            name: "one".into(),
            members: vec![],
            state: grove_domain::SessionState::Attached,
            terminals: 0,
            since: 0,
            size: 0,
        };
        handle(
            Input::Daemon(DaemonEvent::SessionChanged(row.clone())),
            &mut s,
            &mut ui,
        );
        assert_eq!(ui.session.as_ref().map(|s| s.0.as_str()), Some("s1"));

        row.state = grove_domain::SessionState::Detached;
        handle(
            Input::Daemon(DaemonEvent::SessionChanged(row)),
            &mut s,
            &mut ui,
        );
        assert!(
            ui.session.is_none(),
            "adopt has nothing to adopt into once the session is not open"
        );
    }

    #[test]
    fn adopting_sends_the_request_and_asks_again_for_the_rows() {
        // Ownership is the daemon's to write, so the pane does not change
        // here; asking again is what makes the answer arrive.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("repo", 1)])),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("repo".into()),
                rows: vec![worktree_row("free", grove_domain::Ownership::Unowned)],
            }),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Sessions(vec![grove_proto::SessionRow {
                id: grove_domain::SessionId("open".into()),
                name: "current".into(),
                members: vec![],
                state: grove_domain::SessionState::Attached,
                terminals: 0,
                since: 0,
                size: 0,
            }])),
            &mut s,
            &mut ui,
        );
        let _ = sent(&mut theirs);

        ui.focus = Focus::Worktrees;
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('a')), &mut s, &mut ui);

        let asked = sent(&mut theirs);
        assert!(
            asked
                .iter()
                .any(|r| matches!(r, Request::AdoptWorktree { .. })),
            "adopt must reach the daemon: {asked:?}"
        );
        assert!(
            asked.contains(&Request::ListWorktrees(grove_domain::RepoId("repo".into()))),
            "and the rows must be asked for again: {asked:?}"
        );
    }

    fn with_terminal(branch: &str, terminal: Option<u64>) -> grove_proto::WorktreeRow {
        let mut row = worktree_row(branch, grove_domain::Ownership::Ours);
        row.terminal = terminal.map(grove_proto::TerminalId);
        row
    }

    /// A dash showing one repo and two worktrees, one with a pty.
    fn dashed(s: &mut State, ui: &mut Ui) {
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("repo", 2)])),
            s,
            ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("repo".into()),
                rows: vec![with_terminal("a", Some(1)), with_terminal("b", Some(2))],
            }),
            s,
            ui,
        );
    }

    #[test]
    fn the_terminal_pane_follows_the_worktree_cursor() {
        // Acceptance: switching selection switches which pty is displayed,
        // without disturbing either — so a detach precedes the attach and the
        // processes are never touched.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        dashed(&mut s, &mut ui);
        let first = sent(&mut theirs);
        assert!(
            first
                .iter()
                .any(|r| matches!(r, Request::AttachTerminal(a) if a.terminal == grove_proto::TerminalId(1))),
            "the first worktree's terminal is attached: {first:?}"
        );

        ui.focus = Focus::Worktrees;
        handle(key(KeyCode::Down), &mut s, &mut ui);
        let second = sent(&mut theirs);
        assert!(
            second.iter().any(
                |r| matches!(r, Request::DetachTerminal(t) if *t == grove_proto::TerminalId(1))
            ),
            "the one we left is detached: {second:?}"
        );
        assert!(
            second
                .iter()
                .any(|r| matches!(r, Request::AttachTerminal(a) if a.terminal == grove_proto::TerminalId(2))),
            "and the one we arrived at is attached: {second:?}"
        );
    }

    #[test]
    fn keys_reach_the_pty_when_the_pane_has_focus() {
        // The pane is a real terminal, not a preview: with focus on it the
        // keystroke belongs to the program inside.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        dashed(&mut s, &mut ui);
        let _ = sent(&mut theirs);

        ui.focus = Focus::Terminal;
        handle(key(KeyCode::Char('l')), &mut s, &mut ui);
        let asked = sent(&mut theirs);
        match asked.as_slice() {
            [Request::Input { terminal, bytes }] => {
                assert_eq!(*terminal, grove_proto::TerminalId(1));
                assert_eq!(bytes, b"l");
            }
            other => panic!("expected the keystroke to reach the pty, got {other:?}"),
        }
    }

    #[test]
    fn the_prefix_still_addresses_grove_from_inside_the_terminal() {
        // §3.1's whole rule: a focused pty takes every key except `^g`. If
        // this broke, there would be no way out of the pane.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        dashed(&mut s, &mut ui);
        let _ = sent(&mut theirs);

        ui.focus = Focus::Terminal;
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('1')), &mut s, &mut ui);
        assert!(
            !ui.panes.visible(Focus::Repos),
            "^g 1 must still hide a pane"
        );
        assert!(
            sent(&mut theirs).is_empty(),
            "a chord addressed to grove must not reach the program"
        );
    }

    #[test]
    fn resizing_tells_the_daemon_the_ptys_new_size() {
        // Acceptance: resize propagates. The daemon owns the process, so it
        // is the one that must call TIOCSWINSZ.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        dashed(&mut s, &mut ui);
        let _ = sent(&mut theirs);

        handle(Input::Terminal(TermEvent::Resize(200, 50)), &mut s, &mut ui);
        let asked = sent(&mut theirs);
        assert!(
            asked
                .iter()
                .any(|r| matches!(r, Request::ResizeTerminal { .. })),
            "a resize must reach the daemon: {asked:?}"
        );
    }

    #[test]
    fn output_for_a_terminal_off_screen_costs_no_frame() {
        // Every attached pty streams; only the one being looked at is worth a
        // redraw. Without this the dash repaints at the rate of the busiest
        // terminal in the session.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        dashed(&mut s, &mut ui);
        let _ = sent(&mut theirs);

        match handle(
            Input::Daemon(DaemonEvent::TerminalOutput {
                terminal: grove_proto::TerminalId(2),
                bytes: b"busy".to_vec(),
            }),
            &mut s,
            &mut ui,
        ) {
            Flow::Continue { redraw } => assert!(!redraw, "that pane is not on screen"),
            Flow::Quit => panic!("output must not quit"),
        }

        match handle(
            Input::Daemon(DaemonEvent::TerminalOutput {
                terminal: grove_proto::TerminalId(1),
                bytes: b"visible".to_vec(),
            }),
            &mut s,
            &mut ui,
        ) {
            Flow::Continue { redraw } => assert!(redraw, "this one is"),
            Flow::Quit => panic!("output must not quit"),
        }
    }

    #[test]
    fn a_worktree_without_a_terminal_can_be_given_one() {
        // Acceptance: an affordance to spawn. A worktree can legitimately
        // have no pty — adopted, or after the daemon restarted.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("repo", 1)])),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("repo".into()),
                rows: vec![with_terminal("bare", None)],
            }),
            &mut s,
            &mut ui,
        );
        let _ = sent(&mut theirs);

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        let asked = sent(&mut theirs);
        assert!(
            asked.iter().any(|r| matches!(
                r,
                Request::SpawnTerminal(grove_proto::TerminalTarget::Worktree(_))
            )),
            "^g enter must ask for a terminal here: {asked:?}"
        );
    }

    #[test]
    fn hiding_the_terminal_pane_does_not_reflow_the_program_inside_it() {
        // The review's medium: sizing the pty from "whatever is on screen"
        // means `^g 3` to glance at something else reflows vim twice, once on
        // the way out and once on the way back.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(Input::Terminal(TermEvent::Resize(200, 50)), &mut s, &mut ui);
        dashed(&mut s, &mut ui);
        let _ = sent(&mut theirs);
        let attached = ui.terminal_pane_size();

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('3')), &mut s, &mut ui);
        assert!(!ui.panes.visible(Focus::Terminal), "^g 3 hides the pane");
        assert_eq!(
            ui.terminal_pane_size(),
            attached,
            "a hidden pane must not resize the pty behind it"
        );
    }

    #[test]
    fn a_spawned_terminal_is_shown_without_waiting_to_be_told_twice() {
        // The review's other medium: `^g enter` created the pty and the pane
        // went on saying there was none, because nothing acted on the reply.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("repo", 1)])),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("repo".into()),
                rows: vec![with_terminal("bare", None)],
            }),
            &mut s,
            &mut ui,
        );
        let _ = sent(&mut theirs);

        handle(
            Input::Daemon(DaemonEvent::TerminalSpawned {
                target: grove_proto::TerminalTarget::Worktree(grove_proto::WorktreeRef {
                    repo: grove_domain::RepoId("repo".into()),
                    branch: "bare".into(),
                }),
                terminal: grove_proto::TerminalId(9),
            }),
            &mut s,
            &mut ui,
        );
        assert_eq!(
            ui.terminals.showing(),
            Some(grove_proto::TerminalId(9)),
            "the pane must show the terminal it just asked for"
        );
        let asked = sent(&mut theirs);
        assert!(
            asked
                .iter()
                .any(|r| matches!(r, Request::AttachTerminal(a) if a.terminal == grove_proto::TerminalId(9))),
            "and attach to it: {asked:?}"
        );
        assert!(
            asked.contains(&Request::ListWorktrees(grove_domain::RepoId("repo".into()))),
            "the row still says it has no terminal, which is the daemon's to correct: {asked:?}"
        );
    }

    /// What the dash paints, row by row, at a given size.
    /// A dash with one repo and three worktrees, sized like a real terminal,
    /// with a session open so it is not the empty state.
    fn a_dash_to_click() -> (State, UnixStream, Ui) {
        let (mut s, theirs) = wired();
        let mut ui = Ui::new();
        handle(Input::Terminal(TermEvent::Resize(160, 40)), &mut s, &mut ui);
        handle(
            Input::Daemon(DaemonEvent::SessionChanged(grove_proto::SessionRow {
                id: grove_domain::SessionId("s1".into()),
                name: "invoice split".into(),
                members: vec!["repo".into()],
                state: grove_domain::SessionState::Attached,
                terminals: 0,
                since: 0,
                size: 0,
            })),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("repo", 3)])),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("repo".into()),
                rows: vec![
                    worktree_row("feat/first", grove_domain::Ownership::Unowned),
                    worktree_row("feat/second", grove_domain::Ownership::Unowned),
                    worktree_row("feat/third", grove_domain::Ownership::Unowned),
                ],
            }),
            &mut s,
            &mut ui,
        );
        (s, theirs, ui)
    }

    /// Where `text` is painted, as a terminal would report a click on it.
    fn where_painted(s: &State, ui: &Ui, text: &str) -> (u16, u16) {
        let screen = painted_dash(s, ui, 160, 40);
        for (row, line) in screen.iter().enumerate() {
            if let Some(byte) = line.find(text) {
                let column = line[..byte].chars().count();
                return (column as u16, row as u16);
            }
        }
        panic!("{text} is not on screen:\n{}", screen.join("\n"));
    }

    fn click(column: u16, row: u16) -> Input {
        Input::Terminal(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
    }

    fn wheel(kind: MouseEventKind, column: u16, row: u16) -> Input {
        Input::Terminal(TermEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
    }

    #[test]
    fn clicking_a_worktree_selects_the_row_painted_under_the_pointer() {
        // The acceptance that matters: not "a click selects a row" but "a
        // click selects the row you can see under the pointer". The position
        // comes from the painted screen, so any disagreement between the
        // geometry the mouse uses and the one the dash draws with fails here.
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        ui.focus = Focus::Repos;
        let (column, row) = where_painted(&s, &ui, "feat/third");
        handle(click(column, row), &mut s, &mut ui);
        assert_eq!(
            ui.worktrees
                .selected()
                .map(|row| row.worktree.branch.as_str()),
            Some("feat/third")
        );
        assert_eq!(ui.focus, Focus::Worktrees, "and the pane takes focus");
    }

    #[test]
    fn clicking_a_pane_header_focuses_it_without_selecting_a_row() {
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        ui.focus = Focus::Repos;
        let (column, row) = where_painted(&s, &ui, "WORKTREES");
        handle(click(column, row), &mut s, &mut ui);
        assert_eq!(ui.focus, Focus::Worktrees);
        assert_eq!(
            ui.worktrees
                .selected()
                .map(|row| row.worktree.branch.as_str()),
            Some("feat/first"),
            "a header is not a row"
        );
    }

    #[test]
    fn clicking_the_terminal_pane_gives_it_the_keyboard() {
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        ui.focus = Focus::Worktrees;
        // The terminal pane's header is the selection: `repo · branch`.
        let (column, row) = where_painted(&s, &ui, "repo · feat/first");
        handle(click(column, row), &mut s, &mut ui);
        assert_eq!(ui.focus, Focus::Terminal);
    }

    #[test]
    fn a_click_in_the_gap_between_panes_does_nothing() {
        // Acceptance: a click that lands on nothing does nothing.
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        ui.focus = Focus::Worktrees;
        let (frames, _) = dash::layout(ui.body_area(), ui.drawn_panes(), &ui.theme);
        let gap = frames.repos.expect("repos is drawn").right();
        let flow = handle(click(gap, 5), &mut s, &mut ui);
        assert!(matches!(flow, Flow::Continue { redraw: false }));
        assert_eq!(ui.focus, Focus::Worktrees);
    }

    #[test]
    fn the_wheel_moves_the_list_under_the_pointer_without_taking_focus() {
        // Pointing, not typing: the wheel acts where it is, and the keyboard
        // stays where it was.
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        ui.focus = Focus::Terminal;
        // A row other than the selected one: the selected branch is also in
        // the terminal pane's header, which the wheel would scroll instead.
        let (column, row) = where_painted(&s, &ui, "feat/second");
        handle(
            wheel(MouseEventKind::ScrollDown, column, row),
            &mut s,
            &mut ui,
        );
        assert_eq!(
            ui.worktrees
                .selected()
                .map(|row| row.worktree.branch.as_str()),
            Some("feat/second")
        );
        assert_eq!(ui.focus, Focus::Terminal, "focus stays put");
    }

    #[test]
    fn clicking_the_session_name_opens_the_picker() {
        // The mock draws it as a control. It is the terminal's version.
        let (mut s, mut theirs, mut ui) = a_dash_to_click();
        let (column, row) = where_painted(&s, &ui, "session: invoice split");
        handle(click(column + 3, row), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Picker);
        assert!(
            sent(&mut theirs)
                .iter()
                .any(|request| matches!(request, Request::ListSessions)),
            "and asks for the sessions, as ^g s does"
        );
    }

    #[test]
    fn the_dash_under_an_overlay_does_not_take_clicks() {
        // Acceptance: mouse events never reach a pane that is not on screen.
        // Under the palette the dash is washed out, and a click aimed at the
        // palette must not select a worktree that happens to be beneath it.
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        let (column, row) = where_painted(&s, &ui, "feat/third");
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        handle(click(column, row), &mut s, &mut ui);
        assert_eq!(
            ui.worktrees
                .selected()
                .map(|row| row.worktree.branch.as_str()),
            Some("feat/first")
        );
    }

    /// The dash above, with a second, detached session stored, and the
    /// session picker open over it.
    fn a_picker_to_click() -> (State, UnixStream, Ui) {
        let (mut s, mut theirs, mut ui) = a_dash_to_click();
        handle(
            Input::Daemon(DaemonEvent::Sessions(vec![
                grove_proto::SessionRow {
                    id: grove_domain::SessionId("s1".into()),
                    name: "invoice split".into(),
                    members: vec!["repo".into()],
                    state: grove_domain::SessionState::Attached,
                    terminals: 0,
                    since: 0,
                    size: 0,
                },
                grove_proto::SessionRow {
                    id: grove_domain::SessionId("s2".into()),
                    name: "retry jitter".into(),
                    members: vec!["repo".into()],
                    state: grove_domain::SessionState::Detached,
                    terminals: 0,
                    since: 0,
                    size: 0,
                },
            ])),
            &mut s,
            &mut ui,
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('s')), &mut s, &mut ui);
        let _ = sent(&mut theirs);
        (s, theirs, ui)
    }

    #[test]
    fn a_click_on_a_session_selects_it_and_a_second_click_resumes_it() {
        // The mock's pick-list behaviour: the first click is "this one", the
        // second is "go".
        let (mut s, mut theirs, mut ui) = a_picker_to_click();
        let (column, row) = where_painted(&s, &ui, "retry jitter");
        handle(click(column, row), &mut s, &mut ui);
        assert_eq!(
            ui.sessions.selected().map(|row| row.name.as_str()),
            Some("retry jitter")
        );
        assert!(
            !sent(&mut theirs)
                .iter()
                .any(|request| matches!(request, Request::OpenSession(_))),
            "one click selects; it does not resume"
        );

        handle(click(column, row), &mut s, &mut ui);
        assert!(
            sent(&mut theirs).contains(&Request::OpenSession(grove_domain::SessionId("s2".into()))),
            "the second click resumes the session under it"
        );
    }

    #[test]
    fn a_footer_hint_is_a_button_for_its_key() {
        // `esc back` in the picker's footer closes it, as `esc` does.
        let (mut s, _theirs, mut ui) = a_picker_to_click();
        let (column, row) = where_painted(&s, &ui, "esc back");
        handle(click(column, row), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Dash);
    }

    #[test]
    fn a_click_outside_an_overlay_closes_it() {
        let (mut s, _theirs, mut ui) = a_picker_to_click();
        assert_eq!(ui.screen, Screen::Picker);
        // Top-left corner of the screen: the washed-out dash, not the box.
        handle(click(1, 1), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Dash);
    }

    #[test]
    fn a_click_inside_an_overlay_on_nothing_does_nothing() {
        // The header line of the box is not a row and not a button.
        let (mut s, mut theirs, mut ui) = a_picker_to_click();
        let (column, row) = where_painted(&s, &ui, "session ❯");
        handle(click(column, row), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Picker, "it stays open");
        assert!(sent(&mut theirs).is_empty(), "and asks for nothing");
    }

    #[test]
    fn a_palette_command_runs_on_a_second_click() {
        let (mut s, mut theirs, mut ui) = a_dash_to_click();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        let _ = sent(&mut theirs);
        let (column, row) = where_painted(&s, &ui, "prune ");
        handle(click(column, row), &mut s, &mut ui);
        assert_eq!(
            ui.palette.selected().map(|entry| entry.name),
            Some("prune".to_string())
        );
        assert!(!sent(&mut theirs).contains(&Request::ListPruneCandidates));
        handle(click(column, row), &mut s, &mut ui);
        assert!(sent(&mut theirs).contains(&Request::ListPruneCandidates));
    }

    #[test]
    fn every_click_on_a_prune_row_toggles_it() {
        // The tick lists toggle on every click, as `space` does — the mock's
        // prune rows do exactly that.
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        for c in "prune".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        handle(
            Input::Daemon(DaemonEvent::PruneCandidates(vec![
                prune_candidate("web-app", "feat/done", vec![]),
                prune_candidate("sdk-js", "feat/also-done", vec![]),
            ])),
            &mut s,
            &mut ui,
        );
        assert_eq!(ui.screen, Screen::Prune);
        let before = ui.prune.count();
        let (column, row) = where_painted(&s, &ui, "feat/also-done");
        handle(click(column, row), &mut s, &mut ui);
        assert_eq!(ui.prune.count(), before - 1, "the click unticked it");
        handle(click(column, row), &mut s, &mut ui);
        assert_eq!(ui.prune.count(), before, "and the next one ticks it again");
    }

    #[test]
    fn a_repo_is_ticked_by_clicking_it_in_the_add_picker() {
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        let mut outsider = repo_row("elsewhere", 0);
        outsider.member = false;
        let mut member = repo_row("repo", 3);
        member.member = true;
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![member, outsider])),
            &mut s,
            &mut ui,
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        for c in "add".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        let (column, row) = where_painted(&s, &ui, "elsewhere");
        handle(click(column, row), &mut s, &mut ui);
        assert_eq!(
            ui.palette.select().map(|select| select.count()),
            Some(1),
            "the clicked repo is ticked"
        );
    }

    #[test]
    fn the_wheel_moves_an_overlays_cursor() {
        let (mut s, _theirs, mut ui) = a_picker_to_click();
        let (column, row) = where_painted(&s, &ui, "invoice split  ");
        handle(
            wheel(MouseEventKind::ScrollDown, column, row),
            &mut s,
            &mut ui,
        );
        assert_eq!(
            ui.sessions.selected().map(|row| row.name.as_str()),
            Some("retry jitter")
        );
    }

    #[test]
    fn a_click_outside_the_scratch_shell_leaves_it_as_its_own_key_does() {
        // `esc` belongs to the program in the pty, so the way out is `^g i`,
        // and a click outside the box presses that rather than `esc`.
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('i')), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Shell);
        handle(click(1, 1), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Dash);
    }

    /// A dash at 160x40 with worktree `a`'s terminal on screen, optionally
    /// with its program having asked for the mouse in SGR.
    fn a_terminal_to_click(mouse: bool) -> (State, UnixStream, Ui) {
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(Input::Terminal(TermEvent::Resize(160, 40)), &mut s, &mut ui);
        // An open session, or the dash is the first-run guidance and draws no
        // terminal pane at all.
        handle(
            Input::Daemon(DaemonEvent::SessionChanged(grove_proto::SessionRow {
                id: grove_domain::SessionId("s1".into()),
                name: "one".into(),
                members: vec!["repo".into()],
                state: grove_domain::SessionState::Attached,
                terminals: 1,
                since: 0,
                size: 0,
            })),
            &mut s,
            &mut ui,
        );
        dashed(&mut s, &mut ui);
        assert!(
            empty::Empty::of(ui.repos.workspace_count(), ui.repos.member_count(), true).is_none(),
            "the fixture must be a real dash, not the guidance"
        );
        if mouse {
            handle(
                Input::Daemon(DaemonEvent::TerminalOutput {
                    terminal: grove_proto::TerminalId(1),
                    bytes: b"\x1b[?1000h\x1b[?1006h".to_vec(),
                }),
                &mut s,
                &mut ui,
            );
        }
        let _ = sent(&mut theirs);
        (s, theirs, ui)
    }

    fn terminal_rows(ui: &Ui) -> Rect {
        dash::layout(ui.body_area(), ui.drawn_panes(), &ui.theme)
            .1
            .terminal
            .expect("the terminal pane is on screen")
    }

    #[test]
    fn a_program_that_asked_for_the_mouse_gets_the_click_where_it_landed() {
        // vim with `set mouse=a`: the click must arrive at the cell under the
        // pointer, counted from the pty's own top-left, in the encoding the
        // program chose — and the pane takes the keyboard as a click on it
        // would anywhere.
        let (mut s, mut theirs, mut ui) = a_terminal_to_click(true);
        ui.focus = Focus::Worktrees;
        let pty = terminal_rows(&ui);
        handle(click(pty.x + 3, pty.y + 2), &mut s, &mut ui);
        assert!(
            sent(&mut theirs).contains(&Request::Input {
                terminal: grove_proto::TerminalId(1),
                bytes: b"\x1b[<0;4;3M".to_vec(),
            }),
            "the program must receive the click at 4;3"
        );
        assert_eq!(ui.focus, Focus::Terminal);
    }

    #[test]
    fn a_program_that_did_not_ask_leaves_the_mouse_with_grove() {
        // A plain shell never asked. The wheel over it is grove's scrollback,
        // and nothing is typed into the program.
        let (mut s, mut theirs, mut ui) = a_terminal_to_click(false);
        let pty = terminal_rows(&ui);
        handle(
            wheel(MouseEventKind::ScrollUp, pty.x + 3, pty.y + 2),
            &mut s,
            &mut ui,
        );
        assert!(
            !sent(&mut theirs)
                .iter()
                .any(|request| matches!(request, Request::Input { .. })),
            "nothing may reach a program that did not ask"
        );
    }

    #[test]
    fn the_program_only_has_the_mouse_over_its_own_pane() {
        // vim having the mouse must not steal a click on the WORKTREES list.
        let (mut s, mut theirs, mut ui) = a_terminal_to_click(true);
        let (frames, rows) = dash::layout(ui.body_area(), ui.drawn_panes(), &ui.theme);
        let list = rows.worktrees.expect("worktrees on screen");
        let _ = frames;
        handle(click(list.x + 1, list.y + 1), &mut s, &mut ui);
        assert!(
            !sent(&mut theirs)
                .iter()
                .any(|request| matches!(request, Request::Input { .. })),
        );
        assert_eq!(
            ui.worktrees
                .selected()
                .map(|row| row.worktree.branch.as_str()),
            Some("b"),
            "the list took the click"
        );
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Input {
        Input::Terminal(TermEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
    }

    /// Press at `from`, drag to `to`, let go.
    fn drag(s: &mut State, ui: &mut Ui, from: (u16, u16), to: (u16, u16)) {
        handle(
            mouse(MouseEventKind::Down(MouseButton::Left), from.0, from.1),
            s,
            ui,
        );
        handle(
            mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1),
            s,
            ui,
        );
        handle(
            mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1),
            s,
            ui,
        );
    }

    #[test]
    fn dragging_across_a_branch_copies_it() {
        // The acceptance: drag over text in a pane, let go, and it is on the
        // clipboard — exactly the characters that were highlighted.
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        let (column, row) = where_painted(&s, &ui, "feat/second");
        let end = column + u16::try_from("feat/second".len()).unwrap() - 1;
        drag(&mut s, &mut ui, (column, row), (end, row));
        assert_eq!(ui.clipboard.as_deref(), Some("feat/second"));
    }

    #[test]
    fn a_click_is_still_a_click_and_copies_nothing() {
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        let (column, row) = where_painted(&s, &ui, "feat/third");
        handle(click(column, row), &mut s, &mut ui);
        handle(
            mouse(MouseEventKind::Up(MouseButton::Left), column, row),
            &mut s,
            &mut ui,
        );
        assert_eq!(ui.clipboard, None, "no drag, no copy");
        assert!(ui.selection.is_none(), "and nothing left highlighted");
        assert_eq!(
            ui.worktrees
                .selected()
                .map(|row| row.worktree.branch.as_str()),
            Some("feat/third"),
            "the click did what a click does"
        );
    }

    #[test]
    fn a_selection_never_leaves_the_pane_it_started_in() {
        // Dragging from the list out over the terminal pane selects to the
        // list's edge, not into the terminal beside it.
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        let (column, row) = where_painted(&s, &ui, "feat/third");
        let far_right = ui.width - 2;
        drag(&mut s, &mut ui, (column, row), (far_right, row));
        let copied = ui.clipboard.clone().expect("something was selected");
        assert!(copied.starts_with("feat/third"), "{copied:?}");
        // Past the list's edge there is its border, the gap, and the next
        // pane's border before anything of the terminal's — so a selection
        // that leaked would carry a `│` whatever the terminal is showing.
        assert!(
            !copied.contains('│'),
            "nothing past the pane's edge may be in it: {copied:?}"
        );
    }

    #[test]
    fn the_selection_is_highlighted_until_a_key_is_pressed() {
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        let (column, row) = where_painted(&s, &ui, "feat/second");
        drag(&mut s, &mut ui, (column, row), (column + 4, row));
        let reversed = |ui: &Ui, s: &State| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 40)).unwrap();
            terminal.draw(|f| draw(f, s, ui)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            (0..5)
                .filter(|dx| {
                    buffer[(column + dx, row)]
                        .style()
                        .add_modifier
                        .contains(ratatui::style::Modifier::REVERSED)
                })
                .count()
        };
        assert_eq!(reversed(&ui, &s), 5, "the dragged cells are shown selected");
        handle(key(KeyCode::Down), &mut s, &mut ui);
        assert_eq!(reversed(&ui, &s), 0, "a key ends the selection");
    }

    #[test]
    fn a_drag_over_a_program_that_took_the_mouse_is_the_programs() {
        // vim with mouse=a selects in its own way; grove must not also
        // start one on top of it.
        let (mut s, mut theirs, mut ui) = a_terminal_to_click(true);
        let pty = terminal_rows(&ui);
        drag(
            &mut s,
            &mut ui,
            (pty.x + 1, pty.y + 1),
            (pty.x + 6, pty.y + 1),
        );
        assert!(ui.selection.is_none());
        assert_eq!(ui.clipboard, None);
        // The program asked for mode 1000 — presses and releases, no motion
        // — so that is what it gets: the gesture's ends, and grove keeps out.
        let asked = sent(&mut theirs);
        let reached = |prefix: &[u8], end: u8| {
            asked.iter().any(|request| matches!(
                request,
                Request::Input { bytes, .. } if bytes.starts_with(prefix) && bytes.last() == Some(&end)
            ))
        };
        assert!(
            reached(b"\x1b[<0;", b'M'),
            "the press reached it: {asked:?}"
        );
        assert!(reached(b"\x1b[<0;", b'm'), "and the release: {asked:?}");
    }

    fn parsed(args: &[&str]) -> Invocation {
        parse_args(args.iter().map(std::ffi::OsString::from))
    }

    #[test]
    fn flags_are_answered_not_taken_as_a_workspace() {
        // `grove --version` used to start a daemon for a workspace called
        // `--version`.
        assert_eq!(parsed(&["--version"]), Invocation::Version);
        assert_eq!(parsed(&["-V"]), Invocation::Version);
        assert_eq!(parsed(&["--help"]), Invocation::Help);
        assert_eq!(parsed(&["-h"]), Invocation::Help);
    }

    #[test]
    fn a_path_is_a_workspace_and_none_means_here() {
        assert_eq!(parsed(&[]), Invocation::Run(None));
        assert_eq!(
            parsed(&["~/code"]),
            Invocation::Run(Some(PathBuf::from("~/code")))
        );
    }

    #[test]
    fn an_unknown_flag_is_refused_rather_than_becoming_a_path() {
        assert!(matches!(parsed(&["-x"]), Invocation::Bad(why) if why.contains("-x")));
        assert!(matches!(parsed(&["a", "b"]), Invocation::Bad(why) if why.contains('b')));
    }

    #[test]
    fn a_path_that_starts_with_a_dash_goes_after_two_dashes() {
        assert_eq!(
            parsed(&["--", "-odd"]),
            Invocation::Run(Some(PathBuf::from("-odd")))
        );
        assert!(matches!(parsed(&["--"]), Invocation::Bad(_)));
    }

    #[test]
    fn the_scratch_shell_is_the_size_of_the_box_it_is_drawn_in() {
        // It was sized like the dash's terminal pane — a third of the width —
        // and then drawn in the scratch box, which is 98 columns: a shell
        // wrapping at 50 inside a box twice as wide.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(Input::Terminal(TermEvent::Resize(160, 40)), &mut s, &mut ui);
        ui.scratch = Some(grove_proto::TerminalId(9));
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('i')), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Shell);

        let body = ui.body_area();
        let (rows, hints) = ui.overlay_shape(body).expect("the shell is an overlay");
        let drawn = shell_pty_area(overlay::layout(body, rows, hints).parts.body);
        let asked = sent(&mut theirs);
        assert!(
            asked.iter().any(|request| matches!(
                request,
                Request::AttachTerminal(attach)
                    if attach.terminal == grove_proto::TerminalId(9)
                        && (attach.rows, attach.cols) == (drawn.height, drawn.width)
            )),
            "the shell must attach at {}x{}: {asked:?}",
            drawn.height,
            drawn.width
        );

        // And a resize while it is open resizes it to the box, not the pane.
        handle(Input::Terminal(TermEvent::Resize(120, 30)), &mut s, &mut ui);
        let body = ui.body_area();
        let (rows, hints) = ui.overlay_shape(body).expect("still open");
        let drawn = shell_pty_area(overlay::layout(body, rows, hints).parts.body);
        let asked = sent(&mut theirs);
        assert!(
            asked.iter().any(|request| matches!(
                request,
                Request::ResizeTerminal { rows, cols, .. }
                    if (*rows, *cols) == (drawn.height, drawn.width)
            )),
            "a resize must follow the box: {asked:?}"
        );
    }

    #[test]
    fn the_wheel_scrolls_help_and_a_click_puts_it_away() {
        // #111's acceptance: every keyboard affordance has a mouse one. Help
        // was the screen the mouse could not touch at all.
        let (mut s, _theirs, mut ui) = a_dash_to_click();
        handle(Input::Terminal(TermEvent::Resize(160, 12)), &mut s, &mut ui);
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('?')), &mut s, &mut ui);
        assert!(ui.helping.is_some());
        let top = painted_dash(&s, &ui, 160, 12).join("\n");

        handle(wheel(MouseEventKind::ScrollDown, 10, 5), &mut s, &mut ui);
        let scrolled = painted_dash(&s, &ui, 160, 12).join("\n");
        assert_ne!(top, scrolled, "the wheel must move help");
        assert!(ui.helping.is_some(), "and leave it open");

        handle(click(10, 5), &mut s, &mut ui);
        assert!(ui.helping.is_none(), "a click puts it away");
        assert_eq!(ui.screen, Screen::Dash, "back where it was opened");
    }

    #[test]
    fn the_pty_is_the_size_of_the_pane_that_draws_it() {
        // Since the dash grew a header row, a blank line and padding (#96),
        // the pty was sized by the old arithmetic — a border and nothing
        // else, under a one-row bar — so it was two rows taller and two
        // columns wider than the area it is drawn into. A shell prompt at the
        // top hides that; vim loses its last lines and its right edge.
        let mut ui = Ui::new();
        handle(
            Input::Terminal(TermEvent::Resize(160, 40)),
            &mut connected(),
            &mut ui,
        );
        let body = Rect {
            x: 0,
            y: 0,
            width: 160,
            height: 38,
        };
        let drawn = dash::layout(body, ui.panes, &ui.theme)
            .1
            .terminal
            .expect("the terminal pane is on screen at 160 columns");
        assert_eq!(
            ui.terminal_pane_size(),
            (drawn.height, drawn.width),
            "rows and columns of the pty must be the rows and columns painted"
        );
    }

    fn painted_dash(state: &State, ui: &Ui, width: u16, height: u16) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("a test terminal");
        terminal
            .draw(|f| draw(f, state, ui))
            .expect("the dash draws");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn an_empty_workspace_draws_guidance_rather_than_blank_boxes() {
        // Acceptance: both empty cases produce copy, and the keys shown are
        // live from that state.
        let mut s = connected();
        let mut ui = Ui::new();
        handle(Input::Daemon(DaemonEvent::Repos(vec![])), &mut s, &mut ui);

        let screen = painted_dash(&s, &ui, 100, 20).join("\n");
        assert!(screen.contains("grove is empty"), "{screen}");
        assert!(screen.contains("scan"), "{screen}");
        assert!(
            screen.contains(&keymap::key_label(Action::OpenPalette)),
            "the key must be the one the keymap binds: {screen}"
        );
        assert!(screen.contains("no repos found"), "{screen}");
        // At 80 columns the guidance only fits because the empty state drops
        // the terminal pane; three panes clip it mid-sentence.
        assert!(
            screen.contains("point grove at your clones"),
            "the guidance must not be cut off: {screen}"
        );
    }

    #[test]
    fn the_bar_names_a_session_the_moment_it_exists() {
        // It said `session: 18d72b99d427b674-0` for a session called "invoice
        // split": the id is the fallback for a session the TUI knows nothing
        // else about, and a freshly created one was exactly that — the event
        // that announced it carried the name and was thrown away.
        let mut s = connected();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::SessionChanged(grove_proto::SessionRow {
                id: grove_domain::SessionId("18d72b99d427b674-0".into()),
                name: "invoice split".into(),
                members: vec![],
                state: grove_domain::SessionState::Attached,
                terminals: 0,
                since: 0,
                size: 0,
            })),
            &mut s,
            &mut ui,
        );
        assert_eq!(ui.session_name(), "session: invoice split");
    }

    #[test]
    fn starting_a_session_does_not_require_one() {
        // The last door on the first run: the dash says to start a session,
        // the palette takes the name, and `enter` answered "no open session
        // to session new in" — the one command that creates the thing it was
        // being refused for.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        assert!(ui.session.is_none(), "a fresh grove has no session");

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        for c in "session new".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        for c in "invoice split".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);

        assert_eq!(ui.note, None, "it must not refuse");
        let asked = sent(&mut theirs);
        assert!(
            asked.iter().any(|request| matches!(
                request,
                Request::SessionNew { name } if name == "invoice split"
            )),
            "the daemon must have been asked for the session: {asked:?}"
        );
    }

    #[test]
    fn a_session_is_not_created_without_a_name() {
        // The other half: `enter` on an empty name would ask the daemon for a
        // session called nothing.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        for c in "session new".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        handle(key(KeyCode::Enter), &mut s, &mut ui);

        assert!(ui.note.is_some(), "it must say why nothing happened");
        let asked = sent(&mut theirs);
        assert!(
            !asked
                .iter()
                .any(|request| matches!(request, Request::SessionNew { .. })),
            "and must not have asked: {asked:?}"
        );
    }

    #[test]
    fn a_session_name_can_contain_a_space() {
        // The mock's own session is called "invoice split". Typing it gave
        // `invoicesplit`: space is bound to toggle on the palette screen, and
        // `session new` was handed an empty picker to toggle, so the key did
        // nothing at all and the character was lost.
        let mut s = connected();
        let mut ui = Ui::new();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        for c in "session new".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        for c in "invoice split".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        assert_eq!(ui.palette.argument(), Some("invoice split"));
    }

    #[test]
    fn a_first_run_is_told_to_start_a_session_before_adding_repos() {
        // Found by running it: the dash said "add repos to this session" with
        // no session open, and `^g /  add` answered "no open session to add
        // in" to a user following the screen's own instructions.
        let mut s = connected();
        let mut ui = Ui::new();
        let mut outsider = repo_row("elsewhere", 3);
        outsider.member = false;
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![outsider])),
            &mut s,
            &mut ui,
        );

        let screen = painted_dash(&s, &ui, 100, 26).join("\n");
        let start = screen
            .find("session new")
            .expect("step one starts a session");
        let add = screen.find("add <repo>").expect("then repos are added");
        assert!(start < add, "in that order: {screen}");
    }

    #[test]
    fn a_workspace_with_repos_but_no_members_gets_different_copy() {
        // Telling this user to scan sends them looking for a fault that is
        // not there — the repos are already found.
        let mut s = connected();
        let mut ui = Ui::new();
        // With a session open — without one the dash has an earlier thing to
        // say, which is the case below.
        handle(
            Input::Daemon(DaemonEvent::SessionChanged(grove_proto::SessionRow {
                id: grove_domain::SessionId("s1".into()),
                name: "one".into(),
                members: vec![],
                state: grove_domain::SessionState::Attached,
                terminals: 0,
                since: 0,
                size: 0,
            })),
            &mut s,
            &mut ui,
        );
        let mut outsider = repo_row("elsewhere", 3);
        outsider.member = false;
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![outsider])),
            &mut s,
            &mut ui,
        );

        let screen = painted_dash(&s, &ui, 100, 20).join("\n");
        assert!(screen.contains("this session has no repos"), "{screen}");
        assert!(screen.contains("add <repo>"), "{screen}");
        assert!(
            !screen.contains("point grove at your clones"),
            "step one is behind this user: {screen}"
        );
    }

    #[test]
    fn the_empty_dash_still_draws_after_two_legal_hides() {
        // End to end through the keys, because the bug was in what the draw
        // path derived rather than in what the toggles allowed: both presses
        // are legal — the terminal stays visible — and the empty state hid it,
        // leaving a screen with nothing on it.
        let mut s = connected();
        let mut ui = Ui::new();
        handle(Input::Daemon(DaemonEvent::Repos(vec![])), &mut s, &mut ui);

        for pane in ['1', '2'] {
            handle(prefix(), &mut s, &mut ui);
            handle(key(KeyCode::Char(pane)), &mut s, &mut ui);
        }

        let screen = painted_dash(&s, &ui, 100, 20).join("\n");
        assert!(
            screen.contains("grove is empty"),
            "the guidance is the only thing left to read here: {screen}"
        );
    }

    #[test]
    fn the_dash_stops_being_empty_the_moment_a_member_arrives() {
        // Acceptance: the transition to the populated dash. The guidance has
        // to go in the same frame the rows arrive, or it lingers over them.
        let mut s = connected();
        let mut ui = Ui::new();
        handle(Input::Daemon(DaemonEvent::Repos(vec![])), &mut s, &mut ui);
        assert!(
            painted_dash(&s, &ui, 100, 20)
                .join("\n")
                .contains("grove is empty")
        );

        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("now-a-member", 1)])),
            &mut s,
            &mut ui,
        );
        let screen = painted_dash(&s, &ui, 100, 20).join("\n");
        assert!(!screen.contains("grove is empty"), "{screen}");
        assert!(screen.contains("now-a-member"), "{screen}");
    }

    #[test]
    fn the_palette_opens_takes_keys_directly_and_closes_clean() {
        // It is not a pty, so no prefix once it is open — and `esc` closes
        // without side effects, which means nothing was sent.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Palette);

        for c in "sca".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        assert_eq!(ui.palette.input(), "sca");
        assert_eq!(
            ui.palette.selected().map(|c| c.name),
            Some("scan".to_string())
        );

        // A command line you cannot correct is a worse command line than one
        // you cannot filter.
        handle(key(KeyCode::Backspace), &mut s, &mut ui);
        assert_eq!(ui.palette.input(), "sc");

        handle(key(KeyCode::Esc), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Dash);
        assert!(
            sent(&mut theirs).is_empty(),
            "esc must close without doing anything"
        );
    }

    #[test]
    fn running_a_command_reaches_the_daemon_and_closes_the_palette() {
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        for c in "scan".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);

        assert!(
            sent(&mut theirs).contains(&Request::Scan),
            "scan must reach the daemon"
        );
        assert_eq!(
            ui.screen,
            Screen::Dash,
            "a command line that stays open invites the same command twice"
        );
    }

    #[test]
    fn a_command_that_needs_an_argument_opens_its_picker() {
        // #23 could only say "this needs an argument". Now `enter` steps into
        // argument mode, and for `add` that is a list of the repos the session
        // does *not* hold — the ones the REPOS pane never shows.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        let mut outsider = repo_row("api-gateway", 0);
        outsider.member = false;
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("billing", 1), outsider])),
            &mut s,
            &mut ui,
        );
        let _ = sent(&mut theirs);

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        for c in "add".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);

        assert_eq!(ui.screen, Screen::Palette, "still open, now arguing");
        assert_eq!(
            ui.palette.arguing().map(|c| c.name.clone()),
            Some("add".to_string())
        );
        let rows: Vec<&str> = ui
            .palette
            .select()
            .expect("a picker")
            .rows()
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(rows, ["api-gateway"], "only what is not already a member");
        assert!(
            sent(&mut theirs).is_empty(),
            "opening a picker must not change anything"
        );
    }

    #[test]
    fn new_creates_one_worktree_per_checked_repo_and_adopts_the_newcomers() {
        // The acceptance, end to end: space toggles, enter creates. A checked
        // non-member joins the session first, because a worktree belongs to a
        // member.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        let mut outsider = repo_row("api-gateway", 0);
        outsider.member = false;
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("billing", 1), outsider])),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Sessions(vec![grove_proto::SessionRow {
                id: grove_domain::SessionId("open".into()),
                name: "current".into(),
                members: vec![],
                state: grove_domain::SessionState::Attached,
                terminals: 0,
                since: 0,
                size: 0,
            }])),
            &mut s,
            &mut ui,
        );
        let _ = sent(&mut theirs);

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        for c in "new".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        for c in "feat/split".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        // billing is checked as a member; check the outsider too.
        handle(key(KeyCode::Down), &mut s, &mut ui);
        handle(key(KeyCode::Char(' ')), &mut s, &mut ui);
        assert_eq!(ui.palette.select().expect("a picker").count(), 2);

        handle(key(KeyCode::Enter), &mut s, &mut ui);
        let asked = sent(&mut theirs);
        assert!(
            asked.iter().any(|r| matches!(
                r,
                Request::AddMember { repo, .. } if repo.0 == "api-gateway"
            )),
            "the newcomer joins the session: {asked:?}"
        );
        match asked
            .iter()
            .find(|r| matches!(r, Request::NewWorktrees { .. }))
        {
            Some(Request::NewWorktrees { branch, repos, .. }) => {
                assert_eq!(branch, "feat/split");
                assert_eq!(repos.len(), 2, "one worktree per checked repo");
            }
            other => panic!("expected NewWorktrees, got {other:?}"),
        }
        assert_eq!(ui.screen, Screen::Dash, "it ran, so the palette closes");
    }

    #[test]
    fn enter_with_nothing_checked_says_so_rather_than_creating_nothing() {
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("billing", 1)])),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Sessions(vec![grove_proto::SessionRow {
                id: grove_domain::SessionId("open".into()),
                name: "current".into(),
                members: vec![],
                state: grove_domain::SessionState::Attached,
                terminals: 0,
                since: 0,
                size: 0,
            }])),
            &mut s,
            &mut ui,
        );
        let _ = sent(&mut theirs);

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        for c in "new".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        for c in "feat/x".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        // Uncheck the only member.
        handle(key(KeyCode::Char(' ')), &mut s, &mut ui);
        handle(key(KeyCode::Enter), &mut s, &mut ui);

        let note = ui.note.clone().expect("a reason");
        assert!(note.contains("checked"), "{note}");
        assert!(sent(&mut theirs).is_empty(), "nothing to create");
        assert_eq!(ui.screen, Screen::Palette);
    }

    #[test]
    fn a_branch_already_checked_out_offers_the_worktree_that_has_it() {
        // Acceptance, and §7's flow: refusing and stopping would leave the
        // user to go and find the worktree themselves.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Sessions(vec![grove_proto::SessionRow {
                id: grove_domain::SessionId("open".into()),
                name: "current".into(),
                members: vec![],
                state: grove_domain::SessionState::Attached,
                terminals: 0,
                since: 0,
                size: 0,
            }])),
            &mut s,
            &mut ui,
        );
        let _ = sent(&mut theirs);

        let existing = grove_proto::WorktreeRef {
            repo: grove_domain::RepoId("billing".into()),
            branch: "feat/split".into(),
        };
        handle(
            Input::Daemon(DaemonEvent::BranchCheckedOutElsewhere {
                repo: grove_domain::RepoId("billing".into()),
                branch: "feat/split".into(),
                existing: existing.clone(),
                existing_path: PathBuf::from("/w/other/feat-split"),
            }),
            &mut s,
            &mut ui,
        );
        let note = ui.note.clone().expect("the offer");
        assert!(note.contains("/w/other/feat-split"), "{note}");
        assert!(note.contains("adopts"), "{note}");

        // And the key it names takes over that worktree, not whatever the
        // WORKTREES cursor happens to be on.
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('a')), &mut s, &mut ui);
        let asked = sent(&mut theirs);
        assert!(
            asked.iter().any(|r| matches!(
                r,
                Request::AdoptWorktree { worktree, .. } if *worktree == existing
            )),
            "the offered worktree is the one adopted: {asked:?}"
        );
        assert!(ui.conflict.is_none(), "the offer is spent");
    }

    fn prune_candidate(
        repo: &str,
        branch: &str,
        blockers: Vec<grove_proto::PruneBlocker>,
    ) -> grove_proto::PruneCandidate {
        grove_proto::PruneCandidate {
            worktree: grove_proto::WorktreeRef {
                repo: grove_domain::RepoId(repo.into()),
                branch: branch.into(),
            },
            state: if blockers.is_empty() {
                grove_proto::PruneState::Merged
            } else {
                grove_proto::PruneState::Neither
            },
            size: 100 * 1024 * 1024,
            blockers,
        }
    }

    fn session_row(name: &str, state: grove_domain::SessionState) -> grove_proto::SessionRow {
        grove_proto::SessionRow {
            id: grove_domain::SessionId(name.into()),
            name: name.into(),
            members: vec!["billing".into()],
            state,
            terminals: 2,
            since: 0,
            size: 0,
        }
    }

    /// The picker, open, with one attached and one detached session.
    fn picking(s: &mut State, ui: &mut Ui, theirs: &mut UnixStream) {
        handle(prefix(), s, ui);
        handle(key(KeyCode::Char('s')), s, ui);
        handle(
            Input::Daemon(DaemonEvent::Sessions(vec![
                session_row("open one", grove_domain::SessionState::Attached),
                session_row("away one", grove_domain::SessionState::Detached),
            ])),
            s,
            ui,
        );
        let _ = sent(theirs);
    }

    #[test]
    fn the_picker_asks_for_the_list_before_showing_it() {
        // A list that fills in underneath someone already pressing `X` is the
        // worst possible version of this screen.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('s')), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Picker);
        assert!(sent(&mut theirs).contains(&Request::ListSessions));
    }

    #[test]
    fn x_routes_to_the_confirm_and_sends_nothing() {
        // Acceptance: `X` never acts directly. Ending removes worktrees, and
        // §2 makes that the one thing grove asks about.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        picking(&mut s, &mut ui, &mut theirs);

        handle(key(KeyCode::Char('X')), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::EndSession, "the confirm, not the act");
        assert_eq!(
            ui.ending.session().map(|id| id.0.as_str()),
            Some("open one")
        );
        assert!(
            sent(&mut theirs).is_empty(),
            "nothing may be sent before the confirm"
        );
    }

    #[test]
    fn resume_replaces_the_open_session() {
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        picking(&mut s, &mut ui, &mut theirs);

        handle(key(KeyCode::Down), &mut s, &mut ui);
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        let asked = sent(&mut theirs);
        assert!(
            asked.iter().any(|r| matches!(
                r,
                Request::OpenSession(id) if id.0 == "away one"
            )),
            "{asked:?}"
        );
        assert_eq!(ui.screen, Screen::Dash, "the dash is that session's now");
        assert!(
            asked.contains(&Request::ListRepos),
            "and its repos are asked for: {asked:?}"
        );
    }

    #[test]
    fn resuming_what_is_already_open_is_refused() {
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        picking(&mut s, &mut ui, &mut theirs);

        handle(key(KeyCode::Enter), &mut s, &mut ui);
        let note = ui.note.clone().expect("a reason");
        assert!(note.contains("already open"), "{note}");
        assert!(sent(&mut theirs).is_empty());
        assert_eq!(ui.screen, Screen::Picker, "still choosing");
    }

    #[test]
    fn detach_and_close_send_their_own_requests() {
        // Three verbs for three states, and each has to reach the daemon as
        // itself: detach keeps terminals, close kills them.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        picking(&mut s, &mut ui, &mut theirs);

        handle(key(KeyCode::Char('d')), &mut s, &mut ui);
        assert!(
            sent(&mut theirs)
                .iter()
                .any(|r| matches!(r, Request::DetachSession(_)))
        );

        handle(key(KeyCode::Char('c')), &mut s, &mut ui);
        assert!(
            sent(&mut theirs)
                .iter()
                .any(|r| matches!(r, Request::CloseSession(_)))
        );
    }

    #[test]
    fn the_confirm_asks_every_member_repo_before_it_shows_a_total() {
        // Acceptance: the figures match what the daemon removes. They are the
        // daemon's rows, one repo at a time, and the screen says it is
        // counting until they are all back.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![
                repo_row("billing", 1),
                repo_row("web", 1),
            ])),
            &mut s,
            &mut ui,
        );
        let mut row = session_row("invoice split", grove_domain::SessionState::Attached);
        row.members = vec!["billing".into(), "web".into()];
        handle(
            Input::Daemon(DaemonEvent::Sessions(vec![row])),
            &mut s,
            &mut ui,
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('s')), &mut s, &mut ui);
        let _ = sent(&mut theirs);

        handle(key(KeyCode::Char('X')), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::EndSession);
        let asked = sent(&mut theirs);
        for repo in ["billing", "web"] {
            assert!(
                asked.contains(&Request::ListWorktrees(grove_domain::RepoId(repo.into()))),
                "{repo} was not asked about: {asked:?}"
            );
        }
        assert!(!ui.ending.ready(), "nothing has answered yet");

        // Confirming while it is still counting must not send anything.
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        assert!(
            sent(&mut theirs).is_empty(),
            "a total nobody has read is not a confirmation"
        );
        assert_eq!(ui.screen, Screen::EndSession);
    }

    #[test]
    fn the_confirm_removes_only_after_both_repos_answer() {
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("billing", 1)])),
            &mut s,
            &mut ui,
        );
        let mut row = session_row("invoice split", grove_domain::SessionState::Attached);
        row.members = vec!["billing".into()];
        handle(
            Input::Daemon(DaemonEvent::Sessions(vec![row])),
            &mut s,
            &mut ui,
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('s')), &mut s, &mut ui);
        handle(key(KeyCode::Char('X')), &mut s, &mut ui);
        let _ = sent(&mut theirs);

        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("billing".into()),
                rows: vec![worktree_row("feat/x", grove_domain::Ownership::Ours)],
            }),
            &mut s,
            &mut ui,
        );
        assert!(ui.ending.ready());
        assert_eq!(ui.ending.totals().worktrees, 1);

        handle(key(KeyCode::Enter), &mut s, &mut ui);
        let asked = sent(&mut theirs);
        assert!(
            asked
                .iter()
                .any(|r| matches!(r, Request::EndSession(id) if id.0 == "invoice split")),
            "{asked:?}"
        );
        assert_eq!(ui.screen, Screen::Dash);
    }

    #[test]
    fn esc_leaves_the_confirm_without_removing_anything() {
        // The safe default, and the reason `X` routes here at all.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Sessions(vec![session_row(
                "invoice split",
                grove_domain::SessionState::Attached,
            )])),
            &mut s,
            &mut ui,
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('s')), &mut s, &mut ui);
        handle(key(KeyCode::Char('X')), &mut s, &mut ui);
        let _ = sent(&mut theirs);

        handle(key(KeyCode::Esc), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Picker, "back where it came from");
        assert!(ui.ending.session().is_none());
        assert!(
            sent(&mut theirs).is_empty(),
            "nothing may be sent by cancelling"
        );
    }

    #[test]
    fn the_confirm_never_counts_another_sessions_worktree() {
        // The rule this screen exists for. Another session's are read-only
        // and the clone is the repository itself.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("billing", 1)])),
            &mut s,
            &mut ui,
        );
        let mut row = session_row("invoice split", grove_domain::SessionState::Attached);
        row.members = vec!["billing".into()];
        handle(
            Input::Daemon(DaemonEvent::Sessions(vec![row])),
            &mut s,
            &mut ui,
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('s')), &mut s, &mut ui);
        handle(key(KeyCode::Char('X')), &mut s, &mut ui);
        let _ = sent(&mut theirs);

        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("billing".into()),
                rows: vec![
                    worktree_row("ours", grove_domain::Ownership::Ours),
                    worktree_row(
                        "theirs",
                        grove_domain::Ownership::Other(grove_domain::SessionId("other".into())),
                    ),
                    worktree_row("clone", grove_domain::Ownership::Clone),
                    worktree_row("free", grove_domain::Ownership::Unowned),
                ],
            }),
            &mut s,
            &mut ui,
        );
        assert_eq!(
            ui.ending.totals().worktrees,
            1,
            "only the one this session owns"
        );
    }

    #[test]
    fn help_explains_the_screen_you_were_on_and_gives_it_back() {
        // `^g ?` is a glance, not a destination — and it must describe the
        // palette when opened from the palette, not itself.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Palette);

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('?')), &mut s, &mut ui);
        assert_eq!(ui.helping, Some(Screen::Palette));
        let screen = painted_dash(&s, &ui, 90, 18).join("\n");
        assert!(screen.contains("keys · palette"), "{screen}");
        assert!(screen.contains("keys reach it directly"), "{screen}");

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('?')), &mut s, &mut ui);
        assert!(ui.helping.is_none());
        assert_eq!(ui.screen, Screen::Palette, "back where it came from");
        assert!(sent(&mut theirs).is_empty(), "help sends nothing");
    }

    #[test]
    fn help_scrolls_when_the_terminal_is_short() {
        let mut s = connected();
        let mut ui = Ui::new();
        handle(Input::Terminal(TermEvent::Resize(90, 8)), &mut s, &mut ui);
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('?')), &mut s, &mut ui);

        let top = painted_dash(&s, &ui, 90, 8).join("\n");
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Down), &mut s, &mut ui);
        let scrolled = painted_dash(&s, &ui, 90, 8).join("\n");
        assert_ne!(top, scrolled, "^g ↓ must move the help");
    }

    #[test]
    fn the_diff_asks_for_the_selected_worktree_then_one_file_at_a_time() {
        // §4.4: hunks come per file, so opening the screen does not pay for
        // every file's patch — and moving the cursor is what asks for the
        // next one.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("repo", 1)])),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("repo".into()),
                rows: vec![worktree_row("feat/x", grove_domain::Ownership::Ours)],
            }),
            &mut s,
            &mut ui,
        );
        let _ = sent(&mut theirs);

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('d')), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Diff);
        match sent(&mut theirs).as_slice() {
            [Request::DiffWorktree { file, .. }] => {
                assert!(file.is_none(), "the whole list first, no file named");
            }
            other => panic!("expected a diff request, got {other:?}"),
        }

        handle(
            Input::Daemon(DaemonEvent::Diff {
                worktree: grove_proto::WorktreeRef {
                    repo: grove_domain::RepoId("repo".into()),
                    branch: "feat/x".into(),
                },
                base: "origin/main".into(),
                files: vec![
                    grove_proto::DiffFile {
                        path: "a.ts".into(),
                        status: 'M',
                        added: 1,
                        removed: 0,
                    },
                    grove_proto::DiffFile {
                        path: "b.ts".into(),
                        status: 'A',
                        added: 2,
                        removed: 0,
                    },
                ],
                selected: Some("a.ts".into()),
                hunks: vec![grove_proto::DiffLine::Added("one".into())],
                added: 3,
                removed: 0,
            }),
            &mut s,
            &mut ui,
        );
        let _ = sent(&mut theirs);

        handle(key(KeyCode::Down), &mut s, &mut ui);
        match sent(&mut theirs).as_slice() {
            [Request::DiffWorktree { file, .. }] => {
                assert_eq!(
                    file.as_deref(),
                    Some("b.ts"),
                    "the file now under the cursor"
                );
            }
            other => panic!("moving the cursor must ask for that file, got {other:?}"),
        }
    }

    /// A UI whose Lua came from `source`.
    fn with_lua(source: &str) -> Ui {
        let loaded = grove_lua::TuiRuntime::load_source(source, "test");
        let mut ui = Ui::with_config(loaded.runtime.config(), loaded.error, Depth::True);
        ui.with_lua(loaded.runtime);
        ui
    }

    #[test]
    fn a_user_keymap_runs_from_the_key_it_was_given() {
        // End to end: the binding is parsed, merged, and reached by the
        // router — which is what "merges over the built-ins" has to mean.
        let mut s = connected();
        let mut ui = with_lua(
            "local grove = require('grove')\n             grove.keymap('^g w', function() end)\n",
        );
        assert!(ui.config_note.is_none(), "nothing to report for a free key");

        handle(prefix(), &mut s, &mut ui);
        match handle(key(KeyCode::Char('w')), &mut s, &mut ui) {
            Flow::Continue { redraw } => assert!(redraw, "it ran"),
            Flow::Quit => panic!("a keymap must not quit"),
        }
        assert!(ui.note.is_none(), "and said nothing, because it worked");
    }

    #[test]
    fn a_user_command_is_listed_beside_the_built_ins_and_runs() {
        // Acceptance: invocable from the palette, and visibly the user's.
        let (mut s, mut theirs) = wired();
        let mut ui = with_lua(
            "local grove = require('grove')\n             grove.command('review', function(pr) end)\n",
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);

        // Tall enough for every command plus the user's: the palette is
        // `height:fit-content`, so a short screen cuts the last rows and this
        // test would be about the screen rather than the listing.
        let screen = painted_dash(&s, &ui, 100, 26).join("\n");
        assert!(screen.contains("review"), "{screen}");
        assert!(
            screen.contains("from your config"),
            "marked as theirs: {screen}"
        );
        // And beside the built-ins, not in a section of its own.
        assert!(screen.contains("scan"), "{screen}");

        for c in "review".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        assert_eq!(
            ui.palette.arguing().map(|c| c.name.clone()),
            Some("review".to_string()),
            "it takes whatever is typed after the name"
        );
        for c in "4471".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Dash, "it ran, so the palette closed");
        assert!(ui.note.is_none(), "and said nothing, because it worked");
        assert!(
            sent(&mut theirs).is_empty(),
            "a user command is its own implementation"
        );
    }

    #[test]
    fn a_user_command_creates_a_worktree_end_to_end() {
        // #33's first acceptance, and what #88 existed to make possible: the
        // callback computes, records, and grove performs — one request on the
        // wire, from a command grove knows nothing about.
        let (mut s, mut theirs) = wired();
        let mut ui = with_lua(
            "local grove = require('grove')\n             grove.command('start', function(branch)\n               grove.new_worktree({ repo = grove.current_repo(), branch = branch })\n             end)\n",
        );
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("billing", 1)])),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Sessions(vec![grove_proto::SessionRow {
                id: grove_domain::SessionId("open".into()),
                name: "current".into(),
                members: vec![],
                state: grove_domain::SessionState::Attached,
                terminals: 0,
                since: 0,
                size: 0,
            }])),
            &mut s,
            &mut ui,
        );
        let _ = sent(&mut theirs);

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        for c in "start".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        for c in "feat/from-lua".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);

        match sent(&mut theirs).as_slice() {
            [Request::NewWorktrees { branch, repos, .. }] => {
                assert_eq!(branch, "feat/from-lua");
                assert_eq!(repos, &[grove_domain::RepoId("billing".into())]);
            }
            other => panic!("expected one NewWorktrees, got {other:?}"),
        }
    }

    #[test]
    fn a_keymap_can_open_the_palette_prefilled() {
        // §10.3's own example. Purely local — the palette is grove's screen,
        // so this one is not a request at all.
        let (mut s, mut theirs) = wired();
        let mut ui = with_lua(
            "local grove = require('grove')\n             grove.keymap('^g w', function() grove.palette('new ') end)\n",
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('w')), &mut s, &mut ui);

        assert_eq!(ui.screen, Screen::Palette);
        assert_eq!(ui.palette.input(), "new ");
        assert!(sent(&mut theirs).is_empty(), "nothing asked of the daemon");
    }

    #[test]
    fn an_ask_that_cannot_be_performed_says_why() {
        // The helpers record whatever the script asked for, including things
        // grove cannot do right now. Silence would leave the user thinking it
        // worked.
        let (mut s, mut theirs) = wired();
        let mut ui = with_lua(
            "local grove = require('grove')\n             grove.keymap('^g w', function() grove.open_session('nowhere') end)\n",
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('w')), &mut s, &mut ui);
        assert!(
            ui.note.clone().is_some_and(|n| n.contains("nowhere")),
            "{:?}",
            ui.note
        );
        assert!(sent(&mut theirs).is_empty());
    }

    #[test]
    fn a_user_command_that_hangs_leaves_the_tui_responsive() {
        let mut s = connected();
        let mut ui = with_lua(
            "local grove = require('grove')\n             grove.command('hang', function() while true do end end)\n",
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        for c in "hang".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        let started = std::time::Instant::now();
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "held for {:?}",
            started.elapsed()
        );
        assert!(
            ui.note.clone().is_some_and(|n| n.contains("ran too long")),
            "{:?}",
            ui.note
        );
    }

    #[test]
    fn a_user_column_is_computed_when_the_rows_arrive_and_painted_with_them() {
        // Acceptance: it renders without blocking navigation, because it is
        // computed at the invalidation point rather than during a frame.
        let (mut s, _theirs) = wired();
        let mut ui = with_lua(
            "local grove = require('grove')\n             grove.column('pr', function(wt) return 'open' end)\n",
        );
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("repo", 1)])),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("repo".into()),
                rows: vec![worktree_row("feat/x", grove_domain::Ownership::Ours)],
            }),
            &mut s,
            &mut ui,
        );

        assert_eq!(ui.columns.live().len(), 1);
        assert_eq!(ui.columns.live()[0].cell("repo", "feat/x"), "open");
        let screen = painted_dash(&s, &ui, 120, 14).join("\n");
        assert!(
            screen.contains("open"),
            "the user's column is drawn: {screen}"
        );
    }

    #[test]
    fn a_column_that_hangs_does_not_stop_the_dash_arriving() {
        // The acceptance that matters: a slow column leaves the UI
        // responsive and the cell empty.
        let (mut s, _theirs) = wired();
        let mut ui = with_lua(
            "local grove = require('grove')\n             grove.column('slow', function() while true do end end)\n",
        );
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("repo", 1)])),
            &mut s,
            &mut ui,
        );
        let started = std::time::Instant::now();
        handle(
            Input::Daemon(DaemonEvent::Worktrees {
                repo: grove_domain::RepoId("repo".into()),
                rows: vec![worktree_row("feat/x", grove_domain::Ownership::Ours)],
            }),
            &mut s,
            &mut ui,
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "the rows waited {:?} on a user column",
            started.elapsed()
        );
        assert_eq!(ui.columns.live()[0].cell("repo", "feat/x"), "");
        assert!(
            ui.note
                .clone()
                .is_some_and(|note| note.contains("timed out")),
            "and it is reported"
        );

        // Navigation still works.
        ui.focus = Focus::Worktrees;
        handle(key(KeyCode::Down), &mut s, &mut ui);
    }

    #[test]
    fn a_keymap_taking_a_grove_key_is_reported_at_load() {
        // Acceptance. It still wins — it is their config — but a key that
        // used to open the session picker and now does something else needs
        // to be said out loud.
        let ui = with_lua(
            "local grove = require('grove')\n             grove.keymap('^g s', function() end)\n",
        );
        let note = ui.config_note.clone().expect("a report");
        assert!(note.contains("^g s"), "{note}");
        assert!(note.contains("sessions"), "{note}");
    }

    #[test]
    fn a_keymap_that_throws_is_disabled_and_said_once() {
        // §10.5: disabled rather than failing again on every keystroke.
        let mut s = connected();
        let mut ui = with_lua(
            "local grove = require('grove')\n             grove.keymap('^g w', function() error('boom') end)\n",
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('w')), &mut s, &mut ui);
        let note = ui.note.clone().expect("a report");
        assert!(note.contains("boom"), "{note}");
        assert!(note.contains("disabled"), "{note}");

        ui.note = None;
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('w')), &mut s, &mut ui);
        assert!(ui.note.is_none(), "it is not offered a second time");
    }

    #[test]
    fn a_keymap_that_hangs_leaves_the_tui_responsive() {
        // The one that matters: user code runs inside this loop, so a
        // `while true do end` must not take the screen with it.
        let mut s = connected();
        let mut ui = with_lua(
            "local grove = require('grove')\n             grove.keymap('^g w', function() while true do end end)\n",
        );
        let started = std::time::Instant::now();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('w')), &mut s, &mut ui);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "the loop was held for {:?}",
            started.elapsed()
        );
        let note = ui.note.clone().expect("a report");
        assert!(note.contains("ran too long"), "{note}");

        // And grove still works afterwards.
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('1')), &mut s, &mut ui);
        assert!(!ui.panes.visible(Focus::Repos), "^g 1 still hides a pane");
    }

    #[test]
    fn the_diff_needs_a_worktree_and_says_so() {
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('d')), &mut s, &mut ui);
        assert!(ui.note.is_some(), "it says why nothing opened");
        assert!(sent(&mut theirs).is_empty());
    }

    #[test]
    fn the_scratch_shell_opens_at_the_daemons_own_cwd() {
        // Acceptance: the configured `scratch_cwd`, not `~/grove` and not
        // wherever grove was started. `cwd: None` is how the protocol says
        // "your configured one" — sending a path here would be the client
        // deciding something that belongs to the daemon VM (§10.4).
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('i')), &mut s, &mut ui);

        assert_eq!(ui.screen, Screen::Shell);
        match sent(&mut theirs).as_slice() {
            [Request::SpawnTerminal(grove_proto::TerminalTarget::Scratch { cwd })] => {
                assert!(cwd.is_none(), "the daemon's configured cwd, not ours");
            }
            other => panic!("expected a scratch spawn, got {other:?}"),
        }
    }

    #[test]
    fn reopening_the_shell_reattaches_rather_than_spawning_another() {
        // Acceptance: it survives closing and reopening. A second spawn would
        // leave two shells running in the same place, one of them invisible.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('i')), &mut s, &mut ui);
        handle(
            Input::Daemon(DaemonEvent::TerminalSpawned {
                target: grove_proto::TerminalTarget::Scratch { cwd: None },
                terminal: grove_proto::TerminalId(5),
            }),
            &mut s,
            &mut ui,
        );
        assert_eq!(ui.scratch, Some(grove_proto::TerminalId(5)));
        let _ = sent(&mut theirs);

        // Leave and come back.
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('i')), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Dash, "^g i is the way out too");
        let _ = sent(&mut theirs);

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('i')), &mut s, &mut ui);
        let asked = sent(&mut theirs);
        assert!(
            !asked.iter().any(|r| matches!(r, Request::SpawnTerminal(_))),
            "a second shell must not be spawned: {asked:?}"
        );
        assert!(
            asked.iter().any(|r| matches!(
                r,
                Request::AttachTerminal(a) if a.terminal == grove_proto::TerminalId(5)
            )),
            "it re-attaches to the one that exists: {asked:?}"
        );
    }

    #[test]
    fn a_scratch_shell_that_exits_is_forgotten_so_the_next_one_opens() {
        // Found while writing up #28: the terminal was forgotten in
        // `Terminals` but not here, so `^g i` after the shell died would
        // re-attach to a dead id for the rest of the session.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('i')), &mut s, &mut ui);
        handle(
            Input::Daemon(DaemonEvent::TerminalSpawned {
                target: grove_proto::TerminalTarget::Scratch { cwd: None },
                terminal: grove_proto::TerminalId(5),
            }),
            &mut s,
            &mut ui,
        );
        handle(
            Input::Daemon(DaemonEvent::TerminalExited {
                terminal: grove_proto::TerminalId(5),
                status: Some(0),
            }),
            &mut s,
            &mut ui,
        );
        assert!(ui.scratch.is_none(), "the dead shell is forgotten");

        // Leave the overlay — it is still open over a pty that is gone — and
        // come back.
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('i')), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Dash);
        let _ = sent(&mut theirs);

        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('i')), &mut s, &mut ui);
        assert!(
            sent(&mut theirs)
                .iter()
                .any(|r| matches!(r, Request::SpawnTerminal(_))),
            "so the next ^g i opens a new one"
        );
    }

    #[test]
    fn esc_reaches_the_shell_rather_than_closing_it() {
        // Acceptance, and §4.5's point: it is a pty, so `esc` belongs to the
        // program inside. Leaving is `^g i` again.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('i')), &mut s, &mut ui);
        handle(
            Input::Daemon(DaemonEvent::TerminalSpawned {
                target: grove_proto::TerminalTarget::Scratch { cwd: None },
                terminal: grove_proto::TerminalId(5),
            }),
            &mut s,
            &mut ui,
        );
        let _ = sent(&mut theirs);

        handle(key(KeyCode::Esc), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Shell, "esc must not close the overlay");
        match sent(&mut theirs).as_slice() {
            [Request::Input { bytes, .. }] => assert_eq!(bytes, &[0x1b]),
            other => panic!("esc must reach the shell, got {other:?}"),
        }
    }

    #[test]
    fn prune_asks_the_daemon_and_opens_on_its_answer() {
        // The picker's whole point is that the safe rows are the daemon's
        // judgement. Opening on the keystroke would show an empty list that
        // fills in under a user already pressing `a`.
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        for c in "prune".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);

        assert!(
            sent(&mut theirs).contains(&Request::ListPruneCandidates),
            "it asks first"
        );
        assert_ne!(ui.screen, Screen::Prune, "and waits for the answer");

        handle(
            Input::Daemon(DaemonEvent::PruneCandidates(vec![
                prune_candidate("web-app", "feat/done", vec![]),
                prune_candidate(
                    "sdk-js",
                    "wip/x",
                    vec![grove_proto::PruneBlocker::Dirty { files: 2 }],
                ),
            ])),
            &mut s,
            &mut ui,
        );
        assert_eq!(ui.screen, Screen::Prune);
        assert_eq!(ui.prune.count(), 1, "only the daemon's safe row");
    }

    #[test]
    fn enter_removes_exactly_what_is_checked_and_waits_to_say_so() {
        let (mut s, mut theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::PruneCandidates(vec![
                prune_candidate("web-app", "feat/done", vec![]),
                prune_candidate(
                    "sdk-js",
                    "wip/x",
                    vec![grove_proto::PruneBlocker::Dirty { files: 2 }],
                ),
            ])),
            &mut s,
            &mut ui,
        );
        let _ = sent(&mut theirs);

        handle(key(KeyCode::Enter), &mut s, &mut ui);
        match sent(&mut theirs).as_slice() {
            [Request::Prune(worktrees)] => {
                assert_eq!(worktrees.len(), 1, "the blocked row is not included");
                assert_eq!(worktrees[0].branch, "feat/done");
            }
            other => panic!("expected one prune, got {other:?}"),
        }
        assert_eq!(
            ui.screen,
            Screen::Prune,
            "this is the destructive one: the screen stays until the daemon says what happened"
        );

        handle(
            Input::Daemon(DaemonEvent::Pruned {
                removed: vec![grove_proto::WorktreeRef {
                    repo: grove_domain::RepoId("web-app".into()),
                    branch: "feat/done".into(),
                }],
                failed: vec![],
                reclaimed: 412 * 1024 * 1024,
            }),
            &mut s,
            &mut ui,
        );
        assert_eq!(ui.screen, Screen::Dash);
        let note = ui.note.clone().expect("what happened");
        assert!(note.contains("412 MB"), "{note}");
    }

    #[test]
    fn a_prune_that_partly_failed_says_which_row_and_why() {
        // "Pruned 4" after removing four of five is a lie about the fifth.
        let mut s = connected();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Pruned {
                removed: vec![grove_proto::WorktreeRef {
                    repo: grove_domain::RepoId("a".into()),
                    branch: "gone".into(),
                }],
                failed: vec![(
                    grove_proto::WorktreeRef {
                        repo: grove_domain::RepoId("b".into()),
                        branch: "busy".into(),
                    },
                    "a terminal is running in it".into(),
                )],
                reclaimed: 0,
            }),
            &mut s,
            &mut ui,
        );
        let note = ui.note.clone().expect("a report");
        assert!(note.contains("busy"), "{note}");
        assert!(note.contains("terminal is running"), "{note}");
        assert!(note.contains("1 of 2"), "{note}");
    }

    #[test]
    fn a_selects_safe_rows_only_from_the_keyboard() {
        let mut s = connected();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::PruneCandidates(vec![
                prune_candidate("a", "safe", vec![]),
                prune_candidate("b", "dirty", vec![grove_proto::PruneBlocker::Unmerged]),
            ])),
            &mut s,
            &mut ui,
        );
        // Uncheck the safe row, then ask for all safe back.
        handle(key(KeyCode::Char(' ')), &mut s, &mut ui);
        assert_eq!(ui.prune.count(), 0);
        handle(key(KeyCode::Char('a')), &mut s, &mut ui);
        assert_eq!(ui.prune.count(), 1, "a must never check the blocked row");
    }

    #[test]
    fn esc_from_an_argument_goes_back_to_the_command_list() {
        let (mut s, _theirs) = wired();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("billing", 1)])),
            &mut s,
            &mut ui,
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);
        for c in "new".chars() {
            handle(key(KeyCode::Char(c)), &mut s, &mut ui);
        }
        handle(key(KeyCode::Enter), &mut s, &mut ui);
        assert!(ui.palette.arguing().is_some());

        handle(key(KeyCode::Esc), &mut s, &mut ui);
        assert!(ui.palette.arguing().is_none(), "back to choosing");
        assert_eq!(ui.screen, Screen::Palette, "and still open");

        handle(key(KeyCode::Esc), &mut s, &mut ui);
        assert_eq!(ui.screen, Screen::Dash, "now it closes");
    }

    #[test]
    fn the_palette_takes_the_body_so_its_summaries_are_readable() {
        // It used to draw into the WORKTREES pane, which is twenty-odd
        // columns wide: every summary came out truncated and a user command's
        // marker was cut off entirely. A command line needs the width.
        let mut s = connected();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("repo", 1)])),
            &mut s,
            &mut ui,
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);

        let screen = painted_dash(&s, &ui, 100, 26).join("\n");
        assert!(screen.contains("scan"), "{screen}");
        assert!(
            screen.contains("re-walk the workspace for repos"),
            "the summary is not truncated: {screen}"
        );
        // And it is the command line the mock draws: a prompt, and a count of
        // what is being chosen from.
        assert!(screen.contains("❯"), "{screen}");
        assert!(screen.contains("commands"), "{screen}");
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

    /// A short-lived directory under the real temp dir.
    ///
    /// Short on purpose: these tests bind unix sockets, and `sockaddr_un` runs
    /// out of room around 108 bytes. A helper that nested deeply would fail
    /// here for a reason that has nothing to do with what is being tested.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("grove-{label}-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&path).expect("a scratch directory");
            Self(path)
        }

        /// An executable that runs `body`, standing in for the daemon.
        fn fake_daemon(&self, body: &str) -> PathBuf {
            let path = self.0.join("fake-groved");
            // Written by a child process, never by this one. The tests run on
            // parallel threads, and a thread that forks while this process
            // holds the script open for writing hands that write handle to its
            // child until the child execs. Executing a file something still
            // has open for writing is ETXTBSY — "Text file busy" — and that
            // failed one run in thirty. A file this process never opens for
            // writing has no handle here to leak.
            let status = std::process::Command::new("sh")
                .args([
                    "-c",
                    r#"printf '%s\n' '#!/bin/sh' "$1" > "$2" && chmod 755 "$2""#,
                    "sh",
                    body,
                ])
                .arg(&path)
                .status()
                .expect("sh to write the fake daemon");
            assert!(status.success(), "the fake daemon could not be written");
            path
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_daemon_grove_starts_is_the_one_installed_beside_it() {
        // The pair speak a versioned protocol to each other. A $PATH that
        // finds some other groved pairs an upgraded TUI with a stale daemon,
        // which fails later and far less clearly than this.
        let scratch = Scratch::new("beside");
        let grove = scratch.join("grove");
        let groved = scratch.join("groved");
        std::fs::write(&groved, "").expect("a daemon to sit beside");
        assert_eq!(daemon_binary(Some(&grove), None), groved);
    }

    #[test]
    fn with_nothing_beside_it_the_daemon_comes_off_the_path() {
        // `cargo run`, or a half-installed pair. Bare, so exec searches $PATH
        // and reports for itself what it did or did not find.
        let scratch = Scratch::new("bare");
        assert_eq!(
            daemon_binary(Some(&scratch.join("grove")), None),
            PathBuf::from("groved")
        );
        assert_eq!(daemon_binary(None, None), PathBuf::from("groved"));
    }

    #[test]
    fn grove_daemon_names_the_daemon_outright() {
        let scratch = Scratch::new("explicit");
        let groved = scratch.join("groved");
        std::fs::write(&groved, "").expect("a daemon to sit beside");
        assert_eq!(
            daemon_binary(Some(&scratch.join("grove")), Some("/opt/groved".into())),
            PathBuf::from("/opt/groved"),
            "an explicit daemon must win over the one beside grove"
        );
    }

    #[test]
    fn the_daemon_is_told_which_workspace_and_left_in_its_own_process_group() {
        // ^C in the terminal running the TUI goes to the foreground process
        // group. The daemon holds every session's shells, so it must not be in
        // it: quitting grove is not meant to kill your work.
        let scratch = Scratch::new("pgid");
        let record = scratch.join("argv");
        // Both groups are read inside the fake, from the fake's own view: its
        // parent is the test process, so one `ps` answers both halves of the
        // question without this crate taking a libc dependency for it.
        let daemon = scratch.fake_daemon(&format!(
            "{{ echo \"$1 $2\"; ps -o pgid= -p $$; ps -o pgid= -p $PPID; }} > {} 2>&1; exit 3",
            record.display()
        ));
        let socket = scratch.join("s.sock");
        let started = start_daemon(&daemon, Path::new("/some/workspace"), &socket);
        assert!(started.is_err(), "the fake exits without binding anything");

        let said = std::fs::read_to_string(&record).expect("the fake must have run");
        let mut lines = said.lines();
        assert_eq!(
            lines.next().expect("argv"),
            "--workspace /some/workspace",
            "the daemon has to be told which workspace, or it serves the cwd"
        );
        let mut group = || -> i32 {
            lines
                .next()
                .expect("a process group id")
                .trim()
                .parse()
                .expect("a process group id")
        };
        let theirs = group();
        let ours = group();
        assert_ne!(
            theirs, ours,
            "the daemon must be in its own process group, not the TUI's"
        );
    }

    #[test]
    fn a_daemon_that_dies_reports_what_it_said() {
        // The daemon's output goes to a file, because by the time it fails the
        // terminal may be in raw mode or gone. That file is no use unless its
        // last words reach the person who typed `grove`.
        let scratch = Scratch::new("died");
        let daemon = scratch.fake_daemon("echo 'workspace is not a directory' >&2; exit 1");
        let socket = scratch.join("s.sock");
        let reason = start_daemon(&daemon, Path::new("/nowhere"), &socket)
            .expect_err("a daemon that exits 1 has not started");
        assert!(
            reason.contains("workspace is not a directory"),
            "the reason must carry what the daemon said: {reason}"
        );
        assert!(
            reason.contains("exited"),
            "and that it was the daemon that failed: {reason}"
        );
    }

    #[test]
    fn a_daemon_that_takes_the_socket_late_is_waited_for() {
        // groved binds before it walks the workspace, so this is usually
        // instant — but "usually" is not a contract, and a TUI that gave up
        // after one failed connect would race a daemon it started itself.
        let scratch = Scratch::new("late");
        let socket = scratch.join("s.sock");
        let daemon = scratch.fake_daemon("sleep 5");
        let binding = socket.clone();
        let listener = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            std::os::unix::net::UnixListener::bind(&binding).expect("bind")
        });
        let started = start_daemon(&daemon, Path::new("/w"), &socket);
        let _listener = listener.join().expect("the binding thread");
        assert!(
            started.is_ok(),
            "a socket that appears within the timeout must be connected to: {started:?}"
        );
    }

    #[test]
    fn a_daemon_that_is_already_running_is_not_started_again() {
        // The common case after the first run of the day. Starting a second
        // daemon would be harmless — it would find the socket taken and exit —
        // but paying for a process to discover that on every `grove` is not.
        let scratch = Scratch::new("running");
        let socket = scratch.join("s.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("a daemon");
        let reached = reach_daemon(Path::new("/w"), &socket);
        assert!(reached.is_ok(), "the live socket must be used: {reached:?}");
        assert!(
            !socket.with_extension("log").exists(),
            "nothing was started, so there is no daemon log to have written"
        );
    }
}
