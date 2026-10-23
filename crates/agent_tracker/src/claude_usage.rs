//! How much of your Claude plan is used, in the status bar.
//!
//! # Where the numbers come from
//!
//! Claude Code already has them: every reply from Anthropic carries how much
//! of the five-hour session and the weekly limit is used, and when each
//! resets. Claude Code passes those to its status line command as
//! `rate_limits`, a documented part of the status line's input. Bench installs
//! a status line command that saves that input to a file, and the tracker's
//! sweep reads the file — the same arrangement as the hooks in [`crate::hooks`].
//!
//! So Bench signs in to nothing, reads no credentials and makes no requests.
//! The cost is that the numbers are as fresh as the last reply any Claude
//! session got, which is why the status bar says how old they are.
//!
//! Claude Code has one status line command. Installing replaces whatever was
//! there, and keeps it, so uninstalling puts it back as it was.
//!
//! # Remote hosts
//!
//! Claude Code on a remote host has its own settings and may be signed in to
//! a different plan, so each host is its own report: the status line is
//! installed in the host's settings and saves to a file in the host's home.
//! The hosts are the ones the windows' projects are on, open or not, and the
//! status bar shows a ring for this machine and one for each of them.
//!
//! A host is read over an open project's connection when there is one, and
//! otherwise over a fresh ssh connection that cannot prompt — so a host that
//! needs a password is read only while a project on it is open.

use std::collections::HashMap;
use std::f32::consts::{FRAC_PI_2, TAU};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use gpui::{
    AnyElement, AsyncApp, Entity, Hsla, PathBuilder, Pixels, Point, PromptLevel, Subscription,
    Task, WeakEntity, actions, canvas, point,
};
use remote::{
    Interactive, RemoteClient, RemoteConnectionIdentity, RemoteConnectionOptions,
    SshConnectionOptions,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::{OffsetDateTime, UtcOffset};
use ui::{ButtonLike, Tooltip, prelude::*};
use util::shell::ShellKind;
use workspace::{
    HideStatusItem, MultiWorkspace, StatusItemView, Toast, Workspace, item::ItemHandle,
    notifications::NotificationId,
};

use crate::hooks::{claude_settings_path, read_settings, shell_quote, write_settings};
use crate::{AgentTracker, AgentsChanged};

actions!(
    agents,
    [
        /// Sets Claude Code's status line to record your plan usage, so that
        /// Bench can show it in the status bar.
        InstallClaudeStatusLine,
        /// Puts back the status line Claude Code had before Bench's.
        UninstallClaudeStatusLine,
    ]
);

/// Where Bench's status line command saves Claude Code's input.
pub fn usage_path() -> PathBuf {
    paths::data_dir().join("claude-usage.json")
}

/// Where the status line that Bench's replaced is kept, to put back.
fn previous_path() -> PathBuf {
    paths::data_dir().join("claude-status-line-before-bench.json")
}

/// Where, from a remote host's home, Bench's status line saves Claude Code's
/// input, Claude Code keeps its settings, and the status line Bench replaced
/// is kept.
const HOST_USAGE_FILE: &str = ".bench/claude-usage.json";
const HOST_SETTINGS_FILE: &str = ".claude/settings.json";
const HOST_PREVIOUS_FILE: &str = ".bench/claude-status-line-before-bench.json";

/// How often the windows are looked over for the hosts their projects are on.
/// Only a walk over what is in memory, so often enough that a host shows up
/// soon after its first project is added.
pub(crate) const HOST_SYNC: Duration = Duration::from_secs(5);

/// How many [`HOST_SYNC`]s between reads of every host. A read is a command
/// on the host, so less often than the local file's `stat`.
pub(crate) const HOST_READ_EVERY: u32 = 6;

/// Where the numbers can be read in full, from the status bar item.
const USAGE_PAGE: &str = "https://claude.ai/settings/usage";

/// How often the status bar item redraws on its own, so that "as of 3 min
/// ago" and a window that has since reset stay true between reports.
const REDRAW_EVERY: Duration = Duration::from_secs(30);

/// One of the plan's limits: how much of it is used, and when it starts over.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UsageWindow {
    /// 0 to 100.
    pub used_percentage: f64,
    /// Seconds since the Unix epoch.
    pub resets_at: Option<i64>,
}

