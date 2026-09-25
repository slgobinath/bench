//! Exact agent state, reported by Claude Code's own hooks.
//!
//! Watching a PTY can tell you that an agent is quiet. It cannot tell you
//! *why*: an agent waiting on a permission prompt and an agent thinking
//! through a long tool call look identical from outside. Claude Code already
//! knows the difference and will say so — its hooks fire on exactly the
//! transitions that matter — so the accurate half of this feature is opt-in
//! configuration rather than cleverness.
//!
//! # How the report gets here
//!
//! Each hook is `cat >> <data dir>/agent-events.json`. No helper binary, no
//! socket, nothing to keep running: Claude Code writes the event's JSON to the
//! hook's stdin, and the hook appends it to a file Bench reads on its sweep.
//! Every event carries `session_id`, `cwd` and `hook_event_name`, which is all
//! the state model needs.
//!
//! Two consequences of that choice, both deliberate:
//!
//! - The file is a *stream of JSON values*, not lines — read with
//!   [`serde_json::Deserializer::into_iter`], because a hook's payload is not
//!   promised to be one line.
//! - Two agents can append at once. Each event is far below `PIPE_BUF`, so an
//!   append is atomic in practice; a torn record would cost one status update
//!   and heal on the next event.
//!
//! # The future MCP work
//!
//! When Bench grows an MCP server for LSP diagnostics, this module is where it
//! lands: the same settings file that carries `hooks` carries `mcpServers`, so
//! installing the server is another key in [`install`]'s merge, and every
//! Claude session started afterwards picks it up with no per-project setup.
//! Nothing about that is implemented here yet — this note exists so that the
//! writer is found rather than rebuilt.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::AgentState;

/// Where the hooks append, and where [`read_appended`] reads from.
pub fn events_path() -> PathBuf {
    paths::data_dir().join("agent-events.json")
}

/// Claude Code's own settings file, the one `~/.claude/settings.json` that
/// applies to every project.
pub fn claude_settings_path() -> PathBuf {
    paths::home_dir().join(".claude").join("settings.json")
}

/// The hook events Bench asks for, and what each one means about the agent.
///
/// `PreToolUse` and `PostToolUse` are here as a heartbeat rather than for
/// their own sake: a long turn is otherwise silent between the prompt and the
/// stop, and the heartbeat is what holds the stay-awake assertion through it.
const EVENTS: &[(&str, AgentState)] = &[
    ("SessionStart", AgentState::Idle),
    ("UserPromptSubmit", AgentState::Working),
    ("PreToolUse", AgentState::Working),
    ("PostToolUse", AgentState::Working),
    ("Notification", AgentState::NeedsInput),
    ("Stop", AgentState::Idle),
];

/// What a read of the events file found. Reading happens off the main thread —
/// see [`read_appended`] — and the result is handed to [`HookReports::ingest`].
pub enum Appended {
    /// Nothing new since the last sweep, which is almost every sweep.
    Nothing,
    Events { text: String, offset: u64 },
    /// The file was truncated, by us or by hand; the bookmark goes back to the
    /// start.
    Restarted,
}

/// Reads whatever the hooks have appended past `offset`, truncating the file
/// first if it has outgrown `max_bytes`.
///
/// A free function, and the only part of this module that touches the disk, so
/// that a caller can keep it on a background thread: this runs on every sweep
/// and the main thread has frames to draw.
///
/// Truncation races an agent appending at the same moment, which costs one
/// status update — the next event puts it right, and the alternative is a file
/// that grows for the life of the machine.
pub fn read_appended(offset: u64, max_bytes: u64) -> Appended {
    let path = events_path();
    let Ok(metadata) = std::fs::metadata(&path) else {
        // No file at all is the ordinary case: nobody has installed the hooks,
        // and the heuristic is carrying the feature on its own.
        return Appended::Nothing;
    };

    if metadata.len() > max_bytes {
        match std::fs::File::create(&path) {
            Ok(_) => return Appended::Restarted,
            Err(error) => log::warn!("truncating {}: {error:#}", path.display()),
        }
    }
    if metadata.len() < offset {
        return Appended::Restarted;
    }
    if metadata.len() == offset {
        return Appended::Nothing;
    }

    match read_from(&path, offset) {
        Ok(text) => Appended::Events {
            text,
            offset: metadata.len(),
        },
        Err(error) => {
            log::warn!("reading {}: {error:#}", path.display());
            Appended::Nothing
        }
    }
}

/// Reports read out of the events file, by session.
#[derive(Default)]
pub struct HookReports {
    sessions: HashMap<String, Report>,
    /// How far into the file the last sweep read. The file is appended to by
    /// other processes, so this is the only bookmark there is.
    offset: u64,
}

pub struct Report {
    pub cwd: PathBuf,
    pub state: AgentState,
    pub at: Instant,
}

