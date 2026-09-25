use crate::{
    CreateSession, Envelope, PROTOCOL_VERSION, Request, Response, SessionInfo, frame, read_frame,
    read_message, screen::Screen, unix_now, write_frame, write_message,
};
use anyhow::{Context as _, Result, anyhow, bail};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

/// How long the daemon lingers with no sessions. Exiting lets the next Bench
/// build start its own daemon, which is the only way a daemon is replaced
/// without anyone losing a shell.
const IDLE_EXIT_AFTER: Duration = Duration::from_secs(60);

/// How long a killed session's shell gets to exit on SIGHUP before it and its
/// process group are sent SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(3);

/// Starts a daemon for `socket` that outlives this process.
#[allow(
    clippy::disallowed_methods,
    reason = "callers are already on a background thread, and the daemon needs std's pre_exec"
)]
pub(crate) fn spawn(executable: &Path, socket: &Path) -> Result<()> {
    if let Some(dir) = socket.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {dir:?}"))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(socket.with_extension("log"))
        .context("opening the terminal host log")?;
    let mut command = Command::new(executable);
    command
        .arg("--terminal-host")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        // A daemon started from a project directory would otherwise keep that
        // directory busy for as long as it runs.
        .current_dir("/");
    // Fork once more and start a new session, so the daemon is neither this
    // process's child (it would linger as a zombie when it exits) nor in its
    // process group (it would receive the signals meant for Bench).
    unsafe {
        command.pre_exec(|| {
            match libc::fork() {
                -1 => return Err(io::Error::last_os_error()),
                0 => {}
                _ => libc::_exit(0),
            }
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let status = command
        .spawn()
        .context("starting the terminal host")?
        .wait()?;
    if !status.success() {
        bail!("starting the terminal host failed with {status}");
    }
    Ok(())
}

/// Runs the daemon until it has been idle for [`IDLE_EXIT_AFTER`].
pub fn run_daemon(socket: &Path) -> Result<()> {
    unsafe {
        // Bench starts the daemon from a background thread, whose blocked and
        // ignored signals survive exec. Left alone, the daemon could not be
        // stopped with SIGTERM, and every shell it starts would inherit the
        // same state, so Ctrl-C would not reach programs run in them.
        let mut unblocked: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut unblocked);
        libc::pthread_sigmask(libc::SIG_SETMASK, &unblocked, std::ptr::null_mut());
        for signal in [
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTERM,
            libc::SIGTSTP,
            libc::SIGTTIN,
            libc::SIGTTOU,
            libc::SIGCHLD,
            libc::SIGWINCH,
        ] {
            libc::signal(signal, libc::SIG_DFL);
        }
        // Sessions must survive the terminal that started the daemon going
        // away, and a client vanishing mid-write must not take it down. A
        // handler rather than SIG_IGN, because an ignored signal would stay
        // ignored in every shell, which then could not be hung up.
        libc::signal(
            libc::SIGHUP,
            ignore_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGPIPE,
            ignore_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }

    let Some(_lock) = acquire_lock(&socket.with_extension("lock"))? else {
        log(format_args!("another terminal host owns {socket:?}; exiting"));
        return Ok(());
    };
    match fs::remove_file(socket) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("removing the stale socket"),
    }
    let listener = UnixListener::bind(socket).with_context(|| format!("binding {socket:?}"))?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    log(format_args!(
        "terminal host {} listening on {socket:?}",
        std::process::id()
    ));

    let host = Arc::new(Host::default());
    thread::spawn({
        let host = host.clone();
        let socket = socket.to_path_buf();
        move || exit_when_idle(&host, &socket)
    });
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                log(format_args!("accepting a connection failed: {error}"));
                continue;
            }
        };
        host.activity().connections += 1;
        let host = host.clone();
        thread::spawn(move || {
            if let Err(error) = serve(&host, stream) {
                log(format_args!("connection failed: {error:#}"));
            }
            let mut activity = host.activity();
            activity.connections -= 1;
            activity.last_change = Instant::now();
        });
    }
    Ok(())
}

extern "C" fn ignore_signal(_: libc::c_int) {}

/// Holds an exclusive lock on `path` for as long as the returned file lives,
/// or returns `None` if another daemon holds it. Retries briefly, since the
/// previous daemon may be exiting right now.
fn acquire_lock(path: &Path) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {path:?}"))?;
    for _ in 0..20 {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(file));
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(None)
}

