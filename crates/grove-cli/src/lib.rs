//! Non-interactive client for Grove's daemon protocol.

use grove_domain::{RepoId, SessionState};
use grove_proto::{
    CliJson, CliWorktrees, Event, Handshake, PROTOCOL_VERSION, PruneCandidate, RepoRow, Request,
    SessionRow, TerminalRow, WorktreeRef, accept_welcome, read_frame, socket_path, write_frame,
};
use std::ffi::OsString;
use std::io;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};

pub const EXIT_USAGE: i32 = 2;
pub const EXIT_NO_DAEMON: i32 = 3;
pub const EXIT_NO_SESSION: i32 = 4;
pub const EXIT_REFUSED: i32 = 5;
pub const EXIT_PROTOCOL: i32 = 6;

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct Failure {
    code: i32,
    message: String,
}

impl Failure {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug)]
struct Options {
    workspace: PathBuf,
    json: bool,
    yes: bool,
    command: CliCommand,
}

#[derive(Debug)]
enum CliCommand {
    List(ListKind),
    SessionNew(String),
    SessionRename(String),
    SessionOpen(String),
    SessionClose,
    SessionDetach,
    SessionEnd,
    Add(Vec<String>),
    Remove(Vec<String>),
    NewWorktrees { branch: String, repos: Vec<String> },
    Fetch(Option<String>),
    Prune { dry_run: bool },
    Snapshot,
    Scan,
    Help,
}

#[derive(Clone, Copy, Debug)]
enum ListKind {
    Repos,
    Worktrees,
    Sessions,
    Terminals,
}

pub fn run(args: impl Iterator<Item = OsString>) -> i32 {
    let options = match parse(args) {
        Ok(options) => options,
        Err(failure) => {
            eprintln!("grove-cli: {}", failure.message);
            return failure.code;
        }
    };
    if matches!(options.command, CliCommand::Help) {
        print_help();
        return 0;
    }
    match execute(options) {
        Ok(()) => 0,
        Err(failure) => {
            eprintln!("grove-cli: {}", failure.message);
            failure.code
        }
    }
}

fn parse(args: impl Iterator<Item = OsString>) -> Result<Options, Failure> {
    let mut workspace = None;
    let mut json = false;
    let mut yes = false;
    let mut words = Vec::new();
    let mut args = args.peekable();
    while let Some(argument) = args.next() {
        if argument == "--workspace" {
            workspace = Some(
                args.next()
                    .map(PathBuf::from)
                    .ok_or_else(|| usage("--workspace requires a path"))?,
            );
        } else if argument == "--json" {
            json = true;
        } else if argument == "--yes" {
            yes = true;
        } else {
            words.push(
                argument
                    .into_string()
                    .map_err(|_| usage("command arguments must be valid UTF-8"))?,
            );
        }
    }
    let workspace = match workspace {
        Some(path) => path,
        None => std::env::current_dir()
            .map_err(|error| usage(format!("could not read current directory: {error}")))?,
    };
    Ok(Options {
        workspace,
        json,
        yes,
        command: parse_command(&words)?,
    })
}