impl UsageWindow {
    /// The share used as of `now`. A window that has reset since it was
    /// reported starts again from nothing; Bench only hears otherwise with the
    /// next reply.
    pub fn used_at(&self, now: i64) -> f64 {
        match self.resets_at {
            Some(resets_at) if resets_at <= now => 0.,
            _ => self.used_percentage.clamp(0., 100.),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeUsage {
    /// The five-hour session limit.
    pub session: Option<UsageWindow>,
    /// The seven-day limit.
    pub weekly: Option<UsageWindow>,
    /// When Claude Code last reported them.
    pub reported_at: SystemTime,
}

/// The part of the status line's input Bench reads. Claude Code sends a good
/// deal more — the model, the workspace, the cost — and none of it is needed.
#[derive(Deserialize)]
struct StatusLineInput {
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct RateLimits {
    five_hour: Option<RawWindow>,
    seven_day: Option<RawWindow>,
}

#[derive(Deserialize)]
struct RawWindow {
    used_percentage: Option<f64>,
    resets_at: Option<f64>,
}

impl RawWindow {
    fn into_window(self) -> Option<UsageWindow> {
        Some(UsageWindow {
            used_percentage: self.used_percentage?,
            resets_at: self.resets_at.map(|resets_at| resets_at as i64),
        })
    }
}

/// Reads what the status line saved, if it has changed since `last_modified`.
///
/// `None` is nothing new — the ordinary answer, since this is asked every
/// sweep. `Some((modified, None))` is a file that could not be read as usage,
/// which leaves what was known in place rather than blanking the status bar.
pub fn read_if_changed(
    last_modified: Option<SystemTime>,
) -> Option<(SystemTime, Option<ClaudeUsage>)> {
    let path = usage_path();
    let modified = std::fs::metadata(&path).and_then(|it| it.modified()).ok()?;
    if Some(modified) == last_modified {
        return None;
    }
    let usage = match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text, modified),
        Err(error) => {
            log::warn!("reading {}: {error:#}", path.display());
            None
        }
    };
    Some((modified, usage))
}

fn parse(text: &str, reported_at: SystemTime) -> Option<ClaudeUsage> {
    let input: StatusLineInput = serde_json::from_str(text).ok()?;
    let limits = input.rate_limits?;
    let usage = ClaudeUsage {
        session: limits.five_hour.and_then(RawWindow::into_window),
        weekly: limits.seven_day.and_then(RawWindow::into_window),
        reported_at,
    };
    (usage.session.is_some() || usage.weekly.is_some()).then_some(usage)
}

/// Whether Claude Code's status line is Bench's.
pub fn installed() -> bool {
    read_settings(&claude_settings_path())
        .ok()
        .and_then(|settings| settings.get("statusLine").map(is_ours))
        .unwrap_or(false)
}

/// Whether Claude Code has a status line of its own that installing would
/// replace — the question to ask before doing it.
fn has_other_status_line() -> bool {
    read_settings(&claude_settings_path())
        .ok()
        .and_then(|settings| settings.get("statusLine").map(|it| !is_ours(it)))
        .unwrap_or(false)
}

fn install() -> Result<PathBuf> {
    let path = claude_settings_path();
    install_at(&path, &previous_path(), &usage_path())?;
    Ok(path)
}

fn install_at(settings_path: &Path, previous_path: &Path, usage_path: &Path) -> Result<()> {
    let mut settings = read_settings(settings_path)?;
    let previous = put_status_line(
        &mut settings,
        command_for(&shell_quote(usage_path)),
        is_ours_at(usage_path),
    );
    if let Some(previous) = previous {
        let previous = serde_json::to_string_pretty(&previous)?;
        if let Some(directory) = previous_path.parent() {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("creating {}", directory.display()))?;
        }
        std::fs::write(previous_path, previous)
            .with_context(|| format!("writing {}", previous_path.display()))?;
    }
    write_settings(settings_path, &settings)
}

/// Puts Bench's status line in `settings`, and gives back the one that was
/// there to keep for uninstalling. Only what was there before Bench's is worth
/// keeping: installing twice must not save Bench's own command as the one to
/// put back, so that gives back `None`.
fn put_status_line(
    settings: &mut Map<String, Value>,
    command: String,
    is_ours: impl Fn(&Value) -> bool,
) -> Option<Value> {
    let current = settings.get("statusLine").cloned();
    let previous = match current {
        Some(current) if is_ours(&current) => None,
        current => Some(current.unwrap_or(Value::Null)),
    };
    settings.insert(
        "statusLine".into(),
        json!({ "type": "command", "command": command }),
    );
    previous
}

/// Takes Bench's status line out of `settings`, putting `previous` back in its
/// place. Someone else's status line is left as it is.
fn take_status_line(
    settings: &mut Map<String, Value>,
    previous: Value,
    is_ours: impl Fn(&Value) -> bool,
) -> Uninstalled {
    if !settings.get("statusLine").is_some_and(is_ours) {
        return Uninstalled::NotOurs;
    }
    if previous.is_null() {
        settings.remove("statusLine");
        Uninstalled::Removed
    } else {
        settings.insert("statusLine".into(), previous);
        Uninstalled::PutBack
    }
}

enum Uninstalled {
    PutBack,
    Removed,
    /// Someone else's status line is there now; it is left alone.
    NotOurs,
}

fn uninstall() -> Result<Uninstalled> {
    uninstall_at(&claude_settings_path(), &previous_path(), &usage_path())
}

fn uninstall_at(
    settings_path: &Path,
    previous_path: &Path,
    usage_path: &Path,
) -> Result<Uninstalled> {
    let mut settings = read_settings(settings_path)?;
    if !settings
        .get("statusLine")
        .is_some_and(is_ours_at(usage_path))
    {
        return Ok(Uninstalled::NotOurs);
    }
    let previous = match std::fs::read_to_string(previous_path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or(Value::Null),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", previous_path.display()));
        }
    };
    let outcome = take_status_line(&mut settings, previous, is_ours_at(usage_path));
    write_settings(settings_path, &settings)?;
    if let Err(error) = std::fs::remove_file(previous_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("removing {}: {error:#}", previous_path.display());
    }
    Ok(outcome)
}

/// The status line command: save Claude Code's input, when it carries the
/// plan's limits, to `file`, a path as the shell is to read it.
///
/// Only input with `rate_limits` is kept, because a session on an API key, or
/// one that has not had a reply yet, sends none, and saving it would wipe the
/// numbers another session reported. It is written beside the file and moved
/// into place, so the sweep never reads half of it; the name carries the
/// shell's pid because two sessions can report at once. It prints nothing, so
/// Claude Code's own status line is empty.
fn command_for(file: &str) -> String {
    format!(
        r#"f={file}; d=$(cat); case "$d" in *'"rate_limits"'*) printf '%s' "$d" > "$f.$$" && mv -f "$f.$$" "$f";; esac"#
    )
}

/// The status line command on a remote host, which saves in the host's home.
fn host_command() -> String {
    command_for(&format!(r#""$HOME"/{HOST_USAGE_FILE}"#))
}

fn is_ours(status_line: &Value) -> bool {
    is_ours_at(&usage_path())(status_line)
}

/// Bench's status line is recognised by the file it saves to, as the hooks
/// are by the file they append to.
fn is_ours_at(usage_path: &Path) -> impl Fn(&Value) -> bool + '_ {
    move |status_line| saves_to(status_line, &usage_path.to_string_lossy())
}

fn is_ours_on_host(status_line: &Value) -> bool {
    saves_to(status_line, HOST_USAGE_FILE)
}

fn saves_to(status_line: &Value, file: &str) -> bool {
    status_line
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.contains(file))
}

/// How a host is reached to run a command on it.
#[derive(Clone)]
enum HostConnection {
    /// An open project's connection, which needs no new login.
    Client(Entity<RemoteClient>),
    /// A fresh ssh connection, for a host no open project is connected to.
    Ssh(SshConnectionOptions),
}