fn exit_when_idle(host: &Host, socket: &Path) {
    loop {
        thread::sleep(Duration::from_secs(5));
        let sessions = host.sessions();
        let activity = host.activity();
        if sessions.is_empty()
            && activity.connections == 0
            && activity.last_change.elapsed() >= IDLE_EXIT_AFTER
        {
            // Unlink while still holding both locks, so no client can connect
            // to a daemon that is about to exit.
            fs::remove_file(socket).ok();
            log(format_args!("terminal host idle; exiting"));
            std::process::exit(0);
        }
    }
}

fn log(message: std::fmt::Arguments) {
    eprintln!("[{}] {message}", unix_now());
}

struct Host {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    activity: Mutex<Activity>,
}

struct Activity {
    connections: usize,
    last_change: Instant,
}

impl Default for Host {
    fn default() -> Self {
        Self {
            sessions: Mutex::default(),
            activity: Mutex::new(Activity {
                connections: 0,
                last_change: Instant::now(),
            }),
        }
    }
}

impl Host {
    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn activity(&self) -> MutexGuard<'_, Activity> {
        self.activity.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn session(&self, id: &str) -> Result<Arc<Session>> {
        self.sessions()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("no terminal session {id}"))
    }

    fn remove(&self, id: &str) {
        self.sessions().remove(id);
        self.activity().last_change = Instant::now();
    }
}

struct Session {
    id: String,
    pid: u32,
    created_at: u64,
    master: Mutex<Box<dyn MasterPty + Send>>,
    input: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    state: Mutex<SessionState>,
}

struct SessionState {
    screen: Screen,
    attached: Option<Attachment>,
    next_attachment: u64,
}

struct Attachment {
    id: u64,
    client_pid: u32,
    output: Sender<Outgoing>,
}

enum Outgoing {
    Output(Vec<u8>),
    Exited(Option<i32>),
    Detached,
}

impl Session {
    fn state(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            pid: self.pid,
            created_at: self.created_at,
            attached: self.state().attached.is_some(),
        }
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        self.master
            .lock()
            .map_err(|_| anyhow!("pty lock poisoned"))?
            .resize(size)?;
        self.state().screen.resize(cols, rows);
        Ok(())
    }

    fn write_input(&self, bytes: &[u8]) -> Result<()> {
        let mut input = self.input.lock().map_err(|_| anyhow!("pty lock poisoned"))?;
        input.write_all(bytes)?;
        input.flush()?;
        Ok(())
    }

    fn kill(&self) {
        // SIGHUP first, like a terminal window closing, so shells can save
        // history and programs can clean up.
        unsafe {
            libc::killpg(self.pid as libc::pid_t, libc::SIGHUP);
        }
        let pid = self.pid;
        thread::spawn(move || {
            thread::sleep(KILL_GRACE);
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        });
        if let Ok(mut killer) = self.killer.lock() {
            killer.kill().ok();
        }
    }
}

fn serve(host: &Arc<Host>, stream: UnixStream) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let envelope: Envelope = match read_message(&mut reader) {
        Ok(envelope) => envelope,
        Err(error) => {
            // Most likely a newer client with a request this daemon predates.
            respond_error(&mut writer, &error)?;
            return Ok(());
        }
    };
    if envelope.version != PROTOCOL_VERSION {
        let error = anyhow!(
            "terminal host speaks protocol {PROTOCOL_VERSION}, not {}",
            envelope.version
        );
        respond_error(&mut writer, &error)?;
        return Ok(());
    }

    let response = match envelope.request {
        Request::Create(request) => create(host, request).map(Response::Created),
        Request::List => Ok(Response::Sessions {
            sessions: host.sessions().values().map(|session| session.info()).collect(),
        }),
        Request::Kill { id, attached_pid } => Ok(Response::Killed {
            killed: kill(host, &id, attached_pid),
        }),
        Request::Attach {
            id,
            cols,
            rows,
            client_pid,
        } => return attach(host, &id, cols, rows, client_pid, reader, writer),
    };
    match response {
        Ok(response) => write_message(&mut writer, &response),
        Err(error) => respond_error(&mut writer, &error),
    }
}

fn respond_error(writer: &mut impl Write, error: &anyhow::Error) -> Result<()> {
    write_message(
        writer,
        &Response::Error {
            message: format!("{error:#}"),
        },
    )
}