fn parse_command(words: &[String]) -> Result<CliCommand, Failure> {
    let strings = |from: usize| words[from..].to_vec();
    match words {
        [] => Err(usage("a command is required; try --help")),
        [help] if help == "--help" || help == "-h" || help == "help" => Ok(CliCommand::Help),
        [command, kind] if command == "ls" => Ok(CliCommand::List(match kind.as_str() {
            "repos" => ListKind::Repos,
            "worktrees" => ListKind::Worktrees,
            "sessions" => ListKind::Sessions,
            "terminals" => ListKind::Terminals,
            _ => return Err(usage("ls expects repos, worktrees, sessions, or terminals")),
        })),
        [session, verb, name] if session == "session" && verb == "new" => {
            Ok(CliCommand::SessionNew(name.clone()))
        }
        [session, verb, name] if session == "session" && verb == "rename" => {
            Ok(CliCommand::SessionRename(name.clone()))
        }
        [session, verb, name] if session == "session" && verb == "open" => {
            Ok(CliCommand::SessionOpen(name.clone()))
        }
        [session, verb] if session == "session" && verb == "close" => Ok(CliCommand::SessionClose),
        [session, verb] if session == "session" && verb == "detach" => {
            Ok(CliCommand::SessionDetach)
        }
        [session, verb] if session == "session" && verb == "end" => Ok(CliCommand::SessionEnd),
        [command] if command == "end" => Ok(CliCommand::SessionEnd),
        [command, rest @ ..] if command == "add" && !rest.is_empty() => {
            Ok(CliCommand::Add(strings(1)))
        }
        [command, rest @ ..] if command == "remove" && !rest.is_empty() => {
            Ok(CliCommand::Remove(strings(1)))
        }
        [command, branch, rest @ ..] if command == "new" => {
            let mut repos = Vec::new();
            let mut index = 0;
            while index < rest.len() {
                if rest[index] != "--repo" || index + 1 >= rest.len() {
                    return Err(usage("new expects --repo <repo> one or more times"));
                }
                repos.push(rest[index + 1].clone());
                index += 2;
            }
            if repos.is_empty() {
                return Err(usage("new requires at least one --repo <repo>"));
            }
            Ok(CliCommand::NewWorktrees {
                branch: branch.clone(),
                repos,
            })
        }
        [command] if command == "fetch" => Ok(CliCommand::Fetch(None)),
        [command, repo] if command == "fetch" => Ok(CliCommand::Fetch(Some(repo.clone()))),
        [command] if command == "prune" => Ok(CliCommand::Prune { dry_run: false }),
        [command, flag] if command == "prune" && flag == "--dry-run" => {
            Ok(CliCommand::Prune { dry_run: true })
        }
        [command] if command == "snapshot" => Ok(CliCommand::Snapshot),
        [command] if command == "scan" => Ok(CliCommand::Scan),
        _ => Err(usage("unknown or malformed command; try --help")),
    }
}

fn usage(message: impl Into<String>) -> Failure {
    Failure::new(EXIT_USAGE, message)
}

fn execute(options: Options) -> Result<(), Failure> {
    let mut client = Client::connect(&options.workspace)?;
    match options.command {
        CliCommand::List(kind) => list(&mut client, kind, options.json),
        CliCommand::SessionNew(name) => {
            mutate(&mut client, Request::SessionNew { name }, |event| {
                matches!(event, Event::SessionChanged(_))
            })
        }
        CliCommand::SessionRename(name) => {
            let session = current_session(&mut client)?;
            mutate(
                &mut client,
                Request::SessionRename {
                    session: session.id,
                    name,
                },
                |event| matches!(event, Event::SessionChanged(_)),
            )
        }
        CliCommand::SessionOpen(name) => {
            let session = named_session(&mut client, &name)?;
            mutate(&mut client, Request::OpenSession(session.id), |event| {
                matches!(event, Event::SessionChanged(_))
            })
        }
        CliCommand::SessionClose => session_state_change(&mut client, RequestKind::Close),
        CliCommand::SessionDetach => session_state_change(&mut client, RequestKind::Detach),
        CliCommand::SessionEnd => {
            let session = current_session(&mut client)?;
            if !options.yes {
                return Err(Failure::new(
                    EXIT_REFUSED,
                    "session end removes its worktrees; rerun with --yes to confirm",
                ));
            }
            let id = session.id;
            mutate(&mut client, Request::EndSession(id.clone()), move |event| {
                matches!(event, Event::SessionEnded(ended) if ended == &id)
                    || matches!(event, Event::SessionChanged(_))
            })
        }
        CliCommand::Add(repos) => membership(&mut client, repos, true),
        CliCommand::Remove(repos) => membership(&mut client, repos, false),
        CliCommand::NewWorktrees { branch, repos } => {
            let session = current_session(&mut client)?;
            let repos = resolve_repos(&mut client, &repos)?;
            mutate(
                &mut client,
                Request::NewWorktrees {
                    session: session.id,
                    branch,
                    repos,
                },
                |event| matches!(event, Event::SessionChanged(_)),
            )
        }
        CliCommand::Fetch(repo) => {
            let session = current_session(&mut client)?;
            let repo = match repo {
                Some(repo) => Some(resolve_repos(&mut client, &[repo])?.remove(0)),
                None => None,
            };
            mutate(
                &mut client,
                Request::Fetch {
                    session: session.id,
                    repo,
                },
                |event| matches!(event, Event::SessionChanged(_)),
            )
        }
        CliCommand::Prune { dry_run } => prune(&mut client, dry_run, options.yes, options.json),
        CliCommand::Snapshot => {
            let session = current_session(&mut client)?;
            mutate(&mut client, Request::SaveSnapshot(session.id), |event| {
                matches!(event, Event::Sessions(_))
            })
        }
        CliCommand::Scan => {
            let events =
                client.ask_until(Request::Scan, |event| matches!(event, Event::Repos(_)))?;
            let repos = repos_from(events)?;
            print_rows(&repos, options.json, |row| {
                format!("{}\t{}\t{}", row.repo.0, row.name, row.base_branch)
            })
        }
        CliCommand::Help => unreachable!(),
    }
}

