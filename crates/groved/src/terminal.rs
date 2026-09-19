use grove_domain::SessionId;
use grove_proto::TerminalId;
use grove_state::{SnapshotTerminal, Store};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TerminalKey {
    Worktree(PathBuf),
    Scratch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub rows: u16,
    pub cols: u16,
    pub contents: String,
    pub formatted: Vec<u8>,
    /// Retained history, oldest first and disjoint from the visible screen.
    pub scrollback: Vec<String>,
    pub retained_scrollback_rows: usize,
    pub alive: bool,
}

pub struct Attachment {
    pub snapshot: TerminalSnapshot,
    pub output: mpsc::Receiver<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreReport {
    pub restored: Vec<TerminalId>,
    pub missing: Vec<SnapshotTerminal>,
    pub failed: Vec<RestoreFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreFailure {
    pub terminal: SnapshotTerminal,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("a terminal already exists for {0:?}")]
    AlreadyExists(TerminalKey),
    #[error("terminal {0:?} does not exist")]
    Missing(TerminalId),
    #[error("scratch terminal {0:?} does not belong in a session snapshot")]
    ScratchSnapshot(TerminalId),
    #[error("terminal size must be non-zero, got {rows}x{cols}")]
    InvalidSize { rows: u16, cols: u16 },
    #[error("could not open or spawn pty: {0}")]
    Pty(String),
    #[error("terminal I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("terminal state lock was poisoned")]
    Poisoned,
    #[error(transparent)]
    State(#[from] grove_state::Error),
}

struct TerminalProcess {
    key: TerminalKey,
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    parser: Arc<Mutex<vt100::Parser>>,
    subscribers: Arc<Mutex<Vec<mpsc::SyncSender<Vec<u8>>>>>,
    alive: Arc<AtomicBool>,
    process_id: Option<u32>,
    rows: u16,
    cols: u16,
}

pub struct TerminalManager {
    terminals: HashMap<TerminalId, TerminalProcess>,
    keys: HashMap<TerminalKey, TerminalId>,
    next_id: AtomicU64,
    shell: PathBuf,
    scratch_cwd: PathBuf,
    scrollback: usize,
}

impl TerminalManager {
    pub fn new(shell: PathBuf, scratch_cwd: PathBuf, scrollback: usize) -> Self {
        Self {
            terminals: HashMap::new(),
            keys: HashMap::new(),
            next_id: AtomicU64::new(1),
            shell,
            scratch_cwd,
            scrollback,
        }
    }

    pub fn spawn_worktree(
        &mut self,
        path: impl Into<PathBuf>,
        rows: u16,
        cols: u16,
    ) -> Result<TerminalId, TerminalError> {
        let path = fs::canonicalize(path.into())?;
        self.spawn(TerminalKey::Worktree(path.clone()), &path, rows, cols)
    }

    pub fn spawn_scratch(&mut self, rows: u16, cols: u16) -> Result<TerminalId, TerminalError> {
        let cwd = self.scratch_cwd.clone();
        self.spawn(TerminalKey::Scratch, &cwd, rows, cols)
    }

    fn spawn(
        &mut self,
        key: TerminalKey,
        cwd: &Path,
        rows: u16,
        cols: u16,
    ) -> Result<TerminalId, TerminalError> {
        validate_size(rows, cols)?;
        if self.keys.contains_key(&key) {
            return Err(TerminalError::AlreadyExists(key));
        }
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system().openpty(size).map_err(pty_error)?;
        let mut command = CommandBuilder::new(&self.shell);
        command.cwd(cwd);
        let child = pair.slave.spawn_command(command).map_err(pty_error)?;
        drop(pair.slave);
        let process_id = child.process_id();
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader().map_err(pty_error)?;
        let writer = pair.master.take_writer().map_err(pty_error)?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, self.scrollback)));
        let subscribers = Arc::new(Mutex::new(Vec::<mpsc::SyncSender<Vec<u8>>>::new()));
        let alive = Arc::new(AtomicBool::new(true));

