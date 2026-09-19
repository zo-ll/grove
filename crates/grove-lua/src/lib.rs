//! Evaluation of Grove's shared Lua configuration in process-specific VMs.

use mlua::{Function, Lua, MultiValue, RegistryKey, Table, Value};
use std::{
    fmt, fs,
    io::Read,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const DEFAULT_ACCENT: &str = "#fab387";
const DEFAULT_CLEAN: &str = "#a6e3a1";
const DEFAULT_DIRTY: &str = "#f9e2af";
const DEFAULT_ERROR: &str = "#f38ba8";
const DEFAULT_MUTED: &str = "#7f849c";
const DEFAULT_WORKTREE_PATH: &str = "~/grove/{repo}/{branch_slug}";
const MAX_BRANCH_SLUG_BYTES: usize = 120;
const BRANCH_SLUG_HASH_BYTES: usize = 16;

/// The result of loading a config, including a recoverable evaluation error.
pub struct LoadOutcome<T> {
    /// A fully usable runtime, containing defaults when evaluation failed.
    pub runtime: T,
    /// The error callers may surface without terminating the process.
    pub error: Option<ConfigError>,
}

/// A config loading or evaluation failure safe to display to the user.
#[derive(Debug)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns the message suitable for a status bar or log entry.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Colors used by the TUI to communicate status and emphasis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Color used for focus and primary accents.
    pub accent: String,
    /// Color used for clean worktrees and successful states.
    pub clean: String,
    /// Color used when a worktree has local changes.
    pub dirty: String,
    /// Color used for failures and destructive warnings.
    pub error: String,
    /// Color used for secondary and inactive text.
    pub muted: String,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: DEFAULT_ACCENT.into(),
            clean: DEFAULT_CLEAN.into(),
            dirty: DEFAULT_DIRTY.into(),
            error: DEFAULT_ERROR.into(),
            muted: DEFAULT_MUTED.into(),
        }
    }
}

/// Border treatment used around TUI panes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Corners {
    /// Curved glyphs visually soften pane boundaries.
    #[default]
    Rounded,
    /// Straight glyphs keep pane boundaries angular.
    Square,
}

/// Vertical spacing used when laying out TUI rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Density {
    /// Extra spacing favors readability.
    #[default]
    Airy,
    /// Reduced spacing favors information density.
    Compact,
}

/// Settings available to the TUI process and no daemon settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TuiConfig {
    /// Status colors used by the dashboard.
    pub theme: Theme,
    /// Border glyph style used by panes and overlays.
    pub corners: Corners,
    /// Row spacing used throughout the interface.
    pub density: Density,
}

/// A registered key binding and its Lua callback.
pub struct KeymapRegistration {
    /// Key sequence that invokes the callback.
    pub key: String,
    callback: RegistryKey,
}

/// A registered palette command and its Lua callback.
pub struct CommandRegistration {
    /// Command name entered in the palette.
    pub name: String,
    callback: RegistryKey,
}

/// A registered user-owned worktree column and its Lua callback.
pub struct ColumnRegistration {
    /// Heading used for the rendered column.
    pub name: String,
    callback: RegistryKey,
}

/// A reusable set of repositories offered when creating a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTemplate {
    /// Template name shown to the user.
    pub name: String,
    /// Repository names initially included by the template.
    pub repos: Vec<String>,
}

/// Registrations owned and executable only by the TUI VM.
#[derive(Default)]
pub struct TuiRegistrations {
    /// User-defined key bindings.
    pub keymaps: Vec<KeymapRegistration>,
    /// User-defined palette commands.
    pub commands: Vec<CommandRegistration>,
    /// User-defined worktree columns.
    pub columns: Vec<ColumnRegistration>,
    /// User-defined starting repository sets.
    pub session_templates: Vec<SessionTemplate>,
}

/// A TUI-owned Lua VM with only TUI settings and registrations exposed in Rust.
pub struct TuiRuntime {
    config: TuiConfig,
    registrations: TuiRegistrations,
    lua: Lua,
}

impl TuiRuntime {
    /// Loads a config file, treating a missing file as an error-free default config.
    pub fn load(path: impl AsRef<Path>) -> LoadOutcome<Self> {
        match fs::read_to_string(path.as_ref()) {
            Ok(source) => Self::load_source(&source, path.as_ref().to_string_lossy().as_ref()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => LoadOutcome {
                runtime: Self::defaults(),
                error: None,
            },
            Err(error) => LoadOutcome {
                runtime: Self::defaults(),
                error: Some(ConfigError::new(format!("could not read config: {error}"))),
            },
        }
    }

    /// Evaluates config source, replacing every partial result with defaults on error.
    pub fn load_source(source: &str, name: &str) -> LoadOutcome<Self> {
        match Self::evaluate(source, name) {
            Ok(runtime) => LoadOutcome {
                runtime,
                error: None,
            },
            Err(error) => LoadOutcome {
                runtime: Self::defaults(),
                error: Some(ConfigError::new(error.to_string())),
            },
        }
    }

    /// Returns the settings visible to the TUI.
    pub fn config(&self) -> &TuiConfig {
        &self.config
    }

    /// Returns the extension registrations owned by the TUI.
    pub fn registrations(&self) -> &TuiRegistrations {
        &self.registrations
    }

    /// Retrieves a keymap callback from this runtime's VM.
    pub fn keymap_callback(&self, index: usize) -> mlua::Result<Function> {
        self.lua
            .registry_value(&self.registrations.keymaps[index].callback)
    }

    /// Retrieves a command callback from this runtime's VM.
    pub fn command_callback(&self, index: usize) -> mlua::Result<Function> {
        self.lua
            .registry_value(&self.registrations.commands[index].callback)
    }

    /// Retrieves a column callback from this runtime's VM.
    pub fn column_callback(&self, index: usize) -> mlua::Result<Function> {
        self.lua
            .registry_value(&self.registrations.columns[index].callback)
    }

    fn defaults() -> Self {
        Self {
            config: TuiConfig::default(),
            registrations: TuiRegistrations::default(),
            lua: Lua::new(),
        }
    }

    fn evaluate(source: &str, name: &str) -> mlua::Result<Self> {
        let lua = Lua::new();
        let config = Arc::new(Mutex::new(Some(TuiConfig::default())));
        let registrations = Arc::new(Mutex::new(Some(TuiRegistrations::default())));
        install_tui_module(&lua, Arc::clone(&config), Arc::clone(&registrations))?;
        lua.load(source).set_name(name).exec()?;
        let config = config
            .lock()
            .expect("TUI config lock poisoned")
            .take()
            .ok_or_else(|| mlua::Error::runtime("config was already consumed"))?;
        let registrations = registrations
            .lock()
            .expect("TUI registrations lock poisoned")
            .take()
            .ok_or_else(|| mlua::Error::runtime("registrations were already consumed"))?;
        Ok(Self {
            config,
            registrations,
            lua,
        })
    }
}

/// A daemon worktree path computed from a template or a Lua function.
pub enum WorktreePath {
    /// A string expanded from Grove's documented placeholders.
    Template(String),
    /// A Lua function called with repository and branch names.
    Function(RegistryKey),
}

/// Settings available to the daemon process and no TUI settings.
pub struct DaemonConfig {
    /// Program spawned for worktree and scratch terminals.
    pub shell: String,
    /// Starting directory for the standalone scratch terminal.
    pub scratch_cwd: String,
    /// Maximum retained lines for each terminal.
    pub scrollback: usize,
    /// Rule used to choose newly-created worktree locations.
    pub worktree_path: WorktreePath,
    /// Text offered when the user starts naming a branch.
    pub branch_template: String,
    /// Age expression after which refs are marked stale.
    pub stale_after: String,
    /// Glob patterns excluded while discovering repositories.
    pub ignore: Vec<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
            scratch_cwd: std::env::var("HOME").unwrap_or_else(|_| "~".into()),
            scrollback: 10_000,
            worktree_path: WorktreePath::Template(DEFAULT_WORKTREE_PATH.into()),
            branch_template: "{type}/{ticket}-{slug}".into(),
            stale_after: "4h".into(),
            ignore: vec!["**/node_modules".into(), "**/.cache".into()],
        }
    }
}