fn create(host: &Arc<Host>, request: CreateSession) -> Result<SessionInfo> {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: request.rows.max(1),
            cols: request.cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening a pty")?;
    let mut command = match request.program {
        Some(program) => {
            let mut command = CommandBuilder::new(program);
            command.args(request.args);
            command
        }
        None => CommandBuilder::new_default_prog(),
    };
    if let Some(cwd) = request.cwd.filter(|cwd| cwd.is_dir()) {
        command.cwd(cwd);
    } else if let Some(home) = std::env::var_os("HOME") {
        command.cwd(PathBuf::from(home));
    }
    for (key, value) in request.env {
        command.env(key, value);
    }
    let mut child = pty
        .slave
        .spawn_command(command)
        .context("spawning the shell")?;
    // The daemon must not hold the terminal side open, or reads on the master
    // would never see the shell's end.
    drop(pty.slave);
    let pid = child
        .process_id()
        .context("the shell has no process id")?;
    let output = pty.master.try_clone_reader()?;
    let input = pty.master.take_writer()?;
    let session = Arc::new(Session {
        id: uuid::Uuid::new_v4().to_string(),
        pid,
        created_at: unix_now(),
        killer: Mutex::new(child.clone_killer()),
        master: Mutex::new(pty.master),
        input: Mutex::new(input),
        state: Mutex::new(SessionState {
            screen: Screen::new(request.cols, request.rows),
            attached: None,
            next_attachment: 0,
        }),
    });
    host.sessions().insert(session.id.clone(), session.clone());
    log(format_args!("session {} started (pid {pid})", session.id));

    thread::spawn({
        let session = session.clone();
        move || pump_output(&session, output)
    });
    thread::spawn({
        let host = host.clone();
        let session = session.clone();
        move || {
            let exit_code = child
                .wait()
                .ok()
                .and_then(|status| i32::try_from(status.exit_code()).ok());
            // Let the output thread forward what the shell wrote last.
            thread::sleep(Duration::from_millis(100));
            host.remove(&session.id);
            if let Some(attachment) = session.state().attached.take() {
                attachment.output.send(Outgoing::Exited(exit_code)).ok();
            }
            log(format_args!(
                "session {} exited with {exit_code:?}",
                session.id
            ));
        }
    });
    Ok(session.info())
}

fn pump_output(session: &Session, mut output: Box<dyn Read + Send>) {
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let len = match output.read(&mut buffer) {
            Ok(0) => return,
            Ok(len) => len,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let bytes = &buffer[..len];
        // The screen and the attached client are updated under one lock, so an
        // attach sees every byte either in its repaint or on its stream.
        let mut state = session.state();
        state.screen.advance(bytes);
        let disconnected = state
            .attached
            .as_ref()
            .is_some_and(|attachment| attachment.output.send(Outgoing::Output(bytes.to_vec())).is_err());
        if disconnected {
            state.attached = None;
        }
    }
}

fn kill(host: &Host, id: &str, attached_pid: Option<u32>) -> bool {
    let Ok(session) = host.session(id) else {
        return false;
    };
    if let Some(attached_pid) = attached_pid
        && let Some(attachment) = session.state().attached.as_ref()
        && attachment.client_pid != attached_pid
    {
        return false;
    }
    log(format_args!("session {id} killed"));
    session.kill();
    true
}

fn attach(
    host: &Host,
    id: &str,
    cols: u16,
    rows: u16,
    client_pid: u32,
    mut reader: BufReader<UnixStream>,
    mut writer: UnixStream,
) -> Result<()> {
    let session = match host.session(id) {
        Ok(session) => session,
        Err(error) => return respond_error(&mut writer, &error),
    };
    // Resize before the repaint, so it is drawn at the client's size and a
    // full-screen program redraws for that size too.
    session.resize(cols, rows)?;

    let (output, outgoing) = mpsc::channel();
    let attachment_id = {
        let mut state = session.state();
        output.send(Outgoing::Output(state.screen.repaint())).ok();
        let attachment_id = state.next_attachment;
        state.next_attachment += 1;
        let previous = state.attached.replace(Attachment {
            id: attachment_id,
            client_pid,
            output,
        });
        if let Some(previous) = previous {
            previous.output.send(Outgoing::Detached).ok();
        }
        attachment_id
    };
    write_message(&mut writer, &Response::Attached)?;
    thread::spawn(move || forward_output(writer, outgoing));

    let result = forward_input(&session, &mut reader);
    let mut state = session.state();
    if state
        .attached
        .as_ref()
        .is_some_and(|attachment| attachment.id == attachment_id)
    {
        state.attached = None;
    }
    result
}