/// What the tracker knows about one remote host's Claude.
pub struct HostUsage {
    name: SharedString,
    /// `None` for a host that is neither connected nor reachable over plain
    /// ssh, such as a WSL distribution no window is open on.
    connection: Option<HostConnection>,
    usage: Option<ClaudeUsage>,
    installed: bool,
    /// Whether the last read failed, so the item can say why it has nothing.
    unreachable: bool,
    _refresh: Option<Task<()>>,
}

impl HostUsage {
    pub fn name(&self) -> &SharedString {
        &self.name
    }

    pub fn usage(&self) -> Option<&ClaudeUsage> {
        self.usage.as_ref()
    }

    /// Whether the host's Claude Code status line is Bench's, as of the last
    /// read.
    pub fn installed(&self) -> bool {
        self.installed
    }

    pub fn unreachable(&self) -> bool {
        self.unreachable
    }
}

impl AgentTracker {
    /// The remote hosts, in the order the status bar shows them.
    pub fn host_claude_usage(&self) -> Vec<(&RemoteConnectionIdentity, &HostUsage)> {
        let mut hosts = self.host_claude_usage.iter().collect::<Vec<_>>();
        hosts.sort_by(|(_, a), (_, b)| a.name.cmp(&b.name));
        hosts
    }

    /// Brings the hosts in line with the projects in the windows: a host gets
    /// its first read as soon as it appears, and is forgotten once no project
    /// is on it.
    pub(crate) fn sync_hosts(&mut self, read_all: bool, cx: &mut Context<Self>) {
        let found = hosts_in_windows(cx);
        let before = self.host_claude_usage.len();
        self.host_claude_usage
            .retain(|host, _| found.contains_key(host));
        let mut changed = self.host_claude_usage.len() != before;
        let mut to_read = Vec::new();
        for (host, (name, connection)) in found {
            match self.host_claude_usage.get_mut(&host) {
                Some(watched) => {
                    watched.connection = connection;
                    if read_all {
                        to_read.push(host);
                    }
                }
                None => {
                    self.host_claude_usage.insert(
                        host.clone(),
                        HostUsage {
                            name,
                            connection,
                            usage: None,
                            installed: false,
                            unreachable: false,
                            _refresh: None,
                        },
                    );
                    changed = true;
                    to_read.push(host);
                }
            }
        }
        for host in to_read {
            self.refresh_host_claude_usage(&host, cx);
        }
        if changed {
            cx.emit(AgentsChanged);
            cx.notify();
        }
    }

    pub(crate) fn refresh_host_claude_usage(
        &mut self,
        host: &RemoteConnectionIdentity,
        cx: &mut Context<Self>,
    ) {
        let Some(watched) = self.host_claude_usage.get_mut(host) else {
            return;
        };
        let Some(connection) = watched.connection.clone() else {
            return;
        };
        let host = host.clone();
        watched._refresh = Some(cx.spawn(async move |this, cx| {
            let report = match run_on_host(&connection, &report_script(), &[], cx).await {
                Ok(output) => match parse_host_report(&output, SystemTime::now()) {
                    Some(report) => Some(report),
                    None => {
                        log::warn!("reading Claude usage on a remote host: unexpected output");
                        return;
                    }
                },
                Err(error) => {
                    // Expected while the host is down or wants a password;
                    // what was last read stays until it can be read again.
                    log::debug!("reading Claude usage on a remote host: {error:#}");
                    None
                }
            };
            this.update(cx, |this, cx| {
                let Some(watched) = this.host_claude_usage.get_mut(&host) else {
                    return;
                };
                let (installed, usage, unreachable) = match report {
                    Some(report) => (
                        report.installed,
                        report.usage.or_else(|| watched.usage.clone()),
                        false,
                    ),
                    None => (watched.installed, watched.usage.clone(), true),
                };
                if watched.installed != installed
                    || watched.usage != usage
                    || watched.unreachable != unreachable
                {
                    watched.installed = installed;
                    watched.usage = usage;
                    watched.unreachable = unreachable;
                    cx.emit(AgentsChanged);
                    cx.notify();
                }
            })
            .ok();
        }));
    }
}

/// The remote hosts of every window's projects, open or not, each with its
/// name and how to reach it: over an open project's live connection when
/// there is one.
fn hosts_in_windows(
    cx: &App,
) -> HashMap<RemoteConnectionIdentity, (SharedString, Option<HostConnection>)> {
    let mut hosts = HashMap::new();
    let multi_workspaces = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .filter_map(|window| window.read(cx).ok());
    for multi_workspace in multi_workspaces {
        for options in multi_workspace
            .project_group_keys()
            .into_iter()
            .filter_map(|key| key.host())
        {
            let connection = match &options {
                RemoteConnectionOptions::Ssh(ssh) => Some(HostConnection::Ssh(ssh.clone())),
                _ => None,
            };
            hosts
                .entry(RemoteConnectionIdentity::from(&options))
                .or_insert_with(|| (options.display_name().into(), connection));
        }
        for workspace in multi_workspace.workspaces() {
            let Some(client) = workspace.read(cx).project().read(cx).remote_client() else {
                continue;
            };
            if client.read(cx).connection().is_none() {
                continue;
            }
            let options = client.read(cx).connection_options();
            let entry = hosts
                .entry(RemoteConnectionIdentity::from(&options))
                .or_insert_with(|| (options.display_name().into(), None));
            entry.1 = Some(HostConnection::Client(client));
        }
    }
    hosts
}

/// What a read of a remote host found.
#[derive(Debug, PartialEq)]
struct HostReport {
    installed: bool,
    /// `None` when there is nothing to read as usage, which leaves what was
    /// known in place, as for the local file.
    usage: Option<ClaudeUsage>,
}

/// Prints the host's clock, when the usage file last changed (or an empty
/// line), the host's Claude Code settings, a record separator, and the usage
/// file. `stat -c` is GNU's, `stat -f` the BSDs'.
fn report_script() -> String {
    format!(
        r#"f="$HOME/{HOST_USAGE_FILE}"
date +%s
stat -c %Y "$f" 2>/dev/null || stat -f %m "$f" 2>/dev/null || echo
cat "$HOME/{HOST_SETTINGS_FILE}" 2>/dev/null
printf '\036'
cat "$f" 2>/dev/null
exit 0"#
    )
}