impl WorktreePath {
    /// Reports whether adopting or releasing would require moving the worktree.
    pub fn contains_session_placeholder(&self) -> bool {
        matches!(self, Self::Template(template) if template.contains("{session}"))
    }
}

/// Values available while expanding a worktree path template.
pub struct WorktreePathContext<'a> {
    /// Repository display name.
    pub repo: &'a str,
    /// Original branch name.
    pub branch: &'a str,
    /// Session name, when a named session is open.
    pub session: &'a str,
    /// Parent directory containing the original checkout.
    pub clone_parent: &'a str,
}

/// A daemon lifecycle notification accepted by `grove.on`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// A new checkout has been created.
    WorktreeCreated,
    /// A checkout has been removed.
    WorktreeRemoved,
    /// An existing checkout has joined a session.
    WorktreeAdopted,
    /// A terminal process has been started.
    TerminalSpawned,
    /// A terminal process has stopped.
    TerminalExited,
    /// A session has become active.
    SessionOpened,
    /// A session's terminals have been stopped without removing worktrees.
    SessionClosed,
    /// A session and its owned worktrees have been removed.
    SessionEnded,
}

/// A lifecycle hook and the Lua callback run for it by the daemon.
pub struct LifecycleRegistration {
    /// Event that triggers this hook.
    pub event: LifecycleEvent,
    callback: RegistryKey,
    disabled: bool,
}

/// Stable data exposed to a lifecycle callback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecyclePayload {
    Worktree {
        repo: String,
        branch: String,
        path: PathBuf,
        clone: PathBuf,
        session: String,
    },
    Terminal {
        terminal: u64,
        repo: String,
        branch: String,
        path: PathBuf,
        session: String,
    },
    Session {
        id: String,
        name: String,
    },
}

/// Most `grove.run` jobs that may be in flight at once.
///
/// A hook is user code inside the daemon; without a cap, one loop exhausts the
/// daemon's threads, processes and file descriptors for everything else.
pub const MAX_CONCURRENT_JOBS: u64 = 16;

/// How long to keep draining output after the shell itself has exited.
///
/// A backgrounded grandchild can hold the pipe open indefinitely; the shell's
/// own output is already flushed by then, so this only needs to be long enough
/// to collect it.
pub const SH_DRAIN: Duration = Duration::from_millis(200);

/// How long `grove.sh` may block the daemon before its command is killed.
pub const SH_TIMEOUT: Duration = Duration::from_secs(10);

/// Signal a whole process group, best effort.
///
/// Directly, not through `/bin/kill`: the group signal is load-bearing — it is
/// what closes pipes a backgrounded grandchild is still holding — and an
/// external binary makes that depend on PATH and on the binary existing at all.
/// On a system without it the signal silently became a no-op and the freeze
/// came back, which is exactly the failure a best-effort call should not hide.
/// `killpg` is a safe function in `nix`, so this costs no `unsafe`.
fn signal_group(pgid: u32) {
    let Ok(pgid) = i32::try_from(pgid) else {
        return;
    };
    // Best effort by design: the group may already be gone, which is success
    // by another name.
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), nix::sys::signal::SIGTERM);
}

/// A contained hook error or the eventual result of an asynchronous command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookReport {
    HookDisabled {
        event: LifecycleEvent,
        registration: usize,
        message: String,
    },
    CommandFinished {
        job: u64,
        command: String,
        success: bool,
        message: String,
    },
}

type TerminalSender = Arc<dyn Fn(u64, &[u8]) -> Result<(), String> + Send + Sync>;

/// A daemon-owned Lua VM with only daemon settings and lifecycle hooks in Rust.
pub struct DaemonRuntime {
    config: DaemonConfig,
    lifecycle: Vec<LifecycleRegistration>,
    lua: Lua,
    reports: mpsc::Receiver<HookReport>,
    terminal_sender: Arc<Mutex<Option<TerminalSender>>>,
}

impl DaemonRuntime {
    /// Loads a config file, treating a missing file as an error-free default config.
    pub fn load(path: impl AsRef<Path>) -> LoadOutcome<Self> {
        match fs::read_to_string(path.as_ref()) {
            Ok(source) => Self::load_source(&source, path.as_ref().to_string_lossy().as_ref()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => LoadOutcome {
                runtime: Self::defaults(),
                error: None,
            },
            Err(error) => LoadOutcome {
                runtime: Self::defaults(),
                error: Some(ConfigError::new(format!("could not read config: {error}"))),
            },
        }
    }

    /// Evaluates config source, replacing every partial result with defaults on error.
    pub fn load_source(source: &str, name: &str) -> LoadOutcome<Self> {
        match Self::evaluate(source, name) {
            Ok(runtime) => LoadOutcome {
                runtime,
                error: None,
            },
            Err(error) => LoadOutcome {
                runtime: Self::defaults(),
                error: Some(ConfigError::new(error.to_string())),
            },
        }
    }

    /// Returns settings visible to the daemon.
    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }

    /// Returns lifecycle hooks owned by the daemon.
    pub fn lifecycle(&self) -> &[LifecycleRegistration] {
        &self.lifecycle
    }

    /// Retrieves a lifecycle callback from this runtime's VM.
    pub fn lifecycle_callback(&self, index: usize) -> mlua::Result<Function> {
        self.lua.registry_value(&self.lifecycle[index].callback)
    }

    /// Installs the daemon-owned terminal input operation used by `grove.send`.
    pub fn set_terminal_sender<F>(&mut self, sender: F)
    where
        F: Fn(u64, &[u8]) -> Result<(), String> + Send + Sync + 'static,
    {
        *self
            .terminal_sender
            .lock()
            .expect("terminal sender lock poisoned") = Some(Arc::new(sender));
    }

    /// Runs matching hooks in registration order. A failing hook is disabled
    /// before the next event, while later registrations still run.
    pub fn fire(&mut self, event: LifecycleEvent, payload: &LifecyclePayload) -> Vec<HookReport> {
        let mut reports = Vec::new();
        for index in 0..self.lifecycle.len() {
            if self.lifecycle[index].event != event || self.lifecycle[index].disabled {
                continue;
            }
            let result = self
                .lua
                .registry_value::<Function>(&self.lifecycle[index].callback)
                .and_then(|callback| callback.call::<()>(payload_table(&self.lua, payload)?));
            if let Err(error) = result {
                self.lifecycle[index].disabled = true;
                reports.push(HookReport::HookDisabled {
                    event,
                    registration: index,
                    message: error.to_string(),
                });
            }
        }
        reports
    }

    /// Returns completed asynchronous command reports without waiting.
    pub fn drain_reports(&self) -> Vec<HookReport> {
        self.reports.try_iter().collect()
    }

    /// Expands the configured template or invokes its Lua function.
    pub fn worktree_path(&self, context: WorktreePathContext<'_>) -> mlua::Result<String> {
        match &self.config.worktree_path {
            WorktreePath::Template(template) => Ok(expand_worktree_template(template, &context)),
            WorktreePath::Function(key) => self
                .lua
                .registry_value::<Function>(key)?
                .call((context.repo, context.branch)),
        }
    }

    fn defaults() -> Self {
        let (_, reports) = mpsc::channel();
        Self {
            config: DaemonConfig::default(),
            lifecycle: Vec::new(),
            lua: Lua::new(),
            reports,
            terminal_sender: Arc::new(Mutex::new(None)),
        }
    }

    fn evaluate(source: &str, name: &str) -> mlua::Result<Self> {
        let lua = Lua::new();
        let config = Arc::new(Mutex::new(Some(DaemonConfig::default())));
        let lifecycle = Arc::new(Mutex::new(Some(Vec::new())));
        let (report_sender, reports) = mpsc::channel();
        let terminal_sender = Arc::new(Mutex::new(None));
        install_daemon_module(
            &lua,
            Arc::clone(&config),
            Arc::clone(&lifecycle),
            report_sender,
            Arc::clone(&terminal_sender),
        )?;
        lua.load(source).set_name(name).exec()?;
        let config = config
            .lock()
            .expect("daemon config lock poisoned")
            .take()
            .ok_or_else(|| mlua::Error::runtime("config was already consumed"))?;
        let lifecycle = lifecycle
            .lock()
            .expect("daemon lifecycle lock poisoned")
            .take()
            .ok_or_else(|| mlua::Error::runtime("lifecycle hooks were already consumed"))?;
        Ok(Self {
            config,
            lifecycle,
            lua,
            reports,
            terminal_sender,
        })
    }
}

fn payload_table(lua: &Lua, payload: &LifecyclePayload) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    match payload {
        LifecyclePayload::Worktree {
            repo,
            branch,
            path,
            clone,
            session,
        } => {
            table.set("repo", repo.as_str())?;
            table.set("branch", branch.as_str())?;
            table.set("path", path.to_string_lossy().as_ref())?;
            table.set("clone", clone.to_string_lossy().as_ref())?;
            table.set("session", session.as_str())?;
        }
        LifecyclePayload::Terminal {
            terminal,
            repo,
            branch,
            path,
            session,
        } => {
            table.set("terminal", *terminal)?;
            table.set("repo", repo.as_str())?;
            table.set("branch", branch.as_str())?;
            table.set("path", path.to_string_lossy().as_ref())?;
            table.set("session", session.as_str())?;
        }
        LifecyclePayload::Session { id, name } => {
            table.set("id", id.as_str())?;
            table.set("name", name.as_str())?;
        }
    }
    Ok(table)
}

/// Converts a branch name into a deterministic, portable path component.
///
/// Normalization is intentionally not injective: for example, `feat/x` and `feat-x`
/// both become `feat-x`. The daemon must reject a worktree path already claimed by a
/// different branch. Results longer than 120 bytes are truncated and receive a stable
/// hash suffix so generated paths remain comfortably below common filesystem limits.
pub fn branch_slug(branch: &str) -> String {
    let mut slug = String::with_capacity(branch.len());
    let mut last_was_separator = false;
    for character in branch.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '.') {
            slug.push(character);
            last_was_separator = false;
        } else if !last_was_separator {
            slug.push('-');
            last_was_separator = true;
        }
    }
    let slug = slug.trim_matches(['-', '.']);
    let slug = if slug.is_empty() { "branch" } else { slug };
    if slug.len() <= MAX_BRANCH_SLUG_BYTES {
        slug.into()
    } else {
        let hash = branch
            .as_bytes()
            .iter()
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
            });
        let prefix_bytes = MAX_BRANCH_SLUG_BYTES - BRANCH_SLUG_HASH_BYTES - 1;
        format!("{}-{hash:016x}", &slug[..prefix_bytes])
    }
}

fn expand_worktree_template(template: &str, context: &WorktreePathContext<'_>) -> String {
    template
        .replace("{repo}", context.repo)
        .replace("{branch}", context.branch)
        .replace("{branch_slug}", &branch_slug(context.branch))
        .replace("{session}", context.session)
        .replace("{clone_parent}", context.clone_parent)
}

