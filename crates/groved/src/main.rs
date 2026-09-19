//! The Grove daemon: owns every pty, survives the TUI exiting.
//!
//! Never depends on `ratatui`, `crossterm` or `tui-term`. Rendering is not its
//! concern; it serves state over `grove-proto`.
//!
//! See `SPEC.md` §8. Implemented in issues #10, #11 and #12.

use groved::{BindOutcome, DaemonSocket};
use std::path::PathBuf;

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
        BindOutcome::Owner(daemon) => daemon.run()?,
        BindOutcome::Existing(_) => {}
    }
    Ok(())
}
