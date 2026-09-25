//! Which agents are running in Bench's terminals, and what each of them is
//! doing.
//!
//! Bench's premise is that the agents are *outside* the editor: they run in
//! terminals, one per worktree, and the editor is the bench they sit on. That
//! leaves the window unable to answer the questions you actually have — is it
//! still working, has it stopped to ask me something, can I close the lid —
//! unless something watches them. This is that something.
//!
//! # Two sources, deliberately
//!
//! **Every terminal is watched** for a foreground process whose name is one of
//! [`AgentSettings::commands`]. That is free, needs no setup, and is exact
//! about *presence*: one `claude` in a pane is one agent. What it cannot be
//! exact about is state — an agent waiting on a permission prompt and an agent
//! thinking through a long tool call are both just a quiet PTY — so the
//! heuristic reads output and the terminal bell and settles for a good guess.
//!
//! **Claude Code's hooks** say it outright, and Bench installs them on request;
//! see [`hooks`]. A report from a hook outranks the heuristic for as long as it
//! is fresh. Nothing depends on the hooks being installed: without them the
//! panel is a little coarser, and that is all.
//!
//! # What it drives
//!
//! - The worktree panel's per-row count and status dot.
//! - A system notification when an agent stops to ask, or finishes.
//! - The stay-awake assertion, so a long run does not end in a sleeping Mac;
//!   see [`awake`].

pub mod awake;
pub mod hooks;
mod keep_awake_button;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, Global, SharedString,
    Subscription, SystemNotification, Task, WeakEntity,
};
use settings::Settings as _;
use terminal::Terminal;

pub use keep_awake_button::{KeepAwakeButton, ToggleKeepAwake, agent_state_color};

/// Which terminal processes count as agents, and what Bench does while one of
/// them is working. See the `agents` section of the settings file.
#[derive(Clone, Debug, PartialEq, settings::RegisterSetting)]
pub struct AgentSettings {
    /// The command names that count as an agent. A list rather than a constant
    /// so that a second agent CLI is a settings line, not a release.
    pub commands: Vec<SharedString>,
    pub notify: bool,
    pub keep_awake: bool,
}

impl AgentSettings {
    /// Whether a foreground process name is one of the agents Bench tracks.
    pub fn is_agent(&self, command: &str) -> bool {
        self.commands.iter().any(|agent| agent == command)
    }
}

impl settings::Settings for AgentSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let agents = content.agents.clone().unwrap_or_default();
        Self {
            commands: agents
                .commands
                .unwrap_or_default()
                .into_iter()
                .map(SharedString::from)
                .collect(),
            notify: agents.notify.unwrap_or(true),
            keep_awake: agents.keep_awake.unwrap_or(true),
        }
    }
}

/// How often the terminals are swept. Fast enough that a state change is seen
/// before you look, slow enough to be nothing: the work is reading a cached
/// process name per terminal and, at most, the tail of one small file.
const SWEEP: Duration = Duration::from_secs(1);

/// How long after its last output an agent is still called working.
///
/// Generous, because output during a tool call comes in bursts with real gaps
/// between them — and because being wrong in this direction reads as "still
/// going" for a moment too long, while being wrong the other way announces
/// that an agent has finished when it has not.
const WORKING_FOR: Duration = Duration::from_secs(4);

/// How long a hook's report outranks the heuristic. Past this, the session is
/// assumed to have ended without saying so — a crash, a `kill`, hooks removed
/// — and the PTY goes back to being the source of truth.
const REPORT_TRUSTED_FOR: Duration = Duration::from_secs(300);

/// How large the hook events file may grow before it is truncated, and how
/// long a report is kept in memory.
const EVENTS_MAX_BYTES: u64 = 1 << 20;
const REPORTS_KEPT_FOR: Duration = Duration::from_secs(3600);

/// What an agent is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentState {
    /// Alive, with nothing to do: sitting at its prompt, waiting for you to
    /// think of something.
    Idle,
    /// Producing output, or inside a turn a hook told us about.
    Working,
    /// Stopped to ask: a permission prompt, a question, a choice. The one
    /// state that is worth interrupting someone over.
    NeedsInput,
}

