//! What a Claude Code session is doing beyond "working": which phase of the
//! work it is in, what it was asked, what it is touching, and what it has
//! spent. This is what the agent board is drawn from.
//!
//! # Where it comes from
//!
//! Claude Code's transcript of the session, `~/.claude/projects/…/<session>.jsonl`,
//! which records every prompt, every tool call with its input, and every mode
//! change, as it happens. The hooks in [`crate::hooks`] could say much the
//! same, but an organization can turn user hooks off
//! (`allowManagedHooksOnly`), and a transcript is always written. The hooks
//! are left to say only what the transcript cannot: whether the agent is
//! waiting on you right now, and what it asked.
//!
//! The transcript is read a little at a time, from where the last read
//! stopped; see [`advance_transcript`]. The first read of a session replays
//! all of it, which is what gives the board a timeline from the start.
//!
//! # Where the phase comes from
//!
//! Nothing says outright "I am reviewing now", so the phase is read from what
//! the session does, strongest evidence first:
//!
//! 1. A skill, run as a tool, typed as a slash command, or named by the
//!    transcript as the one the reply is for. Its name says what the work is
//!    (`code-review`, `ce-plan`), and it holds until the next prompt.
//! 2. Plan mode, then a running subagent whose type says what it is for.
//! 3. The mix of recent tool calls: reading is exploring, editing is coding,
//!    running tests is testing. This is a guess, so a new guess has to lead
//!    for [`HOLD`] tool calls before it replaces the last one; otherwise a
//!    single `cargo test` in the middle of an edit would move the card.

use std::collections::{HashMap, VecDeque};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gpui::SharedString;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// How many recent tool calls the guess is made from.
const TOOL_WINDOW: usize = 8;

/// How many tool calls in a row a new guess has to lead before it replaces the
/// current one.
const HOLD: usize = 3;

const FEED_KEPT: usize = 40;
const SUBAGENTS_KEPT: usize = 8;
const TIMELINE_KEPT: usize = 100;

/// How much of a prompt or a command is kept. The board shows a line or two;
/// the rest is in the terminal.
const TEXT_KEPT: usize = 400;

/// The context window assumed for a session, unless it has used more than
/// this, which only a long-context model can.
const CONTEXT_WINDOW: u64 = 200_000;
const LONG_CONTEXT_WINDOW: u64 = 1_000_000;

/// A column of the agent board.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Phase {
    Planning,
    Exploring,
    Coding,
    Testing,
    Reviewing,
    Shipping,
    Debugging,
}

impl Phase {
    pub const ALL: [Phase; 7] = [
        Phase::Planning,
        Phase::Exploring,
        Phase::Coding,
        Phase::Testing,
        Phase::Reviewing,
        Phase::Shipping,
        Phase::Debugging,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Phase::Planning => "Planning",
            Phase::Exploring => "Exploring",
            Phase::Coding => "Coding",
            Phase::Testing => "Testing",
            Phase::Reviewing => "Reviewing",
            Phase::Shipping => "Shipping",
            Phase::Debugging => "Debugging",
        }
    }
}

impl From<settings::AgentPhaseContent> for Phase {
    fn from(phase: settings::AgentPhaseContent) -> Self {
        match phase {
            settings::AgentPhaseContent::Planning => Phase::Planning,
            settings::AgentPhaseContent::Exploring => Phase::Exploring,
            settings::AgentPhaseContent::Coding => Phase::Coding,
            settings::AgentPhaseContent::Testing => Phase::Testing,
            settings::AgentPhaseContent::Reviewing => Phase::Reviewing,
            settings::AgentPhaseContent::Shipping => Phase::Shipping,
            settings::AgentPhaseContent::Debugging => Phase::Debugging,
        }
    }
}

/// Why a session is in the phase it is in, which the board shows so that a
/// guess does not read as a fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhaseSource {
    Skill(SharedString),
    PlanMode,
    Subagent(SharedString),
    Tools,
}

impl PhaseSource {
    /// Whether the session said so, rather than the tools it used suggesting
    /// it.
    pub fn is_explicit(&self) -> bool {
        !matches!(self, PhaseSource::Tools)
    }