fn install_tui_module(
    lua: &Lua,
    config: Arc<Mutex<Option<TuiConfig>>>,
    registrations: Arc<Mutex<Option<TuiRegistrations>>>,
) -> mlua::Result<()> {
    let module = lua.create_table()?;

    let setup_config = Arc::clone(&config);
    module.set(
        "setup",
        lua.create_function(move |_, table: Table| {
            let mut config = setup_config.lock().expect("TUI config lock poisoned");
            apply_tui_setup(
                config
                    .as_mut()
                    .ok_or_else(|| mlua::Error::runtime("config is unavailable"))?,
                table,
            )
        })?,
    )?;

    let keymaps = Arc::clone(&registrations);
    module.set(
        "keymap",
        lua.create_function(move |lua, (key, callback): (String, Function)| {
            keymaps
                .lock()
                .expect("TUI registrations lock poisoned")
                .as_mut()
                .ok_or_else(|| mlua::Error::runtime("registrations are unavailable"))?
                .keymaps
                .push(KeymapRegistration {
                    key,
                    callback: lua.create_registry_value(callback)?,
                });
            Ok(())
        })?,
    )?;
    let commands = Arc::clone(&registrations);
    module.set(
        "command",
        lua.create_function(move |lua, (name, callback): (String, Function)| {
            commands
                .lock()
                .expect("TUI registrations lock poisoned")
                .as_mut()
                .ok_or_else(|| mlua::Error::runtime("registrations are unavailable"))?
                .commands
                .push(CommandRegistration {
                    name,
                    callback: lua.create_registry_value(callback)?,
                });
            Ok(())
        })?,
    )?;
    let columns = Arc::clone(&registrations);
    module.set(
        "column",
        lua.create_function(move |lua, (name, callback): (String, Function)| {
            columns
                .lock()
                .expect("TUI registrations lock poisoned")
                .as_mut()
                .ok_or_else(|| mlua::Error::runtime("registrations are unavailable"))?
                .columns
                .push(ColumnRegistration {
                    name,
                    callback: lua.create_registry_value(callback)?,
                });
            Ok(())
        })?,
    )?;
    let templates = Arc::clone(&registrations);
    module.set(
        "session_template",
        lua.create_function(move |_, (name, value): (String, Table)| {
            templates
                .lock()
                .expect("TUI registrations lock poisoned")
                .as_mut()
                .ok_or_else(|| mlua::Error::runtime("registrations are unavailable"))?
                .session_templates
                .push(SessionTemplate {
                    name,
                    repos: value.get("repos")?,
                });
            Ok(())
        })?,
    )?;

    module.set("on", lua.create_function(noop)?)?;
    module.set("repo", lua.create_function(noop)?)?;
    install_loaded_module(lua, module)
}

fn install_daemon_module(
    lua: &Lua,
    config: Arc<Mutex<Option<DaemonConfig>>>,
    lifecycle: Arc<Mutex<Option<Vec<LifecycleRegistration>>>>,
    reports: mpsc::Sender<HookReport>,
    terminal_sender: Arc<Mutex<Option<TerminalSender>>>,
) -> mlua::Result<()> {
    let module = lua.create_table()?;

    let setup_config = Arc::clone(&config);
    module.set(
        "setup",
        lua.create_function(move |lua, table: Table| {
            let mut config = setup_config.lock().expect("daemon config lock poisoned");
            apply_daemon_setup(
                lua,
                config
                    .as_mut()
                    .ok_or_else(|| mlua::Error::runtime("config is unavailable"))?,
                table,
            )
        })?,
    )?;
    module.set(
        "on",
        lua.create_function(move |lua, (event, callback): (String, Function)| {
            lifecycle
                .lock()
                .expect("daemon lifecycle lock poisoned")
                .as_mut()
                .ok_or_else(|| mlua::Error::runtime("lifecycle hooks are unavailable"))?
                .push(LifecycleRegistration {
                    event: parse_event(&event)?,
                    callback: lua.create_registry_value(callback)?,
                    disabled: false,
                });
            Ok(())
        })?,
    )?;

    let next_job = Arc::new(AtomicU64::new(1));
    // A hook is user code running inside the daemon, so an unbounded spawn is a
    // way to take the daemon down by accident: `while true do grove.run(..) end`
    // costs a thread, a /bin/sh child and two pipe descriptors per iteration.
    // Refusing past the cap keeps a runaway hook local to itself.
    let in_flight = Arc::new(AtomicU64::new(0));
    module.set(
        "run",
        lua.create_function(move |_, (cwd, command): (String, String)| {
            if in_flight.load(Ordering::SeqCst) >= MAX_CONCURRENT_JOBS {
                return Err(mlua::Error::runtime(format!(
                    "grove.run: {MAX_CONCURRENT_JOBS} jobs already running; \
                     this one was refused rather than exhausting the daemon"
                )));
            }
            in_flight.fetch_add(1, Ordering::SeqCst);

            let job = next_job.fetch_add(1, Ordering::Relaxed);
            let reports = reports.clone();
            let reported_command = command.clone();
            let running = Arc::clone(&in_flight);
            thread::spawn(move || {
                let result = Command::new("/bin/sh")
                    .args(["-lc", &command])
                    .current_dir(cwd)
                    .output();
                let (success, message) = match result {
                    Ok(output) => {
                        let message = if output.status.success() {
                            String::from_utf8_lossy(&output.stdout).trim().to_owned()
                        } else {
                            String::from_utf8_lossy(&output.stderr).trim().to_owned()
                        };
                        (output.status.success(), message)
                    }
                    Err(error) => (false, error.to_string()),
                };
                // Released whatever happened, so a failing command cannot leak
                // a slot and slowly wedge the cap shut.
                running.fetch_sub(1, Ordering::SeqCst);
                let _ = reports.send(HookReport::CommandFinished {
                    job,
                    command: reported_command,
                    success,
                    message,
                });
            });
            Ok(job)
        })?,
    )?;
    module.set(
        "sh",
        lua.create_function(|_, command: String| {
            // Hooks fire while the daemon holds its service lock, so this blocks
            // every other request for the command's duration. Acceptable for
            // reading a value, disastrous for anything slow — hence the bound.
            //
            // Bounding the *shell* is not enough. A backgrounded grandchild
            // (`grove.sh("npm run dev &")`) inherits the pipe's write end, so
            // the shell exits in milliseconds and reading stdout to EOF then
            // blocks until the grandchild does. An earlier version used
            // `wait_with_output` and froze the daemon for the grandchild's
            // lifetime — the very freeze the timeout was added to prevent,
            // reached by a different door.
            //
            // So: its own process group, output drained on threads that cannot
            // hold us, and on timeout the whole group is signalled rather than
            // just the shell.
            let mut child = Command::new("/bin/sh")
                .args(["-lc", &command])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .spawn()
                .map_err(mlua::Error::external)?;

            let pgid = child.id();
            // Channels, not `JoinHandle`s. A reader blocked on a pipe some
            // grandchild still holds never returns, and `join` has no deadline,
            // so joining is the freeze again one line further down. A channel
            // can be given one: whatever arrived by the time the window closes
            // is the output, and the reader finishes detached or not at all.
            let mut out = child.stdout.take();
            let mut err = child.stderr.take();
            let (out_tx, out_rx) = mpsc::channel();
            let (err_tx, err_rx) = mpsc::channel();
            thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(pipe) = out.as_mut() {
                    let _ = pipe.read_to_end(&mut buf);
                }
                let _ = out_tx.send(buf);
            });
            thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(pipe) = err.as_mut() {
                    let _ = pipe.read_to_end(&mut buf);
                }
                let _ = err_tx.send(buf);
            });

            let deadline = Instant::now() + SH_TIMEOUT;
            let mut exited_at: Option<Instant> = None;
            let mut out_buf: Option<Vec<u8>> = None;
            let mut err_buf: Option<Vec<u8>> = None;
            let status = loop {
                if out_buf.is_none() {
                    out_buf = out_rx.try_recv().ok();
                }
                if err_buf.is_none() {
                    err_buf = err_rx.try_recv().ok();
                }
                match child.try_wait().map_err(mlua::Error::external)? {
                    Some(status) if out_buf.is_some() && err_buf.is_some() => break status,
                    // The shell is gone but a pipe is still held, so something
                    // it spawned outlives it. Its own output has long since
                    // flushed; waiting the full timeout for a background job
                    // that may run for hours is the freeze in slow motion.
                    // Give the readers a moment, then close the pipes by
                    // signalling the group and take what arrived.
                    Some(status) if exited_at.is_some_and(|at| at.elapsed() >= SH_DRAIN) => {
                        signal_group(pgid);
                        break status;
                    }
                    Some(_) => {
                        exited_at.get_or_insert_with(Instant::now);
                        thread::sleep(Duration::from_millis(10));
                    }
                    _ if Instant::now() >= deadline => {
                        // Signal the group: killing only the shell leaves its
                        // children running and still holding the pipes.
                        signal_group(pgid);
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(mlua::Error::runtime(format!(
                            "grove.sh: {command:?} exceeded {}s and its process group was killed; \
                             grove.sh blocks the daemon, so use grove.run for slow commands",
                            SH_TIMEOUT.as_secs()
                        )));
                    }
                    None => thread::sleep(Duration::from_millis(10)),
                }
            };

            // One last window — shared, not one each, so the two waits cannot
            // add up. Output written just before the signal still counts; past
            // the window the read is abandoned rather than waited on, because
            // the whole point of the group signal is that the caller is already
            // free, and a pipe-holder that ignored the signal — or a signal that
            // never landed — must not be able to take that back.
            //
            // So the bound on grove.sh is SH_TIMEOUT plus two drain windows:
            // the in-loop one that decides the shell's children are outliving
            // it, and this one.
            let drain_until = Instant::now() + SH_DRAIN;
            let take = |buf: Option<Vec<u8>>, rx: &mpsc::Receiver<Vec<u8>>| match buf {
                Some(buf) => buf,
                None => rx
                    .recv_timeout(drain_until.saturating_duration_since(Instant::now()))
                    .unwrap_or_default(),
            };
            let out = take(out_buf, &out_rx);
            let err = take(err_buf, &err_rx);
            if !status.success() {
                return Err(mlua::Error::runtime(
                    String::from_utf8_lossy(&err).trim().to_owned(),
                ));
            }
            Ok(String::from_utf8_lossy(&out).trim().to_owned())
        })?,
    )?;
    module.set(
        "copy",
        lua.create_function(|_, (from, to): (String, String)| {
            fs::copy(from, to)
                .map(|_| ())
                .map_err(mlua::Error::external)
        })?,
    )?;
    module.set(
        "exists",
        lua.create_function(|_, path: String| Ok(Path::new(&path).exists()))?,
    )?;
    module.set(
        "send",
        lua.create_function(move |_, (terminal, bytes): (u64, mlua::String)| {
            let sender = terminal_sender
                .lock()
                .map_err(|_| mlua::Error::runtime("terminal sender lock poisoned"))?
                .clone()
                .ok_or_else(|| mlua::Error::runtime("terminal sender is unavailable"))?;
            sender(terminal, bytes.as_bytes().as_ref()).map_err(mlua::Error::runtime)
        })?,
    )?;

    for name in ["keymap", "command", "column", "session_template", "repo"] {
        module.set(name, lua.create_function(noop)?)?;
    }
    install_loaded_module(lua, module)
}

