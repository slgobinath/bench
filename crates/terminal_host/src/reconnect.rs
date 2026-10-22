use std::{
    ffi::{OsStr, OsString},
    io::Write as _,
    process::Command,
    thread,
    time::{Duration, Instant},
};

/// What ssh exits with when it could not connect or lost its connection,
/// rather than passing on the exit code of the command it ran.
const SSH_CONNECTION_ERROR: i32 = 255;

const FIRST_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(10);

/// A connection that lasted this long before dropping was working, so the
/// next drop starts the backoff over.
const HEALTHY_CONNECTION: Duration = Duration::from_secs(30);

/// Runs `program` (an `ssh` attaching to a remote session) until it exits for
/// a reason other than losing its connection, returning its exit code.
///
/// A Mac that sleeps drops every ssh connection it has. The session's shell
/// lives on in the remote host's daemon, so the terminal showing it only has
/// to connect again, which this does for as long as the terminal is open.
/// Ctrl-C between attempts ends it.
#[allow(
    clippy::disallowed_methods,
    reason = "this runs as its own process on the terminal's pty, not inside the app"
)]
pub fn run_reconnecting(program: &OsStr, args: &[OsString]) -> i32 {
    let mut delay = FIRST_RETRY_DELAY;
    loop {
        let started = Instant::now();
        match Command::new(program).args(args).status() {
            Ok(status) => match status.code() {
                Some(SSH_CONNECTION_ERROR) => {}
                Some(code) => return code,
                // Killed by a signal, such as the terminal closing.
                None => return 1,
            },
            Err(error) => {
                eprintln!("[could not run {}: {error}]", program.display());
                return 1;
            }
        }
        if started.elapsed() >= HEALTHY_CONNECTION {
            delay = FIRST_RETRY_DELAY;
        }
        // An ssh killed mid-session can leave the pty in raw mode, where a
        // bare newline does not return the cursor.
        eprint!(
            "\r\n[connection lost; reconnecting in {}s]\r\n",
            delay.as_secs()
        );
        std::io::stderr().flush().ok();
        thread::sleep(delay);
        delay = (delay * 2).min(MAX_RETRY_DELAY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_only_while_the_connection_fails() {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let attempts = std::env::temp_dir().join(format!("reconnect-attempts-{id}"));
        // Fails like a dropped ssh twice, then exits like the remote shell did.
        let script = format!(
            "echo x >> '{path}'; [ $(wc -l < '{path}') -ge 3 ] && exit 7; exit 255",
            path = attempts.display()
        );
        let code = run_reconnecting(
            OsStr::new("/bin/sh"),
            &[OsString::from("-c"), OsString::from(script)],
        );
        let attempt_count = std::fs::read_to_string(&attempts)
            .map(|attempts| attempts.lines().count())
            .unwrap_or(0);
        std::fs::remove_file(&attempts).ok();
        assert_eq!(code, 7);
        assert_eq!(attempt_count, 3);
    }
}