#[derive(Clone, Copy)]
enum RequestKind {
    Close,
    Detach,
}

fn session_state_change(client: &mut Client, kind: RequestKind) -> Result<(), Failure> {
    let session = current_session(client)?;
    let request = match kind {
        RequestKind::Close => Request::CloseSession(session.id),
        RequestKind::Detach => Request::DetachSession(session.id),
    };
    mutate(client, request, |event| {
        matches!(event, Event::SessionChanged(_))
    })
}

fn membership(client: &mut Client, names: Vec<String>, add: bool) -> Result<(), Failure> {
    let session = current_session(client)?;
    let repos = resolve_repos(client, &names)?;
    for repo in repos {
        let request = if add {
            Request::AddMember {
                session: session.id.clone(),
                repo,
            }
        } else {
            Request::RemoveMember {
                session: session.id.clone(),
                repo,
            }
        };
        mutate(client, request, |event| {
            matches!(event, Event::SessionChanged(_))
        })?;
    }
    Ok(())
}

fn list(client: &mut Client, kind: ListKind, json: bool) -> Result<(), Failure> {
    match kind {
        ListKind::Repos => {
            let events =
                client.ask_until(Request::ListRepos, |event| matches!(event, Event::Repos(_)))?;
            let rows = repos_from(events)?;
            print_rows(&rows, json, |row| {
                format!("{}\t{}\t{}", row.repo.0, row.name, row.base_branch)
            })
        }
        ListKind::Sessions => {
            let rows = sessions(client)?;
            print_rows(&rows, json, |row| {
                format!("{}\t{}\t{:?}", row.id.0, row.name, row.state)
            })
        }
        ListKind::Terminals => {
            let events = client.ask_until(Request::ListTerminals, |event| {
                matches!(event, Event::Terminals(_))
            })?;
            let rows = events
                .into_iter()
                .find_map(|event| match event {
                    Event::Terminals(rows) => Some(rows),
                    _ => None,
                })
                .ok_or_else(|| protocol("daemon omitted terminal rows"))?;
            print_rows(&rows, json, |row: &TerminalRow| {
                format!("{}\t{:?}", row.terminal.0, row.target)
            })
        }
        ListKind::Worktrees => {
            let repos = repos_from(
                client.ask_until(Request::ListRepos, |event| matches!(event, Event::Repos(_)))?,
            )?;
            let mut groups = Vec::with_capacity(repos.len());
            for repo in repos {
                let id = repo.repo;
                let events = client.ask_until(
                    Request::ListWorktrees(id.clone()),
                    |event| matches!(event, Event::Worktrees { repo, .. } if repo == &id),
                )?;
                let rows = events
                    .into_iter()
                    .find_map(|event| match event {
                        Event::Worktrees { repo, rows } if repo == id => Some(rows),
                        _ => None,
                    })
                    .ok_or_else(|| protocol("daemon omitted worktree rows"))?;
                groups.push(CliWorktrees { repo: id, rows });
            }
            if json {
                print_json(groups)
            } else {
                for group in groups {
                    for row in group.rows {
                        println!(
                            "{}\t{}\t{:?}\t{}\t{}",
                            group.repo.0, row.worktree.branch, row.ownership, row.ahead, row.behind
                        );
                    }
                }
                Ok(())
            }
        }
    }
}