        let reader_parser = Arc::clone(&parser);
        let reader_subscribers = Arc::clone(&subscribers);
        let reader_alive = Arc::clone(&alive);
        thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                    Ok(count) => {
                        let bytes = buffer[..count].to_vec();
                        if let Ok(mut parser) = reader_parser.lock() {
                            parser.process(&bytes);
                            // Keep parser update and publication in one critical
                            // section so attach can take an atomic snapshot.
                            if let Ok(mut listeners) = reader_subscribers.lock() {
                                listeners
                                    .retain(|listener| listener.try_send(bytes.clone()).is_ok());
                            }
                        }
                    }
                }
            }
            reader_alive.store(false, Ordering::Release);
        });
        let wait_alive = Arc::clone(&alive);
        thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
            wait_alive.store(false, Ordering::Release);
        });

        let id = TerminalId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.keys.insert(key.clone(), id);
        self.terminals.insert(
            id,
            TerminalProcess {
                key,
                master: pair.master,
                writer: Mutex::new(writer),
                killer: Mutex::new(killer),
                parser,
                subscribers,
                alive,
                process_id,
                rows,
                cols,
            },
        );
        Ok(id)
    }

    pub fn input(&self, id: TerminalId, bytes: &[u8]) -> Result<(), TerminalError> {
        let terminal = self.terminal(id)?;
        let mut writer = terminal
            .writer
            .lock()
            .map_err(|_| TerminalError::Poisoned)?;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    pub fn resize(&mut self, id: TerminalId, rows: u16, cols: u16) -> Result<(), TerminalError> {
        validate_size(rows, cols)?;
        let terminal = self.terminal_mut(id)?;
        terminal
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(pty_error)?;
        terminal
            .parser
            .lock()
            .map_err(|_| TerminalError::Poisoned)?
            .screen_mut()
            .set_size(rows, cols);
        terminal.rows = rows;
        terminal.cols = cols;
        Ok(())
    }

    pub fn attach(&self, id: TerminalId) -> Result<Attachment, TerminalError> {
        let terminal = self.terminal(id)?;
        // A slow client is disconnected from live output rather than allowed to
        // grow daemon memory without bound. It can reattach from the parser.
        let (sender, output) = mpsc::sync_channel(64);
        let mut parser = terminal
            .parser
            .lock()
            .map_err(|_| TerminalError::Poisoned)?;
        let snapshot = snapshot_from_parser(terminal, &mut parser);
        terminal
            .subscribers
            .lock()
            .map_err(|_| TerminalError::Poisoned)?
            .push(sender);
        Ok(Attachment { snapshot, output })
    }

    pub fn snapshot(&self, id: TerminalId) -> Result<TerminalSnapshot, TerminalError> {
        snapshot(self.terminal(id)?)
    }

    pub fn foreground_process(&self, id: TerminalId) -> Result<Option<String>, TerminalError> {
        let terminal = self.terminal(id)?;
        let Some(shell_pid) = terminal.process_id else {
            return Ok(None);
        };
        let output = Command::new("ps")
            .args(["-o", "tpgid=", "-p", &shell_pid.to_string()])
            .output()?;
        if !output.status.success() {
            return Ok(None);
        }
        let foreground = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .ok();
        let Some(pid) = foreground.filter(|pid| *pid > 0) else {
            return Ok(None);
        };
        Ok(fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|name| name.trim().to_owned()))
    }

    /// Captures terminal metadata in the session's manual snapshot. Commands
    /// are recorded for display only; restore never sends them to a PTY.
    pub fn save_session_snapshot(
        &self,
        store: &Store,
        session: &SessionId,
        terminals: &[TerminalId],
    ) -> Result<(), TerminalError> {
        let terminals = terminals
            .iter()
            .map(|id| {
                let terminal = self.terminal(*id)?;
                let TerminalKey::Worktree(worktree) = &terminal.key else {
                    return Err(TerminalError::ScratchSnapshot(*id));
                };
                Ok(SnapshotTerminal {
                    worktree: worktree.clone(),
                    last_command: self.foreground_process(*id)?,
                    rows: terminal.rows,
                    cols: terminal.cols,
                })
            })
            .collect::<Result<Vec<_>, TerminalError>>()?;
        store.save_snapshot(session, terminals)?;
        Ok(())
    }

    /// Starts a fresh shell for every worktree that still exists. The saved
    /// command is deliberately ignored: a snapshot restores layout, not work.
    pub fn restore_session_snapshot(
        &mut self,
        store: &Store,
        session: &SessionId,
    ) -> Result<RestoreReport, TerminalError> {
        let plan = store.restore_plan(session)?;
        let mut restored = Vec::with_capacity(plan.terminals.len());
        let mut failed = Vec::new();
        for terminal in plan.terminals {
            match self.spawn_worktree(&terminal.worktree, terminal.rows, terminal.cols) {
                Ok(id) => restored.push(id),
                Err(error) => failed.push(RestoreFailure {
                    terminal,
                    message: error.to_string(),
                }),
            }
        }
        Ok(RestoreReport {
            restored,
            missing: plan.missing,
            failed,
        })
    }

    pub fn kill(&mut self, id: TerminalId) -> Result<(), TerminalError> {
        self.terminal(id)?
            .killer
            .lock()
            .map_err(|_| TerminalError::Poisoned)?
            .kill()?;
        let terminal = self
            .terminals
            .remove(&id)
            .ok_or(TerminalError::Missing(id))?;
        self.keys.remove(&terminal.key);
        Ok(())
    }

    pub fn is_alive(&self, id: TerminalId) -> Result<bool, TerminalError> {
        Ok(self.terminal(id)?.alive.load(Ordering::Acquire))
    }

    fn terminal(&self, id: TerminalId) -> Result<&TerminalProcess, TerminalError> {
        self.terminals.get(&id).ok_or(TerminalError::Missing(id))
    }
    fn terminal_mut(&mut self, id: TerminalId) -> Result<&mut TerminalProcess, TerminalError> {
        self.terminals
            .get_mut(&id)
            .ok_or(TerminalError::Missing(id))
    }
}