    pub fn describe(&self) -> SharedString {
        match self {
            PhaseSource::Skill(name) => format!("skill · {name}").into(),
            PhaseSource::PlanMode => "plan mode".into(),
            PhaseSource::Subagent(kind) => format!("subagent · {kind}").into(),
            PhaseSource::Tools => "guessed from recent tools".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Todo {
    pub content: SharedString,
    pub status: TodoStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subagent {
    /// The tool call that started it, whose result says it has finished.
    id: Option<String>,
    pub kind: SharedString,
    pub description: SharedString,
    pub started: SystemTime,
    pub finished: Option<SystemTime>,
}

/// One line of a session's activity feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedEntry {
    pub at: SystemTime,
    pub phase: Option<Phase>,
    pub tool: SharedString,
    pub detail: SharedString,
    /// A moment worth picking out: a skill starting, a subagent, a
    /// compaction.
    pub marked: bool,
}

/// From when a session was in a phase. Each lasts until the next one starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhaseSpan {
    pub phase: Phase,
    pub since: SystemTime,
}

/// Everything the transcript has said about one session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Activity {
    /// The last thing the user asked.
    pub prompt: Option<SharedString>,
    /// What the agent is doing right now, as one line. Cleared when a turn
    /// ends.
    pub action: Option<SharedString>,
    pub todos: Vec<Todo>,
    /// Oldest first.
    pub subagents: Vec<Subagent>,
    /// Newest first.
    pub feed: VecDeque<FeedEntry>,
    /// Oldest first.
    pub timeline: Vec<PhaseSpan>,
    skill: Option<(Phase, SharedString)>,
    plan_mode: bool,
    /// The tool call that proposed leaving plan mode, whose result says
    /// whether it was left.
    exit_plan_mode: Option<String>,
    tools: VecDeque<Phase>,
    guessed: Option<Phase>,
    /// A guess that differs from [`Self::guessed`], and for how many tool
    /// calls in a row it has led.
    challenger: Option<(Phase, usize)>,
}

impl Activity {
    /// The phase the session is in, and why, or `None` before anything has
    /// said.
    pub fn phase(&self) -> Option<(Phase, PhaseSource)> {
        if let Some((phase, name)) = &self.skill {
            return Some((*phase, PhaseSource::Skill(name.clone())));
        }
        if self.plan_mode {
            return Some((Phase::Planning, PhaseSource::PlanMode));
        }
        let running = self
            .subagents
            .iter()
            .rev()
            .filter(|subagent| subagent.finished.is_none())
            .find_map(|subagent| {
                Some((
                    skill_phase(&subagent.kind, &HashMap::default())?,
                    subagent.kind.clone(),
                ))
            });
        if let Some((phase, kind)) = running {
            return Some((phase, PhaseSource::Subagent(kind)));
        }
        self.guessed.map(|phase| (phase, PhaseSource::Tools))
    }

    pub fn running_subagents(&self) -> impl Iterator<Item = &Subagent> {
        self.subagents
            .iter()
            .filter(|subagent| subagent.finished.is_none())
    }

    pub fn todos_done(&self) -> usize {
        self.todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Completed)
            .count()
    }

