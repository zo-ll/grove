//! Evaluation of Grove's shared Lua configuration in process-specific VMs.

use mlua::{Function, Lua, MultiValue, RegistryKey, Table, Value};
use std::{
    fmt, fs,
    path::Path,
    sync::{Arc, Mutex},
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
}

/// A daemon-owned Lua VM with only daemon settings and lifecycle hooks in Rust.
pub struct DaemonRuntime {
    config: DaemonConfig,
    lifecycle: Vec<LifecycleRegistration>,
    lua: Lua,
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
        Self {
            config: DaemonConfig::default(),
            lifecycle: Vec::new(),
            lua: Lua::new(),
        }
    }

    fn evaluate(source: &str, name: &str) -> mlua::Result<Self> {
        let lua = Lua::new();
        let config = Arc::new(Mutex::new(Some(DaemonConfig::default())));
        let lifecycle = Arc::new(Mutex::new(Some(Vec::new())));
        install_daemon_module(&lua, Arc::clone(&config), Arc::clone(&lifecycle))?;
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
        })
    }
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
                });
            Ok(())
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
}
