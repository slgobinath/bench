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
pub mod claude_model;
pub mod claude_usage;
pub mod hooks;
mod keep_awake_button;
mod ports_button;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, Global, SharedString,
    Subscription, SystemNotification, Task, WeakEntity,
};
use settings::Settings as _;
use terminal::Terminal;

pub use claude_model::{ClaudeModel, ModelFamily};
pub use ports_button::PortsButton;
pub use terminal::ListeningPort;
pub use claude_usage::{ClaudeUsage, ClaudeUsageButton};
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

/// Ports are looked for on every this-many sweeps, since it walks the process
/// table and, when something is running, asks `lsof`.
const PORT_SCAN_EVERY: u32 = 3;

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

/// How long the heuristic must have called an agent working before its going
/// quiet is announced as a finished turn.
///
/// An idle Claude Code repaints when the window loses focus or its status line
/// refreshes, which the heuristic sees as [`WORKING_FOR`] of work followed by
/// idleness. The "done" banner is only shown while Bench is in the background,
/// which is exactly when those repaints happen, so without this every switch
/// away from Bench announced the same finished turn again. A real turn keeps
/// the spinner drawing, so it passes this easily.
const MIN_HEURISTIC_TURN: Duration = Duration::from_secs(15);

/// How long a hook's report of work, or of a question, outranks the heuristic.
/// Past this, the turn is assumed to have ended without saying so — an
/// interrupt, a crash, hooks removed — and the PTY goes back to being the
/// source of truth.
///
/// A report that the agent is idle does not run out: see [`is_trusted`].
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
    /// When the current stint of work began, while the agent is working.
    working_since: Option<Instant>,
    /// What the newest Claude Code transcript in this terminal's directory
    /// says about the model; see [`claude_model`].
    model: Option<claude_model::Probe>,
    /// The command in the foreground when it is neither an agent nor the
    /// shell at its prompt: a dev server, a build, a test run.
    running: Option<SharedString>,
}

/// An agent in a terminal-host session that no `Terminal` in this process is
/// attached to. After a restart that is every agent outside the workspaces
/// Bench reopened, which keep running in the daemon the whole time.
#[derive(Clone)]
struct Detached {
    session_id: String,
    directory: PathBuf,
    state: AgentState,
    model: Option<claude_model::Probe>,
}

/// A non-agent process running in a daemon session nothing is attached to.
#[derive(Clone, PartialEq)]
struct DetachedProcess {
    session_id: String,
    directory: PathBuf,
    command: SharedString,
}

/// What the background half of a sweep found in the daemon's unattached
/// sessions.
#[derive(Default)]
struct DetachedSessions {
    agents: Vec<DetachedFound>,
    processes: Vec<DetachedProcess>,
    /// Every unattached session's shell and where it is, for the port scan:
    /// a server can be left running behind an idle prompt.
    shells: Vec<(u32, PathBuf)>,
}

/// What the background half of a sweep found for one detached session.
struct DetachedFound {
    session_id: String,
    directory: PathBuf,
    model: Option<claude_model::Probe>,
}