fn parse_host_report(output: &str, now: SystemTime) -> Option<HostReport> {
    let (host_now, rest) = output.split_once('\n')?;
    let (modified, rest) = rest.split_once('\n')?;
    let (settings, usage) = rest.split_once('\u{1e}')?;
    let host_now: i64 = host_now.trim().parse().ok()?;
    let installed = serde_json::from_str::<Map<String, Value>>(settings)
        .ok()
        .and_then(|settings| settings.get("statusLine").map(is_ours_on_host))
        .unwrap_or(false);
    let usage = modified.trim().parse::<i64>().ok().and_then(|modified| {
        // The host's clock need not agree with this one, so the report's age
        // is taken on the host and counted back from here.
        let age = Duration::from_secs(host_now.saturating_sub(modified).max(0) as u64);
        parse(usage, now.checked_sub(age).unwrap_or(now))
    });
    Some(HostReport { installed, usage })
}

/// Runs `script` in `sh` on the host, with `args` as its positional
/// parameters, and gives back what it printed.
async fn run_on_host(
    connection: &HostConnection,
    script: &str,
    args: &[String],
    cx: &AsyncApp,
) -> Result<String> {
    let mut arguments = vec!["-c".to_owned(), script.to_owned(), "sh".to_owned()];
    arguments.extend_from_slice(args);
    let mut command = match connection {
        HostConnection::Client(client) => {
            let template = client.read_with(cx, |client, _| {
                client.build_command(
                    Some("sh".to_owned()),
                    &arguments,
                    &HashMap::default(),
                    None,
                    None,
                    Interactive::No,
                )
            })?;
            let mut command = util::command::new_command(template.program);
            command.args(template.args).envs(template.env);
            command
        }
        HostConnection::Ssh(options) => {
            let mut remote_command = "sh".to_owned();
            for argument in &arguments {
                let argument = ShellKind::Posix
                    .try_quote(argument)
                    .context("quoting a command for the host")?;
                remote_command.push(' ');
                remote_command.push_str(&argument);
            }
            let mut command = util::command::new_command("ssh");
            command
                .args(options.additional_args_for_scp())
                // Nobody is at a terminal to answer a password or host key
                // prompt; such a host is read while a project on it is open.
                .args(["-o", "BatchMode=yes"])
                .args(["-o", "ConnectTimeout=10"]);
            if let Some(port) = options.port {
                command.args(["-p", &port.to_string()]);
            }
            command.arg(options.ssh_destination()).arg(remote_command);
            command
        }
    };
    command
        .stdin(util::command::Stdio::null())
        .kill_on_drop(true);
    let output = cx
        .background_spawn(async move { command.output().await })
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The host's Claude Code settings, and the status line Bench replaced there.
async fn read_host_settings(
    connection: &HostConnection,
    cx: &AsyncApp,
) -> Result<(Map<String, Value>, Value)> {
    let script = format!(
        r#"cat "$HOME/{HOST_SETTINGS_FILE}" 2>/dev/null
printf '\036'
cat "$HOME/{HOST_PREVIOUS_FILE}" 2>/dev/null
exit 0"#
    );
    let output = run_on_host(connection, &script, &[], cx).await?;
    let (settings, previous) = output
        .split_once('\u{1e}')
        .context("unexpected output reading the host's settings")?;
    // A settings file that cannot be read is not replaced by one that has
    // only the status line in it.
    let settings = if settings.trim().is_empty() {
        Map::new()
    } else {
        serde_json::from_str(settings).with_context(|| format!("reading ~/{HOST_SETTINGS_FILE}"))?
    };
    let previous = serde_json::from_str(previous).unwrap_or(Value::Null);
    Ok((settings, previous))
}

async fn write_on_host(
    connection: &HostConnection,
    file: &str,
    contents: String,
    cx: &AsyncApp,
) -> Result<()> {
    run_on_host(
        connection,
        r#"mkdir -p "$(dirname "$HOME/$1")" && printf '%s' "$2" > "$HOME/$1""#,
        &[file.to_owned(), contents],
        cx,
    )
    .await
    .with_context(|| format!("writing ~/{file}"))?;
    Ok(())
}

fn settings_text(settings: &Map<String, Value>) -> Result<String> {
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    Ok(text)
}

async fn install_on_host(
    connection: &HostConnection,
    mut settings: Map<String, Value>,
    cx: &AsyncApp,
) -> Result<()> {
    let previous = put_status_line(&mut settings, host_command(), is_ours_on_host);
    if let Some(previous) = previous {
        write_on_host(
            connection,
            HOST_PREVIOUS_FILE,
            serde_json::to_string_pretty(&previous)?,
            cx,
        )
        .await?;
    }
    // The status line saves there, and does not make the directory.
    run_on_host(
        connection,
        &format!(r#"mkdir -p "$(dirname "$HOME/{HOST_USAGE_FILE}")""#),
        &[],
        cx,
    )
    .await?;
    write_on_host(
        connection,
        HOST_SETTINGS_FILE,
        settings_text(&settings)?,
        cx,
    )
    .await
}

async fn uninstall_on_host(connection: &HostConnection, cx: &AsyncApp) -> Result<Uninstalled> {
    let (mut settings, previous) = read_host_settings(connection, cx).await?;
    let outcome = take_status_line(&mut settings, previous, is_ours_on_host);
    if matches!(outcome, Uninstalled::NotOurs) {
        return Ok(outcome);
    }
    write_on_host(
        connection,
        HOST_SETTINGS_FILE,
        settings_text(&settings)?,
        cx,
    )
    .await?;
    run_on_host(
        connection,
        &format!(r#"rm -f "$HOME/{HOST_PREVIOUS_FILE}""#),
        &[],
        cx,
    )
    .await?;
    Ok(outcome)
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &InstallClaudeStatusLine, window, cx| {
            match workspace.project().read(cx).remote_client() {
                Some(client) => match host_of_client(&client, cx) {
                    Some(host) => confirm_and_install_on_host(host, window, cx),
                    None => toast(cannot_reach_message(&client, cx), workspace, cx),
                },
                None => confirm_and_install(window, cx),
            }
        });
        workspace.register_action(|workspace, _: &UninstallClaudeStatusLine, window, cx| {
            if let Some(client) = workspace.project().read(cx).remote_client() {
                match host_of_client(&client, cx) {
                    Some(host) => uninstall_from_host(host, window, cx),
                    None => toast(cannot_reach_message(&client, cx), workspace, cx),
                }
                return;
            }
            let message = match uninstall() {
                Ok(outcome) => uninstalled_message(outcome, None),
                Err(error) => {
                    log::error!("uninstalling Bench's Claude status line: {error:#}");
                    format!("Could not change Claude Code's settings: {error}")
                }
            };
            set_installed(installed(), cx);
            toast(message, workspace, cx);
        });
    })
    .detach();
}

