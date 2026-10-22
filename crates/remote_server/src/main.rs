use clap::Parser;
use remote_server::Commands;
use std::io::Write as _;
use std::path::PathBuf;

#[derive(Parser)]
#[command(disable_version_flag = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    /// Used for SSH/Git password authentication, to remove the need for netcat as a dependency,
    /// by having Zed act like netcat communicating over a Unix socket.
    #[arg(long, hide = true)]
    askpass: Option<String>,
    /// Used for recording minidumps on crashes by having the server run a separate
    /// process communicating over a socket.
    #[arg(long, hide = true)]
    crash_handler: Option<PathBuf>,
    /// Used for loading the environment from the project.
    #[arg(long, hide = true)]
    printenv: bool,
    /// Runs the daemon that keeps remote terminals' shells alive while no
    /// client is connected, listening on this Unix socket.
    #[cfg(unix)]
    #[arg(long, hide = true, value_name = "SOCKET")]
    terminal_host: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    // `remote_server --terminal-open <session> [program [args...]]` is what a
    // remote terminal runs over ssh. It is matched before clap, because the
    // shell's own arguments follow it and must not be parsed as ours.
    #[cfg(unix)]
    {
        let mut args = std::env::args_os().skip(1);
        if args
            .next()
            .is_some_and(|flag| flag == terminal_host::OPEN_FLAG)
        {
            let Some(session_id) = args.next().and_then(|id| id.into_string().ok()) else {
                eprintln!("usage: remote_server {} <session> [program [args...]]", terminal_host::OPEN_FLAG);
                std::process::exit(1);
            };
            let program = args.next().map(|program| program.to_string_lossy().into_owned());
            let args = args.map(|arg| arg.to_string_lossy().into_owned());
            let executable = std::env::current_exe()?;
            let socket = remote_server::terminal_host_socket();
            std::process::exit(terminal_host::run_open(
                &executable,
                &socket,
                &session_id,
                program,
                args.collect(),
            ));
        }
    }

    let cli = Cli::parse();

    #[cfg(unix)]
    if let Some(socket) = &cli.terminal_host {
        return terminal_host::run_daemon(socket);
    }

    if let Some(socket_path) = &cli.askpass {
        askpass::main(socket_path);
        return Ok(());
    }

    if let Some(socket) = &cli.crash_handler {
        crashes::crash_server(socket.as_path(), paths::logs_dir().clone());
        return Ok(());
    }

    if cli.printenv {
        util::shell_env::print_env();
        return Ok(());
    }

    if let Some(command) = cli.command {
        use remote_server::ExecuteProxyError;

        let res = remote_server::run(command);
        if let Err(e) = &res
            && let Some(e) = e.downcast_ref::<ExecuteProxyError>()
        {
            std::io::stderr().write_fmt(format_args!("{e:#}\n")).ok();
            // It is important for us to report the proxy spawn exit code here
            // instead of the generic 1 that result returns
            // The client reads the exit code to determine if the server process has died when trying to reconnect
            // signaling that it needs to try spawning a new server
            std::process::exit(e.to_exit_code());
        }
        res
    } else {
        std::io::stderr()
            .write_all(if cfg!(unix) {
                b"usage: remote <run|proxy|version|exec>\n"
            } else {
                b"usage: remote <run|proxy|version>\n"
            })
            .ok();
        std::process::exit(1);
    }
}