/// The agents of one worktree, counted by what they are doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentSummary {
    pub idle: usize,
    pub working: usize,
    pub needs_input: usize,
}

impl AgentSummary {
    pub fn total(&self) -> usize {
        self.idle + self.working + self.needs_input
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// The one state a single dot can show: whatever wants you most.
    pub fn state(&self) -> Option<AgentState> {
        if self.needs_input > 0 {
            Some(AgentState::NeedsInput)
        } else if self.working > 0 {
            Some(AgentState::Working)
        } else if self.idle > 0 {
            Some(AgentState::Idle)
        } else {
            None
        }
    }

    fn count(&mut self, state: AgentState) {
        match state {
            AgentState::Idle => self.idle += 1,
            AgentState::Working => self.working += 1,
            AgentState::NeedsInput => self.needs_input += 1,
        }
    }
}

/// One terminal, and the agent in it if there is one.
struct Watched {
    terminal: WeakEntity<Terminal>,
    /// The agent's command name, absent while the pane is running something
    /// else — a shell, a build, nothing at all.
    command: Option<SharedString>,
    directory: Option<PathBuf>,
    state: AgentState,
    /// When this terminal last produced output, which is the whole of the
    /// heuristic's evidence for "working".
    last_output: Instant,
    /// A bell since the last sweep. Claude Code rings it when it wants you,
    /// and so does every other well-behaved program, which is why it only
    /// counts while an agent is the foreground process.
    bell: bool,
    /// Whether this terminal has ever had an agent in it. Keeps a notification
    /// from firing for the shell prompt an agent leaves behind when it exits.
    had_agent: bool,
}

pub struct AgentTracker {
    watched: HashMap<EntityId, Watched>,
    reports: hooks::HookReports,
    awake: awake::AwakeGuard,
    /// The user's toggle, off the status bar button. Separate from "is the
    /// assertion held", which is what the agents decide.
    keep_awake: bool,
    _sweep: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// Emitted when the tracker's picture of the world changes, so that panels can
/// redraw without observing the entity — an observer here would fire on every
/// sweep whether anything moved or not.
pub struct AgentsChanged;

impl EventEmitter<AgentsChanged> for AgentTracker {}

struct GlobalAgentTracker(Entity<AgentTracker>);

impl Global for GlobalAgentTracker {}

/// Starts tracking. Call once, during app initialisation.
pub fn init(cx: &mut App) {
    AgentSettings::register(cx);
    let tracker = cx.new(AgentTracker::new);
    cx.set_global(GlobalAgentTracker(tracker));
    keep_awake_button::init(cx);
}

impl AgentTracker {
    /// The app-wide tracker, if [`init`] has run. Absent in tests that do not
    /// want it.
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalAgentTracker>()
            .map(|global| global.0.clone())
    }