    /// Takes in one entry of the transcript. Entries that are not the main
    /// conversation — a subagent's own side of it, the copies of requests
    /// Claude Code keeps — are skipped.
    fn take_entry(&mut self, entry: &Value, skill_phases: &HashMap<String, Phase>) {
        if entry.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let at = timestamp(entry).unwrap_or_else(SystemTime::now);
        let cwd = entry.get("cwd").and_then(Value::as_str).unwrap_or_default();

        match entry.get("type").and_then(Value::as_str) {
            Some("permission-mode") => {
                self.plan_mode =
                    entry.get("permissionMode").and_then(Value::as_str) == Some("plan");
            }
            Some("user") => self.take_user(entry, at, skill_phases),
            Some("assistant") => {
                if let Some(name) = entry.get("attributionSkill").and_then(Value::as_str)
                    && self.skill.as_ref().is_none_or(|(_, current)| current != name)
                    && let Some(phase) = skill_phase(name, skill_phases)
                {
                    self.skill = Some((phase, name.to_owned().into()));
                }
                let blocks = entry
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        self.take_tool_call(block, Path::new(cwd), at, skill_phases);
                    }
                }
                if entry.pointer("/message/stop_reason").and_then(Value::as_str)
                    == Some("end_turn")
                {
                    self.finish_turn(at);
                }
            }
            Some("system") => match entry.get("subtype").and_then(Value::as_str) {
                Some("compact_boundary") => {
                    self.push_feed(at, "Compact", "Context compacted", true);
                }
                Some("turn_duration") => self.finish_turn(at),
                _ => {}
            },
            _ => {}
        }

        self.record_phase(at);
        self.stamp_feed();
    }

    fn take_user(&mut self, entry: &Value, at: SystemTime, skill_phases: &HashMap<String, Phase>) {
        // The text of a skill as it was loaded, and other notes Claude Code
        // adds on the user's side.
        if entry.get("isMeta").and_then(Value::as_bool) == Some(true) {
            return;
        }
        match entry.pointer("/message/content") {
            Some(Value::String(text)) => self.take_prompt(text, at, skill_phases),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("tool_result") => {
                            let id = block.get("tool_use_id").and_then(Value::as_str);
                            self.take_tool_result(id, at);
                        }
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                self.take_prompt(text, at, skill_phases);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn take_prompt(&mut self, text: &str, at: SystemTime, skill_phases: &HashMap<String, Phase>) {
        let text = text.trim();
        if let Some(name) = tag(text, "command-name") {
            let name = name.trim_start_matches('/');
            let arguments = tag(text, "command-args").unwrap_or_default();
            self.skill = skill_phase(name, skill_phases).map(|phase| (phase, name.to_owned().into()));
            self.action = None;
            self.prompt = Some(truncate(format!("/{name} {arguments}").trim(), TEXT_KEPT));
            self.push_feed(at, "Command", format!("/{name}"), true);
            return;
        }
        // The output of a local command, an interrupted request, and the
        // like: things Claude Code writes on the user's behalf.
        if text.is_empty() || text.starts_with('<') || text.starts_with("[Request interrupted") {
            return;
        }
        self.skill = None;
        self.action = None;
        self.prompt = Some(truncate(text, TEXT_KEPT));
        self.push_feed(at, "Prompt", truncate(text, 120), false);
    }

    fn take_tool_call(
        &mut self,
        block: &Value,
        cwd: &Path,
        at: SystemTime,
        skill_phases: &HashMap<String, Phase>,
    ) {
        let tool = block.get("name").and_then(Value::as_str).unwrap_or_default();
        let id = block.get("id").and_then(Value::as_str);
        let input = block.get("input").unwrap_or(&Value::Null);
        match tool {
            "Skill" => {
                let name = string_at(input, &["skill", "command", "name"])
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .to_owned();
                if let Some(phase) = skill_phase(&name, skill_phases) {
                    self.skill = Some((phase, name.clone().into()));
                }
                self.action = Some(format!("Running the {name} skill").into());
                self.push_feed(at, "Skill", name, true);
            }
            "Agent" | "Task" => {
                let kind = string_at(input, &["subagent_type"])
                    .unwrap_or("general-purpose")
                    .to_owned();
                let description = string_at(input, &["description"])
                    .unwrap_or_default()
                    .to_owned();
                self.subagents.push(Subagent {
                    id: id.map(str::to_owned),
                    kind: kind.clone().into(),
                    description: truncate(&description, 120),
                    started: at,
                    finished: None,
                });
                if self.subagents.len() > SUBAGENTS_KEPT {
                    self.subagents.remove(0);
                }
                self.action = Some(format!("{kind} › {description}").into());
                self.push_feed(at, "Agent", format!("{kind} started"), true);
            }
            "TodoWrite" => {
                self.todos = todos_in(input);
                let detail = format!("{} of {} done", self.todos_done(), self.todos.len());
                self.push_feed(at, "TodoWrite", detail, false);
            }
            "EnterPlanMode" => {
                self.plan_mode = true;
                self.push_feed(at, tool, "Entered plan mode", true);
            }
            "ExitPlanMode" => {
                self.exit_plan_mode = id.map(str::to_owned);
                self.push_feed(at, tool, "Proposed a plan", true);
            }
            _ => {
                if let Some(phase) = classify(tool, input) {
                    self.guess_from(phase);
                }
                let detail = describe(tool, input, cwd);
                self.action = Some(match verb(tool) {
                    Some(verb) => format!("{verb} {detail}").into(),
                    None => format!("{tool} {detail}").trim().to_owned().into(),
                });
                self.push_feed(at, tool, detail, false);
            }
        }
    }

    fn take_tool_result(&mut self, id: Option<&str>, at: SystemTime) {
        let Some(id) = id else {
            return;
        };
        if let Some(subagent) = self
            .subagents
            .iter_mut()
            .find(|subagent| subagent.finished.is_none() && subagent.id.as_deref() == Some(id))
        {
            subagent.finished = Some(at);
            let detail = format!("{} finished", subagent.kind);
            self.push_feed(at, "Agent", detail, false);
        }
        // Whether the plan was accepted is up to the `permission-mode` entry
        // that follows; a rejected one leaves plan mode on.
        if self.exit_plan_mode.as_deref() == Some(id) {
            self.exit_plan_mode = None;
        }
    }

    fn finish_turn(&mut self, at: SystemTime) {
        if self.action.take().is_some() {
            self.push_feed(at, "Stop", "Turn finished", false);
        }
    }

    fn guess_from(&mut self, phase: Phase) {
        self.tools.push_back(phase);
        while self.tools.len() > TOOL_WINDOW {
            self.tools.pop_front();
        }
        let Some(leader) = leader(&self.tools) else {
            return;
        };
        match self.guessed {
            None => self.guessed = Some(leader),
            Some(current) if current == leader => self.challenger = None,
            Some(_) => {
                let led_for = match self.challenger {
                    Some((challenger, count)) if challenger == leader => count + 1,
                    _ => 1,
                };
                if led_for >= HOLD {
                    self.guessed = Some(leader);
                    self.challenger = None;
                } else {
                    self.challenger = Some((leader, led_for));
                }
            }
        }
    }

    fn record_phase(&mut self, now: SystemTime) {
        let Some((phase, _)) = self.phase() else {
            return;
        };
        if self.timeline.last().map(|span| span.phase) != Some(phase) {
            self.timeline.push(PhaseSpan { phase, since: now });
            if self.timeline.len() > TIMELINE_KEPT {
                self.timeline.remove(0);
            }
        }
    }

    /// Pushes an entry without its phase, which is only known once the whole
    /// entry has been taken in; see [`Self::stamp_feed`].
    fn push_feed(&mut self, at: SystemTime, tool: &str, detail: impl Into<String>, marked: bool) {
        let detail: String = detail.into();
        self.feed.push_front(FeedEntry {
            at,
            phase: None,
            tool: tool.to_owned().into(),
            detail: truncate(&detail, 160),
            marked,
        });
        self.feed.truncate(FEED_KEPT);
    }

    fn stamp_feed(&mut self) {
        let phase = self.phase().map(|(phase, _)| phase);
        for entry in self.feed.iter_mut().take_while(|entry| entry.phase.is_none()) {
            entry.phase = phase;
        }
    }
}

/// The phase a skill, slash command or subagent type puts a session in: the
/// user's own mapping first, then a guess from the words in its name.
pub fn skill_phase(name: &str, custom: &HashMap<String, Phase>) -> Option<Phase> {
    let name = name.trim().trim_start_matches('/');
    let bare = name.rsplit(':').next().unwrap_or(name);
    if let Some(phase) = custom.get(name).or_else(|| custom.get(bare)) {
        return Some(*phase);
    }
    let words: Vec<String> = bare
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    // In this order, because names combine them: a `code-review` is a review
    // and a `plan-review` is too, and `commit-push-pr` is shipping however it
    // starts.
    const RULES: &[(&[&str], Phase)] = &[
        (&["debug", "investigate"], Phase::Debugging),
        (&["review", "reviewer", "feedback", "audit"], Phase::Reviewing),
        (
            &["commit", "push", "pr", "ship", "babysit", "release", "deploy"],
            Phase::Shipping,
        ),
        (&["test", "tests", "verify", "qa"], Phase::Testing),
        (
            &["plan", "planner", "brainstorm", "ideate", "strategy", "spec", "scope"],
            Phase::Planning,
        ),
        (&["explore", "explain", "research", "researcher"], Phase::Exploring),
        (
            &["code", "work", "implement", "build", "simplify", "refactor", "fix"],
            Phase::Coding,
        ),
    ];
    RULES
        .iter()
        .find(|(keywords, _)| words.iter().any(|word| keywords.contains(&word.as_str())))
        .map(|(_, phase)| *phase)
}

/// What one tool call suggests about the phase, if anything.
fn classify(tool: &str, input: &Value) -> Option<Phase> {
    match tool {
        "Read" | "Grep" | "Glob" | "LS" | "WebFetch" | "WebSearch" | "NotebookRead" => {
            Some(Phase::Exploring)
        }
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
            let path = string_at(input, &["file_path", "notebook_path"]).unwrap_or_default();
            if path.contains("/plans/") {
                Some(Phase::Planning)
            } else {
                Some(Phase::Coding)
            }
        }
        "Bash" => classify_command(string_at(input, &["command"]).unwrap_or_default()),
        _ => None,
    }
}