impl HookReports {
    /// Where the next read should start.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Takes in what [`read_appended`] found. Returns whether anything changed,
    /// so a caller can skip a redraw.
    pub fn ingest(&mut self, appended: Appended, now: Instant) -> bool {
        let (text, offset) = match appended {
            Appended::Nothing => return false,
            Appended::Restarted => {
                self.offset = 0;
                return false;
            }
            Appended::Events { text, offset } => (text, offset),
        };
        self.offset = offset;

        let mut changed = false;
        for event in serde_json::Deserializer::from_str(&text).into_iter::<HookEvent>() {
            let Ok(event) = event else {
                // A torn or unknown record. The next one re-states the
                // session's status, so there is nothing to recover.
                continue;
            };
            let Some(state) = state_of(&event.hook_event_name) else {
                continue;
            };
            changed = true;
            self.sessions.insert(
                event.session_id,
                Report {
                    cwd: PathBuf::from(event.cwd),
                    state,
                    at: now,
                },
            );
        }
        changed
    }

    /// The freshest report for a directory, which is how a report finds the
    /// terminal it belongs to: one worktree is one checkout, and an agent's
    /// `cwd` is inside the worktree it was started in.
    pub fn for_directory(&self, directory: &Path) -> Option<&Report> {
        self.sessions
            .values()
            .filter(|report| directory.starts_with(&report.cwd) || report.cwd.starts_with(directory))
            .max_by_key(|report| report.at)
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Drops reports too old to mean anything.
    pub fn prune(&mut self, now: Instant, keep: std::time::Duration) {
        self.sessions
            .retain(|_, report| now.duration_since(report.at) < keep);
    }
}

fn read_from(path: &Path, offset: u64) -> Result<String> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut appended = String::new();
    file.read_to_string(&mut appended)?;
    Ok(appended)
}

fn state_of(event: &str) -> Option<AgentState> {
    EVENTS
        .iter()
        .find(|(name, _)| *name == event)
        .map(|(_, state)| *state)
}

/// The fields of a hook payload that Bench reads. Claude Code sends a good
/// deal more, and sends more over time, so everything else is ignored.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct HookEvent {
    session_id: String,
    cwd: String,
    hook_event_name: String,
}

/// Whether Bench's hooks are already in Claude Code's settings.
pub fn installed() -> bool {
    let Ok(settings) = std::fs::read_to_string(claude_settings_path()) else {
        return false;
    };
    settings.contains(&marker())
}

/// Adds Bench's hooks to `~/.claude/settings.json`, leaving everything else in
/// it alone.
///
/// The file is the user's, not ours: it is read, merged into, and written back
/// whole, and a hook Bench did not write is never touched. Bench's own entries
/// are recognised by the path they append to, so installing twice replaces
/// rather than duplicates.
pub fn install() -> Result<PathBuf> {
    let path = claude_settings_path();
    install_at(&path)?;
    Ok(path)
}

fn install_at(path: &Path) -> Result<()> {
    let mut settings = read_settings(path)?;

    let events = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("the `hooks` key in Claude Code's settings is not an object")?;

    for (event, _) in EVENTS {
        let matchers = events
            .entry(*event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("the `hooks.{event}` key is not an array"))?;

        matchers.retain(|matcher| !is_ours(matcher));
        matchers.push(json!({
            "hooks": [{
                "type": "command",
                "command": command(),
            }]
        }));
    }

    write_settings(path, &settings)
}

/// Takes Bench's hooks back out, leaving every other hook where it was.
pub fn uninstall() -> Result<()> {
    uninstall_at(&claude_settings_path())
}

fn uninstall_at(path: &Path) -> Result<()> {
    let mut settings = read_settings(path)?;

    if let Some(events) = settings.get_mut("hooks").and_then(Value::as_object_mut) {
        for matchers in events.values_mut() {
            if let Some(matchers) = matchers.as_array_mut() {
                matchers.retain(|matcher| !is_ours(matcher));
            }
        }
        events.retain(|_, matchers| !matchers.as_array().is_some_and(|it| it.is_empty()));
    }

    write_settings(path, &settings)
}

fn read_settings(path: &Path) -> Result<Map<String, Value>> {
    match std::fs::read_to_string(path) {
        Ok(settings) if !settings.trim().is_empty() => serde_json::from_str(&settings)
            .with_context(|| format!("reading {}", path.display())),
        // No settings file yet, or an empty one: both mean "start from
        // nothing", and the write below creates it.
        Ok(_) => Ok(Map::new()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_settings(path: &Path, settings: &Map<String, Value>) -> Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("creating {}", directory.display()))?;
    }
    let mut written = serde_json::to_string_pretty(settings)?;
    written.push('\n');
    std::fs::write(path, written).with_context(|| format!("writing {}", path.display()))
}

/// The shell command every one of Bench's hooks runs: append this event to the
/// file Bench reads. `cat` because the payload arrives on stdin and no part of
/// it needs interpreting here.
fn command() -> String {
    format!("cat >> {}", shell_quote(&events_path()))
}

/// What marks a hook as Bench's: the file it appends to. Matching on the path
/// rather than on the whole command means a user who edits the command around
/// it still has their entry replaced rather than duplicated.
fn marker() -> String {
    events_path().to_string_lossy().into_owned()
}