impl Drop for TerminalManager {
    fn drop(&mut self) {
        for terminal in self.terminals.values() {
            if let Ok(mut killer) = terminal.killer.lock() {
                let _ = killer.kill();
            }
        }
    }
}

fn snapshot(terminal: &TerminalProcess) -> Result<TerminalSnapshot, TerminalError> {
    let mut parser = terminal
        .parser
        .lock()
        .map_err(|_| TerminalError::Poisoned)?;
    Ok(snapshot_from_parser(terminal, &mut parser))
}

fn snapshot_from_parser(
    terminal: &TerminalProcess,
    parser: &mut vt100::Parser,
) -> TerminalSnapshot {
    parser.screen_mut().set_scrollback(usize::MAX);
    let retained_scrollback_rows = parser.screen().scrollback();
    let mut scrollback = Vec::with_capacity(retained_scrollback_rows);
    let page_rows = usize::from(terminal.rows.max(1));
    let mut consumed = 0;
    while consumed < retained_scrollback_rows {
        parser
            .screen_mut()
            .set_scrollback(retained_scrollback_rows - consumed);
        let count = page_rows.min(retained_scrollback_rows - consumed);
        scrollback.extend(parser.screen().rows(0, terminal.cols).take(count));
        consumed += count;
    }
    parser.screen_mut().set_scrollback(0);
    TerminalSnapshot {
        rows: terminal.rows,
        cols: terminal.cols,
        contents: parser.screen().contents(),
        formatted: parser.screen().contents_formatted(),
        scrollback,
        retained_scrollback_rows,
        alive: terminal.alive.load(Ordering::Acquire),
    }
}

fn pty_error(error: impl std::fmt::Display) -> TerminalError {
    TerminalError::Pty(error.to_string())
}