fn classify_command(command: &str) -> Option<Phase> {
    let command = command.to_ascii_lowercase();
    if ["git commit", "git push", "gh pr", "s1 push"]
        .iter()
        .any(|shipping| command.contains(shipping))
    {
        return Some(Phase::Shipping);
    }
    let words: Vec<&str> = command
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    if words.iter().any(|word| {
        matches!(
            *word,
            "test" | "tests" | "nextest" | "pytest" | "jest" | "vitest" | "rspec" | "clippy"
        )
    }) {
        return Some(Phase::Testing);
    }
    let program = command
        .split_whitespace()
        .next()
        .map(|program| program.rsplit('/').next().unwrap_or(program))
        .unwrap_or_default();
    let git_reading = program == "git"
        && command
            .split_whitespace()
            .nth(1)
            .is_some_and(|subcommand| matches!(subcommand, "log" | "show" | "diff" | "status" | "blame"));
    if git_reading
        || matches!(
            program,
            "cat" | "sed" | "grep" | "rg" | "find" | "fd" | "ls" | "head" | "tail" | "wc" | "tree"
        )
    {
        return Some(Phase::Exploring);
    }
    None
}

/// The phase most of the recent tool calls suggest; a tie goes to whichever of
/// them was used last.
fn leader(tools: &VecDeque<Phase>) -> Option<Phase> {
    let mut counts: Vec<(Phase, usize, usize)> = Vec::new();
    for (index, phase) in tools.iter().enumerate() {
        match counts.iter_mut().find(|(counted, _, _)| counted == phase) {
            Some((_, count, last)) => {
                *count += 1;
                *last = index;
            }
            None => counts.push((*phase, 1, index)),
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, count, last)| (*count, *last))
        .map(|(phase, _, _)| phase)
}

fn verb(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "Read" | "NotebookRead" => "Reading",
        "Edit" | "MultiEdit" | "NotebookEdit" => "Editing",
        "Write" => "Writing",
        "Bash" => "Running",
        "Grep" | "Glob" | "LS" | "WebSearch" => "Searching",
        "WebFetch" => "Fetching",
        _ => return None,
    })
}

