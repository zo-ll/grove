//! `grove --uninstall`: take grove off this machine.
//!
//! Everything grove put here goes: the running daemons (and with them the
//! terminals they hold), the `grove` and `groved` binaries, the config, the
//! saved sessions, the installer's source copy and the runtime sockets.
//! Worktrees do not: they are the user's checkouts, with their work in them,
//! and removing a program is not a reason to delete someone's branches.
//!
//! The plan is built first and shown, and nothing is touched until the user
//! says yes — or passes `--y`, for scripts. Building and carrying out the
//! plan take their inputs as arguments (the environment, a `/proc` to read),
//! so both are tested against a temporary directory, not this machine.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Where grove's files live, as the environment says.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub home: Option<PathBuf>,
    pub config_home: Option<PathBuf>,
    pub state_home: Option<PathBuf>,
    pub cache_home: Option<PathBuf>,
    pub runtime_dir: Option<PathBuf>,
    /// This `grove` binary; `groved` is removed from beside it.
    pub exe: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Self {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        Self {
            home: var("HOME"),
            config_home: var("XDG_CONFIG_HOME"),
            state_home: var("XDG_STATE_HOME"),
            cache_home: var("XDG_CACHE_HOME"),
            runtime_dir: var("XDG_RUNTIME_DIR"),
            exe: std::env::current_exe()
                .ok()
                .map(|exe| fs::canonicalize(&exe).unwrap_or(exe)),
        }
    }

    /// `$XDG_…_HOME/grove`, else `$HOME/<fallback>/grove`.
    fn grove_dir(&self, xdg: &Option<PathBuf>, fallback: &str) -> Option<PathBuf> {
        xdg.clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(fallback)))
            .map(|base| base.join("grove"))
    }
}

/// A running daemon, and the workspace it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Daemon {
    pub pid: u32,
    pub workspace: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub daemons: Vec<Daemon>,
    pub binaries: Vec<PathBuf>,
    pub dirs: Vec<PathBuf>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.daemons.is_empty() && self.binaries.is_empty() && self.dirs.is_empty()
    }

    /// What will happen, in the order it happens, for the user to read
    /// before saying yes.
    pub fn describe(&self) -> String {
        let mut lines = Vec::new();
        if !self.daemons.is_empty() {
            lines.push("stop the running daemons — their terminals close:".to_string());
            for daemon in &self.daemons {
                lines.push(format!("  groved {:>7}  {}", daemon.pid, daemon.workspace));
            }
        }
        if !self.dirs.is_empty() {
            lines.push("delete config, saved sessions, caches and sockets:".to_string());
            lines.extend(self.dirs.iter().map(|dir| format!("  {}", dir.display())));
        }
        if !self.binaries.is_empty() {
            lines.push("delete the programs:".to_string());
            lines.extend(
                self.binaries
                    .iter()
                    .map(|bin| format!("  {}", bin.display())),
            );
        }
        lines.join("\n")
    }
}

/// Everything `grove --uninstall` would remove, and nothing it would not.
pub fn plan(env: &Env, proc_root: &Path, uid: u32) -> Plan {
    let binaries = env
        .exe
        .iter()
        .flat_map(|exe| {
            let beside = exe.parent().map(|dir| dir.join("groved"));
            std::iter::once(exe.clone()).chain(beside)
        })
        .filter(|path| path.is_file())
        .collect();
    let dirs = [
        env.grove_dir(&env.config_home, ".config"),
        env.grove_dir(&env.state_home, ".local/state"),
        env.grove_dir(&env.cache_home, ".cache"),
        env.runtime_dir.as_ref().map(|dir| dir.join("grove")),
    ]
    .into_iter()
    .flatten()
    .filter(|dir| dir.is_dir())
    .collect();
    Plan {
        daemons: daemons(proc_root, uid),
        binaries,
        dirs,
    }
}

/// This user's `groved` processes, read from a `/proc`.
fn daemons(proc_root: &Path, uid: u32) -> Vec<Daemon> {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut found: Vec<Daemon> = entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if entry.metadata().ok()?.uid() != uid {
                return None;
            }
            let comm = fs::read_to_string(entry.path().join("comm")).ok()?;
            if comm.trim() != "groved" {
                return None;
            }
            let cmdline = fs::read(entry.path().join("cmdline")).unwrap_or_default();
            let args: Vec<String> = cmdline
                .split(|byte| *byte == 0)
                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect();
            let workspace = args
                .iter()
                .position(|arg| arg == "--workspace")
                .and_then(|at| args.get(at + 1))
                .cloned()
                .unwrap_or_else(|| "(workspace unknown)".into());
            Some(Daemon { pid, workspace })
        })
        .collect();
    found.sort_by_key(|daemon| daemon.pid);
    found
}

/// Carry the plan out: daemons first, so nothing is writing into the
/// directories while they go, then the directories, then the programs.
/// Returns what could not be done; everything else was.
pub fn execute(plan: &Plan, proc_root: &Path) -> Vec<String> {
    let mut failed = Vec::new();
    for daemon in &plan.daemons {
        let stopped = Command::new("kill")
            .arg(daemon.pid.to_string())
            .status()
            .is_ok_and(|status| status.success());
        if !stopped {
            failed.push(format!("could not stop groved {}", daemon.pid));
        }
    }
    // A daemon removes its socket as it exits; give them a moment so the
    // runtime directory is not deleted under one still shutting down.
    let deadline = Instant::now() + Duration::from_secs(2);
    while plan
        .daemons
        .iter()
        .any(|daemon| proc_root.join(daemon.pid.to_string()).exists())
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    for dir in &plan.dirs {
        if let Err(error) = fs::remove_dir_all(dir) {
            failed.push(format!("could not delete {}: {error}", dir.display()));
        }
    }
    for binary in &plan.binaries {
        if let Err(error) = fs::remove_file(binary) {
            failed.push(format!("could not delete {}: {error}", binary.display()));
        }
    }
    failed
}