/// A remote host to install on or uninstall from, and how to reach it.
#[derive(Clone)]
struct Host {
    identity: RemoteConnectionIdentity,
    name: SharedString,
    connection: HostConnection,
}

/// The host a project's connection is to: over that connection while it is
/// up, and otherwise over plain ssh.
fn host_of_client(client: &Entity<RemoteClient>, cx: &App) -> Option<Host> {
    let options = client.read(cx).connection_options();
    let connection = if client.read(cx).connection().is_some() {
        HostConnection::Client(client.clone())
    } else {
        match &options {
            RemoteConnectionOptions::Ssh(ssh) => HostConnection::Ssh(ssh.clone()),
            _ => return None,
        }
    };
    Some(Host {
        identity: RemoteConnectionIdentity::from(&options),
        name: options.display_name().into(),
        connection,
    })
}

fn cannot_reach_message(client: &Entity<RemoteClient>, cx: &App) -> String {
    format!(
        "{} is not connected, so its Claude Code settings cannot be changed.",
        client.read(cx).connection_options().display_name()
    )
}

fn uninstalled_message(outcome: Uninstalled, host: Option<&str>) -> String {
    let on_host = host.map(|host| format!(" on {host}")).unwrap_or_default();
    match outcome {
        Uninstalled::PutBack => format!("Claude Code's previous status line{on_host} is back."),
        Uninstalled::Removed => {
            format!("Bench's status line is out of Claude Code's settings{on_host}.")
        }
        Uninstalled::NotOurs => {
            format!("Claude Code's status line{on_host} is not Bench's, so it was left alone.")
        }
    }
}

/// [`confirm_and_install`] for a remote host.
fn confirm_and_install_on_host(host: Host, window: &mut Window, cx: &mut Context<Workspace>) {
    cx.spawn_in(window, async move |workspace, cx| {
        let name = host.name.clone();
        let outcome = async {
            let (settings, _) = read_host_settings(&host.connection, cx).await?;
            if settings
                .get("statusLine")
                .is_some_and(|status_line| !is_ours_on_host(status_line))
            {
                let answer = workspace.update_in(cx, |_, window, cx| {
                    window.prompt(
                        PromptLevel::Info,
                        &format!("Replace Claude Code's status line on {name}?"),
                        Some(
                            "Bench shows your Claude usage by reading what Claude Code passes \
                             to its status line. Your current status line command is kept on \
                             the host, and `agents: uninstall claude status line` puts it back.",
                        ),
                        &["Replace", "Cancel"],
                        cx,
                    )
                })?;
                if answer.await? != 0 {
                    return Ok(false);
                }
            }
            install_on_host(&host.connection, settings, cx).await?;
            anyhow::Ok(true)
        }
        .await;
        let message = match outcome {
            Ok(false) => return anyhow::Ok(()),
            Ok(true) => format!(
                "Claude Code on {name} will now report your usage to Bench. It shows up \
                 after the next reply in a Claude session there."
            ),
            Err(error) => {
                log::error!("installing Bench's Claude status line on {name}: {error:#}");
                format!("Could not change Claude Code's settings on {name}: {error}")
            }
        };
        workspace.update(cx, |workspace, cx| {
            refresh_host(&host.identity, cx);
            toast(message, workspace, cx);
        })
    })
    .detach_and_log_err(cx);
}

fn uninstall_from_host(host: Host, window: &mut Window, cx: &mut Context<Workspace>) {
    cx.spawn_in(window, async move |workspace, cx| {
        let message = match uninstall_on_host(&host.connection, cx).await {
            Ok(outcome) => uninstalled_message(outcome, Some(&host.name)),
            Err(error) => {
                log::error!(
                    "uninstalling Bench's Claude status line on {}: {error:#}",
                    host.name
                );
                format!(
                    "Could not change Claude Code's settings on {}: {error}",
                    host.name
                )
            }
        };
        workspace.update(cx, |workspace, cx| {
            refresh_host(&host.identity, cx);
            toast(message, workspace, cx);
        })
    })
    .detach_and_log_err(cx);
}

fn refresh_host(host: &RemoteConnectionIdentity, cx: &mut App) {
    if let Some(tracker) = AgentTracker::try_global(cx) {
        tracker.update(cx, |tracker, cx| {
            tracker.refresh_host_claude_usage(host, cx)
        });
    }
}

/// Installs, asking first when that would replace a status line of the
/// user's own.
fn confirm_and_install(window: &mut Window, cx: &mut Context<Workspace>) {
    let answer = has_other_status_line().then(|| {
        window.prompt(
            PromptLevel::Info,
            "Replace Claude Code's status line?",
            Some(
                "Bench shows your Claude usage by reading what Claude Code passes to its \
                 status line. Your current status line command is kept, and \
                 `agents: uninstall claude status line` puts it back.",
            ),
            &["Replace", "Cancel"],
            cx,
        )
    });
    cx.spawn_in(window, async move |workspace, cx| {
        if let Some(answer) = answer
            && answer.await? != 0
        {
            return anyhow::Ok(());
        }
        let message = match install() {
            Ok(path) => format!(
                "Claude Code will now report your usage to Bench ({}). It shows up after \
                 the next reply in any Claude session.",
                path.display()
            ),
            Err(error) => {
                log::error!("installing Bench's Claude status line: {error:#}");
                format!("Could not change Claude Code's settings: {error}")
            }
        };
        workspace.update(cx, |workspace, cx| {
            set_installed(installed(), cx);
            toast(message, workspace, cx);
        })
    })
    .detach_and_log_err(cx);
}

