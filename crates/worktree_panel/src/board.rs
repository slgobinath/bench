//! The board: one page in a project's main checkout listing every worktree of
//! it with what wants acting on — a question an agent is waiting on you for,
//! work with no pull request, a merged worktree to delete — and the last recap
//! each agent wrote, so that most trips into a worktree are no longer needed to
//! know whether to make one. The lead agent runs beside the cards, on the
//! board itself: a Claude Code conversation over ACP in the main checkout,
//! which reads the board through Bench's MCP server and relays your answers to
//! the worktrees' agents. It lives as long as the board does and keeps nothing
//! of its own, so closing the board and opening it again loses nothing.
//!
//! The agent board before it showed what each agent was busy with, and that
//! did not save the trips. So the board leads with what needs you, and keeps
//! the agent's own activity to its one-line recap.
//!
//! The agents, issues and rows are the worktree panel's. What the board adds is
//! the nested repositories a worktree holds (`repos/<name>`): their commits
//! and pull requests, which the panel does not look at. It asks git and `gh`
//! only while it is open — once on opening, when asked, when an agent finishes
//! a turn, and every few minutes — and `gh` once per repository, not per
//! worktree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use agent_tracker::{AgentState, AgentSummary, AgentTracker, AgentsChanged, agent_state_color};
use agent_ui::ConversationView;
use anyhow::Context as _;
use git_ui_core::pull_request_color::pull_request_color;
use github_cli::PullRequestState;
use gpui::{
    AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Subscription,
    Task, WeakEntity, Window, actions, prelude::*, px,
};
use linear::StateType;
use project::{Fs, ProjectGroupKey};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

use crate::{
    WorktreePanel, WorktreeRow, edited_ago, nested_worktrees, parse_status, pull_requests_by_branch,
};

actions!(
    board,
    [
        /// Opens the board in this project's main checkout: every worktree,
        /// what is waiting on you in each, and what its work needs next.
        OpenBoard,
    ]
);

/// How often an open board asks git and GitHub again by itself.
const REFRESH_EVERY: Duration = Duration::from_secs(300);

/// The least time between two scans that an agent finishing its turn starts:
/// several agents finishing together are one scan.
const RESCAN_AFTER_AGENTS: Duration = Duration::from_secs(30);

/// What the lead agent is asked as it starts, so that it opens with what
/// needs you. The `lead` skill holds how it reads the board and relays.
const LEAD_FIRST_PROMPT: &str = "Use the lead skill: what needs me?";

/// The lead's column, beside the cards.
const LEAD_WIDTH: f32 = 460.;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenBoard, window, cx| {
            open(workspace, window, cx);
        });
    })
    .detach();
}

/// Opens the board in the project's main checkout, going there first when the
/// action came from another of its worktrees.
///
/// Deferred because finding the main checkout reads every workspace of the
/// window, and an action handler holds this one's lease.
fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace.items_of_type::<BoardView>(cx).next();
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let Some(panel) = workspace.panel::<WorktreePanel>(cx) else {
        return;
    };
    let here = cx.entity();
    let key = workspace.project_group_key(cx);
    window.defer(cx, move |window, cx| {
        let tree = panel.read(cx).tree(cx);
        let Some(main) = tree
            .into_iter()
            .find(|row| row.key.matches(&key))
            .and_then(|row| row.worktrees.into_iter().find(|worktree| worktree.is_main))
        else {
            return;
        };
        let Some(root) = main.root.clone() else {
            return;
        };
        if main.workspace.as_ref() == Some(&here) {
            let Some(fs) = panel.read(cx).fs(cx) else {
                return;
            };
            let workspace = here.downgrade();
            let board =
                cx.new(|cx| BoardView::new(panel.downgrade(), workspace, key, root, fs, cx));
            here.update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(board), None, true, window, cx);
            });
        } else {
            panel.update(cx, |panel, cx| {
                panel.open_worktree(
                    main.key.clone(),
                    root,
                    main.name.clone(),
                    Some(Box::new(OpenBoard)),
                    window,
                    cx,
                );
            });
        }
    });
}

