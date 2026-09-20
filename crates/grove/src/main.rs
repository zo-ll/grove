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
mod empty;
mod endsession;
mod events;
mod help;
mod keymap;
mod palette;
mod prune;
mod repos;
mod select;
mod sessions;
mod statusbar;
mod terminal;
mod terminals;
mod text;
mod theme;
mod worktrees;

use std::io::Stdout;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use dash::Panes;
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
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use repos::Repos;
use sessions::Sessions;
use terminals::Terminals;
use theme::{Depth, Role, Theme};
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
        let body = Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height.saturating_sub(1),
        };
        match dash::split(body, self.panes).terminal {
            // Two columns and two rows of border.
            Some(rect) => (
                rect.height.saturating_sub(2).max(1),
                rect.width.saturating_sub(2).max(1),
            ),
            // Hidden. The pty keeps the size it had rather than taking the
            // whole body: a program inside it would otherwise reflow twice
            // around a `^g 3` to glance at something else, and vim redrawing
            // itself at two different widths is a worse cost than a grid that
            // is briefly the wrong size for a pane nobody is looking at.
            None => self.last_pane_size,
        }
    }

    /// How many lines the help has and how many fit, for the scroll.
    fn help_extent(&self) -> (usize, usize) {
        let screen = self.helping.unwrap_or(self.screen);
        (
            help::Help::lines(screen, &self.theme).len(),
            usize::from(self.height.saturating_sub(1)).max(1),
        )
    }

    /// Record the terminal pane's size while it is on screen, for the times
    /// it is not.
    fn remember_pane_size(&mut self) {
        let body = Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height.saturating_sub(1),
        };
        if let Some(rect) = dash::split(body, self.panes).terminal {
            self.last_pane_size = (
                rect.height.saturating_sub(2).max(1),
                rect.width.saturating_sub(2).max(1),
            );
        }
    }

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
            worktrees: Worktrees::default(),
            terminals: Terminals::default(),
            palette: Palette::default(),
            prune: Prune::default(),
            sessions: Sessions::default(),
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