fn forward_output(mut writer: UnixStream, outgoing: Receiver<Outgoing>) {
    for message in outgoing {
        let result = match message {
            Outgoing::Output(bytes) => write_frame(&mut writer, frame::DATA, &bytes),
            Outgoing::Exited(code) => {
                let code = code.map(i32::to_be_bytes);
                let payload: &[u8] = match &code {
                    Some(code) => code,
                    None => &[],
                };
                write_frame(&mut writer, frame::EXITED, payload)
            }
            Outgoing::Detached => write_frame(&mut writer, frame::DETACHED, &[]),
        };
        if result.is_err() {
            break;
        }
    }
    writer.shutdown(std::net::Shutdown::Both).ok();
}

fn forward_input(session: &Session, reader: &mut BufReader<UnixStream>) -> Result<()> {
    while let Some((kind, payload)) = read_frame(reader)? {
        match kind {
            frame::DATA => session.write_input(&payload)?,
            frame::RESIZE => {
                if let [cols_high, cols_low, rows_high, rows_low] = payload[..] {
                    let cols = u16::from_be_bytes([cols_high, cols_low]);
                    let rows = u16::from_be_bytes([rows_high, rows_low]);
                    session.resize(cols.max(1), rows.max(1))?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TerminalHost;

    fn start_daemon() -> Result<(TerminalHost, PathBuf)> {
        // Not `temp_dir()`: on macOS that path is too long for a socket.
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = PathBuf::from(format!("/tmp/terminal-host-{}", &id[..8]));
        fs::create_dir_all(&dir)?;
        let socket = dir.join("host.sock");
        thread::spawn({
            let socket = socket.clone();
            move || run_daemon(&socket)
        });
        for _ in 0..100 {
            if UnixStream::connect(&socket).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let host = TerminalHost {
            executable: PathBuf::from("/nonexistent"),
            socket,
        };
        Ok((host, dir))
    }

    fn command(script: &str) -> CreateSession {
        CreateSession {
            cwd: None,
            program: Some("/bin/sh".to_string()),
            args: vec!["-c".to_string(), script.to_string()],
            env: HashMap::default(),
            cols: 80,
            rows: 24,
        }
    }

    fn read_until(reader: &mut impl Read, needle: &str) -> Result<Vec<u8>> {
        let mut seen = Vec::new();
        loop {
            let Some((kind, payload)) = read_frame(reader)? else {
                bail!("stream ended before {needle:?}; saw {:?}", String::from_utf8_lossy(&seen));
            };
            if kind == frame::EXITED {
                bail!("session exited before {needle:?}");
            }
            seen.extend(payload);
            if String::from_utf8_lossy(&seen).contains(needle) {
                return Ok(seen);
            }
        }
    }

    #[test]
    fn sessions_outlive_their_clients_and_report_their_exit() -> Result<()> {
        let (host, dir) = start_daemon()?;
        let session = host.create(command("echo ready; read line; echo got:$line; exit 7"))?;
        assert!(host.sessions()?.iter().any(|listed| listed.id == session.id));

        // A client that connects after the output was written still sees it,
        // through the repaint.
        thread::sleep(Duration::from_millis(200));
        let stream = UnixStream::connect(&host.socket)?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = stream;
        write_message(
            &mut writer,
            &Envelope {
                version: PROTOCOL_VERSION,
                request: Request::Attach {
                    id: session.id.clone(),
                    cols: 100,
                    rows: 30,
                    client_pid: 1,
                },
            },
        )?;
        assert!(matches!(read_message(&mut reader)?, Response::Attached));
        read_until(&mut reader, "ready")?;
        assert!(host.sessions()?.iter().any(|listed| listed.attached));

        // Another client's kill is refused while this one is attached.
        assert!(!host.kill(&session.id, Some(2))?);

        write_frame(&mut writer, frame::DATA, b"world\n")?;
        read_until(&mut reader, "got:world")?;
        let exit = loop {
            match read_frame(&mut reader)? {
                Some((frame::EXITED, payload)) => break payload,
                Some(_) => continue,
                None => bail!("stream ended without an exit"),
            }
        };
        assert_eq!(exit, 7i32.to_be_bytes());
        assert!(host.sessions()?.is_empty());
        fs::remove_dir_all(dir).ok();
        Ok(())
    }

    #[test]
    fn killing_a_session_ends_its_shell() -> Result<()> {
        let (host, dir) = start_daemon()?;
        let session = host.create(command("sleep 100"))?;
        assert!(host.kill(&session.id, None)?);
        for _ in 0..100 {
            if host.sessions()?.is_empty() {
                fs::remove_dir_all(dir).ok();
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
        bail!("the killed session is still listed")
    }
}