fn install_loaded_module(lua: &Lua, module: Table) -> mlua::Result<()> {
    let package: Table = lua.globals().get("package")?;
    let loaded: Table = package.get("loaded")?;
    loaded.set("grove", module)
}

fn noop(_: &Lua, _: MultiValue) -> mlua::Result<()> {
    Ok(())
}

fn apply_tui_setup(config: &mut TuiConfig, table: Table) -> mlua::Result<()> {
    if let Some(theme) = optional_table(&table, "theme")? {
        set_optional(&theme, "accent", &mut config.theme.accent)?;
        set_optional(&theme, "clean", &mut config.theme.clean)?;
        set_optional(&theme, "dirty", &mut config.theme.dirty)?;
        set_optional(&theme, "error", &mut config.theme.error)?;
        set_optional(&theme, "muted", &mut config.theme.muted)?;
        if let Some(corners) = optional_string(&theme, "corners")? {
            config.corners = parse_corners(&corners)?;
        }
        if let Some(density) = optional_string(&theme, "density")? {
            config.density = parse_density(&density)?;
        }
    }
    if let Some(corners) = optional_string(&table, "corners")? {
        config.corners = parse_corners(&corners)?;
    }
    if let Some(density) = optional_string(&table, "density")? {
        config.density = parse_density(&density)?;
    }
    Ok(())
}

fn apply_daemon_setup(lua: &Lua, config: &mut DaemonConfig, table: Table) -> mlua::Result<()> {
    set_optional(&table, "shell", &mut config.shell)?;
    set_optional(&table, "scratch_cwd", &mut config.scratch_cwd)?;
    set_optional(&table, "scrollback", &mut config.scrollback)?;
    set_optional(&table, "branch_template", &mut config.branch_template)?;
    set_optional(&table, "stale_after", &mut config.stale_after)?;
    set_optional(&table, "ignore", &mut config.ignore)?;

    match table.get::<Value>("worktree_path")? {
        Value::Nil => {}
        Value::String(value) => {
            config.worktree_path = WorktreePath::Template(value.to_str()?.to_string())
        }
        Value::Function(function) => {
            config.worktree_path = WorktreePath::Function(lua.create_registry_value(function)?);
        }
        value => {
            return Err(mlua::Error::runtime(format!(
                "worktree_path must be a string or function, got {}",
                value.type_name()
            )));
        }
    }
    Ok(())
}