/// Said after the plan, and again at the end: what stays, and why.
pub const WORKTREES_STAY: &str = "\
Worktrees are not touched: branches you checked out with grove (by default
under ~/grove/<repo>/<branch>) stay on disk with your work in them. Remove
one from its repository with `git worktree remove <path>`.";

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "grove-uninstall-{label}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// This process's user: the one that owns what the tests create.
    fn uid() -> u32 {
        fs::metadata("/proc/self").unwrap().uid()
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    /// A machine with grove installed and used, under `root`.
    fn installed(root: &Path) -> Env {
        let home = root.join("home");
        touch(&home.join(".local/bin/grove"));
        touch(&home.join(".local/bin/groved"));
        touch(&home.join(".local/bin/other-tool"));
        touch(&home.join(".config/grove/config.lua"));
        touch(&home.join(".config/other/keep.toml"));
        touch(&home.join(".local/state/grove/abc/sessions.json"));
        touch(&home.join(".cache/grove/src/Cargo.toml"));
        touch(&root.join("run/grove/abc.sock"));
        touch(&home.join("grove/shop/feat-cart/README.md"));
        Env {
            home: Some(home.clone()),
            runtime_dir: Some(root.join("run")),
            exe: Some(home.join(".local/bin/grove")),
            ..Env::default()
        }
    }

    fn fake_proc(root: &Path, processes: &[(u32, &str, &[&str])]) -> PathBuf {
        let proc_root = root.join("proc");
        for (pid, comm, args) in processes {
            let dir = proc_root.join(pid.to_string());
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
            fs::write(dir.join("cmdline"), args.join("\0")).unwrap();
        }
        fs::create_dir_all(&proc_root).unwrap();
        proc_root
    }

    #[test]
    fn the_plan_is_everything_grove_put_here_and_nothing_else() {
        let root = Dir::new("plan");
        let env = installed(&root.0);
        let proc_root = fake_proc(
            &root.0,
            &[
                (41, "groved", &["groved", "--workspace", "/home/az/shop"]),
                (7, "bash", &["bash"]),
            ],
        );
        let plan = plan(&env, &proc_root, uid());
        let home = root.0.join("home");
        assert_eq!(
            plan.daemons,
            vec![Daemon {
                pid: 41,
                workspace: "/home/az/shop".into()
            }]
        );
        assert_eq!(
            plan.binaries,
            vec![
                home.join(".local/bin/grove"),
                home.join(".local/bin/groved")
            ]
        );
        assert_eq!(
            plan.dirs,
            vec![
                home.join(".config/grove"),
                home.join(".local/state/grove"),
                home.join(".cache/grove"),
                root.0.join("run/grove"),
            ]
        );
        let said = plan.describe();
        assert!(said.contains("their terminals close"), "{said}");
        assert!(said.contains("/home/az/shop"), "{said}");
    }

    #[test]
    fn xdg_directories_are_where_grove_looks_first() {
        let root = Dir::new("xdg");
        touch(&root.0.join("cfg/grove/config.lua"));
        touch(&root.0.join("home/.config/grove/config.lua"));
        let env = Env {
            home: Some(root.0.join("home")),
            config_home: Some(root.0.join("cfg")),
            ..Env::default()
        };
        let plan = plan(&env, &root.0.join("no-proc"), uid());
        assert_eq!(plan.dirs, vec![root.0.join("cfg/grove")]);
    }

    #[test]
    fn carrying_it_out_leaves_worktrees_and_everything_else() {
        let root = Dir::new("execute");
        let env = installed(&root.0);
        let proc_root = fake_proc(&root.0, &[]);
        let planned = plan(&env, &proc_root, uid());
        assert!(execute(&planned, &proc_root).is_empty());

        let home = root.0.join("home");
        for gone in [
            home.join(".local/bin/grove"),
            home.join(".local/bin/groved"),
            home.join(".config/grove"),
            home.join(".local/state/grove"),
            home.join(".cache/grove"),
            root.0.join("run/grove"),
        ] {
            assert!(!gone.exists(), "{} should be gone", gone.display());
        }
        for kept in [
            home.join("grove/shop/feat-cart/README.md"),
            home.join(".local/bin/other-tool"),
            home.join(".config/other/keep.toml"),
        ] {
            assert!(kept.exists(), "{} must be kept", kept.display());
        }
        assert!(
            plan(&env, &proc_root, uid()).is_empty(),
            "a second uninstall finds nothing"
        );
    }

    #[test]
    fn another_users_daemon_is_not_ours_to_stop() {
        let root = Dir::new("uid");
        let proc_root = fake_proc(&root.0, &[(41, "groved", &["groved"])]);
        assert!(daemons(&proc_root, uid().wrapping_add(1)).is_empty());
        assert_eq!(
            daemons(&proc_root, uid())[0].workspace,
            "(workspace unknown)"
        );
    }
}