fn prune(client: &mut Client, dry_run: bool, yes: bool, json: bool) -> Result<(), Failure> {
    let events = client.ask_until(Request::ListPruneCandidates, |event| {
        matches!(event, Event::PruneCandidates(_))
    })?;
    let candidates = events
        .into_iter()
        .find_map(|event| match event {
            Event::PruneCandidates(rows) => Some(rows),
            _ => None,
        })
        .ok_or_else(|| protocol("daemon omitted prune candidates"))?;
    if dry_run || !yes {
        print_rows(&candidates, json, |row: &PruneCandidate| {
            format!(
                "{}\t{}\t{:?}",
                row.worktree.repo.0, row.worktree.branch, row.blockers
            )
        })?;
        if dry_run {
            return Ok(());
        }
        return Err(Failure::new(
            EXIT_REFUSED,
            "prune removes worktrees; rerun with --yes to remove safe candidates",
        ));
    }
    let selection: Vec<WorktreeRef> = candidates
        .into_iter()
        .filter(|candidate| candidate.blockers.is_empty())
        .map(|candidate| candidate.worktree)
        .collect();
    let events = client.ask_until(Request::Prune(selection), |event| {
        matches!(event, Event::Pruned { .. })
    })?;
    match events
        .into_iter()
        .find(|event| matches!(event, Event::Pruned { .. }))
    {
        Some(Event::Pruned {
            removed,
            failed,
            reclaimed,
        }) => {
            println!(
                "removed {} worktree(s), reclaimed {reclaimed} bytes",
                removed.len()
            );
            if failed.is_empty() {
                Ok(())
            } else {
                Err(Failure::new(
                    EXIT_REFUSED,
                    format!("{} worktree(s) could not be pruned", failed.len()),
                ))
            }
        }
        _ => Err(protocol("daemon omitted prune result")),
    }
}

fn mutate(
    client: &mut Client,
    request: Request,
    done: impl Fn(&Event) -> bool,
) -> Result<(), Failure> {
    let events = client.ask_until(request, done)?;
    let failures: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::Failed { context, message } => Some(format!("{context}: {message}")),
            _ => None,
        })
        .collect();
    if failures.is_empty() {
        println!("ok");
        Ok(())
    } else {
        Err(Failure::new(EXIT_REFUSED, failures.join("; ")))
    }
}

fn sessions(client: &mut Client) -> Result<Vec<SessionRow>, Failure> {
    let events = client.ask_until(Request::ListSessions, |event| {
        matches!(event, Event::Sessions(_))
    })?;
    events
        .into_iter()
        .find_map(|event| match event {
            Event::Sessions(rows) => Some(rows),
            _ => None,
        })
        .ok_or_else(|| protocol("daemon omitted session rows"))
}

fn current_session(client: &mut Client) -> Result<SessionRow, Failure> {
    let rows = sessions(client)?;
    if let Some(id) = std::env::var_os("GROVE_SESSION_ID").filter(|id| !id.is_empty()) {
        let id = id.to_string_lossy();
        return rows.into_iter().find(|row| row.id.0 == id).ok_or_else(|| {
            Failure::new(EXIT_NO_SESSION, format!("session {id:?} does not exist"))
        });
    }
    rows.into_iter()
        .find(|row| row.state == SessionState::Attached)
        .ok_or_else(|| Failure::new(EXIT_NO_SESSION, "there is no open session"))
}

