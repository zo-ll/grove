//! The Grove daemon: owns every pty, survives the TUI exiting.
//!
//! Never depends on `ratatui`, `crossterm` or `tui-term`. Rendering is not its
//! concern; it serves state over `grove-proto`.
//!
//! See `SPEC.md` §8. Implemented in issues #10, #11 and #12.

use groved::fetch::FetchPolicy;
use groved::session::SessionOrchestrator;
use groved::terminal::TerminalManager;
use groved::{BindOutcome, DaemonSocket};
use std::path::{Path, PathBuf};

fn main() {
    if let Err(error) = run() {
        eprintln!("groved: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let workspace = match args.next() {
        Some(flag) if flag == "--workspace" => args
            .next()
            .map(PathBuf::from)
            .ok_or("--workspace requires a path")?,
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir()?,
    };
    match DaemonSocket::bind(&workspace)? {
        BindOutcome::Owner(daemon) => {
            let loaded = grove_lua::DaemonRuntime::load(config_path());
            if let Some(error) = loaded.error {
                eprintln!("groved: {error}; using defaults");
            }
            let runtime = loaded.runtime;
            let session_paths = runtime.contains_session_path_placeholder();
            let config = runtime.config();
            let shell = PathBuf::from(&config.shell);
            let scratch_cwd = expand_home(&config.scratch_cwd);
            let scrollback = config.scrollback;
            let ignores = config.ignore.clone();
            let fetch = FetchPolicy::from_config(4, config)?;
            let workspace_view = grove_git::Workspace::discover(&workspace, ignores)?;
            let repositories = workspace_view.repositories().to_vec();
            let store =
                grove_state::Store::load(&workspace, if session_paths { "{session}" } else { "" });
            let terminals = TerminalManager::new(shell, scratch_cwd, scrollback);
            let service = SessionOrchestrator::new(
                store,
                repositories,
                workspace_view.root().to_path_buf(),
                terminals,
                fetch,
                runtime,
            )
            .with_socket_path(daemon.path());
            daemon.run(service)?;
        }
        BindOutcome::Existing(_) => {}
    }
    Ok(())
}

fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(std::env::temp_dir)
        .join("grove/config.lua")
}

fn expand_home(value: &str) -> PathBuf {
    groved::session::expand_home(Path::new(value))
}