/// What a tool call is about, as short as it can be said.
fn describe(tool: &str, input: &Value, cwd: &Path) -> String {
    let relative = |path: &str| {
        Path::new(path)
            .strip_prefix(cwd)
            .map(|relative| relative.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_owned())
    };
    let text = match tool {
        "Bash" => string_at(input, &["command"])
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
        "Read" | "Edit" | "MultiEdit" | "Write" | "NotebookEdit" | "NotebookRead" => {
            relative(string_at(input, &["file_path", "notebook_path"]).unwrap_or_default())
        }
        "Grep" => {
            let pattern = string_at(input, &["pattern"]).unwrap_or_default();
            match string_at(input, &["path"]) {
                Some(path) => format!("“{pattern}” in {}", relative(path)),
                None => format!("“{pattern}”"),
            }
        }
        "Glob" => string_at(input, &["pattern"]).unwrap_or_default().to_owned(),
        "WebFetch" => string_at(input, &["url"]).unwrap_or_default().to_owned(),
        "WebSearch" => string_at(input, &["query"]).unwrap_or_default().to_owned(),
        _ => String::new(),
    };
    truncate(&text, 160).to_string()
}

fn todos_in(input: &Value) -> Vec<Todo> {
    input
        .get("todos")
        .and_then(Value::as_array)
        .map(|todos| {
            todos
                .iter()
                .filter_map(|todo| {
                    let content = todo.get("content").and_then(Value::as_str)?;
                    let status = match todo.get("status").and_then(Value::as_str) {
                        Some("completed") => TodoStatus::Completed,
                        Some("in_progress") => TodoStatus::InProgress,
                        _ => TodoStatus::Pending,
                    };
                    Some(Todo {
                        content: truncate(content, 200),
                        status,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The text between `<name>` and `</name>`, which is how the transcript
/// records a slash command.
fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let start = text.find(&format!("<{name}>"))? + name.len() + 2;
    let end = text[start..].find(&format!("</{name}>"))? + start;
    Some(text[start..end].trim())
}

fn timestamp(entry: &Value) -> Option<SystemTime> {
    let text = entry.get("timestamp").and_then(Value::as_str)?;
    OffsetDateTime::parse(text, &Rfc3339).ok().map(SystemTime::from)
}

fn string_at<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
}

fn truncate(text: &str, limit: usize) -> SharedString {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]).into(),
        None => text.to_owned().into(),
    }
}

/// What a session's transcript says it has used.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TranscriptStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// How much of the context window the last reply filled.
    pub context_tokens: u64,
    pub compactions: u32,
    /// Prompts typed, not counting tool results or the transcript's own notes.
    pub prompts: u32,
    /// The id of the last reply counted. A reply is written as several lines,
    /// one per part, each repeating the same usage.
    last_message: Option<String>,
}

impl TranscriptStats {
    /// The share of the prompt tokens that came out of the cache.
    pub fn cache_hit_rate(&self) -> Option<f32> {
        let total = self.input_tokens + self.cache_read_tokens + self.cache_write_tokens;
        (total > 0).then(|| self.cache_read_tokens as f32 / total as f32)
    }

    /// How full the context window is, from 0 to 1.
    pub fn context_fraction(&self) -> f32 {
        let window = if self.context_tokens > CONTEXT_WINDOW {
            LONG_CONTEXT_WINDOW
        } else {
            CONTEXT_WINDOW
        };
        (self.context_tokens as f32 / window as f32).min(1.)
    }

    fn take_entry(&mut self, entry: &Value) {
        if entry.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
            self.compactions += 1;
            return;
        }
        match entry.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let Some(usage) = entry.pointer("/message/usage") else {
                    return;
                };
                let id = entry
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if id.is_some() && id == self.last_message {
                    return;
                }
                self.last_message = id;
                let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
                let input = count("input_tokens");
                let cache_read = count("cache_read_input_tokens");
                let cache_write = count("cache_creation_input_tokens");
                self.input_tokens += input;
                self.output_tokens += count("output_tokens");
                self.cache_read_tokens += cache_read;
                self.cache_write_tokens += cache_write;
                // A subagent's replies fill its own context, not this one.
                if entry.get("isSidechain").and_then(Value::as_bool) != Some(true) {
                    self.context_tokens = input + cache_read + cache_write;
                }
            }
            Some("user") => {
                let is_note = entry.get("isMeta").and_then(Value::as_bool) == Some(true)
                    || entry.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
                    || entry.get("isSidechain").and_then(Value::as_bool) == Some(true);
                let typed = entry
                    .pointer("/message/content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| {
                        !content.starts_with('<') || content.contains("<command-name>")
                    });
                if typed && !is_note {
                    self.prompts += 1;
                }
            }
            _ => {}
        }
    }
}

/// How far into a transcript Bench has read, and what it found there, so that
/// each sweep reads only what was appended.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptProgress {
    path: PathBuf,
    offset: u64,
    pub stats: TranscriptStats,
    pub activity: Activity,
}