fn validate_size(rows: u16, cols: u16) -> Result<(), TerminalError> {
    if rows == 0 || cols == 0 {
        Err(TerminalError::InvalidSize { rows, cols })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("groved-pty-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn wait_for(manager: &TerminalManager, id: TerminalId, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if manager.snapshot(id).unwrap().contents.contains(text) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("terminal never displayed {text:?}");
    }

    #[test]
    fn output_accumulates_without_a_client_and_reattach_gets_current_screen() {
        let cwd = temp_dir();
        let mut manager = TerminalManager::new(PathBuf::from("/bin/sh"), cwd.clone(), 100);
        let id = manager.spawn_worktree(&cwd, 24, 80).unwrap();
        manager.input(id, b"printf 'grove-ready\\n'\r").unwrap();
        wait_for(&manager, id, "grove-ready");
        assert!(manager.foreground_process(id).unwrap().is_some());
        let attachment = manager.attach(id).unwrap();
        assert!(attachment.snapshot.contents.contains("grove-ready"));
        drop(manager);
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn resize_updates_pty_and_parser_and_kill_leaves_cwd() {
        let cwd = temp_dir();
        let mut manager = TerminalManager::new(PathBuf::from("/bin/sh"), cwd.clone(), 100);
        let id = manager.spawn_worktree(&cwd, 24, 80).unwrap();
        manager.resize(id, 40, 120).unwrap();
        let snapshot = manager.snapshot(id).unwrap();
        assert_eq!((snapshot.rows, snapshot.cols), (40, 120));
        manager.kill(id).unwrap();
        assert!(cwd.exists());
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn duplicate_worktree_and_scratch_terminals_are_refused() {
        let cwd = temp_dir();
        let mut manager = TerminalManager::new(PathBuf::from("/bin/sh"), cwd.clone(), 100);
        manager.spawn_worktree(&cwd, 24, 80).unwrap();
        assert!(matches!(
            manager.spawn_worktree(&cwd, 24, 80),
            Err(TerminalError::AlreadyExists(_))
        ));
        manager.spawn_scratch(24, 80).unwrap();
        assert!(matches!(
            manager.spawn_scratch(24, 80),
            Err(TerminalError::AlreadyExists(TerminalKey::Scratch))
        ));
        drop(manager);
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn scrollback_stays_within_the_configured_bound() {
        let cwd = temp_dir();
        let mut manager = TerminalManager::new(PathBuf::from("/bin/sh"), cwd.clone(), 10);
        let id = manager.spawn_worktree(&cwd, 5, 40).unwrap();
        manager
            .input(
                id,
                b"i=0; while [ $i -lt 100 ]; do echo line-$i; i=$((i+1)); done\r",
            )
            .unwrap();
        wait_for(&manager, id, "line-99");
        assert!(manager.snapshot(id).unwrap().retained_scrollback_rows <= 10);
        assert!(manager.snapshot(id).unwrap().scrollback.len() <= 10);
        drop(manager);
        let _ = fs::remove_dir_all(cwd);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn eight_idle_ptys_have_bounded_resident_memory() {
        fn rss_kib() -> u64 {
            fs::read_to_string("/proc/self/status")
                .unwrap()
                .lines()
                .find_map(|line| {
                    line.strip_prefix("VmRSS:")?
                        .split_whitespace()
                        .next()?
                        .parse()
                        .ok()
                })
                .unwrap()
        }
        let cwd = temp_dir();
        let before = rss_kib();
        let mut manager = TerminalManager::new(PathBuf::from("/bin/sh"), cwd.clone(), 1_000);
        for index in 0..8 {
            let path = cwd.join(format!("real-{index}"));
            fs::create_dir_all(&path).unwrap();
            manager.spawn_worktree(path, 24, 80).unwrap();
        }
        thread::sleep(Duration::from_millis(100));
        let growth = rss_kib().saturating_sub(before);
        eprintln!("eight idle PTYs RSS growth: {growth} KiB");
        assert!(growth < 128 * 1024);
        drop(manager);
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn restore_recreates_existing_terminals_without_executing_saved_commands() {
        let root = temp_dir();
        let existing = root.join("existing");
        let deleted = root.join("deleted");
        let marker = root.join("must-not-exist");
        fs::create_dir_all(&existing).unwrap();
        fs::create_dir_all(&deleted).unwrap();
        let session = SessionId("restore-test".into());
        let mut store = Store::load_at(&root, &root, "{repo}/{branch}");
        store
            .create(session.clone(), "Restore test".into())
            .unwrap();
        store
            .save_snapshot(
                &session,
                vec![
                    SnapshotTerminal {
                        worktree: existing.clone(),
                        last_command: Some(format!("touch {}", marker.display())),
                        rows: 24,
                        cols: 80,
                    },
                    SnapshotTerminal {
                        worktree: deleted.clone(),
                        last_command: Some("cargo test".into()),
                        rows: 40,
                        cols: 120,
                    },
                ],
            )
            .unwrap();
        fs::remove_dir_all(&deleted).unwrap();

        let mut manager = TerminalManager::new(PathBuf::from("/bin/sh"), root.clone(), 100);
        let report = manager.restore_session_snapshot(&store, &session).unwrap();
        assert_eq!(report.restored.len(), 1);
        assert_eq!(report.missing.len(), 1);
        assert!(report.failed.is_empty());
        assert_eq!(report.missing[0].worktree, deleted);
        assert_eq!(
            (
                manager.snapshot(report.restored[0]).unwrap().rows,
                manager.snapshot(report.restored[0]).unwrap().cols
            ),
            (24, 80)
        );
        manager.input(report.restored[0], b"pwd\r").unwrap();
        wait_for(&manager, report.restored[0], &existing.to_string_lossy());
        thread::sleep(Duration::from_millis(50));
        assert!(
            !marker.exists(),
            "saved command was executed during restore"
        );
        drop(manager);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn restore_continues_after_one_terminal_fails() {
        let root = temp_dir();
        let first = root.join("first");
        let invalid = root.join("invalid");
        let last = root.join("last");
        for path in [&first, &invalid, &last] {
            fs::create_dir_all(path).unwrap();
        }
        let session = SessionId("partial-restore".into());
        let mut store = Store::load_at(&root, &root, "{repo}/{branch}");
        store.create(session.clone(), "Partial".into()).unwrap();
        store
            .save_snapshot(
                &session,
                vec![
                    SnapshotTerminal {
                        worktree: first,
                        last_command: None,
                        rows: 24,
                        cols: 80,
                    },
                    SnapshotTerminal {
                        worktree: invalid.clone(),
                        last_command: None,
                        rows: 0,
                        cols: 80,
                    },
                    SnapshotTerminal {
                        worktree: last,
                        last_command: None,
                        rows: 24,
                        cols: 80,
                    },
                ],
            )
            .unwrap();

        let mut manager = TerminalManager::new(PathBuf::from("/bin/sh"), root.clone(), 100);
        let report = manager.restore_session_snapshot(&store, &session).unwrap();
        assert_eq!(report.restored.len(), 2);
        assert!(report.missing.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].terminal.worktree, invalid);
        assert!(report.failed[0].message.contains("non-zero"));
        drop(manager);
        let _ = fs::remove_dir_all(root);
    }
}
