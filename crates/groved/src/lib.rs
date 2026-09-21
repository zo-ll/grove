//! Daemon socket ownership and client connection lifecycle.

use fs2::FileExt;
use grove_proto::{
    Event, Handshake, PROTOCOL_VERSION, Request, TerminalId, accept_hello, accept_welcome,
    read_frame, write_frame,
};
use std::env;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

pub mod diff;
pub mod fetch;
pub mod prune;
pub mod session;
pub mod terminal;
mod watch;

const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether a read failed because nothing was said, rather than because the
/// connection is gone.
///
/// Both kinds appear: a socket with a read timeout reports `WouldBlock` on
/// some platforms and `TimedOut` on others, and which one arrives is not a
/// property worth depending on.
fn idle(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("XDG_RUNTIME_DIR is not set")]
    NoRuntimeDirectory,
    #[error("daemon socket operation failed at {path}: {source}")]
    Socket { path: PathBuf, source: io::Error },
    #[error("daemon protocol failed: {0}")]
    Protocol(#[from] grove_proto::FrameError),
    #[error("daemon and client protocol versions differ (daemon {daemon}, client {client})")]
    VersionMismatch { daemon: u32, client: u32 },
    #[error("daemon did not answer the opening handshake")]
    InvalidHandshake,
    #[error("could not spawn groved from {path}: {source}")]
    Spawn { path: PathBuf, source: io::Error },
    #[error("groved did not become ready within {0:?}")]
    StartupTimeout(Duration),
    #[error("could not watch worktrees: {0}")]
    Watch(String),
}

pub enum BindOutcome {
    Owner(DaemonSocket),
    Existing(UnixStream),
}

pub struct DaemonSocket {
    listener: UnixListener,
    socket_path: PathBuf,
}

impl DaemonSocket {
    pub fn bind(workspace: &Path) -> Result<BindOutcome, LifecycleError> {
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or(LifecycleError::NoRuntimeDirectory)?;
        Self::bind_at(&runtime, workspace)
    }

    pub fn bind_at(runtime: &Path, workspace: &Path) -> Result<BindOutcome, LifecycleError> {
        let socket_path = socket_path_at(runtime, workspace);
        let parent = socket_path.parent().expect("socket path has parent");
        fs::create_dir_all(parent).map_err(|source| socket_error(parent, source))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|source| socket_error(parent, source))?;

        if socket_path.exists()
            && let Ok(stream) = configured_stream(&socket_path)
        {
            return Ok(BindOutcome::Existing(stream));
        }