impl TranscriptProgress {
    /// Whether this has read the same transcript as far as `other` has, so
    /// that nothing it shows can have changed.
    pub fn same_as(&self, other: &Self) -> bool {
        self.path == other.path && self.offset == other.offset
    }
}

/// Reads what has been appended to the transcript at `path` since `previous`.
/// A different transcript, or one that got shorter, is read from the start.
///
/// Blocking, for a background thread. The first read of a long session reads
/// all of it, once.
pub fn advance_transcript(
    path: &Path,
    previous: Option<TranscriptProgress>,
    skill_phases: &HashMap<String, Phase>,
) -> Option<TranscriptProgress> {
    let length = std::fs::metadata(path).ok()?.len();
    let mut progress = previous
        .filter(|previous| previous.path == path && previous.offset <= length)
        .unwrap_or_else(|| TranscriptProgress {
            path: path.to_path_buf(),
            offset: 0,
            stats: TranscriptStats::default(),
            activity: Activity::default(),
        });
    if progress.offset == length {
        return Some(progress);
    }
    let appended = match read_from(path, progress.offset) {
        Ok(appended) => appended,
        Err(error) => {
            log::warn!("reading {}: {error:#}", path.display());
            return Some(progress);
        }
    };
    // A line still being written is left for the next read.
    let Some(complete) = appended.iter().rposition(|byte| *byte == b'\n') else {
        return Some(progress);
    };
    let text = String::from_utf8_lossy(&appended[..=complete]);
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        progress.stats.take_entry(&entry);
        progress.activity.take_entry(&entry, skill_phases);
    }
    progress.offset += complete as u64 + 1;
    Some(progress)
}

