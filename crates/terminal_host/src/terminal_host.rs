//! Keeps interactive terminals alive across Bench restarts.
//!
//! A detached daemon (`bench --terminal-host <socket>`) owns every persistent
//! shell's PTY, so quitting, crashing or redeploying Bench leaves the shells
//! running. Each terminal tab runs `bench --terminal-attach <socket> <id>` on
//! its own PTY instead of a shell, the way a tab would run `tmux attach`: the
//! attach client relays bytes between the tab and the daemon, and the daemon
//! repaints the session's screen whenever a client attaches.
//!
//! A session only ends when its shell exits or a client asks for it to be
//! killed. Disconnecting never kills anything.

#[cfg(unix)]
mod attach;
#[cfg(unix)]
mod daemon;
mod screen;

#[cfg(unix)]
pub use attach::run_attach;
#[cfg(unix)]
pub use daemon::run_daemon;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{self, BufRead, Read, Write},
    path::PathBuf,
    sync::OnceLock,
};

/// Bumped on any incompatible change to the requests, responses or frames
/// below. A daemon only serves clients of its own version, so an old daemon
/// that outlives an upgrade keeps its sessions, and the new app falls back to
/// plain shells until that daemon exits.
pub const PROTOCOL_VERSION: u32 = 1;

pub const SOCKET_FILE_NAME: &str = "terminal-host.sock";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    /// The session's shell, which is also its process group leader.
    pub pid: u32,
    /// Unix seconds.
    pub created_at: u64,
    pub attached: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateSession {
    pub cwd: Option<PathBuf>,
    /// `None` runs the user's login shell.
    pub program: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Create(CreateSession),
    List,
    Kill {
        id: String,
        /// Only kill the session if no client is attached, or if this is the
        /// attached client. A tab that lost its session to another Bench
        /// instance must not kill it when it closes.
        attached_pid: Option<u32>,
    },
    Attach {
        id: String,
        cols: u16,
        rows: u16,
        client_pid: u32,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    version: u32,
    #[serde(flatten)]
    request: Request,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum Response {
    Created(SessionInfo),
    Sessions { sessions: Vec<SessionInfo> },
    Killed { killed: bool },
    Attached,
    Error { message: String },
}

/// After an `Attach` response the connection carries frames in both
/// directions: a kind byte, a big-endian `u32` length, then the payload.
mod frame {
    /// Client to daemon: input for the shell. Daemon to client: shell output.
    pub const DATA: u8 = 0;
    /// Client to daemon: `cols` and `rows` as big-endian `u16`s.
    pub const RESIZE: u8 = 1;
    /// Daemon to client: the shell exited, with its exit code as a
    /// big-endian `i32` when it has one.
    pub const EXITED: u8 = 2;
    /// Daemon to client: another client attached to this session.
    pub const DETACHED: u8 = 3;
}

const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

fn write_frame(writer: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&len.to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()
}

/// Returns `None` when the peer closed the connection between frames.
fn read_frame(reader: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds the limit"),
        ));
    }
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    Ok(Some((header[0], payload)))
}

fn write_message(writer: &mut impl Write, message: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()?;
    Ok(())
}

fn read_message<T: for<'de> Deserialize<'de>>(reader: &mut impl BufRead) -> Result<T> {
    let mut line = String::new();
    // A request or response is a few kilobytes at most; the limit keeps a
    // confused peer from growing the buffer without bound.
    reader.take(1024 * 1024).read_line(&mut line)?;
    if line.is_empty() {
        bail!("connection closed before a message arrived");
    }
    Ok(serde_json::from_str(&line)?)
}

static HOST: OnceLock<TerminalHost> = OnceLock::new();

/// Makes persistent terminals available to this process. Until this is
/// called, [`host`] returns `None` and terminals run their shells directly,
/// which is what tests and other embedders get.
pub fn init(executable: PathBuf, socket: PathBuf) {
    HOST.set(TerminalHost { executable, socket }).ok();
}

pub fn host() -> Option<&'static TerminalHost> {
    if cfg!(unix) { HOST.get() } else { None }
}