fn optional_table(table: &Table, key: &str) -> mlua::Result<Option<Table>> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::Table(value) => Ok(Some(value)),
        value => Err(mlua::Error::runtime(format!(
            "{key} must be a table, got {}",
            value.type_name()
        ))),
    }
}

fn optional_string(table: &Table, key: &str) -> mlua::Result<Option<String>> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::String(value) => Ok(Some(value.to_str()?.to_string())),
        value => Err(mlua::Error::runtime(format!(
            "{key} must be a string, got {}",
            value.type_name()
        ))),
    }
}

fn set_optional<T: mlua::FromLua>(table: &Table, key: &str, target: &mut T) -> mlua::Result<()> {
    if let Value::Nil = table.get::<Value>(key)? {
        return Ok(());
    }
    *target = table.get(key)?;
    Ok(())
}

fn parse_corners(value: &str) -> mlua::Result<Corners> {
    match value {
        "rounded" => Ok(Corners::Rounded),
        "square" => Ok(Corners::Square),
        _ => Err(mlua::Error::runtime(format!(
            "corners must be 'rounded' or 'square', got {value:?}"
        ))),
    }
}

fn parse_density(value: &str) -> mlua::Result<Density> {
    match value {
        "airy" => Ok(Density::Airy),
        "compact" => Ok(Density::Compact),
        _ => Err(mlua::Error::runtime(format!(
            "density must be 'airy' or 'compact', got {value:?}"
        ))),
    }
}