fn read_from(path: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Transcript entries one second apart, the way Claude Code writes them.
    struct Session {
        activity: Activity,
        second: u32,
    }

    impl Session {
        fn new() -> Self {
            Self {
                activity: Activity::default(),
                second: 0,
            }
        }

        fn take(&mut self, mut entry: Value) -> &mut Self {
            self.second += 1;
            if let Some(entry) = entry.as_object_mut() {
                entry.insert(
                    "timestamp".to_owned(),
                    json!(format!("2026-10-21T18:00:{:02}.000Z", self.second % 60)),
                );
                entry.entry("cwd").or_insert(json!("/repo"));
            }
            self.activity.take_entry(&entry, &HashMap::default());
            self
        }

        fn prompt(&mut self, text: &str) -> &mut Self {
            self.take(json!({"type": "user", "message": {"role": "user", "content": text}}))
        }

        fn command(&mut self, name: &str, arguments: &str) -> &mut Self {
            self.prompt(&format!(
                "<command-message>{name}</command-message>\n<command-name>/{name}</command-name>\n<command-args>{arguments}</command-args>"
            ))
        }

        fn tool(&mut self, name: &str, input: Value) -> &mut Self {
            self.tool_with_id("toolu_x", name, input)
        }

        fn tool_with_id(&mut self, id: &str, name: &str, input: Value) -> &mut Self {
            self.take(json!({
                "type": "assistant",
                "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]},
            }))
        }

        fn result(&mut self, id: &str) -> &mut Self {
            self.take(json!({
                "type": "user",
                "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": "ok"}]},
            }))
        }

        fn read(&mut self) -> &mut Self {
            self.tool("Read", json!({"file_path": "/repo/a.rs"}))
        }

        fn edit(&mut self) -> &mut Self {
            self.tool("Edit", json!({"file_path": "/repo/a.rs"}))
        }

        fn phase(&self) -> Option<Phase> {
            self.activity.phase().map(|(phase, _)| phase)
        }
    }

    #[test]
    fn skill_names_map_to_phases() {
        let none = HashMap::default();
        let cases = [
            ("signal1-engineering:plan", Some(Phase::Planning)),
            ("plan", Some(Phase::Planning)),
            ("compound-engineering:ce-brainstorm", Some(Phase::Planning)),
            ("signal1-engineering:code", Some(Phase::Coding)),
            ("compound-engineering:ce-work", Some(Phase::Coding)),
            ("signal1-engineering:code-review", Some(Phase::Reviewing)),
            ("signal1-engineering:plan-review", Some(Phase::Reviewing)),
            ("security-review", Some(Phase::Reviewing)),
            ("compound-engineering:ce-debug", Some(Phase::Debugging)),
            ("compound-engineering:ce-commit-push-pr", Some(Phase::Shipping)),
            ("compound-engineering:ce-babysit-pr", Some(Phase::Shipping)),
            ("compound-engineering:ce-test-browser", Some(Phase::Testing)),
            ("Explore", Some(Phase::Exploring)),
            ("code-reviewer", Some(Phase::Reviewing)),
            ("general-purpose", None),
            ("signal1-engineering:compound", None),
            ("/model", None),
        ];
        for (name, expected) in cases {
            assert_eq!(skill_phase(name, &none), expected, "{name}");
        }
    }

    #[test]
    fn the_users_mapping_comes_first_with_or_without_the_plugin() {
        let custom = HashMap::from_iter([
            ("spec-writer".to_owned(), Phase::Planning),
            ("acme:code-review".to_owned(), Phase::Shipping),
        ]);
        assert_eq!(skill_phase("acme:spec-writer", &custom), Some(Phase::Planning));
        assert_eq!(skill_phase("acme:code-review", &custom), Some(Phase::Shipping));
        assert_eq!(skill_phase("other:code-review", &custom), Some(Phase::Reviewing));
    }

    #[test]
    fn commands_suggest_phases() {
        assert_eq!(classify_command("cargo nextest run -p agent_tracker"), Some(Phase::Testing));
        assert_eq!(classify_command("./script/clippy"), Some(Phase::Testing));
        assert_eq!(classify_command("npm test"), Some(Phase::Testing));
        assert_eq!(classify_command("git commit -m 'x'"), Some(Phase::Shipping));
        assert_eq!(classify_command("gh pr create --title x"), Some(Phase::Shipping));
        assert_eq!(classify_command("rg AgentState crates"), Some(Phase::Exploring));
        assert_eq!(classify_command("git log --oneline -5"), Some(Phase::Exploring));
        assert_eq!(classify_command("cargo build"), None);
    }

    /// The case that was missed with hooks turned off: `/plan` typed at the
    /// prompt, recorded only in the transcript.
    #[test]
    fn a_slash_command_in_the_transcript_sets_the_phase() {
        let mut session = Session::new();
        session.command("plan", "test plan");
        assert_eq!(
            session.activity.phase(),
            Some((Phase::Planning, PhaseSource::Skill("plan".into())))
        );
        assert_eq!(session.activity.prompt.as_deref(), Some("/plan test plan"));

        session.take(json!({
            "type": "user",
            "isMeta": true,
            "message": {"content": [{"type": "text", "text": "Base directory for this skill: …"}]},
        }));
        assert_eq!(
            session.activity.prompt.as_deref(),
            Some("/plan test plan"),
            "the skill's own text is not a prompt"
        );
    }

    #[test]
    fn a_skill_holds_its_phase_until_the_next_prompt() {
        let mut session = Session::new();
        session
            .prompt("review this")
            .tool("Skill", json!({"skill": "signal1-engineering:code-review"}))
            .edit()
            .edit()
            .edit()
            .edit();
        assert_eq!(
            session.activity.phase(),
            Some((
                Phase::Reviewing,
                PhaseSource::Skill("signal1-engineering:code-review".into())
            )),
            "editing during a review is still the review"
        );

        session.prompt("thanks, now ship");
        assert_eq!(
            session.activity.phase(),
            Some((Phase::Coding, PhaseSource::Tools)),
            "a new prompt ends the skill and the tools take over"
        );
    }

    #[test]
    fn the_skill_a_reply_is_for_sets_the_phase() {
        let mut session = Session::new();
        session.take(json!({
            "type": "assistant",
            "attributionSkill": "compound-engineering:ce-debug",
            "message": {"content": [{"type": "text", "text": "Looking."}]},
        }));
        assert_eq!(session.phase(), Some(Phase::Debugging));
    }

    #[test]
    fn a_guessed_phase_moves_only_after_the_new_one_has_held() {
        let mut session = Session::new();
        session.read().read().read();
        assert_eq!(session.phase(), Some(Phase::Exploring));

        session.edit().edit().edit().edit();
        assert_eq!(
            session.phase(),
            Some(Phase::Exploring),
            "edits have only just caught up with the reads"
        );

        session.edit().edit();
        assert_eq!(session.phase(), Some(Phase::Coding));
        assert_eq!(
            session
                .activity
                .timeline
                .iter()
                .map(|span| span.phase)
                .collect::<Vec<_>>(),
            vec![Phase::Exploring, Phase::Coding]
        );
    }

    #[test]
    fn one_test_run_in_the_middle_of_coding_does_not_move_the_card() {
        let mut session = Session::new();
        session
            .edit()
            .edit()
            .tool("Bash", json!({"command": "cargo test"}))
            .edit();
        assert_eq!(session.phase(), Some(Phase::Coding));
    }

    #[test]
    fn plan_mode_and_subagents_outrank_the_tools() {
        let mut session = Session::new();
        session.edit().tool_with_id(
            "toolu_review",
            "Agent",
            json!({"subagent_type": "code-reviewer", "description": "review"}),
        );
        assert_eq!(
            session.activity.phase(),
            Some((Phase::Reviewing, PhaseSource::Subagent("code-reviewer".into())))
        );

        session.result("toolu_other");
        assert_eq!(
            session.phase(),
            Some(Phase::Reviewing),
            "another tool's result does not finish the reviewer"
        );

        session.result("toolu_review");
        assert_eq!(session.phase(), Some(Phase::Coding), "the reviewer finished");
        assert_eq!(session.activity.running_subagents().count(), 0);

        session.take(json!({"type": "permission-mode", "permissionMode": "plan"}));
        assert_eq!(
            session.activity.phase(),
            Some((Phase::Planning, PhaseSource::PlanMode))
        );
        session.take(json!({"type": "permission-mode", "permissionMode": "auto"}));
        assert_eq!(session.phase(), Some(Phase::Coding));
    }

    #[test]
    fn a_subagents_own_side_of_the_conversation_is_not_the_sessions() {
        let mut session = Session::new();
        session.read().read().read();
        session.take(json!({
            "type": "assistant",
            "isSidechain": true,
            "message": {"content": [{"type": "tool_use", "id": "a", "name": "Edit", "input": {"file_path": "/repo/a.rs"}}]},
        }));
        assert_eq!(session.activity.action.as_deref(), Some("Reading a.rs"));
    }

    #[test]
    fn todos_and_actions_are_kept_until_the_turn_ends() {
        let mut session = Session::new();
        session
            .tool(
                "TodoWrite",
                json!({"todos": [
                    {"content": "Read the code", "status": "completed"},
                    {"content": "Write the test", "status": "in_progress"},
                    {"content": "Fix it", "status": "pending"}
                ]}),
            )
            .tool("Bash", json!({"command": "cargo nextest run\necho done"}));
        let activity = &session.activity;
        assert_eq!(activity.todos.len(), 3);
        assert_eq!(activity.todos_done(), 1);
        assert_eq!(activity.todos[1].status, TodoStatus::InProgress);
        assert_eq!(activity.action.as_deref(), Some("Running cargo nextest run"));
        assert_eq!(activity.feed[0].phase, Some(Phase::Testing));

        session.take(json!({"type": "system", "subtype": "turn_duration"}));
        assert_eq!(session.activity.action, None);
        assert_eq!(session.activity.feed[0].detail.as_ref(), "Turn finished");
    }

    #[test]
    fn paths_are_shown_relative_to_the_session() {
        let mut session = Session::new();
        session.tool("Read", json!({"file_path": "/repo/crates/a.rs"}));
        assert_eq!(session.activity.action.as_deref(), Some("Reading crates/a.rs"));
        assert_eq!(session.activity.feed[0].detail.as_ref(), "crates/a.rs");
        assert_eq!(
            session.activity.feed[0].at,
            SystemTime::from(
                OffsetDateTime::parse("2026-10-21T18:00:01.000Z", &Rfc3339).expect("a time")
            ),
            "the feed keeps the transcript's own time"
        );
    }

    #[test]
    fn transcripts_are_read_a_reply_at_a_time() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let path = directory.path().join("session.jsonl");
        let reply = |id: &str, input: u64| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"{id}","usage":{{"input_tokens":{input},"output_tokens":10,"cache_read_input_tokens":900,"cache_creation_input_tokens":100}}}}}}"#
            )
        };
        let none = HashMap::default();
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n{}\n",
                r#"{"type":"user","message":{"role":"user","content":"<command-message>plan</command-message>\n<command-name>/plan</command-name>"}}"#,
                reply("a", 50),
                reply("a", 50),
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"x"}]}}"#,
            ),
        )
        .expect("a transcript");

        let first = advance_transcript(&path, None, &none).expect("progress");
        assert_eq!(first.stats.input_tokens, 50, "a reply's parts count once");
        assert_eq!(first.stats.output_tokens, 10);
        assert_eq!(first.stats.prompts, 1, "a tool result is not a prompt");
        assert_eq!(first.stats.context_tokens, 1050);
        assert_eq!(
            first.activity.phase().map(|(phase, _)| phase),
            Some(Phase::Planning),
            "the same read takes in what the session is doing"
        );

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("the transcript");
        use std::io::Write as _;
        write!(
            file,
            "{}\n{}\n{{\"type\":\"assistant\"",
            r#"{"type":"system","subtype":"compact_boundary"}"#,
            reply("b", 20)
        )
        .expect("appending");

        let second = advance_transcript(&path, Some(first.clone()), &none).expect("progress");
        assert!(!second.same_as(&first));
        assert_eq!(second.stats.compactions, 1);
        assert_eq!(second.stats.input_tokens, 70);
        assert_eq!(second.stats.context_tokens, 1020);
        assert!(
            second.stats.cache_hit_rate().is_some_and(|rate| rate > 0.8),
            "most of the prompt came from the cache"
        );
        assert!(
            advance_transcript(&path, Some(second.clone()), &none)
                .is_some_and(|third| third.same_as(&second)),
            "a line still being written is not read"
        );

        let elsewhere = directory.path().join("other.jsonl");
        std::fs::write(&elsewhere, format!("{}\n", reply("c", 5))).expect("a transcript");
        let third = advance_transcript(&elsewhere, Some(second), &none).expect("progress");
        assert_eq!(third.stats.input_tokens, 5, "a new transcript starts from nothing");
        assert_eq!(third.activity.phase(), None);
    }
}