    fn new(cx: &mut Context<Self>) -> Self {
        // Every terminal in the application, wherever it lives — a dock panel,
        // a center pane, a split — without terminal_view or workspace having
        // to hand them over.
        let subscriptions = vec![cx.observe_new::<Terminal>(|_, _, cx| {
            let terminal = cx.entity();
            if let Some(tracker) = AgentTracker::try_global(cx) {
                tracker.update(cx, |tracker, cx| tracker.watch(terminal, cx));
            }
        })];

        Self {
            watched: HashMap::new(),
            reports: hooks::HookReports::default(),
            awake: awake::AwakeGuard::new(),
            keep_awake: AgentSettings::get_global(cx).keep_awake,
            _sweep: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(SWEEP).await;
                    let Ok(offset) = this.read_with(cx, |this, _| this.reports.offset()) else {
                        return;
                    };
                    // Off the main thread: this is a `stat` every second, and
                    // occasionally a read, on a thread that has frames to draw.
                    let appended = cx
                        .background_spawn(async move {
                            hooks::read_appended(offset, EVENTS_MAX_BYTES)
                        })
                        .await;
                    if this.update(cx, |this, cx| this.sweep(appended, cx)).is_err() {
                        return;
                    }
                }
            }),
            _subscriptions: subscriptions,
        }
    }

    fn watch(&mut self, terminal: Entity<Terminal>, cx: &mut Context<Self>) {
        let id = terminal.entity_id();
        self._subscriptions.push(cx.subscribe(
            &terminal,
            move |this, _, event: &terminal::Event, _| {
                let Some(watched) = this.watched.get_mut(&id) else {
                    return;
                };
                match event {
                    terminal::Event::Wakeup => watched.last_output = Instant::now(),
                    terminal::Event::Bell => watched.bell = true,
                    _ => {}
                }
            },
        ));

        self.watched.insert(
            id,
            Watched {
                terminal: terminal.downgrade(),
                command: None,
                directory: None,
                state: AgentState::Idle,
                last_output: Instant::now(),
                bell: false,
                had_agent: false,
            },
        );
    }

    /// One pass: who is running, what are they doing, and what does that mean
    /// for the machine staying awake and for the user being told.
    fn sweep(&mut self, appended: hooks::Appended, cx: &mut Context<Self>) {
        let now = Instant::now();
        let settings = AgentSettings::get_global(cx).clone();

        self.reports.ingest(appended, now);
        self.reports.prune(now, REPORTS_KEPT_FOR);

        let mut changed = false;
        let mut any_working = false;
        let mut announcements = Vec::new();

        self.watched.retain(|_, watched| {
            let Some(terminal) = watched.terminal.upgrade() else {
                return false;
            };

            let command = terminal
                .read(cx)
                .foreground_process_command_name()
                .map(SharedString::from)
                .filter(|command| settings.is_agent(command));
            let directory = terminal.read(cx).working_directory();

            let was = watched.had_agent.then_some(watched.state);
            let bell = std::mem::take(&mut watched.bell);

            let Some(command) = command else {
                watched.command = None;
                watched.had_agent = false;
                changed |= was.is_some();
                return true;
            };

            let reported = directory
                .as_deref()
                .and_then(|directory| self.reports.for_directory(directory))
                .filter(|report| now.duration_since(report.at) < REPORT_TRUSTED_FOR);

            let state = match reported {
                Some(report) => report.state,
                // No hook to tell us, so: a bell is a request for attention,
                // recent output is work, and neither is an agent at its prompt.
                None if bell => AgentState::NeedsInput,
                None if now.duration_since(watched.last_output) < WORKING_FOR => {
                    AgentState::Working
                }
                None => AgentState::Idle,
            };

            any_working |= state == AgentState::Working;
            changed |= was != Some(state) || watched.command.as_ref() != Some(&command);

            if let Some(announcement) = announcement(was, state, &command, directory.as_deref()) {
                announcements.push(announcement);
            }

            watched.command = Some(command);
            watched.directory = directory;
            watched.state = state;
            watched.had_agent = true;
            true
        });

        self.awake.set(self.keep_awake && any_working, now);

        if settings.notify {
            for announcement in announcements {
                announce(announcement, cx);
            }
        }
        if changed {
            cx.emit(AgentsChanged);
            cx.notify();
        }
    }

    /// The agents whose working directory is inside `directory` — one
    /// worktree's worth, when called with a worktree root.
    pub fn summary_for(&self, directory: &Path) -> AgentSummary {
        let mut summary = AgentSummary::default();
        for watched in self.watched.values() {
            if watched.command.is_none() {
                continue;
            }
            if watched
                .directory
                .as_deref()
                .is_some_and(|agent_directory| agent_directory.starts_with(directory))
            {
                summary.count(watched.state);
            }
        }
        summary
    }

    /// Every agent Bench can see, wherever it is running.
    pub fn summary(&self) -> AgentSummary {
        let mut summary = AgentSummary::default();
        for watched in self.watched.values() {
            if watched.command.is_some() {
                summary.count(watched.state);
            }
        }
        summary
    }

    /// Whether the machine is being held awake right now.
    pub fn is_holding_awake(&self) -> bool {
        self.awake.is_held()
    }

    /// Whether the user has left the stay-awake behaviour turned on.
    pub fn keep_awake(&self) -> bool {
        self.keep_awake
    }

    pub fn set_keep_awake(&mut self, keep_awake: bool, cx: &mut Context<Self>) {
        self.keep_awake = keep_awake;
        if !keep_awake {
            self.awake.release();
        }
        cx.emit(AgentsChanged);
        cx.notify();
    }

    /// Whether any hook has reported, which is how the UI can say that the
    /// exact half of this is switched on.
    pub fn has_hook_reports(&self) -> bool {
        !self.reports.is_empty()
    }
}