pub struct AgentTracker {
    watched: HashMap<EntityId, Watched>,
    detached: Vec<Detached>,
    detached_processes: Vec<DetachedProcess>,
    /// What the terminals' programs are listening on, as of the last scan.
    ports: Vec<ListeningPort>,
    reports: hooks::HookReports,
    awake: awake::AwakeGuard,
    /// The user's toggle, off the status bar button. Separate from "is the
    /// assertion held", which is what the agents decide.
    keep_awake: bool,
    /// What Claude Code last reported about the plan's limits; see
    /// [`claude_usage`].
    claude_usage: Option<ClaudeUsage>,
    /// When the usage file last changed, so a sweep reads it only when it
    /// has.
    claude_usage_modified: Option<std::time::SystemTime>,
    claude_status_line_installed: bool,
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
    claude_usage::init(cx);
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
            detached: Vec::new(),
            detached_processes: Vec::new(),
            ports: Vec::new(),
            reports: hooks::HookReports::default(),
            awake: awake::AwakeGuard::new(),
            keep_awake: AgentSettings::get_global(cx).keep_awake,
            claude_usage: None,
            claude_usage_modified: None,
            claude_status_line_installed: false,
            _sweep: cx.spawn(async move |this, cx| {
                let installed = cx.background_spawn(async { claude_usage::installed() }).await;
                if this
                    .update(cx, |this, cx| this.set_claude_status_line_installed(installed, cx))
                    .is_err()
                {
                    return;
                }
                let mut sweeps: u32 = 0;
                loop {
                    cx.background_executor().timer(SWEEP).await;
                    sweeps = sweeps.wrapping_add(1);
                    let scan_ports = sweeps.is_multiple_of(PORT_SCAN_EVERY);
                    let Ok((
                        offset,
                        usage_modified,
                        model_requests,
                        agent_commands,
                        previous_models,
                        live_shells,
                    )) =
                        this.read_with(cx, |this, cx| {
                            (
                                this.reports.offset(),
                                this.claude_usage_modified,
                                this.model_requests(),
                                AgentSettings::get_global(cx).commands.clone(),
                                this.detached
                                    .iter()
                                    .map(|detached| {
                                        (detached.session_id.clone(), detached.model.clone())
                                    })
                                    .collect::<HashMap<_, _>>(),
                                scan_ports.then(|| this.terminal_shells(cx)),
                            )
                        })
                    else {
                        return;
                    };
                    // Off the main thread: this is a `stat` or two every
                    // second, and occasionally a read, on a thread that has
                    // frames to draw.
                    let (appended, usage, models, detached, ports) = cx
                        .background_spawn(async move {
                            let models: Vec<_> = model_requests
                                .into_iter()
                                .map(|(terminal, directory, previous)| {
                                    (terminal, claude_model::probe(&directory, previous.as_ref()))
                                })
                                .collect();
                            let detached = find_detached(&agent_commands, &previous_models);
                            let ports = match live_shells {
                                Some(mut shells) => {
                                    shells.extend(detached.shells.iter().cloned());
                                    match terminal::listening_ports(shells).await {
                                        Ok(ports) => Some(ports),
                                        Err(error) => {
                                            log::debug!("scanning for listening ports: {error:#}");
                                            None
                                        }
                                    }
                                }
                                None => None,
                            };
                            (
                                hooks::read_appended(offset, EVENTS_MAX_BYTES),
                                claude_usage::read_if_changed(usage_modified),
                                models,
                                detached,
                                ports,
                            )
                        })
                        .await;
                    if this
                        .update(cx, |this, cx| {
                            this.take_models(models, cx);
                            this.take_claude_usage(usage, cx);
                            this.sweep(appended, cx);
                            this.take_detached(detached, cx);
                            this.take_ports(ports, cx);
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }),
            _subscriptions: subscriptions,
        }
    }

    /// The terminals with an agent in them, for the sweep to look up the model
    /// of off the main thread: where each is, and what was found last time.
    fn model_requests(&self) -> Vec<(EntityId, PathBuf, Option<claude_model::Probe>)> {
        self.watched
            .iter()
            .filter(|(_, watched)| watched.command.is_some())
            .filter_map(|(id, watched)| {
                Some((*id, watched.directory.clone()?, watched.model.clone()))
            })
            .collect()
    }

    fn take_models(
        &mut self,
        models: Vec<(EntityId, Option<claude_model::Probe>)>,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        for (terminal, probe) in models {
            let Some(watched) = self.watched.get_mut(&terminal) else {
                continue;
            };
            changed |= watched.model.as_ref().and_then(|probe| probe.model.as_ref())
                != probe.as_ref().and_then(|probe| probe.model.as_ref());
            watched.model = probe;
        }
        if changed {
            cx.emit(AgentsChanged);
            cx.notify();
        }
    }

    /// Each terminal's shell and where the terminal is, for the port scan.
    fn terminal_shells(&self, cx: &App) -> Vec<(u32, PathBuf)> {
        self.watched
            .values()
            .filter_map(|watched| {
                let terminal = watched.terminal.upgrade()?;
                let terminal = terminal.read(cx);
                Some((terminal.shell_pid()?, terminal.working_directory()?))
            })
            .collect()
    }

    fn take_ports(&mut self, ports: Option<Vec<ListeningPort>>, cx: &mut Context<Self>) {
        let Some(ports) = ports else {
            return;
        };
        if ports != self.ports {
            self.ports = ports;
            cx.emit(AgentsChanged);
            cx.notify();
        }
    }

    /// The ports the programs in terminals inside any of `directories` are
    /// listening on, lowest first, one entry per port.
    pub fn ports_for(&self, directories: &[PathBuf]) -> Vec<ListeningPort> {
        let mut ports: Vec<ListeningPort> = self
            .ports
            .iter()
            .filter(|port| {
                directories
                    .iter()
                    .any(|directory| port.directory.starts_with(directory))
            })
            .cloned()
            .collect();
        ports.sort_by_key(|port| port.port);
        ports.dedup_by_key(|port| port.port);
        ports
    }

    /// Runs after `sweep` so the hook reports it reads include this pass's.
    fn take_detached(&mut self, found: DetachedSessions, cx: &mut Context<Self>) {
        let now = Instant::now();
        let DetachedSessions {
            agents, processes, ..
        } = found;
        let detached: Vec<Detached> = agents
            .into_iter()
            .map(|found| {
                // Without a terminal there is no output to judge by, so only a
                // hook can say more than "an agent is there".
                let state = self
                    .reports
                    .for_directory(&found.directory)
                    .filter(|report| is_trusted(report, now))
                    .map_or(AgentState::Idle, |report| report.state);
                Detached {
                    session_id: found.session_id,
                    directory: found.directory,
                    state,
                    model: found.model,
                }
            })
            .collect();

        let describe = |detached: &[Detached]| {
            detached
                .iter()
                .map(|detached| {
                    (
                        detached.session_id.clone(),
                        detached.directory.clone(),
                        detached.state,
                        detached.model.as_ref().and_then(|probe| probe.model.clone()),
                    )
                })
                .collect::<Vec<_>>()
        };
        let changed = describe(&self.detached) != describe(&detached)
            || self.detached_processes != processes;
        self.detached = detached;
        self.detached_processes = processes;
        if changed {
            cx.emit(AgentsChanged);
            cx.notify();
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
                working_since: None,
                model: None,
                running: None,
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

            let foreground = terminal
                .read(cx)
                .foreground_process_command_name()
                .map(SharedString::from);
            let command = foreground
                .clone()
                .filter(|command| settings.is_agent(command));
            // Unknown counts as the shell: an icon for a process that may not
            // be there is worse than none for one that is.
            let running = foreground.filter(|_| {
                command.is_none() && terminal.read(cx).foreground_process_is_shell() == Some(false)
            });
            let directory = terminal.read(cx).working_directory();

            let was = watched.had_agent.then_some(watched.state);
            let bell = std::mem::take(&mut watched.bell);

            let Some(command) = command else {
                watched.command = None;
                watched.had_agent = false;
                watched.working_since = None;
                watched.model = None;
                changed |= was.is_some() || watched.running != running;
                watched.running = running;
                if watched.running.is_some() {
                    watched.directory = directory;
                }
                return true;
            };
            changed |= watched.running.take().is_some();

            let reported = directory
                .as_deref()
                .and_then(|directory| self.reports.for_directory(directory))
                .filter(|report| is_trusted(report, now));

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

            // A hook's report of a finished turn is exact; only the heuristic's
            // needs the stint of work behind it checked.
            let worked_for = reported.is_none().then(|| {
                watched
                    .working_since
                    .map_or(Duration::ZERO, |since| now.duration_since(since))
            });
            watched.working_since = match state {
                AgentState::Working => watched.working_since.or(Some(now)),
                AgentState::Idle | AgentState::NeedsInput => None,
            };

            if let Some(announcement) =
                announcement(was, state, worked_for, &command, directory.as_deref())
            {
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
        for detached in &self.detached {
            if detached.directory.starts_with(directory) {
                summary.count(detached.state);
            }
        }
        summary
    }

    /// The models the agents inside `directory` are using, each once.
    pub fn models_for(&self, directory: &Path) -> Vec<ClaudeModel> {
        let mut models: Vec<ClaudeModel> = Vec::new();
        for watched in self.watched.values() {
            if watched.command.is_none()
                || !watched
                    .directory
                    .as_deref()
                    .is_some_and(|agent_directory| agent_directory.starts_with(directory))
            {
                continue;
            }
            if let Some(model) = watched.model.as_ref().and_then(|probe| probe.model.clone())
                && !models.contains(&model)
            {
                models.push(model);
            }
        }
        for detached in &self.detached {
            if !detached.directory.starts_with(directory) {
                continue;
            }
            if let Some(model) = detached.model.as_ref().and_then(|probe| probe.model.clone())
                && !models.contains(&model)
            {
                models.push(model);
            }
        }
        models
    }

    /// The commands running inside `directory` that are not agents, each once.
    pub fn processes_for(&self, directory: &Path) -> Vec<SharedString> {
        let mut commands: Vec<SharedString> = Vec::new();
        let watched = self
            .watched
            .values()
            .filter_map(|watched| Some((watched.directory.as_deref()?, watched.running.as_ref()?)));
        let detached = self
            .detached_processes
            .iter()
            .map(|process| (process.directory.as_path(), &process.command));
        for (process_directory, command) in watched.chain(detached) {
            if process_directory.starts_with(directory) && !commands.contains(command) {
                commands.push(command.clone());
            }
        }
        commands
    }

    /// The model the agent in one terminal is using, if it has one and it is
    /// known.
    pub fn model_for(&self, terminal: EntityId) -> Option<ClaudeModel> {
        self.watched
            .get(&terminal)
            .filter(|watched| watched.command.is_some())
            .and_then(|watched| watched.model.as_ref()?.model.clone())
    }

    /// What the agent in one terminal is doing, if that terminal has one.
    pub fn state_for(&self, terminal: EntityId) -> Option<AgentState> {
        self.watched
            .get(&terminal)
            .filter(|watched| watched.command.is_some())
            .map(|watched| watched.state)
    }

    /// Every agent Bench can see, wherever it is running.
    pub fn summary(&self) -> AgentSummary {
        let mut summary = AgentSummary::default();
        for watched in self.watched.values() {
            if watched.command.is_some() {
                summary.count(watched.state);
            }
        }
        for detached in &self.detached {
            summary.count(detached.state);
        }
        summary
    }

    /// Whether the machine is being held awake right now.
    pub fn is_holding_awake(&self) -> bool {
        self.awake.is_held()
    }

    /// Whether the user has left the stay-awake behaviour turned on.
    /// What Claude Code last reported about the plan's limits, if it has.
    pub fn claude_usage(&self) -> Option<&ClaudeUsage> {
        self.claude_usage.as_ref()
    }

    /// Whether Claude Code's status line is Bench's, which is what reports the
    /// usage. As of launch, and of the last install or uninstall from Bench.
    pub fn claude_status_line_installed(&self) -> bool {
        self.claude_status_line_installed
    }

    pub(crate) fn set_claude_status_line_installed(
        &mut self,
        installed: bool,
        cx: &mut Context<Self>,
    ) {
        if self.claude_status_line_installed != installed {
            self.claude_status_line_installed = installed;
            cx.emit(AgentsChanged);
            cx.notify();
        }
    }

    /// Takes in what [`claude_usage::read_if_changed`] found. A file that
    /// changed but could not be read as usage leaves the last report standing.
    fn take_claude_usage(
        &mut self,
        read: Option<(std::time::SystemTime, Option<ClaudeUsage>)>,
        cx: &mut Context<Self>,
    ) {
        let Some((modified, usage)) = read else {
            return;
        };
        self.claude_usage_modified = Some(modified);
        if let Some(usage) = usage
            && self.claude_usage.as_ref() != Some(&usage)
        {
            self.claude_usage = Some(usage);
            cx.emit(AgentsChanged);
            cx.notify();
        }
    }

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
///
/// `worked_for` is how long the heuristic had called the agent working, or
/// `None` when a hook reported the transition.
fn announcement(
    was: Option<AgentState>,
    now: AgentState,
    worked_for: Option<Duration>,
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
        (AgentState::Working, AgentState::Idle)
            if worked_for.is_none_or(|worked_for| worked_for >= MIN_HEURISTIC_TURN) =>
        {
            Some(Announcement {
                tag,
                title: format!("{command} is done").into(),
                body: format!("{where_} — the turn has finished.").into(),
                interrupting: false,
            })
        }
        _ => None,
    }
}

/// Posts the banner, unless the user is plainly already watching.
///
/// Whether a hook's report still outranks the heuristic.
///
/// An idle report stays trusted for as long as it is kept. Nothing ends
/// idleness but a prompt, and a prompt is a hook of its own; while the agent
/// sits at its prompt the heuristic has nothing true to add, and plenty false —
/// the prompt redraws as you type in it, and redrawing is what the heuristic
/// calls work. A session that ended is no agent at all, which the foreground
/// process says, whatever its last report was.
/// What is running in daemon sessions nothing is attached to. An attached
/// session is already covered by its `Terminal`, in this process or another
/// Bench's.
fn find_detached(
    agent_commands: &[SharedString],
    previous_models: &HashMap<String, Option<claude_model::Probe>>,
) -> DetachedSessions {
    let Some(host) = terminal_host::host() else {
        return DetachedSessions::default();
    };
    let sessions = match host.sessions() {
        Ok(sessions) => sessions,
        Err(error) => {
            log::debug!("listing terminal host sessions: {error:#}");
            return DetachedSessions::default();
        }
    };
    let mut found = DetachedSessions::default();
    for session in sessions.into_iter().filter(|session| !session.attached) {
        let Some(foreground) = terminal::session_foreground(session.pid) else {
            continue;
        };
        found.shells.push((
            session.pid,
            foreground
                .shell_directory
                .clone()
                .unwrap_or_else(|| foreground.directory.clone()),
        ));
        let Some(command) = foreground.command else {
            continue;
        };
        if agent_commands.iter().any(|agent| *agent == command) {
            let previous = previous_models.get(&session.id).and_then(Option::as_ref);
            found.agents.push(DetachedFound {
                model: claude_model::probe(&foreground.directory, previous),
                directory: foreground.directory,
                session_id: session.id,
            });
        } else if !foreground.is_shell {
            found.processes.push(DetachedProcess {
                session_id: session.id,
                directory: foreground.directory,
                command: command.into(),
            });
        }
    }
    found
}

fn is_trusted(report: &hooks::Report, now: Instant) -> bool {
    report.state == AgentState::Idle || now.duration_since(report.at) < REPORT_TRUSTED_FOR
}

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
    fn an_idle_report_outlasts_a_busy_one() {
        let now = Instant::now();
        let report = |state, age| hooks::Report {
            cwd: PathBuf::from("/repo"),
            state,
            at: now.checked_sub(age).unwrap_or(now),
        };
        let long_ago = REPORT_TRUSTED_FOR + Duration::from_secs(60);
        assert!(is_trusted(&report(AgentState::Working, Duration::ZERO), now));
        assert!(!is_trusted(&report(AgentState::Working, long_ago), now));
        assert!(!is_trusted(&report(AgentState::NeedsInput, long_ago), now));
        assert!(is_trusted(&report(AgentState::Idle, long_ago), now));
    }

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
                None,
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
            None,
            &"claude".into(),
            Some(Path::new("/repo/fix-login")),
        )
        .expect("an agent that stopped to ask is worth a banner");
        assert!(asking.interrupting);
        assert!(asking.body.contains("fix-login"), "say which worktree");

        let done = announcement(
            Some(AgentState::Working),
            AgentState::Idle,
            None,
            &"claude".into(),
            Some(Path::new("/repo/fix-login")),
        )
        .expect("a finished turn is worth a banner too");
        assert!(!done.interrupting);

        assert!(
            announcement(
                Some(AgentState::Idle),
                AgentState::Working,
                None,
                &"claude".into(),
                None
            )
            .is_none(),
            "starting work is not news"
        );
    }

    #[test]
    fn a_repaint_is_not_announced_as_a_finished_turn() {
        let finish = |worked_for| {
            announcement(
                Some(AgentState::Working),
                AgentState::Idle,
                worked_for,
                &"claude".into(),
                Some(Path::new("/repo/fix-login")),
            )
        };
        assert!(
            finish(Some(WORKING_FOR + Duration::from_secs(1))).is_none(),
            "one burst of output, like a repaint on losing focus, is not a turn"
        );
        assert!(finish(Some(MIN_HEURISTIC_TURN)).is_some());
        assert!(
            finish(None).is_some(),
            "a hook's report of a finished turn is announced however short"
        );
    }
}