fn set_installed(installed: bool, cx: &mut App) {
    if let Some(tracker) = AgentTracker::try_global(cx) {
        tracker.update(cx, |tracker, cx| {
            tracker.set_claude_status_line_installed(installed, cx)
        });
    }
}

fn toast(message: String, workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<InstallClaudeStatusLine>(), message),
        cx,
    );
}

/// The status bar's view of your Claude plans: for this machine and for each
/// remote host the windows' projects are on, a ring filling up with the
/// session's usage, and the percentage beside it.
pub struct ClaudeUsageButton {
    tracker: Option<Entity<AgentTracker>>,
    workspace: WeakEntity<Workspace>,
    _subscription: Option<Subscription>,
    _redraw: Task<()>,
}

impl ClaudeUsageButton {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let tracker = AgentTracker::try_global(cx);
        let subscription = tracker
            .as_ref()
            .map(|tracker| cx.subscribe(tracker, |_, _, _: &AgentsChanged, cx| cx.notify()));
        let redraw = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REDRAW_EVERY).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        });
        Self {
            tracker,
            workspace,
            _subscription: subscription,
            _redraw: redraw,
        }
    }
}

/// One ring in the status bar: what it shows, and where clicking an empty one
/// installs the status line.
struct UsageRing {
    title: SharedString,
    usage: Option<ClaudeUsage>,
    installed: bool,
    unreachable: bool,
    /// `None` for this machine.
    host: Option<Host>,
}

impl Render for ClaudeUsageButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(tracker) = self.tracker.as_ref().map(|tracker| tracker.read(cx)) else {
            return div().into_any_element();
        };
        let mut rings = vec![UsageRing {
            title: "Claude usage".into(),
            usage: tracker.claude_usage().cloned(),
            installed: tracker.claude_status_line_installed(),
            unreachable: false,
            host: None,
        }];
        rings.extend(
            tracker
                .host_claude_usage()
                .into_iter()
                .map(|(identity, watched)| UsageRing {
                    title: format!("Claude usage on {}", watched.name()).into(),
                    usage: watched.usage().cloned(),
                    installed: watched.installed(),
                    unreachable: watched.unreachable(),
                    host: watched.connection.clone().map(|connection| Host {
                        identity: identity.clone(),
                        name: watched.name().clone(),
                        connection,
                    }),
                }),
        );
        h_flex()
            .gap_1()
            .children(
                rings
                    .into_iter()
                    .enumerate()
                    .map(|(index, ring)| self.render_ring_item(index, ring, cx)),
            )
            .into_any_element()
    }
}