struct Announcement {
    tag: SharedString,
    title: SharedString,
    body: SharedString,
    /// Whether this is worth showing while the user is looking at Bench.
    interrupting: bool,
}

/// What, if anything, to tell the user about a transition.
///
/// Only two transitions are worth a banner: an agent that has stopped to ask,
/// and an agent that has finished. Everything else — starting, carrying on,
/// going quiet for a moment — is what the panel is for.
fn announcement(
    was: Option<AgentState>,
    now: AgentState,
    command: &SharedString,
    directory: Option<&Path>,
) -> Option<Announcement> {
    let was = was?;
    if was == now {
        return None;
    }
    let where_ = directory
        .and_then(|directory| directory.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "a worktree".to_owned());

    let tag = SharedString::from(format!("agent:{where_}"));
    match (was, now) {
        (_, AgentState::NeedsInput) => Some(Announcement {
            tag,
            title: format!("{command} needs you").into(),
            body: format!("{where_} — it has stopped to ask something.").into(),
            interrupting: true,
        }),
        (AgentState::Working, AgentState::Idle) => Some(Announcement {
            tag,
            title: format!("{command} is done").into(),
            body: format!("{where_} — the turn has finished.").into(),
            interrupting: false,
        }),
        _ => None,
    }
}

/// Posts the banner, unless the user is plainly already watching.
///
/// A notification for an agent that has stopped to ask is shown whatever the
/// user is doing: it is blocking, and being in another Bench window is exactly
/// when it is easy to miss. "Done" is not blocking, so it is held back while
/// Bench is the active application — the panel already says so, and a banner
/// over the terminal you are reading is noise.
fn announce(announcement: Announcement, cx: &mut App) {
    if !announcement.interrupting && cx.active_window().is_some() {
        return;
    }
    cx.show_system_notification(SystemNotification {
        tag: announcement.tag,
        title: announcement.title,
        body: announcement.body,
        actions: Vec::new(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_shows_whatever_wants_you_most() {
        let mut summary = AgentSummary::default();
        summary.count(AgentState::Idle);
        assert_eq!(summary.state(), Some(AgentState::Idle));

        summary.count(AgentState::Working);
        assert_eq!(summary.state(), Some(AgentState::Working));

        summary.count(AgentState::NeedsInput);
        assert_eq!(
            summary.state(),
            Some(AgentState::NeedsInput),
            "one agent asking outranks any number of them working"
        );
        assert_eq!(summary.total(), 3);
    }

    #[test]
    fn nothing_is_announced_for_an_agent_that_was_not_there_before() {
        assert!(
            announcement(
                None,
                AgentState::NeedsInput,
                &"claude".into(),
                Some(Path::new("/repo/fix"))
            )
            .is_none(),
            "the first sweep of a terminal is not a transition"
        );
    }

    #[test]
    fn stopping_to_ask_interrupts_and_finishing_does_not() {
        let asking = announcement(
            Some(AgentState::Working),
            AgentState::NeedsInput,
            &"claude".into(),
            Some(Path::new("/repo/fix-login")),
        )
        .expect("an agent that stopped to ask is worth a banner");
        assert!(asking.interrupting);
        assert!(asking.body.contains("fix-login"), "say which worktree");

        let done = announcement(
            Some(AgentState::Working),
            AgentState::Idle,
            &"claude".into(),
            Some(Path::new("/repo/fix-login")),
        )
        .expect("a finished turn is worth a banner too");
        assert!(!done.interrupting);

        assert!(
            announcement(
                Some(AgentState::Idle),
                AgentState::Working,
                &"claude".into(),
                None
            )
            .is_none(),
            "starting work is not news"
        );
    }
}
