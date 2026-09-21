use grove_domain::{Ownership, RepoId, SessionId, SessionState};
use grove_proto::{
    CliJson, CliWorktrees, Event, Handshake, PROTOCOL_VERSION, RepoRow, Request, SessionRow,
    WorktreeRef, WorktreeRow, accept_hello, read_frame, workspace_hash, write_frame,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("grove-cli-{unique}"));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn socket_path(runtime: &Path, workspace: &Path) -> PathBuf {
    runtime
        .join("grove")
        .join(format!("{}.sock", workspace_hash(workspace)))
}

fn listener(runtime: &Path, workspace: &Path) -> UnixListener {
    let socket = socket_path(runtime, workspace);
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    UnixListener::bind(socket).unwrap()
}

fn greet(stream: &mut UnixStream) {
    let hello: Request = read_frame(stream).unwrap();
    assert!(matches!(accept_hello(&hello), Handshake::Agreed { .. }));
    write_frame(
        stream,
        &Event::Welcome {
            version: PROTOCOL_VERSION,
            ownership_movable: true,
        },
    )
    .unwrap();
}

fn run(runtime: &Path, workspace: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_grove-cli"))
        .env("XDG_RUNTIME_DIR", runtime)
        .arg("--workspace")
        .arg(workspace)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn repos_json_is_parseable_protocol_data() {
    let temp = TempDir::new();
    let runtime = temp.0.join("runtime");
    let workspace = temp.0.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let listener = listener(&runtime, &workspace);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        greet(&mut stream);
        assert_eq!(
            read_frame::<_, Request>(&mut stream).unwrap(),
            Request::ListRepos
        );
        write_frame(
            &mut stream,
            &Event::Repos(vec![RepoRow {
                repo: RepoId("billing".into()),
                name: "billing-service".into(),
                base_branch: "origin/main".into(),
                base_from_origin_head: true,
                worktrees: 2,
                dirty: true,
                member: true,
            }]),
        )
        .unwrap();
    });

    let output = run(&runtime, &workspace, &["ls", "repos", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed = CliJson::<Vec<RepoRow>>::from_json(stdout.trim()).unwrap();
    assert_eq!(parsed.protocol, PROTOCOL_VERSION);
    assert_eq!(parsed.result[0].repo, RepoId("billing".into()));
    assert!(output.stderr.is_empty());
    server.join().unwrap();
}

#[test]
fn no_autostart_has_its_own_exit_code() {
    let temp = TempDir::new();
    let workspace = temp.0.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_grove-cli"))
        .env("XDG_RUNTIME_DIR", temp.0.join("runtime"))
        .env("GROVE_NO_AUTOSTART", "1")
        .arg("--workspace")
        .arg(&workspace)
        .args(["ls", "sessions", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("GROVE_NO_AUTOSTART"));
}

#[test]
fn missing_daemon_is_started_and_waited_for() {
    let temp = TempDir::new();
    let runtime = temp.0.join("runtime");
    let workspace = temp.0.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let marker = temp.0.join("started");
    let daemon = temp.0.join("fake-groved");
    fs::write(
        &daemon,
        "#!/bin/sh\ntouch \"$GROVE_AUTOSTART_MARKER\"\nsleep 1\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&daemon).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&daemon, permissions).unwrap();

    let server_runtime = runtime.clone();
    let server_workspace = workspace.clone();
    let server_marker = marker.clone();
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !server_marker.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "daemon was not started"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let listener = listener(&server_runtime, &server_workspace);
        let (mut stream, _) = listener.accept().unwrap();
        greet(&mut stream);
        assert_eq!(
            read_frame::<_, Request>(&mut stream).unwrap(),
            Request::ListSessions
        );
        write_frame(&mut stream, &Event::Sessions(vec![])).unwrap();
    });

    let output = Command::new(env!("CARGO_BIN_EXE_grove-cli"))
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("GROVE_DAEMON", &daemon)
        .env("GROVE_AUTOSTART_MARKER", &marker)
        .arg("--workspace")
        .arg(&workspace)
        .args(["ls", "sessions", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed = CliJson::<Vec<SessionRow>>::from_json(stdout.trim()).unwrap();
    assert!(parsed.result.is_empty());
    server.join().unwrap();
}

#[test]
fn end_without_yes_refuses_without_sending_the_destructive_request() {
    let temp = TempDir::new();
    let runtime = temp.0.join("runtime");
    let workspace = temp.0.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let listener = listener(&runtime, &workspace);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        greet(&mut stream);
        assert_eq!(
            read_frame::<_, Request>(&mut stream).unwrap(),
            Request::ListSessions
        );
        write_frame(
            &mut stream,
            &Event::Sessions(vec![SessionRow {
                id: SessionId("invoice".into()),
                name: "invoice split".into(),
                members: vec![],
                state: SessionState::Attached,
                terminals: 0,
                since: 0,
                size: 0,
            }]),
        )
        .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert!(read_frame::<_, Request>(&mut stream).is_err());
    });

    let output = run(&runtime, &workspace, &["end"]);
    assert_eq!(output.status.code(), Some(5));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--yes"));
    server.join().unwrap();
}

#[test]
fn no_open_session_has_its_own_exit_code() {
    let temp = TempDir::new();
    let runtime = temp.0.join("runtime");
    let workspace = temp.0.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let listener = listener(&runtime, &workspace);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        greet(&mut stream);
        assert_eq!(
            read_frame::<_, Request>(&mut stream).unwrap(),
            Request::ListSessions
        );
        write_frame(&mut stream, &Event::Sessions(vec![])).unwrap();
    });

    let output = run(&runtime, &workspace, &["snapshot"]);
    assert_eq!(output.status.code(), Some(4));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no open session"));
    server.join().unwrap();
}