/// Run the command the palette has selected, or say why it cannot yet.
///
/// Only the argumentless commands that already have a request behind them run
/// today. The rest are honest about it rather than closing the palette and
/// doing nothing: a command line that swallows `enter` teaches the user that
/// grove is unreliable, which is a harder thing to unlearn than a wait.
fn run_command(chosen: Option<&'static palette::Command>, state: &mut State, ui: &mut Ui) -> bool {
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

    let request = match command.name {
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

/// Carry out a command that has its argument.
///
/// Every one of these names the open session, because membership and
/// worktrees belong to a session rather than to the workspace. Checking a
/// non-member in `new`'s picker adds it first — §4.2 says picking one is how
/// it becomes a member, so the two requests go together rather than leaving
/// the user to do it twice.
fn confirm_argument(state: &mut State, ui: &mut Ui) -> bool {
    let Some(command) = ui.palette.arguing() else {
        return false;
    };
    let argument = ui.palette.argument().unwrap_or_default().to_string();
    let Some(session) = ui.session.clone() else {
        ui.note = Some(format!("no open session to {} in", command.name));
        return true;
    };

    let picked: Vec<select::Row> = ui
        .palette
        .select()
        .map(|select| select.checked().into_iter().cloned().collect())
        .unwrap_or_default();

    let mut requests: Vec<Request> = Vec::new();
    match command.name {
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
        "session new" => requests.push(Request::SessionNew {
            name: argument.trim().to_string(),
        }),
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
                            let (rows, cols) = ui.terminal_pane_size();
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
                Routed::Act(Action::OpenPicker) => {
                    // Ask before showing: a list of sessions that fills in
                    // underneath someone already pressing `X` is the worst
                    // possible version of this screen.
                    if let Err(e) = send(state, &Request::ListSessions) {
                        ui.note = Some(format!("could not reach the daemon: {e}"));
                    }
                    ui.screen = Screen::Picker;
                    Flow::Continue { redraw: true }
                }
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
            let (rows, cols) = ui.terminal_pane_size();
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
        Input::Terminal(_) => Flow::Continue { redraw: false },

        // The REPOS pane's data. Kept ahead of the catch-all below because
        // that one only narrates the event on the status line, which for a
        // list of repos would say a lot and show nothing.
        // One session's state moved. Ownership requests name the open session,
        // so a stale answer here adopts into the wrong one — which is worse
        // than the status line this event used to produce.
        Input::Daemon(DaemonEvent::SessionChanged(row)) => {
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
            let (rows, cols) = ui.terminal_pane_size();
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
    // One row reserved at the bottom for the status bar, per SPEC §4.1.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());
    let (body_area, bar_area) = (chunks[0], chunks[1]);

    // Help comes before every screen's own branch: it explains whichever one
    // is underneath, so it has to win the draw whatever that screen is.
    if let Some(explaining) = ui.helping
        && matches!(state, State::Connected { .. })
    {
        ui.help
            .render(f.buffer_mut(), body_area, explaining, &ui.theme);
        status_bar(f, bar_area, ui);
        return;
    }

    // The dash is a frame of panes rather than a paragraph, so it takes the
    // body whole. Everything the shell says about its own state — no daemon,
    // no connection yet — still goes through the block below, because those
    // are not screens and #22's empty state is about a workspace with nothing
    // in it, not about a daemon grove cannot reach.
    if matches!(state, State::Connected { .. }) && ui.screen == Screen::Dash {
        let empty = empty::Empty::of(ui.repos.workspace_count(), ui.repos.member_count());
        // §4.1's empty state draws two panes, not three: nothing is selected,
        // so there is no terminal to show, and the guidance needs the width
        // more than an empty box does. The user's own toggles are untouched —
        // this is what is drawn, not what they asked for.
        let panes = match empty {
            Some(_) => ui.panes.for_guidance(),
            None => ui.panes,
        };
        let inner = dash::render(f.buffer_mut(), body_area, panes, ui.focus, &ui.theme);
        if let Some(area) = inner.repos {
            ui.repos
                .render(f.buffer_mut(), area, &ui.theme, ui.focus == Focus::Repos);
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
                    ui.worktrees.render(
                        f.buffer_mut(),
                        area,
                        &ui.theme,
                        ui.focus == Focus::Worktrees,
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
        status_bar(f, bar_area, ui);
        return;
    }

    if matches!(state, State::Connected { .. }) && ui.screen == Screen::Shell {
        // The whole body: it is a terminal, not a pane, and §4.5 gives it the
        // screen rather than a corner of one.
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ui.theme.border())
            .border_style(ui.theme.style(Role::Muted))
            .title(Span::styled(" scratch ", ui.theme.style(Role::Accent)));
        let inner = block.inner(body_area);
        f.render_widget(block, body_area);
        ui.terminals
            .render(f.buffer_mut(), inner, &ui.theme, ui.scratch.is_some());
        status_bar(f, bar_area, ui);
        return;
    }

    if matches!(state, State::Connected { .. }) && ui.screen == Screen::EndSession {
        let inner = dash::render(f.buffer_mut(), body_area, ui.panes, ui.focus, &ui.theme);
        if let Some(area) = inner.worktrees.or(inner.repos).or(inner.terminal) {
            ui.ending.render(f.buffer_mut(), area, &ui.theme);
        }
        status_bar(f, bar_area, ui);
        return;
    }

    if matches!(state, State::Connected { .. }) && ui.screen == Screen::Picker {
        let inner = dash::render(f.buffer_mut(), body_area, ui.panes, ui.focus, &ui.theme);
        if let Some(area) = inner.worktrees.or(inner.repos).or(inner.terminal) {
            ui.sessions.render(f.buffer_mut(), area, &ui.theme);
        }
        status_bar(f, bar_area, ui);
        return;
    }

    if matches!(state, State::Connected { .. }) && ui.screen == Screen::Prune {
        let inner = dash::render(f.buffer_mut(), body_area, ui.panes, ui.focus, &ui.theme);
        if let Some(area) = inner.worktrees.or(inner.repos).or(inner.terminal) {
            ui.prune.render(f.buffer_mut(), area, &ui.theme);
        }
        status_bar(f, bar_area, ui);
        return;
    }

    if matches!(state, State::Connected { .. }) && ui.screen == Screen::Palette {
        // Over the dash rather than beside it: §4.2 calls it grove's command
        // line, and a command line that moves the screen under it makes the
        // thing you were looking at harder to act on.
        let inner = dash::render(f.buffer_mut(), body_area, ui.panes, ui.focus, &ui.theme);
        let over = inner.worktrees.or(inner.repos).or(inner.terminal);
        if let Some(area) = over {
            ui.palette.render(f.buffer_mut(), area, &ui.theme);
        }
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

/// The one row along the bottom, drawn the same way whatever is above it.
fn status_bar(f: &mut ratatui::Frame, area: Rect, ui: &Ui) {
    let context = if ui.router.prefix_pending() {
        "^g …".to_string()
    } else {
        String::new()
    };
    // A refusal outranks the config note: it is about the key just pressed,
    // and the config note has been true since startup.
    let note = ui.note.as_deref().or(ui.config_note.as_deref());
    f.render_widget(
        Paragraph::new(statusbar::render(
            ui.screen,
            ui.focus,
            &context,
            "no session",
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
    fn a_workspace_with_repos_but_no_members_gets_different_copy() {
        // Telling this user to scan sends them looking for a fault that is
        // not there — the repos are already found.
        let mut s = connected();
        let mut ui = Ui::new();
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
        assert_eq!(ui.palette.selected().map(|c| c.name), Some("scan"));

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
        assert_eq!(ui.palette.arguing().map(|c| c.name), Some("add"));
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
    fn the_palette_draws_over_the_dash() {
        let mut s = connected();
        let mut ui = Ui::new();
        handle(
            Input::Daemon(DaemonEvent::Repos(vec![repo_row("repo", 1)])),
            &mut s,
            &mut ui,
        );
        handle(prefix(), &mut s, &mut ui);
        handle(key(KeyCode::Char('/')), &mut s, &mut ui);

        let screen = painted_dash(&s, &ui, 100, 20).join("\n");
        assert!(
            screen.contains("scan"),
            "the command list is on screen: {screen}"
        );
        assert!(
            screen.contains("REPOS"),
            "over the dash, not instead of it: {screen}"
        );
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