/// One checkout inside a worktree: the worktree's own, or a nested
/// repository's.
#[derive(Clone, Debug, PartialEq)]
pub struct BoardCheckout {
    pub name: SharedString,
    /// Commits on the branch that its repository's default branch does not
    /// have, as of the last fetch.
    pub ahead: u32,
    /// How many entries `git status` lists.
    pub changes: usize,
    pub pull_request: Option<PullRequestState>,
}

/// One worktree as the board shows it, for callers outside the panel.
pub struct BoardEntry {
    pub root: Option<PathBuf>,
    pub name: SharedString,
    /// The identifier of its Linear issue, such as `RB-352`.
    pub issue: Option<SharedString>,
    pub title: SharedString,
    pub issue_state: Option<SharedString>,
    pub agents: AgentSummary,
    pub last_active: Option<SystemTime>,
    /// What an agent here is waiting on you for, when one is.
    pub question: Option<SharedString>,
    /// The newest recap an agent here wrote.
    pub recap: Option<SharedString>,
    /// Its own checkout first, then the nested repositories'.
    pub checkouts: Vec<BoardCheckout>,
    pub hints: Vec<SharedString>,
}

pub struct BoardView {
    panel: WeakEntity<WorktreePanel>,
    workspace: WeakEntity<Workspace>,
    key: ProjectGroupKey,
    /// The project's main checkout, which the board lives in and does not list.
    main_root: PathBuf,
    fs: Arc<dyn Fs>,
    /// Each worktree's checkouts, by its root, as of the last scan.
    checkouts: HashMap<PathBuf, Vec<BoardCheckout>>,
    scanned_at: Option<Instant>,
    scanning: Option<Task<()>>,
    /// The lead agent's conversation, once it is started.
    lead: Option<Entity<ConversationView>>,
    focus_handle: FocusHandle,
    _poll: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl BoardView {
    fn new(
        panel: WeakEntity<WorktreePanel>,
        workspace: WeakEntity<Workspace>,
        key: ProjectGroupKey,
        main_root: PathBuf,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(tracker) = AgentTracker::try_global(cx) {
            subscriptions.push(cx.subscribe(&tracker, |this, _, _: &AgentsChanged, cx| {
                // A turn ending is when an agent has just committed, pushed or
                // opened a pull request.
                if this
                    .scanned_at
                    .is_none_or(|at| at.elapsed() >= RESCAN_AFTER_AGENTS)
                {
                    this.refresh(cx);
                }
                cx.notify();
            }));
        }
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_EVERY).await;
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            panel,
            workspace,
            key,
            main_root,
            fs,
            checkouts: HashMap::default(),
            scanned_at: None,
            scanning: None,
            lead: None,
            focus_handle: cx.focus_handle(),
            _poll: poll,
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this
    }