impl ClaudeUsageButton {
    fn render_ring_item(&self, index: usize, ring: UsageRing, cx: &App) -> AnyElement {
        let track = cx.theme().colors().border;
        let id = ("claude-usage", index);
        let Some(usage) = ring.usage else {
            // Nothing reported yet. Until the status line is installed, the
            // item is how you find out that it can be.
            let title = &ring.title;
            let tooltip = if ring.unreachable {
                format!("{title}: the host could not be reached without a password")
            } else if ring.installed {
                let where_reported = if ring.host.is_some() {
                    "in a Claude session on that host"
                } else {
                    "in any Claude session on this machine"
                };
                format!("{title} shows up after the next reply {where_reported}")
            } else {
                format!("Show {title} here")
            };
            let workspace = self.workspace.clone();
            let installed = ring.installed;
            let host = ring.host;
            return ButtonLike::new(id)
                .child(render_ring(0., track, track))
                .tooltip(Tooltip::text(tooltip))
                .on_click(move |_, window, cx| {
                    if installed {
                        return;
                    }
                    let host = host.clone();
                    workspace
                        .update(cx, |_, cx| match host {
                            Some(host) => confirm_and_install_on_host(host, window, cx),
                            None => confirm_and_install(window, cx),
                        })
                        .ok();
                })
                .into_any_element();
        };

        let now = unix_now();
        let session = usage.session.map(|window| window.used_at(now));
        let shown = session
            .or_else(|| usage.weekly.map(|window| window.used_at(now)))
            .unwrap_or(0.);
        let fill = usage_color(shown).color(cx);
        let title = ring.title;
        ButtonLike::new(id)
            .child(
                h_flex()
                    .gap_1()
                    .child(render_ring(shown as f32 / 100., fill, track))
                    .child(
                        Label::new(format!("{shown:.0}%"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .tooltip(Tooltip::element(move |_, _| {
                render_details(&title, &usage, unix_now()).into_any_element()
            }))
            .on_click(|_, _, cx| cx.open_url(USAGE_PAGE))
            .into_any_element()
    }
}

fn render_details(title: &SharedString, usage: &ClaudeUsage, now: i64) -> impl IntoElement {
    let row = |name: &'static str, window: Option<UsageWindow>| {
        h_flex()
            .gap_3()
            .justify_between()
            .child(Label::new(name).size(LabelSize::Small))
            .child(
                Label::new(match window {
                    Some(window) => format!(
                        "{:.0}%{}",
                        window.used_at(now),
                        window
                            .resets_at
                            .filter(|resets_at| *resets_at > now)
                            .map(|resets_at| format!(" · resets {}", format_reset(resets_at, now)))
                            .unwrap_or_default()
                    ),
                    None => "–".to_owned(),
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
    };
    let age = usage
        .reported_at
        .elapsed()
        .map(format_age)
        .unwrap_or_else(|_| "just now".to_owned());
    v_flex()
        .gap_1()
        .child(Label::new(title.clone()))
        .child(row("Session", usage.session))
        .child(row("Weekly", usage.weekly))
        .child(
            Label::new(format!("As of {age} · click for details"))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
}

/// The ring beside the percentage: the track all the way round, and the used
/// share over it, clockwise from the top. Drawn in pieces of at most a quarter
/// turn, as the dashboard's donut is.
fn render_ring(fraction: f32, fill: Hsla, track: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let center = bounds.center();
            let outer = bounds.size.width.min(bounds.size.height) / 2.;
            let inner = outer * 0.55;
            for (from, sweep, color) in [(0., 1., track), (0., fraction.clamp(0., 1.), fill)] {
                if sweep <= 0. {
                    continue;
                }
                let start = -FRAC_PI_2 + from * TAU;
                let sweep = sweep * TAU;
                let pieces = (sweep / FRAC_PI_2).ceil().max(1.) as usize;
                let step = sweep / pieces as f32;
                for piece in 0..pieces {
                    let a = start + step * piece as f32;
                    let b = a + step;
                    let mut path = PathBuilder::fill();
                    path.move_to(polar(center, outer, a));
                    path.arc_to(
                        point(outer, outer),
                        px(0.),
                        false,
                        true,
                        polar(center, outer, b),
                    );
                    path.line_to(polar(center, inner, b));
                    path.arc_to(
                        point(inner, inner),
                        px(0.),
                        false,
                        false,
                        polar(center, inner, a),
                    );
                    path.close();
                    match path.build() {
                        Ok(path) => window.paint_path(path, color),
                        Err(error) => log::debug!("drawing the usage ring: {error}"),
                    }
                }
            }
        },
    )
    .flex_none()
    .size(px(12.))
}

fn polar(center: Point<Pixels>, radius: Pixels, angle: f32) -> Point<Pixels> {
    point(
        center.x + radius * angle.cos(),
        center.y + radius * angle.sin(),
    )
}

/// Calm until it matters: the accent colour most of the time, a warning past
/// three quarters, an error once there is little left.
fn usage_color(used: f64) -> Color {
    if used >= 90. {
        Color::Error
    } else if used >= 75. {
        Color::Warning
    } else {
        Color::Accent
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// When a limit resets, where you are: the time alone if that is today, the
/// weekday with it otherwise.
fn format_reset(resets_at: i64, now: i64) -> String {
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let (Ok(at), Ok(today)) = (
        OffsetDateTime::from_unix_timestamp(resets_at),
        OffsetDateTime::from_unix_timestamp(now),
    ) else {
        return String::new();
    };
    let at = at.to_offset(offset);
    let (hour, minute) = (at.hour(), at.minute());
    let hour12 = match hour % 12 {
        0 => 12,
        hour => hour,
    };
    let time = format!(
        "{hour12}:{minute:02} {}",
        if hour < 12 { "AM" } else { "PM" }
    );
    if at.date() == today.to_offset(offset).date() {
        time
    } else {
        let weekday = at.weekday().to_string();
        format!("{} {time}", weekday.get(..3).unwrap_or(&weekday))
    }
}

fn format_age(age: Duration) -> String {
    let minutes = age.as_secs() / 60;
    match minutes {
        0 => "just now".to_owned(),
        1..=59 => format!("{minutes} min ago"),
        _ => format!("{} h ago", minutes / 60),
    }
}

impl StatusItemView for ClaudeUsageButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    #[test]
    fn reads_the_limits_claude_code_passes_its_status_line() {
        let text = r#"{
            "model": { "display_name": "Opus" },
            "rate_limits": {
                "five_hour": { "used_percentage": 23.5, "resets_at": 1738425600 },
                "seven_day": { "used_percentage": 41, "resets_at": 1746456789 }
            }
        }"#;
        let usage = parse(text, UNIX_EPOCH).expect("usage");
        assert_eq!(
            usage.session,
            Some(UsageWindow {
                used_percentage: 23.5,
                resets_at: Some(1738425600)
            })
        );
        assert_eq!(usage.weekly.map(|window| window.used_percentage), Some(41.));
    }

    #[test]
    fn input_without_limits_is_not_usage() {
        assert!(parse(r#"{ "model": { "display_name": "Opus" } }"#, UNIX_EPOCH).is_none());
        assert!(parse(r#"{ "rate_limits": {} }"#, UNIX_EPOCH).is_none());
        assert!(parse("not json", UNIX_EPOCH).is_none());
    }

    #[test]
    fn a_window_that_has_reset_starts_again_from_nothing() {
        let window = UsageWindow {
            used_percentage: 80.,
            resets_at: Some(1_000),
        };
        assert_eq!(window.used_at(999), 80.);
        assert_eq!(window.used_at(1_000), 0.);
    }

    /// The command runs in `sh` with Claude Code's input on stdin. Input with
    /// the limits is saved; input without is dropped, so it cannot wipe what
    /// another session reported.
    // Blocking is what a test wants here: it waits for `sh` to finish.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn the_status_line_command_saves_only_input_with_limits() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let directory = tempfile::tempdir().expect("a temp dir");
        let path = directory
            .path()
            .join("a dir with spaces")
            .join("usage.json");
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the dir");

        let run = |input: &str| {
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg(command_for(&shell_quote(&path)))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("sh");
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(input.as_bytes())
                .expect("the input");
            let output = child.wait_with_output().expect("sh to finish");
            assert!(output.status.success());
            assert!(output.stdout.is_empty(), "the status line prints nothing");
        };

        let with_limits = r#"{"rate_limits":{"five_hour":{"used_percentage":12,"resets_at":1}}}"#;
        run(with_limits);
        assert_eq!(std::fs::read_to_string(&path).expect("saved"), with_limits);

        run(r#"{"model":{"display_name":"Opus"}}"#);
        assert_eq!(
            std::fs::read_to_string(&path).expect("still saved"),
            with_limits,
            "input without limits leaves the last report alone"
        );
    }

    /// On a remote host, the status line saves in the host's home, and the
    /// report read back over the connection finds it, along with whether the
    /// host's status line is Bench's. Run here in `sh` with a stand-in home.
    // Blocking is what a test wants here: it waits for `sh` to finish.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn a_hosts_report_is_read_from_its_home() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let home = tempfile::tempdir().expect("a temp dir");
        let run = |script: &str, input: &str| {
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .env("HOME", home.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("sh");
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(input.as_bytes())
                .expect("the input");
            let output = child.wait_with_output().expect("sh to finish");
            assert!(output.status.success());
            String::from_utf8(output.stdout).expect("UTF-8")
        };

        let nothing = parse_host_report(&run(&report_script(), ""), UNIX_EPOCH)
            .expect("a report with nothing installed");
        assert_eq!(
            nothing,
            HostReport {
                installed: false,
                usage: None
            }
        );

        let mut settings = Map::new();
        settings.insert("model".into(), json!("opus"));
        put_status_line(&mut settings, host_command(), is_ours_on_host);
        let settings_path = home.path().join(HOST_SETTINGS_FILE);
        std::fs::create_dir_all(settings_path.parent().expect("a parent")).expect("the dir");
        std::fs::write(&settings_path, settings_text(&settings).expect("settings"))
            .expect("the settings");
        std::fs::create_dir_all(home.path().join(".bench")).expect("the dir");
        run(
            &host_command(),
            r#"{"rate_limits":{"five_hour":{"used_percentage":37,"resets_at":4102444800}}}"#,
        );

        let now = SystemTime::now();
        let report = parse_host_report(&run(&report_script(), ""), now).expect("a report");
        assert!(report.installed);
        let usage = report.usage.expect("usage");
        assert_eq!(
            usage.session.map(|window| window.used_percentage),
            Some(37.)
        );
        assert!(
            now.duration_since(usage.reported_at)
                .is_ok_and(|age| age < Duration::from_secs(60)),
            "the report's age comes from the host's clock"
        );
    }

    #[test]
    fn a_hosts_settings_name_bench_by_the_file_in_its_home() {
        let mut settings = Map::new();
        let theirs = json!({ "type": "command", "command": "echo hi" });
        settings.insert("statusLine".into(), theirs.clone());

        let previous = put_status_line(&mut settings, host_command(), is_ours_on_host);
        assert_eq!(previous, Some(theirs.clone()));
        assert_eq!(
            put_status_line(&mut settings, host_command(), is_ours_on_host),
            None,
            "installing twice keeps nothing of Bench's own"
        );

        let outcome = take_status_line(&mut settings, theirs.clone(), is_ours_on_host);
        assert!(matches!(outcome, Uninstalled::PutBack));
        assert_eq!(settings["statusLine"], theirs);
    }

    /// Installing keeps the status line that was there, installing twice does
    /// not lose it, and uninstalling puts it back.
    #[test]
    fn installing_keeps_the_previous_status_line_to_put_back() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let settings_path = directory.path().join("settings.json");
        let previous_path = directory.path().join("previous.json");
        let usage_path = directory.path().join("usage.json");
        let theirs = json!({ "type": "command", "command": "~/my-status-line.sh" });
        std::fs::write(
            &settings_path,
            json!({ "model": "opus", "statusLine": theirs }).to_string(),
        )
        .expect("their settings");

        install_at(&settings_path, &previous_path, &usage_path).expect("install");
        install_at(&settings_path, &previous_path, &usage_path).expect("install again");

        let settings: Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(is_ours_at(&usage_path)(&settings["statusLine"]));
        assert_eq!(settings["model"], "opus", "other settings are left alone");

        let outcome = uninstall_at(&settings_path, &previous_path, &usage_path).expect("uninstall");
        assert!(matches!(outcome, Uninstalled::PutBack));
        let settings: Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["statusLine"], theirs, "theirs is back as it was");
        assert!(!previous_path.exists());
    }

    #[test]
    fn uninstalling_with_nothing_before_removes_the_status_line() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let settings_path = directory.path().join("settings.json");
        let previous_path = directory.path().join("previous.json");
        let usage_path = directory.path().join("usage.json");

        install_at(&settings_path, &previous_path, &usage_path).expect("install");
        let outcome = uninstall_at(&settings_path, &previous_path, &usage_path).expect("uninstall");

        assert!(matches!(outcome, Uninstalled::Removed));
        let settings: Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(settings.get("statusLine").is_none());
    }

    /// A status line someone set after Bench's is theirs, not Bench's to
    /// remove.
    #[test]
    fn uninstalling_leaves_a_status_line_that_is_not_ours() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let settings_path = directory.path().join("settings.json");
        let theirs = json!({ "type": "command", "command": "echo hi" });
        std::fs::write(&settings_path, json!({ "statusLine": theirs }).to_string())
            .expect("their settings");

        let outcome = uninstall_at(
            &settings_path,
            &directory.path().join("previous.json"),
            &directory.path().join("usage.json"),
        )
        .expect("uninstall");

        assert!(matches!(outcome, Uninstalled::NotOurs));
        let settings: Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["statusLine"], theirs);
    }

    /// The status bar item draws with nothing reported and with a report —
    /// the dashboard once crashed on its first draw, and a status bar item is
    /// drawn in every window.
    #[gpui::test]
    async fn the_status_bar_item_draws(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::init(cx);
        });
        let (_button, cx) =
            cx.add_window_view(|_, cx| ClaudeUsageButton::new(WeakEntity::new_invalid(), cx));
        cx.run_until_parked();

        cx.update(|_, cx| {
            let tracker = AgentTracker::try_global(cx).expect("the tracker");
            tracker.update(cx, |tracker, cx| {
                tracker.take_claude_usage(
                    Some((
                        SystemTime::now(),
                        parse(
                            r#"{"rate_limits":{"five_hour":{"used_percentage":82,"resets_at":4102444800}}}"#,
                            SystemTime::now(),
                        ),
                    )),
                    cx,
                );
            });
            assert!(tracker.read(cx).claude_usage().is_some());
        });
        cx.run_until_parked();
    }

    #[test]
    fn ages_read_as_people_say_them() {
        assert_eq!(format_age(Duration::from_secs(20)), "just now");
        assert_eq!(format_age(Duration::from_secs(180)), "3 min ago");
        assert_eq!(format_age(Duration::from_secs(7_300)), "2 h ago");
    }
}