fn is_ours(matcher: &Value) -> bool {
    matcher
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(&marker()))
            })
        })
}

/// Single-quoted for `sh`, which is what runs a hook's command. The data
/// directory is under the user's home and can hold a space — `Application
/// Support` does on every Mac.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reported_event_maps_to_a_state() {
        assert_eq!(state_of("Notification"), Some(AgentState::NeedsInput));
        assert_eq!(state_of("Stop"), Some(AgentState::Idle));
        assert_eq!(state_of("UserPromptSubmit"), Some(AgentState::Working));
        assert_eq!(
            state_of("PreCompact"),
            None,
            "an event Bench did not ask for says nothing about the agent"
        );
    }

    /// The events file is appended to by several agents and is not promised to
    /// be one JSON value per line, so it is read as a stream of values.
    #[test]
    fn a_stream_of_values_is_read_whatever_the_whitespace() {
        let mut reports = HookReports::default();
        let text = r#"
            {"session_id":"a","cwd":"/repo/fix","hook_event_name":"Notification"}
            {"session_id":"b","cwd":"/repo/main",
             "hook_event_name":"UserPromptSubmit"}{"session_id":"a","cwd":"/repo/fix","hook_event_name":"Stop"}
        "#;

        let changed = reports.ingest(
            Appended::Events {
                text: text.to_owned(),
                offset: text.len() as u64,
            },
            Instant::now(),
        );

        assert!(changed);
        assert_eq!(reports.offset(), text.len() as u64);
        assert_eq!(
            reports.for_directory(Path::new("/repo/fix")).map(|r| r.state),
            Some(AgentState::Idle),
            "the last word about a session wins"
        );
        assert_eq!(
            reports
                .for_directory(Path::new("/repo/main"))
                .map(|r| r.state),
            Some(AgentState::Working)
        );
        assert!(reports.for_directory(Path::new("/elsewhere")).is_none());
    }

    /// A truncated file is read from the top again rather than from the middle
    /// of a value.
    #[test]
    fn a_restart_moves_the_bookmark_back() {
        let mut reports = HookReports::default();
        reports.ingest(
            Appended::Events {
                text: r#"{"session_id":"a","cwd":"/repo","hook_event_name":"Stop"}"#.to_owned(),
                offset: 4096,
            },
            Instant::now(),
        );
        assert_eq!(reports.offset(), 4096);

        reports.ingest(Appended::Restarted, Instant::now());

        assert_eq!(reports.offset(), 0);
        assert!(
            reports.for_directory(Path::new("/repo")).is_some(),
            "what was already reported is still known"
        );
    }

    /// Installing twice must not leave two copies, and must not disturb a hook
    /// the user wrote themselves.
    #[test]
    fn our_hooks_are_recognised_by_the_file_they_append_to() {
        let ours = json!({"hooks": [{"type": "command", "command": command()}]});
        let theirs = json!({"hooks": [{"type": "command", "command": "echo hello"}]});

        assert!(is_ours(&ours));
        assert!(!is_ours(&theirs));
    }

    /// The file belongs to the user and holds things Bench knows nothing
    /// about. Installing must add to it, never replace it — and installing
    /// twice must not leave two copies of the same hook.
    #[test]
    fn installing_adds_to_the_users_settings_without_disturbing_them() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let path = directory.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "model": "opus",
                "hooks": {
                    "Stop": [
                        {"hooks": [{"type": "command", "command": "echo mine"}]}
                    ]
                }
            }"#,
        )
        .expect("their settings");

        install_at(&path).expect("install");
        install_at(&path).expect("install again");

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            settings["model"], "opus",
            "a key Bench knows nothing about is left alone"
        );

        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "their hook, and exactly one of ours");
        assert!(
            stop.iter().any(|matcher| !is_ours(matcher)),
            "their own Stop hook survives"
        );
        assert_eq!(
            stop.iter().filter(|matcher| is_ours(matcher)).count(),
            1,
            "installing twice replaces rather than duplicates"
        );
        assert!(
            settings["hooks"]["Notification"].is_array(),
            "every event Bench asked for is installed"
        );

        uninstall_at(&path).expect("uninstall");

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "ours is gone");
        assert!(!is_ours(&stop[0]), "theirs is not");
        assert!(
            settings["hooks"].get("Notification").is_none(),
            "an event only Bench wanted goes away with it"
        );
        assert_eq!(settings["model"], "opus");
    }

    /// A machine with no Claude Code settings yet is the ordinary first run.
    #[test]
    fn installing_creates_the_file_when_there_is_none() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let path = directory.path().join("nested").join("settings.json");

        install_at(&path).expect("install");

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(is_ours(&settings["hooks"]["Stop"][0]));
    }

    #[test]
    fn a_path_with_a_space_survives_the_shell() {
        let quoted = shell_quote(Path::new("/Users/x/Application Support/Bench/agent-events.json"));
        assert_eq!(
            quoted,
            "'/Users/x/Application Support/Bench/agent-events.json'"
        );
    }
}