pub struct TerminalHost {
    executable: PathBuf,
    socket: PathBuf,
}

impl TerminalHost {
    /// The program a terminal runs to show `session_id`.
    pub fn attach_command(&self, session_id: &str) -> (String, Vec<String>) {
        (
            self.executable.to_string_lossy().into_owned(),
            vec![
                "--terminal-attach".to_string(),
                self.socket.to_string_lossy().into_owned(),
                session_id.to_string(),
            ],
        )
    }

    /// Starts the daemon if it is not running.
    pub fn create(&self, request: CreateSession) -> Result<SessionInfo> {
        match self.request(Request::Create(request), true)? {
            Response::Created(session) => Ok(session),
            response => Err(unexpected(response)),
        }
    }

    /// Lists the daemon's sessions, or none if it is not running.
    pub fn sessions(&self) -> Result<Vec<SessionInfo>> {
        if !self.socket.exists() {
            return Ok(Vec::new());
        }
        match self.request(Request::List, false)? {
            Response::Sessions { sessions } => Ok(sessions),
            response => Err(unexpected(response)),
        }
    }

    pub fn kill(&self, id: &str, attached_pid: Option<u32>) -> Result<bool> {
        if !self.socket.exists() {
            return Ok(false);
        }
        let request = Request::Kill {
            id: id.to_string(),
            attached_pid,
        };
        match self.request(request, false)? {
            Response::Killed { killed } => Ok(killed),
            response => Err(unexpected(response)),
        }
    }

    #[cfg(not(unix))]
    fn request(&self, _request: Request, _start_daemon: bool) -> Result<Response> {
        bail!("persistent terminals are only supported on unix")
    }

    #[cfg(unix)]
    fn request(&self, request: Request, start_daemon: bool) -> Result<Response> {
        let stream = if start_daemon {
            self.connect_or_start()?
        } else {
            connect(&self.socket)?
        };
        let mut reader = io::BufReader::new(stream.try_clone()?);
        let mut writer = stream;
        write_message(
            &mut writer,
            &Envelope {
                version: PROTOCOL_VERSION,
                request,
            },
        )?;
        match read_message(&mut reader)? {
            Response::Error { message } => Err(anyhow!(message)),
            response => Ok(response),
        }
    }

    #[cfg(unix)]
    fn connect_or_start(&self) -> Result<std::os::unix::net::UnixStream> {
        if let Ok(stream) = connect(&self.socket) {
            return Ok(stream);
        }
        daemon::spawn(&self.executable, &self.socket)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match connect(&self.socket) {
                Ok(stream) => return Ok(stream),
                Err(error) if std::time::Instant::now() >= deadline => {
                    return Err(error.context("the terminal host did not start"));
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
            }
        }
    }
}

#[cfg(unix)]
fn connect(socket: &std::path::Path) -> Result<std::os::unix::net::UnixStream> {
    use anyhow::Context as _;
    std::os::unix::net::UnixStream::connect(socket)
        .with_context(|| format!("connecting to the terminal host at {socket:?}"))
}

fn unexpected(response: Response) -> anyhow::Error {
    anyhow!("unexpected response from the terminal host: {response:?}")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() -> Result<()> {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, frame::DATA, b"hello")?;
        write_frame(&mut buffer, frame::RESIZE, &[0, 80, 0, 24])?;
        let mut reader = buffer.as_slice();
        assert_eq!(
            read_frame(&mut reader)?,
            Some((frame::DATA, b"hello".to_vec()))
        );
        assert_eq!(
            read_frame(&mut reader)?,
            Some((frame::RESIZE, vec![0, 80, 0, 24]))
        );
        assert_eq!(read_frame(&mut reader)?, None);
        Ok(())
    }

    #[test]
    fn requests_carry_their_version() -> Result<()> {
        let json = serde_json::to_string(&Envelope {
            version: PROTOCOL_VERSION,
            request: Request::Kill {
                id: "a".into(),
                attached_pid: None,
            },
        })?;
        assert_eq!(
            json,
            r#"{"version":1,"op":"kill","id":"a","attached_pid":null}"#
        );
        Ok(())
    }
}
