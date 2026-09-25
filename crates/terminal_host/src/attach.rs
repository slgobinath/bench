use crate::{
    Envelope, PROTOCOL_VERSION, Request, Response, frame, read_frame, read_message, write_frame,
    write_message,
};
use anyhow::{Context as _, Result, bail};
use std::{
    io::{self, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI32, Ordering},
    },
    thread,
};

/// Relays this process's terminal to session `id` until the session exits or
/// is attached elsewhere, returning the exit code to exit with.
pub fn run_attach(socket: &Path, id: &str) -> i32 {
    match attach(socket, id) {
        Ok(code) => code,
        Err(error) => {
            // The raw-mode guard is gone by now, so plain newlines are fine.
            eprintln!("\n[{error:#}]");
            1
        }
    }
}

fn attach(socket: &Path, id: &str) -> Result<i32> {
    let stream = UnixStream::connect(socket).context("the terminal host is not running")?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let writer = Arc::new(Mutex::new(stream));
    let (cols, rows) = window_size().unwrap_or((80, 24));
    write_message(
        &mut *lock(&writer),
        &Envelope {
            version: PROTOCOL_VERSION,
            request: Request::Attach {
                id: id.to_string(),
                cols,
                rows,
                client_pid: std::process::id(),
            },
        },
    )?;
    match read_message(&mut reader)? {
        Response::Attached => {}
        Response::Error { message } => bail!("{message}"),
        response => bail!("unexpected response from the terminal host: {response:?}"),
    }

    let _raw_mode = RawMode::enable();
    forward_resizes(writer.clone());
    thread::spawn(move || forward_input(&writer));

    let mut stdout = io::stdout().lock();
    loop {
        let Some((kind, payload)) = read_frame(&mut reader)? else {
            bail!("lost the connection to the terminal host");
        };
        match kind {
            frame::DATA => {
                stdout.write_all(&payload)?;
                stdout.flush()?;
            }
            frame::EXITED => {
                return Ok(match payload[..] {
                    [a, b, c, d] => i32::from_be_bytes([a, b, c, d]),
                    _ => 0,
                });
            }
            frame::DETACHED => {
                stdout.write_all(b"\r\n[this session was opened in another window]\r\n")?;
                stdout.flush()?;
                return Ok(0);
            }
            _ => {}
        }
    }
}

fn lock(writer: &Mutex<UnixStream>) -> std::sync::MutexGuard<'_, UnixStream> {
    writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn forward_input(writer: &Mutex<UnixStream>) {
    let mut stdin = io::stdin().lock();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let len = match stdin.read(&mut buffer) {
            Ok(0) => return,
            Ok(len) => len,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if write_frame(&mut *lock(writer), frame::DATA, &buffer[..len]).is_err() {
            return;
        }
    }
}

/// The write end of a pipe the SIGWINCH handler pokes, since a signal handler
/// can do little more than `write`.
static RESIZE_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_window_change(_: libc::c_int) {
    let fd = RESIZE_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe {
            libc::write(fd, [0u8].as_ptr().cast(), 1);
        }
    }
}

fn forward_resizes(writer: Arc<Mutex<UnixStream>>) {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return;
    }
    let [read_fd, write_fd] = fds;
    RESIZE_PIPE.store(write_fd, Ordering::Relaxed);
    unsafe {
        libc::signal(
            libc::SIGWINCH,
            on_window_change as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
    thread::spawn(move || {
        let mut byte = 0u8;
        loop {
            let read = unsafe { libc::read(read_fd, (&raw mut byte).cast(), 1) };
            if read < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if read <= 0 {
                return;
            }
            let Some((cols, rows)) = window_size() else {
                continue;
            };
            let mut payload = [0u8; 4];
            payload[..2].copy_from_slice(&cols.to_be_bytes());
            payload[2..].copy_from_slice(&rows.to_be_bytes());
            if write_frame(&mut *lock(&writer), frame::RESIZE, &payload).is_err() {
                return;
            }
        }
    });
}

fn window_size() -> Option<(u16, u16)> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut size) };
    (result == 0 && size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
}

/// Passes every byte through untouched while alive. The session's own pty
/// does the line editing and signal generation.
struct RawMode {
    original: libc::termios,
}

impl RawMode {
    fn enable() -> Option<Self> {
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return None;
        }
        let mut raw = original;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(Self { original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
}