        let lock_path = socket_path.with_extension("lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|source| socket_error(&lock_path, source))?;
        // flock blocks until the holder releases, but it can also be
        // interrupted by a signal and return EINTR — once in a long run of
        // runs, on a loaded machine, which is exactly the shape of the
        // unrepeatable failure this file's tests saw once. A lock
        // acquisition is seconds-cheap to retry, so interrupting a signal
        // costs a retry, not a daemon lifecycle.
        let lock_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match lock.lock_exclusive() {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    // A sustained signal storm is not a healthy condition,
                    // but it is a reportable one: bind_at is library API, so
                    // the answer is an Err like its sibling arms, not a
                    // panic the daemon cannot survive gracefully.
                    if Instant::now() >= lock_deadline {
                        return Err(socket_error(
                            &lock_path,
                            io::Error::new(
                                io::ErrorKind::TimedOut,
                                "socket lock acquisition was interrupted for five seconds",
                            ),
                        ));
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                Err(source) => return Err(socket_error(&lock_path, source)),
            }
        }

        if socket_path.exists() {
            match connect_stale(&socket_path) {
                Ok(stream) => return Ok(BindOutcome::Existing(stream)),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    fs::remove_file(&socket_path)
                        .map_err(|source| socket_error(&socket_path, source))?;
                }
                Err(source) => return Err(socket_error(&socket_path, source)),
            }
        }
        let listener = UnixListener::bind(&socket_path)
            .map_err(|source| socket_error(&socket_path, source))?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
            .map_err(|source| socket_error(&socket_path, source))?;
        drop(lock);
        Ok(BindOutcome::Owner(Self {
            listener,
            socket_path,
        }))
    }

    pub fn path(&self) -> &Path {
        &self.socket_path
    }

    pub fn run(self, service: session::SessionOrchestrator) -> Result<(), LifecycleError> {
        let service = Arc::new(Mutex::new(service));
        let updates = watch::Broadcaster::default();
        let _watcher = watch::WorktreeWatcher::start(Arc::clone(&service), updates.clone())
            .map_err(|error| LifecycleError::Watch(error.to_string()))?;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let service = Arc::clone(&service);
                    let receiver = updates.subscribe();
                    thread::spawn(move || {
                        let _ = serve_client_for(stream, Some(service), Some(receiver), IO_TIMEOUT);
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

impl Drop for DaemonSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

pub fn socket_path(workspace: &Path) -> Result<PathBuf, LifecycleError> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or(LifecycleError::NoRuntimeDirectory)?;
    Ok(socket_path_at(&runtime, workspace))
}

pub fn socket_path_at(runtime: &Path, workspace: &Path) -> PathBuf {
    runtime
        .join("grove")
        .join(format!("{}.sock", grove_state::workspace_hash(workspace)))
}

pub fn connect(workspace: &Path) -> Result<UnixStream, LifecycleError> {
    let path = socket_path(workspace)?;
    connect_path(&path)
}

pub fn connect_path(path: &Path) -> Result<UnixStream, LifecycleError> {
    let mut stream = configured_stream(path).map_err(|source| socket_error(path, source))?;
    write_frame(
        &mut stream,
        &Request::Hello {
            version: PROTOCOL_VERSION,
        },
    )?;
    let event: Event = read_frame(&mut stream)?;
    match accept_welcome(&event) {
        Handshake::Agreed { .. } => Ok(stream),
        Handshake::Mismatch { daemon, client } => {
            Err(LifecycleError::VersionMismatch { daemon, client })
        }
        Handshake::NotHello => Err(LifecycleError::InvalidHandshake),
    }
}

pub fn connect_or_spawn(
    workspace: &Path,
    daemon_executable: &Path,
) -> Result<UnixStream, LifecycleError> {
    match connect(workspace) {
        Ok(stream) => return Ok(stream),
        // A busy daemon's backlog answers EAGAIN (WouldBlock) rather than
        // refused: the daemon is present, so the wait loop below — which
        // owns the deadline — should see it again, not this arm.
        Err(LifecycleError::Socket { source, .. })
            if matches!(
                source.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::WouldBlock
            ) => {}
        Err(error) => return Err(error),
    }
    let mut command = Command::new(daemon_executable);
    command
        .arg("--workspace")
        .arg(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|source| LifecycleError::Spawn {
            path: daemon_executable.to_owned(),
            source,
        })?;
    let timeout = Duration::from_secs(3);
    let deadline = Instant::now() + timeout;
    loop {
        match connect(workspace) {
            Ok(stream) => return Ok(stream),
            Err(LifecycleError::Socket { source, .. })
                if matches!(
                    source.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::WouldBlock
                ) && Instant::now() < deadline =>
            {
                // WouldBlock here is a saturated accept backlog: the daemon
                // is alive, and attaching is worth waiting its window out —
                // not a reason to report a startup failure.
                thread::sleep(Duration::from_millis(20));
            }
            Err(LifecycleError::Socket { .. }) => {
                return Err(LifecycleError::StartupTimeout(timeout));
            }
            Err(error) => return Err(error),
        }
    }
}

fn configured_stream(path: &Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

#[cfg(test)]
fn serve_client(stream: UnixStream) -> Result<(), grove_proto::FrameError> {
    serve_client_with(stream, None)
}

#[cfg(test)]
fn serve_client_with(
    stream: UnixStream,
    service: Option<Arc<Mutex<session::SessionOrchestrator>>>,
) -> Result<(), grove_proto::FrameError> {
    serve_client_for(stream, service, None, IO_TIMEOUT)
}

#[cfg(test)]
fn serve_client_with_watcher(
    stream: UnixStream,
    service: Arc<Mutex<session::SessionOrchestrator>>,
) -> Result<(), grove_proto::FrameError> {
    let updates = watch::Broadcaster::default();
    let receiver = updates.subscribe();
    let _watcher = watch::WorktreeWatcher::start(Arc::clone(&service), updates).unwrap();
    serve_client_for(stream, Some(service), Some(receiver), IO_TIMEOUT)
}

/// A client's connection, with the read timeout the caller wants.
///
/// The timeout is a parameter only so the tests can be quick about proving
/// what it does — and, more to the point, what it does not do.
fn serve_client_for(
    mut stream: UnixStream,
    service: Option<Arc<Mutex<session::SessionOrchestrator>>>,
    updates: Option<mpsc::Receiver<Event>>,
    timeout: Duration,
) -> Result<(), grove_proto::FrameError> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let hello: Request = read_frame(&mut stream)?;
    // The capability is a workspace fact, decided by the service the socket
    // serves: when a session-scoped path template makes worktree location
    // depend on the owning session, adopt and release are refused, and the
    // client should withhold the affordance rather than offer a key that only
    // fails (§2.4). The daemon's refusal stays; this is the affordance, not
    // the enforcement.
    let ownership_movable = service
        .as_ref()
        // Fail closed: a service the caller cannot ask (poisoned lock) must
        // not advertise a capability it did not see.
        .and_then(|service| service.lock().ok().map(|s| s.ownership_movable()))
        .unwrap_or(false);
    match accept_hello(&hello) {
        Handshake::Agreed { .. } => write_frame(
            &mut stream,
            &Event::Welcome {
                version: PROTOCOL_VERSION,
                ownership_movable,
            },
        )?,
        Handshake::Mismatch { daemon, client } => {
            write_frame(&mut stream, &Event::VersionMismatch { daemon, client })?;
            return Ok(());
        }
        Handshake::NotHello => {
            write_frame(
                &mut stream,
                &Event::Failed {
                    context: "handshake".into(),
                    message: "first request must be Hello".into(),
                },
            )?;
            return Ok(());
        }
    }

    // Every frame the client receives — responses and streamed terminal
    // output alike — goes through one writer, so frames cannot interleave
    // mid-sequence the way two threads writing one socket directly would.
    let (outbound, outbound_rx) = mpsc::sync_channel::<Event>(256);
    let mut writer = stream.try_clone().map_err(grove_proto::FrameError::from)?;
    thread::spawn(move || {
        for event in outbound_rx {
            if write_frame(&mut writer, &event).is_err() {
                break;
            }
        }
    });
    if let Some(updates) = updates {
        let outbound = outbound.clone();
        thread::spawn(move || {
            for event in updates {
                match outbound.try_send(event) {
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                }
            }
        });
    }

    // This client's live terminal feed, if attached. At most one at a time:
    // a pane follows one terminal, and a new attach or an explicit detach
    // replaces or drops the old one.
    let mut live: Option<LiveFeed> = None;
    let handle_attach = |stream_feed: &mut Option<LiveFeed>, attach: &grove_proto::Attach| {
        // Replacement and detach both end the old feed by dropping it; Drop
        // sets the stop flag, the forwarder exits within its polling window,
        // and the abandoned subscriber is cleaned up by the pty reader.
        let _ = stream_feed.take();
        let response = if let Some(service) = &service {
            match service.lock() {
                Ok(mut service) => service
                    .attach_terminal(*attach)
                    .map_err(|error| error.to_string()),
                Err(_) => Err("session orchestrator lock was poisoned".into()),
            }
        } else {
            Err("request handling is not installed yet".into())
        };
        match response {
            Ok(outcome) => {
                // The order is the eviction fix: the screen goes out, the
                // live feed starts, and only then does the backfill go out —
                // so output produced while history loads drains on this
                // client's channel instead of piling up behind the bounded
                // subscriber buffer and ending the stream silently.
                if outbound.send(outcome.screen).is_err() {
                    return;
                }
                *stream_feed = Some(LiveFeed::spawn(
                    attach.terminal,
                    outcome.output,
                    outbound.clone(),
                ));
                for event in outcome.backfill {
                    if outbound.send(event).is_err() {
                        return;
                    }
                }
            }
            Err(message) => {
                let _ = outbound.send(Event::Failed {
                    context: format!("attach terminal {:?}", attach.terminal),
                    message,
                });
            }
        }
    };

    loop {
        let request = match read_frame::<_, Request>(&mut stream) {
            Ok(request) => request,
            // A client with nothing to say is the normal case, not a dead
            // one. grove is a dashboard: it connects, asks for the workspace,
            // and then sits there for as long as the user is doing something
            // else. This loop used to be `while let Ok(..)`, so the read
            // timeout dropped every idle client after five seconds — the TUI
            // would draw the dash, and then go grey while nobody touched it.
            //
            // The timeout is here so a peer that vanished without closing
            // cannot pin this thread forever. That is a different question
            // from how long a user may look at the screen.
            Err(grove_proto::FrameError::Io(error)) if idle(&error) => continue,
            Err(_) => break,
        };
        match &request {
            Request::AttachTerminal(attach) => handle_attach(&mut live, attach),
            Request::DetachTerminal(_) => {
                // Dropping the feed ends only this client's stream: the pty
                // keeps running, and the subscriber is cleaned up by the pty
                // reader the next time output arrives.
                let _ = live.take();
            }
            _ => {
                let events = if let Some(service) = &service {
                    match service.lock() {
                        Ok(mut service) => service.handle_request(request),
                        Err(_) => vec![Event::Failed {
                            context: "daemon state".into(),
                            message: "session orchestrator lock was poisoned".into(),
                        }],
                    }
                } else {
                    vec![Event::Failed {
                        context: request.failure_context().into(),
                        message: "request handling is not installed yet".into(),
                    }]
                };
                for event in events {
                    if outbound.send(event).is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
    Ok(())
}

/// Forwards one terminal's live output to the attached client until the feed
/// is stopped (detach, re-attach, connection gone) or the channel closes
/// (terminal killed or died — the client sees the stream end, which is its
/// signal that the pane ended; the `TerminalExited` event itself is emitted
/// once, to the killer, never once per attached client).
struct LiveFeed {
    stop: Arc<AtomicBool>,
}

impl Drop for LiveFeed {
    // Every way a feed can be lost — detach, re-attach, replacement, or
    // connection teardown — goes through here, so a client that disconnects
    // while attached cannot pin the forwarder and the connection writer for
    // as long as the pty lives.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl LiveFeed {
    fn spawn(
        terminal: TerminalId,
        output: mpsc::Receiver<Vec<u8>>,
        outbound: mpsc::SyncSender<Event>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        thread::spawn(move || {
            loop {
                if flag.load(Ordering::Acquire) {
                    break;
                }
                match output.recv_timeout(Duration::from_millis(100)) {
                    Ok(bytes) => {
                        if outbound
                            .send(Event::TerminalOutput { terminal, bytes })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self { stop }
    }
}

// Every way a feed can be lost — detach, re-attach, replacement, connection
// teardown — goes through the Drop below.

/// Connects to a socket that is believed stale, retrying briefly first.
///
/// The stale check is one connect away from a spurious lifecycle error: on a
/// loaded machine a live daemon's accept backlog can saturate, and connect
/// answers EAGAIN — a daemon that is present, not a stale file. Retrying for
/// a bounded window lets a busy-but-alive daemon accept; a socket that keeps
/// refusing is stale and the caller removes it.
fn connect_stale(path: &Path) -> io::Result<UnixStream> {
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        match configured_stream(path) {
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(1));
            }
            other => return other,
        }
    }
}

fn socket_error(path: &Path, source: io::Error) -> LifecycleError {
    LifecycleError::Socket {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_domain::RepoId;
    use grove_git::Repository;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "groved-lifecycle-{}-{unique}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn second_bind_attaches_to_the_existing_daemon() {
        let temp = TempDir::new();
        let first = DaemonSocket::bind_at(&temp.0, Path::new("/workspace")).unwrap();
        assert!(matches!(first, BindOutcome::Owner(_)));
        let second = DaemonSocket::bind_at(&temp.0, Path::new("/workspace")).unwrap();
        assert!(matches!(second, BindOutcome::Existing(_)));
    }

    #[test]
    fn stale_socket_is_replaced_and_permissions_are_private() {
        let temp = TempDir::new();
        let path = socket_path_at(&temp.0, Path::new("/workspace"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        drop(UnixListener::bind(&path).unwrap());
        // Every test in this file holds its own runtime directory, named by
        // pid, timestamp and sequence — so no leftover socket from any other
        // run or test can ever be probed here, which is the isolation the
        // reported one-off failure asked for. The probe itself is a bounded
        // wait, not a busy spin: under a loaded suite, yield_now is a hot
        // loop for as long as the deadline runs.
        let deadline = Instant::now() + Duration::from_secs(1);
        while UnixStream::connect(&path).is_ok() {
            assert!(
                Instant::now() < deadline,
                "closed listener stayed connectable"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let outcome = DaemonSocket::bind_at(&temp.0, Path::new("/workspace")).unwrap();
        let BindOutcome::Owner(owner) = outcome else {
            panic!("stale socket was not replaced")
        };
        assert_eq!(
            fs::metadata(owner.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(owner.path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn a_client_that_says_nothing_is_still_connected() {
        // grove is a dashboard. It connects, asks what is in the workspace,
        // and then sits there while the user does something else — which is
        // most of the time it is open. The read timeout used to end the
        // connection on the first silence, so the TUI drew the dash once and
        // went grey five seconds later without anyone touching it.
        let (client, server) = UnixStream::pair().unwrap();
        let quick = Duration::from_millis(100);
        let worker = thread::spawn(move || serve_client_for(server, None, None, quick));
        let mut client = client;
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write_frame(
            &mut client,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let _: Event = read_frame(&mut client).unwrap();

        // Several times the timeout, saying nothing at all.
        thread::sleep(quick * 4);

        // And it is still there to be asked.
        write_frame(&mut client, &Request::ListRepos).unwrap();
        let answer: Result<Event, _> = read_frame(&mut client);
        assert!(
            answer.is_ok(),
            "an idle client must not be hung up on: {answer:?}"
        );
        drop(client);
        let _ = worker.join();
    }

    #[test]
    fn handshake_is_bounded_and_successful() {
        let (client, server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || serve_client(server).unwrap());
        let mut client = client;
        client.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        write_frame(
            &mut client,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let event: Event = read_frame(&mut client).unwrap();
        // No service to ask, so the capability fails closed: a harness that
        // cannot know a workspace must not advertise a capability for it.
        assert_eq!(
            accept_welcome(&event),
            Handshake::Agreed {
                ownership_movable: Some(false)
            }
        );
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    fn attach_over_the_socket_streams_screen_then_live_output() {
        use grove_proto::{Attach, ScrollbackRequest};
        let temp = TempDir::new();
        let runtime = grove_lua::DaemonRuntime::load(temp.0.join("missing.lua")).runtime;
        let fetch = fetch::FetchPolicy::from_config(2, runtime.config()).unwrap();
        let store = grove_state::Store::load_at(&temp.0, &temp.0, "");
        let terminals =
            terminal::TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 200);
        let service = session::SessionOrchestrator::new(
            store,
            Vec::new(),
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        );
        let service = Arc::new(Mutex::new(service));
        let (client, server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || serve_client_with(server, Some(service)).unwrap());
        let mut client = client;
        client.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        write_frame(
            &mut client,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        // The handshake carries the workspace capability: this service's
        // store was built without a session-scoped template, so the pane may
        // offer adopt and release.
        let Event::Welcome {
            version,
            ownership_movable,
        } = read_frame(&mut client).unwrap()
        else {
            panic!("expected Welcome")
        };
        assert_eq!(version, PROTOCOL_VERSION);
        assert!(ownership_movable);

        // Spawn over the socket; the id comes back and the attach follows it.
        write_frame(
            &mut client,
            &Request::SpawnTerminal(grove_proto::TerminalTarget::Scratch { cwd: None }),
        )
        .unwrap();
        let Event::TerminalSpawned { terminal, .. } = read_frame(&mut client).unwrap() else {
            panic!("expected TerminalSpawned")
        };
        write_frame(
            &mut client,
            &Request::AttachTerminal(Attach {
                terminal,
                scrollback: ScrollbackRequest::None,
                rows: 24,
                cols: 80,
            }),
        )
        .unwrap();
        // Screen first. Live output may interleave from the moment the
        // screen is sent — the contract permits it — so the scrollback search
        // skips anything else rather than assuming the next two frames.
        let Event::TerminalScreen { screen, .. } = read_frame(&mut client).unwrap() else {
            panic!("screen must arrive first")
        };
        assert_eq!(screen.rows, 24);
        let (seq, line_count, done) = loop {
            match read_frame(&mut client) {
                Ok(Event::TerminalScrollback {
                    seq, lines, done, ..
                }) => break (seq, lines.len(), done),
                Ok(Event::TerminalOutput { .. }) => continue,
                Ok(event) => panic!("expected scrollback after the screen, got {event:?}"),
                Err(error) => panic!("no scrollback after the screen: {error}"),
            }
        };
        assert_eq!((seq, line_count, done), (0, 0, true));

        // Live output interleave: input typed after attach reaches the client
        // as TerminalOutput, through the same writer as every other frame.
        write_frame(
            &mut client,
            &Request::Input {
                terminal,
                bytes: b"echo socket-stream\r".to_vec(),
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut seen = false;
        while !seen && Instant::now() < deadline {
            if let Ok(Event::TerminalOutput { bytes, .. }) = read_frame(&mut client) {
                seen = bytes.windows(6).any(|window| window == b"socket");
            }
        }
        assert!(seen, "live output never arrived over the socket");

        // The pty survives detach, and killing reports the exit exactly once.
        write_frame(&mut client, &Request::DetachTerminal(terminal)).unwrap();
        write_frame(
            &mut client,
            &Request::Input {
                terminal,
                bytes: b"echo still-running\r".to_vec(),
            },
        )
        .unwrap();
        write_frame(&mut client, &Request::KillTerminal(terminal)).unwrap();
        // After the detach the feed stops, but frames already queued in the
        // writer may still be in flight: read past any of them to the exit.
        let event = loop {
            match read_frame(&mut client) {
                Ok(Event::TerminalOutput { .. }) => continue,
                Ok(event) => break event,
                Err(error) => panic!("no TerminalExited for the kill: {error}"),
            }
        };
        let Event::TerminalExited {
            terminal: exited, ..
        } = event
        else {
            panic!("expected TerminalExited for the kill, got {event:?}")
        };
        assert_eq!(exited, terminal);
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    fn post_handshake_session_requests_reach_the_orchestrator() {
        let temp = TempDir::new();
        let runtime = grove_lua::DaemonRuntime::load(temp.0.join("missing.lua")).runtime;
        let fetch = fetch::FetchPolicy::from_config(2, runtime.config()).unwrap();
        let store = grove_state::Store::load_at(&temp.0, &temp.0, "");
        let terminals =
            terminal::TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let service = session::SessionOrchestrator::new(
            store,
            Vec::new(),
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        );
        let service = Arc::new(Mutex::new(service));
        let (client, server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || serve_client_with(server, Some(service)).unwrap());
        let mut client = client;
        client.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        write_frame(
            &mut client,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let _: Event = read_frame(&mut client).unwrap();
        write_frame(
            &mut client,
            &Request::OpenSession(grove_domain::SessionId("missing".into())),
        )
        .unwrap();
        let event: Event = read_frame(&mut client).unwrap();
        assert!(matches!(
            event,
            Event::Failed { message, .. } if message.contains("does not exist")
        ));
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    fn worktree_changes_are_pushed_without_polling_an_idle_client() {
        let temp = TempDir::new();
        let repo = temp.0.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Grove Test"]);
        git(&repo, &["config", "user.email", "grove@example.test"]);
        fs::write(repo.join("tracked"), "clean\n").unwrap();
        git(&repo, &["add", "tracked"]);
        git(&repo, &["commit", "-qm", "base"]);
        git(&repo, &["remote", "add", "origin", "."]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&repo, &["branch", "--set-upstream-to=origin/main", "main"]);

        let runtime = grove_lua::DaemonRuntime::load(temp.0.join("missing.lua")).runtime;
        let fetch = fetch::FetchPolicy::from_config(2, runtime.config()).unwrap();
        let store = grove_state::Store::load_at(&temp.0, &temp.0, "");
        let terminals =
            terminal::TerminalManager::new(PathBuf::from("/bin/sh"), temp.0.clone(), 100);
        let service = session::SessionOrchestrator::new(
            store,
            vec![Repository {
                id: RepoId("repo".into()),
                name: "repo".into(),
                path: repo.clone(),
                base_branch: None,
            }],
            temp.0.clone(),
            terminals,
            fetch,
            runtime,
        );
        let service = Arc::new(Mutex::new(service));
        let (client, server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || serve_client_with_watcher(server, service).unwrap());
        let mut client = client;
        client.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        write_frame(
            &mut client,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let _: Event = read_frame(&mut client).unwrap();
        write_frame(&mut client, &Request::ListWorktrees(RepoId("repo".into()))).unwrap();
        let Event::Worktrees { rows, .. } = read_frame(&mut client).unwrap() else {
            panic!("expected initial Worktrees")
        };
        assert_eq!(rows[0].dirty_files, 0);

        client
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        assert!(
            read_frame::<_, Event>(&mut client).is_err(),
            "an idle workspace must not push a repaint"
        );

        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        fs::write(repo.join("tracked"), "dirty\n").unwrap();
        let Event::Worktrees { repo: pushed, rows } = read_frame(&mut client).unwrap() else {
            panic!("expected a pushed Worktrees event")
        };
        assert_eq!(pushed, RepoId("repo".into()));
        assert_eq!(rows[0].dirty_files, 1);

        git(&repo, &["add", "tracked"]);
        git(&repo, &["commit", "-qm", "next"]);
        let Event::Worktrees { rows, .. } = read_frame(&mut client).unwrap() else {
            panic!("expected a pushed Worktrees event after commit")
        };
        assert_eq!(rows[0].dirty_files, 0);
        assert_eq!(rows[0].ahead, 1);

        client
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        assert!(
            read_frame::<_, Event>(&mut client).is_err(),
            "one git operation burst must produce only one debounced update"
        );

        drop(client);
        worker.join().unwrap();
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    use grove_lua::DaemonRuntime;
    use grove_state::Store;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("groved-capability-{label}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// The handshake is the one wire fact the client sees before any
    /// request; the review found every committed assertion pinned `true`,
    /// which a hardcoded flag would have passed. This one is session-scoped
    /// and fails against a hardcoded `true`.
    #[test]
    fn a_session_scoped_workspace_reports_its_capability_as_false() {
        let temp = temp_dir("session-scoped");
        let runtime = DaemonRuntime::load(
            env::temp_dir().join(format!("groved-missing-{}.lua", std::process::id())),
        )
        .runtime;
        let store = Store::load_at(&temp.join("state"), &temp, "{session}/{repo}/{branch_slug}");
        let fetch = fetch::FetchPolicy::from_config(2, runtime.config()).unwrap();
        let terminals = terminal::TerminalManager::new(PathBuf::from("/bin/sh"), temp.clone(), 100);
        let service = session::SessionOrchestrator::new(
            store,
            Vec::new(),
            temp.clone(),
            terminals,
            fetch,
            runtime,
        );
        let service = Arc::new(Mutex::new(service));
        let (client, server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || serve_client_with(server, Some(service)).unwrap());
        let mut client = client;
        client.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        write_frame(
            &mut client,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let Event::Welcome {
            ownership_movable, ..
        } = read_frame(&mut client).unwrap()
        else {
            panic!("expected Welcome")
        };
        // §2.4: the worktree's location depends on the session that made it,
        // so ownership cannot change hands — the pane must not offer the key.
        assert!(!ownership_movable);
        drop(client);
        worker.join().unwrap();
    }
}
