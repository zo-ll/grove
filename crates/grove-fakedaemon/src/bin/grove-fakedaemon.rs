//! Run a scenario as a daemon the TUI can connect to.
//!
//! ```text
//! grove-fakedaemon --scenario invoice-split --workspace ~/anywhere
//! grove --workspace ~/anywhere
//! ```
//!
//! The socket path is derived exactly as the TUI derives it, so no flag has to
//! agree between the two: point both at the same workspace and they meet. The
//! workspace need not exist or contain anything — that is the point.

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut scenario = "invoice-split".to_string();
    let mut workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--scenario" => scenario = args.next().unwrap_or_default(),
            "--workspace" => workspace = args.next().map(PathBuf::from).unwrap_or(workspace),
            "--list" => {
                for s in grove_fakedaemon::scenario::all() {
                    println!("{:<14} {}", s.name, s.covers);
                }
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    let Some(sc) = grove_fakedaemon::scenario::by_name(&scenario) else {
        eprintln!("no scenario named {scenario:?}. --list shows them.");
        return ExitCode::FAILURE;
    };

    let path = grove_proto::socket_path(&workspace);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // A socket left by an earlier run would make bind fail; this is a test
    // harness, so reclaiming it is right rather than demanding cleanup.
    let _ = std::fs::remove_file(&path);

    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    eprintln!("serving {:?} at {}", sc.name, path.display());
    eprintln!("covers: {}", sc.covers);

    for conn in listener.incoming() {
        match conn {
            Ok(c) => {
                if let Err(e) = grove_fakedaemon::serve(&sc, c) {
                    eprintln!("connection ended: {e}");
                }
            }
            Err(e) => eprintln!("accept failed: {e}"),
        }
    }
    ExitCode::SUCCESS
}
