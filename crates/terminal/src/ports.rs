//! Which TCP ports the programs running in terminals are listening on.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ListeningPort {
    pub port: u16,
    /// The listening program's name, such as `node` or `java`.
    pub command: String,
    /// The directory of the terminal the program was started from.
    pub directory: PathBuf,
}

/// The ports listened on by everything running under the given shells.
///
/// `shells` pairs each terminal's shell with the directory the terminal is
/// in. A dev server is rarely the shell's own child — `npm run dev` binds its
/// port a few processes down — so the whole tree under each shell is checked.
/// Asks `lsof`, which comes with macOS and most Linux systems, because the
/// socket structures of the native APIs are not something `libc` provides.
///
/// Does blocking work before it awaits; call it off the main thread.
pub async fn listening_ports(shells: Vec<(u32, PathBuf)>) -> Result<Vec<ListeningPort>> {
    let owners = processes_under(&shells);
    if owners.is_empty() {
        return Ok(Vec::new());
    }
    let pids = owners
        .keys()
        .map(|pid| pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    // `lsof` exits 1 when nothing matches, which is an answer and not an
    // error, so only a failure to run it is one.
    let output = util::command::new_command("lsof")
        .args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-F", "pcn", "-p", &pids])
        .output()
        .await
        .context("running lsof")?;
    let mut ports = parse_lsof(&String::from_utf8_lossy(&output.stdout), &owners);
    ports.sort();
    ports.dedup();
    Ok(ports)
}

/// Whether a command line is a language server, which listens on ports of its
/// own for its clients and is not something the user is running. The tree is
/// not walked below one either, since what it starts is its own.
fn is_language_server(argv: &[String]) -> bool {
    const MARKERS: [&str; 13] = [
        "language-server",
        "language_server",
        "languageserver",
        "langserver",
        "jdtls",
        "jdt.ls",
        "rust-analyzer",
        "gopls",
        "pyright",
        "pylsp",
        "clangd",
        "tsserver",
        "analysis_server",
    ];
    argv.iter().any(|argument| {
        let argument = argument.to_ascii_lowercase();
        argument == "--lsp"
            || argument.starts_with("--lsp=")
            || MARKERS.iter().any(|marker| argument.contains(marker))
    })
}

/// Every process under a shell, excluding the shell: it never listens, and an
/// idle terminal then costs no `lsof` at all.
fn processes_under(shells: &[(u32, PathBuf)]) -> HashMap<u32, PathBuf> {
    if shells.is_empty() {
        return HashMap::new();
    }
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::OnlyIfNotSet),
    );
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }

    let mut owners = HashMap::new();
    for (shell, directory) in shells {
        let mut queue: VecDeque<Pid> = children
            .get(&Pid::from_u32(*shell))
            .cloned()
            .unwrap_or_default()
            .into();
        let mut seen = HashSet::new();
        while let Some(pid) = queue.pop_front() {
            if !seen.insert(pid) {
                continue;
            }
            let argv: Vec<String> = system
                .process(pid)
                .map(|process| {
                    process
                        .cmd()
                        .iter()
                        .map(|argument| argument.to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            if is_language_server(&argv) {
                continue;
            }
            owners
                .entry(pid.as_u32())
                .or_insert_with(|| directory.clone());
            queue.extend(children.get(&pid).into_iter().flatten().copied());
        }
    }
    owners
}

/// `lsof -F pcn` output: a `p` line starts a process, `c` names it, and each
/// `n` line is a socket, like `*:3000`, `127.0.0.1:5005` or `[::1]:8080`.
fn parse_lsof(output: &str, owners: &HashMap<u32, PathBuf>) -> Vec<ListeningPort> {
    let mut ports = Vec::new();
    let mut directory: Option<&PathBuf> = None;
    let mut command = String::new();
    for line in output.lines() {
        let Some(value) = line.get(1..) else {
            continue;
        };
        match line.as_bytes().first() {
            Some(b'p') => {
                directory = value.parse::<u32>().ok().and_then(|pid| owners.get(&pid));
                command.clear();
            }
            Some(b'c') => command = value.to_owned(),
            Some(b'n') => {
                let Some(directory) = directory else {
                    continue;
                };
                if let Some(port) = value
                    .rsplit_once(':')
                    .and_then(|(_, port)| port.parse::<u16>().ok())
                {
                    ports.push(ListeningPort {
                        port,
                        command: command.clone(),
                        directory: directory.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    ports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_servers_are_recognised_by_their_command_line() {
        let argv = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        for line in [
            "node /x/node_modules/yaml-language-server/bin/yaml-language-server --stdio",
            "java -jar org.eclipse.equinox.launcher.jar -data /cache/jdtls-1234",
            "/rustup/bin/rust-analyzer",
            "dart language-server --protocol=lsp",
            "dartaotruntime analysis_server_aot.dart.snapshot --lsp",
            "/bin/gopls serve",
            "node /x/typescript/lib/tsserver.js",
        ] {
            assert!(is_language_server(&argv(line)), "{line}");
        }
        for line in [
            "node /app/node_modules/.bin/next dev",
            "java -agentlib:jdwp=transport=dt_socket,address=localhost:5005 -jar app.jar",
            "python3 -m http.server 8000",
            "/usr/local/bin/vite --port 3000",
        ] {
            assert!(!is_language_server(&argv(line)), "{line}");
        }
    }

    #[test]
    fn parses_ports_from_every_address_form() {
        let owners = HashMap::from([
            (100, PathBuf::from("/work/a")),
            (200, PathBuf::from("/work/b")),
        ]);
        let output = "p100\ncnode\nn*:3000\nn127.0.0.1:5005\np200\ncjava\nn[::1]:8080\np300\ncother\nn*:9999\n";
        let mut ports = parse_lsof(output, &owners);
        ports.sort();
        assert_eq!(
            ports
                .iter()
                .map(|port| (port.port, port.command.as_str(), port.directory.to_str().unwrap()))
                .collect::<Vec<_>>(),
            vec![
                (3000, "node", "/work/a"),
                (5005, "node", "/work/a"),
                (8080, "java", "/work/b"),
            ]
        );
    }

    /// A server a few processes below the shell, the way `npm run dev` is.
    #[cfg(unix)]
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "the test needs a real child process and may block"
    )]
    fn finds_a_port_listened_on_below_the_shell() {
        let mut shell = std::process::Command::new("sh")
            .args(["-c", "python3 -m http.server 0 --bind 127.0.0.1 >/dev/null 2>&1 & wait"])
            .spawn()
            .expect("starting a shell");
        let directory = PathBuf::from("/work/dev");

        let mut found = Vec::new();
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            found = futures::executor::block_on(listening_ports(vec![(shell.id(), directory.clone())]))
                .expect("scanning");
            if !found.is_empty() {
                break;
            }
        }
        // Not killed through the shell: the server is its child and would
        // outlive it.
        for pid in processes_under(&[(shell.id(), directory.clone())]).into_keys() {
            std::process::Command::new("kill")
                .arg(pid.to_string())
                .status()
                .ok();
        }
        shell.kill().ok();
        shell.wait().ok();

        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].port > 0);
        assert_eq!(found[0].directory, directory);
        assert!(found[0].command.starts_with("Python") || found[0].command.starts_with("python"));
    }
}
