//! Daemon socket ownership and client connection lifecycle.

use fs2::FileExt;
use grove_proto::{
    Event, Handshake, PROTOCOL_VERSION, Request, accept_hello, accept_welcome, read_frame,
    write_frame,
};
use std::env;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

pub mod prune;
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

        if socket_path.exists() {
            if let Ok(stream) = configured_stream(&socket_path) {
                return Ok(BindOutcome::Existing(stream));
            }
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

    pub fn run(self) -> Result<(), LifecycleError> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    thread::spawn(move || {
                        let _ = serve_client(stream);
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

fn serve_client(mut stream: UnixStream) -> Result<(), grove_proto::FrameError> {
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
    while let Ok(request) = read_frame::<_, Request>(&mut stream) {
        write_frame(
            &mut stream,
            &Event::Failed {
                context: format!("{request:?}"),
                message: "request handling is not installed yet".into(),
            },
        )?;
    }
    Ok(())
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
}
