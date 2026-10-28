//! Which remote hosts Bench's MCP socket is forwarded to.
//!
//! An agent on a host can drive Bench, which includes opening pages and
//! running scripts in its browser, so a host only gets the socket after the
//! user installed Bench's MCP server there. The choice is kept in a file so
//! that the socket is forwarded again whenever the project reconnects.

use anyhow::{Context as _, Result};

use crate::SshConnectionOptions;

/// The Unix socket on the host that is forwarded to Bench's MCP socket.
pub fn remote_socket_path() -> String {
    let user: String = std::env::var("USER")
        .unwrap_or_default()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .collect();
    let user = if user.is_empty() { "user" } else { &user };
    format!("/tmp/bench-mcp-{user}.sock")
}

fn host_key(options: &SshConnectionOptions) -> String {
    format!(
        "{}:{}",
        options.ssh_destination(),
        options.port.unwrap_or(22)
    )
}

fn read_hosts() -> Result<Vec<String>> {
    let path = paths::mcp_hosts_path();
    match std::fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn is_enabled(options: &SshConnectionOptions) -> Result<bool> {
    Ok(read_hosts()?.contains(&host_key(options)))
}

pub fn enable(options: &SshConnectionOptions) -> Result<()> {
    let mut hosts = read_hosts()?;
    let key = host_key(options);
    if hosts.contains(&key) {
        return Ok(());
    }
    hosts.push(key);
    let path = paths::mcp_hosts_path();
    std::fs::write(path, serde_json::to_string_pretty(&hosts)?)
        .with_context(|| format!("writing {}", path.display()))
}