fn named_session(client: &mut Client, name: &str) -> Result<SessionRow, Failure> {
    let matches: Vec<_> = sessions(client)?
        .into_iter()
        .filter(|row| row.name == name || row.id.0 == name)
        .collect();
    match matches.as_slice() {
        [row] => Ok(row.clone()),
        [] => Err(Failure::new(
            EXIT_NO_SESSION,
            format!("session {name:?} does not exist"),
        )),
        _ => Err(Failure::new(
            EXIT_REFUSED,
            format!("session name {name:?} is ambiguous; use its id"),
        )),
    }
}

fn resolve_repos(client: &mut Client, names: &[String]) -> Result<Vec<RepoId>, Failure> {
    let rows = repos_from(
        client.ask_until(Request::ListRepos, |event| matches!(event, Event::Repos(_)))?,
    )?;
    names
        .iter()
        .map(|name| {
            rows.iter()
                .find(|row| row.name == *name || row.repo.0 == *name)
                .map(|row| row.repo.clone())
                .ok_or_else(|| Failure::new(EXIT_REFUSED, format!("repo {name:?} does not exist")))
        })
        .collect()
}

fn repos_from(events: Vec<Event>) -> Result<Vec<RepoRow>, Failure> {
    events
        .into_iter()
        .find_map(|event| match event {
            Event::Repos(rows) => Some(rows),
            _ => None,
        })
        .ok_or_else(|| protocol("daemon omitted repo rows"))
}

fn print_rows<T: grove_proto::CliSerialize>(
    rows: &[T],
    json: bool,
    human: impl Fn(&T) -> String,
) -> Result<(), Failure> {
    if json {
        print_json(rows)
    } else {
        for row in rows {
            println!("{}", human(row));
        }
        Ok(())
    }
}

fn print_json<T: grove_proto::CliSerialize>(result: T) -> Result<(), Failure> {
    let json = CliJson::new(result)
        .to_json()
        .map_err(|error| protocol(format!("could not encode JSON result: {error}")))?;
    println!("{json}");
    Ok(())
}

fn protocol(message: impl Into<String>) -> Failure {
    Failure::new(EXIT_PROTOCOL, message)
}

struct Client {
    stream: UnixStream,
}