#[test]
fn script_can_create_add_branch_and_list_two_repos() {
    let temp = TempDir::new();
    let runtime = temp.0.join("runtime");
    let workspace = temp.0.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let listener = listener(&runtime, &workspace);
    let server = thread::spawn(move || {
        let repos = vec![repo("billing"), repo("web")];
        let mut session: Option<SessionRow> = None;
        let mut worktrees: Vec<WorktreeRow> = Vec::new();
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            greet(&mut stream);
            while let Ok(request) = read_frame::<_, Request>(&mut stream) {
                let event = match request {
                    Request::SessionNew { name } => {
                        let row = SessionRow {
                            id: SessionId("session-1".into()),
                            name,
                            members: vec![],
                            state: SessionState::Attached,
                            terminals: 0,
                            since: 0,
                            size: 0,
                        };
                        session = Some(row.clone());
                        Event::SessionChanged(row)
                    }
                    Request::ListSessions => Event::Sessions(session.clone().into_iter().collect()),
                    Request::ListRepos => Event::Repos(repos.clone()),
                    Request::AddMember { repo, .. } => {
                        let row = session.as_mut().unwrap();
                        row.members.push(repo.0);
                        Event::SessionChanged(row.clone())
                    }
                    Request::NewWorktrees { branch, repos, .. } => {
                        worktrees = repos
                            .into_iter()
                            .map(|repo| worktree(repo, &branch))
                            .collect();
                        Event::SessionChanged(session.clone().unwrap())
                    }
                    Request::ListWorktrees(repo) => Event::Worktrees {
                        rows: worktrees
                            .iter()
                            .filter(|row| row.worktree.repo == repo)
                            .cloned()
                            .collect(),
                        repo,
                    },
                    other => panic!("unexpected request: {other:?}"),
                };
                write_frame(&mut stream, &event).unwrap();
            }
        }
    });

    for args in [
        vec!["session", "new", "invoice split"],
        vec!["add", "billing", "web"],
        vec!["new", "feat/invoice", "--repo", "billing", "--repo", "web"],
    ] {
        let output = run(&runtime, &workspace, &args);
        assert!(
            output.status.success(),
            "{}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = run(&runtime, &workspace, &["ls", "worktrees", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed =
        CliJson::<Vec<CliWorktrees>>::from_json(String::from_utf8(output.stdout).unwrap().trim())
            .unwrap();
    assert_eq!(parsed.result.len(), 2);
    assert!(
        parsed.result.iter().all(|group| {
            group.rows.len() == 1 && group.rows[0].worktree.branch == "feat/invoice"
        })
    );
    server.join().unwrap();
}

fn repo(name: &str) -> RepoRow {
    RepoRow {
        repo: RepoId(name.into()),
        name: name.into(),
        base_branch: "origin/main".into(),
        base_from_origin_head: true,
        worktrees: 0,
        dirty: false,
        member: false,
    }
}

fn worktree(repo: RepoId, branch: &str) -> WorktreeRow {
    WorktreeRow {
        worktree: WorktreeRef {
            repo,
            branch: branch.into(),
        },
        detached: false,
        ownership: Ownership::Ours,
        ahead: 0,
        behind: 0,
        dirty_files: 0,
        age: 0,
        size: 0,
        terminal: None,
        foreground: None,
        stale: false,
    }
}
