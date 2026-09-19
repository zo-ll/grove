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

pub mod fetch;
pub mod prune;
pub mod session;
pub mod terminal;

const IO_TIMEOUT: Duration = Duration::from_secs(5);

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
        lock.lock_exclusive()
            .map_err(|source| socket_error(&lock_path, source))?;

        if socket_path.exists() {
            match configured_stream(&socket_path) {
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
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let service = Arc::clone(&service);
                    thread::spawn(move || {
                        let _ = serve_client_with(stream, Some(service));
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
        Handshake::Agreed => Ok(stream),
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
        Err(LifecycleError::Socket { source, .. })
            if matches!(
                source.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
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
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && Instant::now() < deadline =>
            {
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

fn serve_client_with(
    mut stream: UnixStream,
    service: Option<Arc<Mutex<session::SessionOrchestrator>>>,
) -> Result<(), grove_proto::FrameError> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let hello: Request = read_frame(&mut stream)?;
    match accept_hello(&hello) {
        Handshake::Agreed => write_frame(
            &mut stream,
            &Event::Welcome {
                version: PROTOCOL_VERSION,
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

    while let Ok(request) = read_frame::<_, Request>(&mut stream) {
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
                        context: format!("{request:?}"),
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
    fn drop(&mut self) {
        // Every path that loses a feed — detach, re-attach, replacement, or
        // the connection ending — goes through here, so a client that
        // disconnects while attached cannot pin the forwarder and the
        // connection writer for as long as the pty lives.
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
// teardown — goes through Drop, which sets the stop flag; the forwarder exits
// within its polling window and the abandoned subscriber is cleaned up by the
// pty reader the next time output arrives.

fn socket_error(path: &Path, source: io::Error) -> LifecycleError {
    LifecycleError::Socket {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let deadline = Instant::now() + Duration::from_secs(1);
        while UnixStream::connect(&path).is_ok() {
            assert!(
                Instant::now() < deadline,
                "closed listener stayed connectable"
            );
            thread::yield_now();
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
        assert_eq!(accept_welcome(&event), Handshake::Agreed);
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
        let _: Event = read_frame(&mut client).unwrap();

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
        // Screen first, then the terminal empty-chunk, in that order.
        let Event::TerminalScreen { screen, .. } = read_frame(&mut client).unwrap() else {
            panic!("screen must arrive first")
        };
        assert_eq!(screen.rows, 24);
        let Event::TerminalScrollback {
            seq, lines, done, ..
        } = read_frame(&mut client).unwrap()
        else {
            panic!("expected scrollback after the screen")
        };
        assert_eq!((seq, lines.len(), done), (0, 0, true));

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
}