impl Client {
    fn connect(workspace: &Path) -> Result<Self, Failure> {
        let socket = socket_path(workspace);
        let mut stream = reach_daemon(workspace, &socket)?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(|error| protocol(format!("could not configure daemon socket: {error}")))?;
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(|error| protocol(format!("could not configure daemon socket: {error}")))?;
        write_frame(
            &mut stream,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .map_err(|error| protocol(format!("could not greet daemon: {error}")))?;
        let welcome: Event = read_frame(&mut stream)
            .map_err(|error| protocol(format!("daemon did not greet back: {error}")))?;
        match accept_welcome(&welcome) {
            Handshake::Agreed { .. } => Ok(Self { stream }),
            Handshake::Mismatch { daemon, client } => Err(protocol(format!(
                "protocol mismatch: daemon v{daemon}, client v{client}"
            ))),
            Handshake::NotHello => Err(protocol("daemon sent an invalid greeting")),
        }
    }

    fn ask_until(
        &mut self,
        request: Request,
        done: impl Fn(&Event) -> bool,
    ) -> Result<Vec<Event>, Failure> {
        let failure_context = request.failure_context();
        write_frame(&mut self.stream, &request)
            .map_err(|error| protocol(format!("could not send request: {error}")))?;
        let mut events = Vec::new();
        loop {
            let event: Event = read_frame(&mut self.stream)
                .map_err(|error| protocol(format!("daemon response failed: {error}")))?;
            let is_done = done(&event);
            let direct_failure = match &event {
                Event::Failed { context, message } if context == failure_context => {
                    Some(format!("{context}: {message}"))
                }
                _ => None,
            };
            events.push(event);
            if is_done {
                return Ok(events);
            }
            if let Some(message) = direct_failure {
                return Err(Failure::new(EXIT_REFUSED, message));
            }
        }
    }
}

fn reach_daemon(workspace: &Path, socket: &Path) -> Result<UnixStream, Failure> {
    match UnixStream::connect(socket) {
        Ok(stream) => return Ok(stream),
        Err(error) if missing_daemon(&error) => {}
        Err(error) => {
            return Err(Failure::new(
                EXIT_NO_DAEMON,
                format!("could not connect to {}: {error}", socket.display()),
            ));
        }
    }
    if std::env::var_os("GROVE_NO_AUTOSTART").is_some_and(|value| !value.is_empty()) {
        return Err(Failure::new(
            EXIT_NO_DAEMON,
            format!(
                "no daemon at {} and GROVE_NO_AUTOSTART is set",
                socket.display()
            ),
        ));
    }
    start_daemon(workspace, socket)
}

fn start_daemon(workspace: &Path, socket: &Path) -> Result<UnixStream, Failure> {
    let binary = daemon_binary();
    let log = socket.with_extension("log");
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            Failure::new(
                EXIT_NO_DAEMON,
                format!("could not create {}: {error}", parent.display()),
            )
        })?;
    }
    let stdout = std::fs::File::create(&log).map_err(|error| {
        Failure::new(
            EXIT_NO_DAEMON,
            format!("could not open {}: {error}", log.display()),
        )
    })?;
    let stderr = stdout.try_clone().map_err(|error| {
        Failure::new(
            EXIT_NO_DAEMON,
            format!("could not clone daemon log: {error}"),
        )
    })?;
    let mut child = ProcessCommand::new(&binary)
        .arg("--workspace")
        .arg(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .process_group(0)
        .spawn()
        .map_err(|error| {
            Failure::new(
                EXIT_NO_DAEMON,
                format!("could not start {}: {error}", binary.display()),
            )
        })?;
    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    loop {
        if let Ok(stream) = UnixStream::connect(socket) {
            return Ok(stream);
        }
        if let Ok(Some(status)) = child.try_wait() {
            if let Ok(stream) = UnixStream::connect(socket) {
                return Ok(stream);
            }
            return Err(Failure::new(
                EXIT_NO_DAEMON,
                format!(
                    "{} exited ({status}) before taking {}; see {}",
                    binary.display(),
                    socket.display(),
                    log.display()
                ),
            ));
        }
        if Instant::now() >= deadline {
            return Err(Failure::new(
                EXIT_NO_DAEMON,
                format!(
                    "{} did not take {} within {}s; see {}",
                    binary.display(),
                    socket.display(),
                    DAEMON_START_TIMEOUT.as_secs(),
                    log.display()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn daemon_binary() -> PathBuf {
    if let Some(explicit) = std::env::var_os("GROVE_DAEMON") {
        return explicit.into();
    }
    if let Some(beside) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::parent)
        .map(|directory| directory.join("groved"))
        .filter(|path| path.is_file())
    {
        return beside;
    }
    PathBuf::from("groved")
}

fn missing_daemon(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused | io::ErrorKind::WouldBlock
    )
}

fn print_help() {
    println!(
        "grove-cli [--workspace PATH] [--json] [--yes] COMMAND\n\
         \n\
         Commands:\n\
           ls repos|worktrees|sessions|terminals\n\
           session new NAME | rename NAME | open NAME | close | detach | end\n\
           add REPO... | remove REPO...\n\
           new BRANCH --repo REPO [--repo REPO...]\n\
           fetch [REPO]\n\
           prune [--dry-run]\n\
           snapshot\n\
           scan\n\
         \n\
         Exit codes: 0 success, 2 usage, 3 no daemon, 4 no session, 5 refused, 6 protocol.\n\
         session end and prune require --yes before removing anything."
    );
}