    /// The project's worktrees, the main checkout aside.
    fn worktrees(&self, cx: &App) -> Vec<WorktreeRow> {
        let Some(panel) = self.panel.upgrade() else {
            return Vec::new();
        };
        panel
            .read(cx)
            .tree(cx)
            .into_iter()
            .find(|row| row.key.matches(&self.key))
            .map(|row| {
                row.worktrees
                    .into_iter()
                    .filter(|worktree| !worktree.is_main)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Asks git and GitHub again about every worktree, unless a scan is
    /// already running.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.scanning.is_some() {
            return;
        }
        let roots: Vec<PathBuf> = self
            .worktrees(cx)
            .into_iter()
            .filter_map(|worktree| worktree.root)
            .collect();
        let project_name: SharedString = self
            .main_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        let scan = scan(self.fs.clone(), self.main_root.clone(), project_name, roots);
        self.scanning = Some(cx.spawn(async move |this, cx| {
            let checkouts = cx.background_spawn(scan).await;
            this.update(cx, |this, cx| {
                this.checkouts = checkouts;
                this.scanned_at = Some(Instant::now());
                this.scanning = None;
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn go_to(&self, destination: Destination, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.panel.upgrade() else {
            return;
        };
        let Destination {
            workspace,
            key,
            root,
            name,
        } = destination;
        panel.update(cx, |panel, cx| match (workspace, root) {
            (Some(workspace), _) => panel.activate(workspace, window, cx),
            (None, Some(root)) => panel.open_worktree(key, root, name, None, window, cx),
            (None, None) => {}
        });
    }

    /// Starts the lead agent beside the cards.
    fn start_lead(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        let lead = agent_ui::new_claude_conversation(
            self.workspace.clone(),
            project,
            &self.main_root,
            "Lead".into(),
            Some(LEAD_FIRST_PROMPT.to_string()),
            window,
            cx,
        );
        lead.focus_handle(cx).focus(window, cx);
        self.lead = Some(lead);
        cx.notify();
    }

    fn render_header(&self, cards: &[Card], cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let waiting = cards
            .iter()
            .filter(|card| card.agents.needs_input > 0)
            .count();
        let scanned = match (&self.scanning, self.scanned_at) {
            (Some(_), _) => "Checking…".to_string(),
            (None, Some(at)) => format!("Checked {}", edited_ago(at.elapsed())),
            (None, None) => String::new(),
        };

        h_flex()
            .px_3()
            .py_2()
            .gap_3()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(Label::new("Board").weight(gpui::FontWeight::SEMIBOLD))
            .child(
                Label::new(format!(
                    "{} worktrees{}",
                    cards.len(),
                    match waiting {
                        0 => String::new(),
                        1 => " · 1 waiting on you".to_string(),
                        waiting => format!(" · {waiting} waiting on you"),
                    }
                ))
                .size(LabelSize::Small)
                .color(if waiting > 0 {
                    Color::Warning
                } else {
                    Color::Muted
                }),
            )
            .child(div().flex_1())
            .child(
                Label::new(scanned)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Button::new("board-refresh", "Refresh")
                    .label_size(LabelSize::Small)
                    .disabled(self.scanning.is_some())
                    .tooltip(Tooltip::text(
                        "Ask git and GitHub again about every worktree's commits and pull requests",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(if self.lead.is_some() {
                Button::new("board-close-lead", "Close Lead")
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(
                        "End the lead agent's conversation. It keeps nothing the board doesn't show",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.lead = None;
                        cx.notify();
                    }))
            } else {
                Button::new("board-start-lead", "Start Lead Agent")
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(
                        "Start Claude Code beside the cards, to tell you what needs you and pass your answers on",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.start_lead(window, cx)))
            })
    }

    fn render_card(&self, index: usize, card: &Card, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let state = card.agents.state();
        let destination = Destination {
            workspace: card.worktree.workspace.clone(),
            key: card.worktree.key.clone(),
            root: card.worktree.root.clone(),
            name: card.worktree.name.clone(),
        };
        let since = card
            .worktree
            .last_active
            .and_then(|at| SystemTime::now().duration_since(at).ok());
        let agent_text = match state {
            Some(AgentState::NeedsInput) => "Waiting on you".to_string(),
            Some(AgentState::Working) => "Working".to_string(),
            Some(AgentState::Idle) => match since {
                Some(since) => format!("Idle, active {}", edited_ago(since)),
                None => "Idle".to_string(),
            },
            None => match since {
                Some(since) => format!("No agent, active {}", edited_ago(since)),
                None => "No agent".to_string(),
            },
        };
        let ahead: u32 = card.checkouts.iter().map(|checkout| checkout.ahead).sum();
        let changes: usize = card.checkouts.iter().map(|checkout| checkout.changes).sum();

        v_flex()
            .id(("board-card", index))
            .w_full()
            .px_3()
            .py_2()
            .gap_1()
            .rounded_md()
            .border_1()
            .cursor_pointer()
            .bg(colors.elevated_surface_background)
            .border_color(if state == Some(AgentState::NeedsInput) {
                status.warning_border
            } else {
                colors.border_variant
            })
            .hover(|style| style.border_color(colors.border))
            .on_click(
                cx.listener(move |this, _, window, cx| this.go_to(destination.clone(), window, cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        ui::Indicator::dot()
                            .color(state.map_or(Color::Disabled, agent_state_color)),
                    )
                    .child(
                        Label::new(card.label.clone())
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD)
                            .buffer_font(cx),
                    )
                    .child(
                        div().min_w_0().flex_1().child(
                            Label::new(card.title.clone())
                                .size(LabelSize::Small)
                                .truncate(),
                        ),
                    )
                    .when_some(card.issue_state.clone(), |this, state| {
                        this.child(
                            Label::new(state)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        Label::new(agent_text)
                            .size(LabelSize::XSmall)
                            .color(state.map_or(Color::Muted, agent_state_color)),
                    ),
            )
            .when_some(card.question.clone(), |this, question| {
                this.child(
                    div()
                        .px_1p5()
                        .py_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(status.warning_border)
                        .bg(status.warning_background)
                        .child(
                            Label::new(question)
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        ),
                )
            })
            .when(card.question.is_none(), |this| {
                this.when_some(card.recap.clone(), |this, recap| {
                    this.child(
                        Label::new(recap)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
            })
            .child(
                h_flex()
                    .gap_3()
                    .flex_wrap()
                    .child(
                        Label::new(match (ahead, changes) {
                            (0, 0) => "No commits or changes".to_string(),
                            (ahead, 0) => format!("{ahead} commits"),
                            (0, changes) => format!("{changes} changes"),
                            (ahead, changes) => format!("{ahead} commits · {changes} changes"),
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                    .children(card.checkouts.iter().filter_map(|checkout| {
                        let state = checkout.pull_request?;
                        Some(
                            Label::new(format!("{} PR {}", checkout.name, state.label()))
                                .size(LabelSize::XSmall)
                                .color(pull_request_color(state)),
                        )
                    }))
                    .children(card.hints.iter().map(|hint| {
                        Label::new(hint.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Warning)
                    })),
            )
            .into_any_element()
    }
}

/// Where clicking a card goes: the worktree's workspace when the window has it
/// open, or else what opening it takes.
#[derive(Clone)]
struct Destination {
    workspace: Option<gpui::Entity<Workspace>>,
    key: ProjectGroupKey,
    root: Option<PathBuf>,
    name: SharedString,
}

/// What the board shows of one worktree.
struct Card {
    worktree: WorktreeRow,
    agents: AgentSummary,
    label: SharedString,
    title: SharedString,
    issue_state: Option<SharedString>,
    question: Option<SharedString>,
    recap: Option<SharedString>,
    checkouts: Vec<BoardCheckout>,
    hints: Vec<SharedString>,
}

impl BoardView {
    /// The project's main checkout, which the board lives in.
    pub fn main_root(&self) -> &Path {
        &self.main_root
    }

    /// When git and GitHub were last asked, `None` before the first scan ends.
    pub fn scanned_at(&self) -> Option<Instant> {
        self.scanned_at
    }

    pub fn is_scanning(&self) -> bool {
        self.scanning.is_some()
    }

    /// The worktrees in the order the board shows them.
    pub fn entries(&self, cx: &App) -> Vec<BoardEntry> {
        self.cards(cx)
            .into_iter()
            .map(|card| BoardEntry {
                root: card.worktree.root,
                name: card.worktree.name,
                issue: card
                    .worktree
                    .issue
                    .as_ref()
                    .map(|issue| issue.identifier.clone()),
                title: card.title,
                issue_state: card.issue_state,
                agents: card.agents,
                last_active: card.worktree.last_active,
                question: card.question,
                recap: card.recap,
                checkouts: card.checkouts,
                hints: card.hints,
            })
            .collect()
    }

    /// The cards, those waiting on you first, then those at work, then the
    /// rest by when their agent was last active.
    fn cards(&self, cx: &App) -> Vec<Card> {
        let tracker = AgentTracker::try_global(cx);
        let agents = tracker
            .as_ref()
            .map(|tracker| tracker.read(cx).agents())
            .unwrap_or_default();
        let mut cards: Vec<Card> = self
            .worktrees(cx)
            .into_iter()
            .map(|worktree| {
                let root = worktree.root.clone();
                let here: Vec<_> = agents
                    .iter()
                    .filter(|agent| {
                        root.as_deref()
                            .zip(agent.directory)
                            .is_some_and(|(root, directory)| directory.starts_with(root))
                    })
                    .collect();
                let question = here.iter().find_map(|agent| agent.question.clone());
                let recap = here
                    .iter()
                    .filter_map(|agent| Some((agent.state, agent.activity?.recap.clone()?)))
                    .max_by_key(|(state, _)| *state)
                    .map(|(_, recap)| recap);
                let checkouts = root
                    .as_ref()
                    .and_then(|root| self.checkouts.get(root))
                    .cloned()
                    .unwrap_or_default();
                let issue_done = worktree.issue.as_ref().is_some_and(|issue| {
                    matches!(issue.state.kind, StateType::Completed | StateType::Canceled)
                });
                let hints = hints(&worktree.agents, &checkouts, issue_done);
                Card {
                    agents: worktree.agents,
                    label: worktree
                        .issue
                        .as_ref()
                        .map(|issue| issue.identifier.clone())
                        .unwrap_or_else(|| worktree.name.clone()),
                    title: worktree
                        .title
                        .clone()
                        .or_else(|| worktree.issue.as_ref().map(|issue| issue.title.clone()))
                        .or_else(|| worktree.branch.clone())
                        .unwrap_or_default(),
                    issue_state: worktree
                        .issue
                        .as_ref()
                        .map(|issue| issue.state.name.clone()),
                    question,
                    recap,
                    checkouts,
                    hints,
                    worktree,
                }
            })
            .collect();
        cards.sort_by(|a, b| {
            b.agents
                .state()
                .filter(|state| *state != AgentState::Idle)
                .cmp(&a.agents.state().filter(|state| *state != AgentState::Idle))
                .then_with(|| b.worktree.last_active.cmp(&a.worktree.last_active))
        });
        cards
    }
}

/// What a worktree's work needs next, in words a card can show.
fn hints(
    agents: &AgentSummary,
    checkouts: &[BoardCheckout],
    issue_done: bool,
) -> Vec<SharedString> {
    let mut hints = Vec::new();
    if agents.total() > 1 {
        hints.push(format!("{} agents here", agents.total()).into());
    }
    let pull_requests: Vec<PullRequestState> = checkouts
        .iter()
        .filter_map(|checkout| checkout.pull_request)
        .collect();
    let open = pull_requests
        .iter()
        .any(|state| matches!(state, PullRequestState::Open | PullRequestState::Draft));
    let ahead: u32 = checkouts.iter().map(|checkout| checkout.ahead).sum();
    let changes: usize = checkouts.iter().map(|checkout| checkout.changes).sum();

    if pull_requests.contains(&PullRequestState::Merged) && !open {
        hints.push(match changes {
            0 => "Merged, ready to delete".into(),
            changes => format!("Merged, but {changes} uncommitted changes").into(),
        });
    } else if issue_done && !open {
        hints.push("Issue is done".into());
    } else if pull_requests.is_empty() {
        if ahead > 0 {
            hints.push("No pull request yet".into());
        }
    } else {
        for checkout in checkouts {
            if checkout.ahead > 0 && checkout.pull_request.is_none() {
                hints.push(format!("{} has commits but no pull request", checkout.name).into());
            }
        }
    }
    hints
}

/// Every worktree's checkouts: its own, named after the project, and each
/// nested repository's, named after its folder. `gh` is asked once for each
/// repository, which every worktree's checkout of it shares.
async fn scan(
    fs: Arc<dyn Fs>,
    main_root: PathBuf,
    project_name: SharedString,
    roots: Vec<PathBuf>,
) -> HashMap<PathBuf, Vec<BoardCheckout>> {
    let Some(git) = which::which("git").ok() else {
        return HashMap::default();
    };

    // (worktree root, [(name, checkout path, the repository to ask gh in)])
    let mut found: Vec<(PathBuf, Vec<(SharedString, PathBuf, PathBuf)>)> = Vec::new();
    for root in roots {
        let mut checkouts = vec![(project_name.clone(), root.clone(), main_root.clone())];
        for nested in nested_worktrees(fs.clone(), root.clone()).await.linked {
            let Some(repository) = nested.common_dir.parent() else {
                continue;
            };
            let name = nested
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            checkouts.push((name.into(), nested.path.clone(), repository.to_path_buf()));
        }
        found.push((root, checkouts));
    }

    let mut repositories: Vec<PathBuf> = found
        .iter()
        .flat_map(|(_, checkouts)| {
            checkouts
                .iter()
                .map(|(_, _, repository)| repository.clone())
        })
        .collect();
    repositories.sort();
    repositories.dedup();
    let pull_requests: HashMap<PathBuf, HashMap<SharedString, PullRequestState>> =
        futures::future::join_all(repositories.into_iter().map(|repository| async move {
            let found = pull_requests_by_branch(repository.clone()).await;
            (repository, found)
        }))
        .await
        .into_iter()
        .collect();

    futures::future::join_all(found.into_iter().map(|(root, checkouts)| {
        let git = git.clone();
        let pull_requests = &pull_requests;
        async move {
            let mut states = Vec::new();
            for (name, path, repository) in checkouts {
                let (branch, ahead, changes) = checkout_state(&git, &path).await;
                let pull_request = branch.and_then(|branch| {
                    pull_requests
                        .get(&repository)?
                        .get(branch.as_str())
                        .copied()
                });
                states.push(BoardCheckout {
                    name,
                    ahead,
                    changes,
                    pull_request,
                });
            }
            (root, states)
        }
    }))
    .await
    .into_iter()
    .collect()
}

/// A checkout's branch, its commits that the default branch does not have,
/// and how many entries `git status` lists.
async fn checkout_state(git: &Path, directory: &Path) -> (Option<String>, u32, usize) {
    let run = |args: Vec<String>| {
        let mut command = util::command::new_command(git);
        command.current_dir(directory).kill_on_drop(true).args(args);
        async move {
            let output = command.output().await.log_err()?;
            output.status.success().then_some(output.stdout)
        }
    };
    let text = |stdout: Vec<u8>| {
        String::from_utf8(stdout)
            .ok()
            .map(|text| text.trim().to_string())
    };

    let branch = run(vec!["symbolic-ref".into(), "--short".into(), "HEAD".into()])
        .await
        .and_then(text)
        .filter(|branch| !branch.is_empty());
    let base = run(vec![
        "symbolic-ref".into(),
        "--short".into(),
        "refs/remotes/origin/HEAD".into(),
    ])
    .await
    .and_then(text)
    .unwrap_or_else(|| "origin/main".to_string());
    let ahead = run(vec![
        "rev-list".into(),
        "--count".into(),
        format!("{base}..HEAD"),
    ])
    .await
    .and_then(text)
    .and_then(|count| count.parse().ok())
    .unwrap_or(0);
    // Without optional locks, for the same reason as the panel's own status:
    // the index lock can make an agent's git command here fail.
    let status = run(vec![
        "--no-optional-locks".into(),
        "status".into(),
        "--porcelain=v1".into(),
        "--branch".into(),
        "-z".into(),
        "--untracked-files=normal".into(),
    ])
    .await
    .unwrap_or_default();
    (branch, ahead, parse_status(&status).paths.len())
}

impl Render for BoardView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cards = self.cards(cx);
        // A worktree made since the last scan has nothing to show until one
        // looks at it.
        let unscanned = cards.iter().any(|card| {
            card.worktree
                .root
                .as_ref()
                .is_some_and(|root| !self.checkouts.contains_key(root))
        });
        if unscanned && self.scanned_at.is_some() && self.scanning.is_none() {
            self.refresh(cx);
        }
        let colors = cx.theme().colors();
        let background = colors.editor_background;
        let border = colors.border_variant;

        v_flex()
            .key_context("Board")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(background)
            .child(self.render_header(&cards, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(self.render_cards(&cards, cx))
                    .children(self.lead.clone().map(|lead| {
                        v_flex()
                            .h_full()
                            .w(px(LEAD_WIDTH))
                            .flex_none()
                            .border_l_1()
                            .border_color(border)
                            .child(lead)
                    })),
            )
    }
}

impl BoardView {
    fn render_cards(&self, cards: &[Card], cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("board-cards")
            .h_full()
            .flex_1()
            .min_w_0()
            .p_3()
            .gap_2()
            .overflow_y_scroll()
            .when(cards.is_empty(), |this| {
                this.child(Label::new("No worktrees besides this one yet.").color(Color::Muted))
            })
            .children(
                cards
                    .iter()
                    .enumerate()
                    .map(|(index, card)| self.render_card(index, card, cx)),
            )
    }
}

impl Focusable for BoardView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for BoardView {}

impl Item for BoardView {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Board".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout(name: &str, ahead: u32, pull_request: Option<PullRequestState>) -> BoardCheckout {
        BoardCheckout {
            name: name.into(),
            ahead,
            changes: 0,
            pull_request,
        }
    }

    fn agents(idle: usize) -> AgentSummary {
        AgentSummary {
            idle,
            ..AgentSummary::default()
        }
    }

    #[test]
    fn work_with_no_pull_request_anywhere_gets_one_hint() {
        let checkouts = [checkout("workspace", 2, None), checkout("service", 5, None)];
        assert_eq!(
            hints(&agents(1), &checkouts, false),
            vec!["No pull request yet"]
        );
        let nothing_yet = [checkout("workspace", 0, None)];
        assert!(hints(&agents(1), &nothing_yet, false).is_empty());
    }

    #[test]
    fn a_repository_left_out_of_the_pull_requests_is_named() {
        let checkouts = [
            checkout("workspace", 1, Some(PullRequestState::Open)),
            checkout("service", 3, None),
            checkout("app", 0, None),
        ];
        assert_eq!(
            hints(&agents(2), &checkouts, false),
            vec!["2 agents here", "service has commits but no pull request"]
        );
    }

    #[test]
    fn merged_work_is_ready_to_delete_unless_something_is_uncommitted() {
        let mut checkouts = vec![
            checkout("workspace", 1, Some(PullRequestState::Merged)),
            checkout("service", 4, Some(PullRequestState::Merged)),
        ];
        assert_eq!(
            hints(&agents(0), &checkouts, true),
            vec!["Merged, ready to delete"]
        );
        checkouts[1].changes = 3;
        assert_eq!(
            hints(&agents(0), &checkouts, true),
            vec!["Merged, but 3 uncommitted changes"]
        );
    }

    #[test]
    fn an_open_pull_request_keeps_a_done_issue_from_looking_finished() {
        let checkouts = [
            checkout("workspace", 1, Some(PullRequestState::Merged)),
            checkout("service", 4, Some(PullRequestState::Open)),
        ];
        assert!(hints(&agents(0), &checkouts, true).is_empty());
    }
}

impl WorktreePanel {
    /// Makes the worktree at `root` the one the window shows, opening it when
    /// the window does not have it open.
    pub fn show_worktree(
        &mut self,
        root: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let worktree = self
            .tree(cx)
            .into_iter()
            .flat_map(|row| row.worktrees)
            .find(|worktree| worktree.root.as_deref() == Some(root))
            .with_context(|| format!("{} is not a worktree Bench lists", root.display()))?;
        match worktree.workspace {
            Some(workspace) => self.activate(workspace, window, cx),
            None => self.open_worktree(
                worktree.key,
                root.to_path_buf(),
                worktree.name,
                None,
                window,
                cx,
            ),
        }
        Ok(())
    }
}