fn parse_event(value: &str) -> mlua::Result<LifecycleEvent> {
    match value {
        "worktree_created" => Ok(LifecycleEvent::WorktreeCreated),
        "worktree_removed" => Ok(LifecycleEvent::WorktreeRemoved),
        "worktree_adopted" => Ok(LifecycleEvent::WorktreeAdopted),
        "terminal_spawned" => Ok(LifecycleEvent::TerminalSpawned),
        "terminal_exited" => Ok(LifecycleEvent::TerminalExited),
        "session_opened" => Ok(LifecycleEvent::SessionOpened),
        "session_closed" => Ok(LifecycleEvent::SessionClosed),
        "session_ended" => Ok(LifecycleEvent::SessionEnded),
        _ => Err(mlua::Error::runtime(format!(
            "unknown lifecycle event {value:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "grove-lua-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    const SHARED_CONFIG: &str = r##"
        local grove = require("grove")
        grove.setup({
            shell = "/usr/bin/fish",
            scratch_cwd = "/tmp",
            scrollback = 42,
            worktree_path = "{clone_parent}/trees/{repo}/{branch_slug}/{session}",
            branch_template = "feat/{slug}",
            stale_after = "2h",
            ignore = { "vendor" },
            theme = {
                accent = "#ffffff",
                clean = "#00ff00",
                dirty = "#ffff00",
                error = "#ff0000",
                muted = "#888888",
                corners = "square",
                density = "compact",
            },
        })
        grove.keymap("^g w", function() return "key" end)
        grove.command("review", function() return "command" end)
        grove.column("pr", function() return "column" end)
        grove.session_template("frontend", { repos = { "web-app", "design-system" } })
        grove.on("worktree_created", function() return "event" end)
    "##;

    #[test]
    fn tui_loads_only_its_surface_and_accepts_daemon_entries() {
        let loaded = TuiRuntime::load_source(SHARED_CONFIG, "test-config");
        assert!(loaded.error.is_none());
        assert_eq!(loaded.runtime.config().theme.accent, "#ffffff");
        assert_eq!(loaded.runtime.config().corners, Corners::Square);
        assert_eq!(loaded.runtime.config().density, Density::Compact);
        assert_eq!(loaded.runtime.registrations().keymaps.len(), 1);
        assert_eq!(loaded.runtime.registrations().commands.len(), 1);
        assert_eq!(loaded.runtime.registrations().columns.len(), 1);
        assert_eq!(loaded.runtime.registrations().session_templates.len(), 1);
        assert_eq!(
            loaded.runtime.registrations().session_templates[0].repos,
            ["web-app", "design-system"]
        );
        assert_eq!(
            loaded
                .runtime
                .keymap_callback(0)
                .unwrap()
                .call::<String>(())
                .unwrap(),
            "key"
        );
    }

    #[test]
    fn daemon_loads_only_its_surface_and_accepts_tui_registrations() {
        let loaded = DaemonRuntime::load_source(SHARED_CONFIG, "test-config");
        assert!(loaded.error.is_none());
        assert_eq!(loaded.runtime.config().shell, "/usr/bin/fish");
        assert_eq!(loaded.runtime.config().scrollback, 42);
        assert_eq!(loaded.runtime.config().ignore, ["vendor"]);
        assert_eq!(loaded.runtime.lifecycle().len(), 1);
        assert_eq!(
            loaded.runtime.lifecycle()[0].event,
            LifecycleEvent::WorktreeCreated
        );
        assert!(
            loaded
                .runtime
                .config()
                .worktree_path
                .contains_session_placeholder()
        );
        assert_eq!(
            loaded
                .runtime
                .worktree_path(WorktreePathContext {
                    repo: "billing-service",
                    branch: "feat/ABC-4471-invoice-split",
                    session: "invoice-split",
                    clone_parent: "/code",
                })
                .unwrap(),
            "/code/trees/billing-service/feat-ABC-4471-invoice-split/invoice-split"
        );
    }

    #[test]
    fn function_worktree_path_is_evaluated_in_the_daemon_vm() {
        let loaded = DaemonRuntime::load_source(
            r#"
                local grove = require("grove")
                grove.setup({ worktree_path = function(repo, branch)
                    return "/mnt/" .. repo .. "/" .. branch
                end })
            "#,
            "function-config",
        );
        assert!(loaded.error.is_none());
        assert_eq!(
            loaded
                .runtime
                .worktree_path(WorktreePathContext {
                    repo: "monorepo",
                    branch: "main",
                    session: "unused",
                    clone_parent: "/code",
                })
                .unwrap(),
            "/mnt/monorepo/main"
        );
    }

    #[test]
    fn branch_slug_is_deterministic_and_filesystem_safe() {
        assert_eq!(
            branch_slug("feat/ABC-4471-invoice-split"),
            "feat-ABC-4471-invoice-split"
        );
        assert_eq!(branch_slug("../../bad branch"), "bad-branch");
        assert_eq!(branch_slug("///"), "branch");

        assert_eq!(branch_slug("feat/x"), branch_slug("feat-x"));

        let long_branch = format!("feat/{}", "a".repeat(300));
        let similar_long_branch = format!("feat/{}b", "a".repeat(300));
        let long_slug = branch_slug(&long_branch);
        assert_eq!(long_slug, branch_slug(&long_branch));
        assert_eq!(long_slug.len(), MAX_BRANCH_SLUG_BYTES);
        assert_ne!(long_slug, branch_slug(&similar_long_branch));
    }

    #[test]
    fn any_evaluation_error_discards_all_partial_state() {
        let loaded = TuiRuntime::load_source(
            r##"
                local grove = require("grove")
                grove.setup({ theme = { accent = "#ffffff" } })
                grove.keymap("x", function() end)
                error("broken")
            "##,
            "broken-config",
        );
        assert!(loaded.error.is_some());
        assert_eq!(loaded.runtime.config(), &TuiConfig::default());
        assert!(loaded.runtime.registrations().keymaps.is_empty());
    }

    #[test]
    fn registration_errors_also_discard_all_partial_state() {
        let loaded = DaemonRuntime::load_source(
            r#"
                local grove = require("grove")
                grove.setup({ shell = "/bin/fish" })
                grove.on("not_an_event", function() end)
            "#,
            "broken-registration",
        );
        assert!(loaded.error.is_some());
        assert_eq!(loaded.runtime.config().shell, DaemonConfig::default().shell);
        assert!(loaded.runtime.lifecycle().is_empty());
    }

    #[test]
    fn missing_file_is_defaults_without_an_error() {
        let path = std::env::temp_dir().join(format!(
            "grove-missing-config-{}-{}.lua",
            std::process::id(),
            line!()
        ));
        let loaded = TuiRuntime::load(path);
        assert!(loaded.error.is_none());
        assert_eq!(loaded.runtime.config(), &TuiConfig::default());
    }

    #[test]
    fn lifecycle_events_expose_stable_payloads_in_registration_order() {
        let output = temp_path("events");
        let source = format!(
            r#"
            local grove = require("grove")
            local function record(name)
              return function(value)
                local file = assert(io.open({output:?}, "a"))
                file:write(name .. ":" .. (value.repo or value.id) .. ":" .. (value.branch or value.name) .. "\n")
                file:close()
              end
            end
            grove.on("worktree_created", record("created-1"))
            grove.on("worktree_created", record("created-2"))
            grove.on("worktree_removed", record("removed"))
            grove.on("worktree_adopted", record("adopted"))
            grove.on("terminal_spawned", record("spawned"))
            grove.on("terminal_exited", record("exited"))
            grove.on("session_opened", record("opened"))
            grove.on("session_closed", record("closed"))
            grove.on("session_ended", record("ended"))
            "#,
            output = output.to_string_lossy()
        );
        let mut runtime = DaemonRuntime::load_source(&source, "events").runtime;
        let worktree = LifecyclePayload::Worktree {
            repo: "api".into(),
            branch: "feat/hooks".into(),
            path: "/trees/api".into(),
            clone: "/code/api".into(),
            session: "session-1".into(),
        };
        let terminal = LifecyclePayload::Terminal {
            terminal: 7,
            repo: "api".into(),
            branch: "feat/hooks".into(),
            path: "/trees/api".into(),
            session: "session-1".into(),
        };
        let session = LifecyclePayload::Session {
            id: "session-1".into(),
            name: "hooks".into(),
        };
        for event in [
            LifecycleEvent::WorktreeCreated,
            LifecycleEvent::WorktreeRemoved,
            LifecycleEvent::WorktreeAdopted,
        ] {
            assert!(runtime.fire(event, &worktree).is_empty());
        }
        for event in [
            LifecycleEvent::TerminalSpawned,
            LifecycleEvent::TerminalExited,
        ] {
            assert!(runtime.fire(event, &terminal).is_empty());
        }
        for event in [
            LifecycleEvent::SessionOpened,
            LifecycleEvent::SessionClosed,
            LifecycleEvent::SessionEnded,
        ] {
            assert!(runtime.fire(event, &session).is_empty());
        }
        assert_eq!(
            fs::read_to_string(&output).unwrap(),
            "created-1:api:feat/hooks\ncreated-2:api:feat/hooks\nremoved:api:feat/hooks\nadopted:api:feat/hooks\nspawned:api:feat/hooks\nexited:api:feat/hooks\nopened:session-1:hooks\nclosed:session-1:hooks\nended:session-1:hooks\n"
        );
        let _ = fs::remove_file(output);
    }

    #[test]
    fn throwing_hook_is_disabled_without_skipping_later_registrations() {
        let output = temp_path("contained");
        let source = format!(
            r#"
            local grove = require("grove")
            grove.on("session_opened", function() error("broken hook") end)
            grove.on("session_opened", function()
              local file = assert(io.open({output:?}, "a")); file:write("ok\n"); file:close()
            end)
            "#,
            output = output.to_string_lossy()
        );
        let mut runtime = DaemonRuntime::load_source(&source, "contained").runtime;
        let payload = LifecyclePayload::Session {
            id: "one".into(),
            name: "one".into(),
        };
        let reports = runtime.fire(LifecycleEvent::SessionOpened, &payload);
        assert!(matches!(
            reports.as_slice(),
            [HookReport::HookDisabled { .. }]
        ));
        assert!(
            runtime
                .fire(LifecycleEvent::SessionOpened, &payload)
                .is_empty()
        );
        assert_eq!(fs::read_to_string(&output).unwrap(), "ok\nok\n");
        let _ = fs::remove_file(output);
    }

    #[test]
    fn run_refuses_past_the_concurrency_cap_rather_than_exhausting_the_daemon() {
        // A hook is user code inside the daemon. Without a cap, `while true do
        // grove.run(..) end` costs a thread, a /bin/sh child and two pipe
        // descriptors per iteration until the daemon cannot serve anything.
        let source = format!(
            "local grove = require('grove')\n\
             grove.setup({{}})\n\
             grove.on('worktree_created', function(wt)\n\
               for _ = 1, {} do grove.run(wt.path, 'sleep 5') end\n\
             end)\n",
            MAX_CONCURRENT_JOBS + 8
        );
        let mut runtime = DaemonRuntime::load_source(&source, "cap").runtime;
        let reports = runtime.fire(
            LifecycleEvent::WorktreeCreated,
            &LifecyclePayload::Worktree {
                repo: "r".into(),
                branch: "b".into(),
                path: std::env::temp_dir(),
                clone: std::env::temp_dir(),
                session: "s".into(),
            },
        );
        // The hook is disabled at the point of refusal, and the refusal says
        // why rather than failing obscurely.
        assert!(
            reports.iter().any(|r| matches!(
                r,
                HookReport::HookDisabled { message, .. } if message.contains("already running")
            )),
            "expected a refusal past the cap, got {reports:?}"
        );
    }

    #[test]
    fn sh_still_returns_normal_output_promptly() {
        // The drain window must not cost a fast command its result.
        let source = "local grove = require('grove')\n\
                      grove.setup({})\n\
                      grove.on('session_opened', function()\n\
                        local v = grove.sh('printf hello')\n\
                        if v ~= 'hello' then error('got: ' .. tostring(v)) end\n\
                      end)\n";
        let mut runtime = DaemonRuntime::load_source(source, "sh-ok").runtime;
        let started = Instant::now();
        let reports = runtime.fire(
            LifecycleEvent::SessionOpened,
            &LifecyclePayload::Session {
                id: "s".into(),
                name: "s".into(),
            },
        );
        assert!(
            reports.is_empty(),
            "a working command must not report a failure: {reports:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn sh_does_not_block_on_a_backgrounded_grandchild_holding_the_pipe() {
        // The shell exits in milliseconds; a backgrounded child inherits the
        // pipe's write end, so reading stdout to EOF waits for *it*. Bounding
        // only the shell left the daemon frozen for the grandchild's lifetime.
        let source = "local grove = require('grove')\n\
                      grove.setup({})\n\
                      grove.on('session_opened', function() grove.sh('sleep 30 &') end)\n";
        let mut runtime = DaemonRuntime::load_source(source, "grandchild").runtime;
        let started = Instant::now();
        let _ = runtime.fire(
            LifecycleEvent::SessionOpened,
            &LifecyclePayload::Session {
                id: "s".into(),
                name: "s".into(),
            },
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < SH_TIMEOUT + Duration::from_secs(2),
            "grove.sh blocked {elapsed:?} on a backgrounded grandchild; \
             the shell exits at once, so anything near 30s means the pipe held us"
        );
    }

    #[test]
    fn sh_does_not_block_on_a_grandchild_that_ignores_the_signal() {
        // Closing the pipes by signalling the group only works if the group
        // takes the signal. A hook that traps TERM — or a system where the
        // signal never lands at all — must still not freeze the daemon, so the
        // drain window has to bound the read as well as the signal.
        let source = "local grove = require('grove')\n\
                      grove.setup({})\n\
                      grove.on('session_opened', function() grove.sh('trap \"\" TERM; sleep 30 &') end)\n";
        let mut runtime = DaemonRuntime::load_source(source, "deaf-grandchild").runtime;
        let started = Instant::now();
        let _ = runtime.fire(
            LifecycleEvent::SessionOpened,
            &LifecyclePayload::Session {
                id: "s".into(),
                name: "s".into(),
            },
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < SH_TIMEOUT + Duration::from_secs(2),
            "grove.sh blocked {elapsed:?} on a grandchild that ignored the group signal; \
             the read must be bounded by the drain window, not by the signal working"
        );
    }

    #[test]
    fn sh_is_bounded_so_a_hanging_command_cannot_freeze_the_daemon() {
        // Hooks fire while the daemon holds its service lock, so an unbounded
        // grove.sh blocks every other request for as long as the command runs.
        let source = "local grove = require('grove')\n\
                      grove.setup({})\n\
                      grove.on('session_opened', function() grove.sh('sleep 120') end)\n";
        let mut runtime = DaemonRuntime::load_source(source, "sh-timeout").runtime;
        let started = Instant::now();
        let reports = runtime.fire(
            LifecycleEvent::SessionOpened,
            &LifecyclePayload::Session {
                id: "s".into(),
                name: "s".into(),
            },
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < SH_TIMEOUT + Duration::from_secs(5),
            "grove.sh blocked for {elapsed:?}; the bound did not apply"
        );
        assert!(
            reports.iter().any(|r| matches!(
                r,
                HookReport::HookDisabled { message, .. } if message.contains("exceeded")
            )),
            "expected a timeout report, got {reports:?}"
        );
    }

    #[test]
    fn run_is_async_and_reports_completion() {
        let directory = temp_path("async-dir");
        fs::create_dir_all(&directory).unwrap();
        let source = format!(
            r#"local grove = require("grove"); grove.on("session_opened", function()
                grove.run({directory:?}, "sleep 1; printf done > completed")
            end)"#,
            directory = directory.to_string_lossy()
        );
        let mut runtime = DaemonRuntime::load_source(&source, "async").runtime;
        let started = Instant::now();
        assert!(
            runtime
                .fire(
                    LifecycleEvent::SessionOpened,
                    &LifecyclePayload::Session {
                        id: "one".into(),
                        name: "one".into()
                    }
                )
                .is_empty()
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        let deadline = Instant::now() + Duration::from_secs(3);
        let report = loop {
            if let Some(report) = runtime.drain_reports().into_iter().next() {
                break report;
            }
            assert!(
                Instant::now() < deadline,
                "async command did not report completion"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(matches!(
            report,
            HookReport::CommandFinished { success: true, .. }
        ));
        assert_eq!(
            fs::read_to_string(directory.join("completed")).unwrap(),
            "done"
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn hook_helpers_copy_query_shell_and_send_terminal_input() {
        let source = temp_path("copy-source");
        let target = temp_path("copy-target");
        fs::write(&source, "copied").unwrap();
        let config = format!(
            r#"
            local grove = require("grove")
            grove.on("terminal_spawned", function(terminal)
              grove.copy({source:?}, {target:?})
              assert(grove.exists({target:?}))
              grove.send(terminal.terminal, grove.sh("printf shell-output"))
            end)
            "#,
            source = source.to_string_lossy(),
            target = target.to_string_lossy(),
        );
        let mut runtime = DaemonRuntime::load_source(&config, "helpers").runtime;
        let sent = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&sent);
        runtime.set_terminal_sender(move |terminal, bytes| {
            captured.lock().unwrap().push((terminal, bytes.to_vec()));
            Ok(())
        });
        assert!(
            runtime
                .fire(
                    LifecycleEvent::TerminalSpawned,
                    &LifecyclePayload::Terminal {
                        terminal: 9,
                        repo: "api".into(),
                        branch: "main".into(),
                        path: "/tmp/api".into(),
                        session: "one".into(),
                    },
                )
                .is_empty()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "copied");
        assert_eq!(*sent.lock().unwrap(), [(9, b"shell-output".to_vec())]);
        let _ = fs::remove_file(source);
        let _ = fs::remove_file(target);
    }
}
