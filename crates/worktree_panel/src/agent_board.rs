//! Every agent in one panel, grouped by whether it needs you, is working or
//! is idle, with what each is doing — its phase of the work, its current tool
//! call, its todos — on its card. A card opens the agent's detail as a tab in
//! the worktree the agent is in.
//!
//! The worktree panel answers "which of my worktrees has an agent, and does it
//! need me". The board answers the next question — what is each of them
//! actually doing. The phases are read from Claude Code's hooks; see
//! [`agent_tracker::activity`]. The worktree facts on each card (its issue,
//! pull request and changes) are the worktree panel's own, so the board asks
//! git and GitHub nothing the panel has not already.
//!
//! It is a dock panel rather than a tab because a tab belongs to one
//! worktree's workspace: opened from several worktrees it was several tabs,
//! each one the same board.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use agent_tracker::{
    Activity, AgentState, AgentTracker, AgentsChanged, ClaudeModel, Phase, PhaseSource, TodoStatus,
    TranscriptStats, agent_state_color,
};
use git_ui_core::pull_request_color::pull_request_color;
use github_cli::PullRequestState;
use gpui::{
    Action, App, AsyncWindowContext, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Hsla,
    Pixels, SharedString, Subscription, Task, WeakEntity, Window, actions, hsla, prelude::*, px,
    relative,
};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::item::{Item, ItemEvent};
use workspace::{MultiWorkspace, Pane, Workspace};
use worktree_metadata::hue_color;

use crate::{WorktreeListing, WorktreePanel};

actions!(
    agent_board,
    [
        /// Toggles focus on the agent board: every agent, grouped by whether
        /// it needs you, is working or is idle.
        ToggleFocus,
    ]
);

/// Shows an agent's detail in the worktree it is dispatched in: what a card
/// asks a worktree it had to open to do once it is showing.
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = agent_board, no_json, no_register)]
struct OpenAgentDetail {
    agent: SharedString,
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<AgentBoardPanel>(window, cx);
        });
        workspace.register_action(|workspace, action: &OpenAgentDetail, window, cx| {
            let Some(multi_workspace) = workspace.multi_workspace().and_then(WeakEntity::upgrade)
            else {
                return;
            };
            let here = cx.entity().downgrade();
            let agent = action.agent.clone();
            // After this update has let go of the workspace, which opening
            // the detail updates.
            window.defer(cx, move |window, cx| {
                open_detail(multi_workspace, here, agent, None, None, window, cx);
            });
        });
    })
    .detach();
}

const DETAIL_WIDTH: f32 = 760.;

/// The groups of the board, in the order they are shown.
const GROUPS: [AgentState; 3] = [
    AgentState::NeedsInput,
    AgentState::Working,
    AgentState::Idle,
];

pub struct AgentBoardPanel {
    workspace: WeakEntity<Workspace>,
    multi_workspace: WeakEntity<MultiWorkspace>,
    /// Whether this is the panel its dock is showing; see [`Self::new`].
    active: bool,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl AgentBoardPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            let handle = workspace.clone();
            workspace.update_in(cx, |workspace, _window, cx| {
                let multi_workspace = workspace
                    .multi_workspace()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("the agent board needs a multi workspace"))?;
                anyhow::Ok(cx.new(|cx| Self::new(handle, multi_workspace, cx)))
            })?
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        multi_workspace: WeakEntity<MultiWorkspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        // Every worktree has its own copy of this panel, and the tracker
        // notifies whenever what an agent is doing moves on, so only the copy
        // that is showing redraws for that. A change of state is redrawn
        // everywhere, because the dock's button counts the agents that need
        // you. The tracker is a global entity outside the dock and pane tree,
        // so neither closes a notify ring.
        let mut subscriptions = Vec::new();
        if let Some(tracker) = AgentTracker::try_global(cx) {
            subscriptions.push(cx.subscribe(&tracker, |_, _, _: &AgentsChanged, cx| cx.notify()));
            subscriptions.push(cx.observe(&tracker, |this, _, cx| {
                if this.active {
                    cx.notify();
                }
            }));
        }
        Self {
            workspace,
            multi_workspace,
            active: false,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    /// Whether another worktree's left dock is showing this panel; see the
    /// worktree panel's `starts_open`.
    fn showing_in_another_worktree(&self, cx: &App) -> bool {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return false;
        };
        let own = self.workspace.entity_id();
        multi_workspace
            .read(cx)
            .workspaces()
            .filter(|workspace| workspace.entity_id() != own)
            .any(|workspace| {
                let dock = workspace.read(cx).dock_at_position(DockPosition::Left);
                let dock = dock.read(cx);
                dock.is_open()
                    && dock
                        .active_panel()
                        .is_some_and(|panel| panel.persistent_name() == Self::persistent_name())
            })
    }

    fn open_detail(
        &mut self,
        agent: SharedString,
        directory: Option<PathBuf>,
        terminal: Option<EntityId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let fallback = self.workspace.clone();
        // After this update has let go of the panel: activating a workspace
        // reads every panel in every dock, this one included.
        window.defer(cx, move |window, cx| {
            open_detail(
                multi_workspace,
                fallback,
                agent,
                directory.as_deref(),
                terminal,
                window,
                cx,
            );
        });
    }

    fn render_summary(&self, cards: &[Card], cx: &mut Context<Self>) -> AnyElement {
        let used: u64 = cards
            .iter()
            .filter_map(|card| card.stats.as_ref())
            .map(|stats| stats.input_tokens + stats.cache_read_tokens + stats.cache_write_tokens)
            .sum();
        h_flex()
            .px_2()
            .py_1p5()
            .gap_1p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new("Agents").size(LabelSize::Small))
            .child(
                Label::new(cards.len().to_string())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .when(used > 0, |this| {
                this.child(
                    Label::new(format!("{} tokens in", tokens(used)))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .into_any_element()
    }

    fn render_group(
        &self,
        state: AgentState,
        cards: &[&Card],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .gap_1p5()
            .child(
                h_flex()
                    .px_1()
                    .gap_1p5()
                    .child(ui::Indicator::dot().color(agent_state_color(state)))
                    .child(
                        Label::new(state_label(state))
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(cards.len().to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .children(
                cards
                    .iter()
                    .map(|card| self.render_card(card, cx))
                    .collect::<Vec<_>>(),
            )
            .into_any_element()
    }

    fn render_card(&self, card: &Card, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let activity = card.activity.as_ref();
        let phase = card.phase();
        let question = card.question.clone();
        let action = activity.and_then(|activity| activity.action.clone());
        let running = activity.and_then(|activity| activity.running_subagents().last().cloned());
        let todos = activity
            .filter(|activity| !activity.todos.is_empty())
            .map(|activity| {
                let current = activity
                    .todos
                    .iter()
                    .find(|todo| todo.status == TodoStatus::InProgress)
                    .map(|todo| todo.content.clone());
                (activity.todos_done(), activity.todos.len(), current)
            });
        let id = card.id.clone();
        let directory = card.directory.clone();
        let terminal = card.terminal;

        v_flex()
            .id(SharedString::from(format!("agent-card-{}", card.id)))
            .w_full()
            .p_2()
            .gap_1p5()
            .rounded_md()
            .border_1()
            .cursor_pointer()
            .bg(colors.elevated_surface_background)
            .border_color(if card.state == AgentState::NeedsInput {
                status.warning_border
            } else {
                colors.border_variant
            })
            .hover(|style| style.border_color(colors.border))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_detail(id.clone(), directory.clone(), terminal, window, cx);
            }))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1()
                            .items_start()
                            .when_some(card.issue.clone(), |this, issue| {
                                this.child(
                                    Label::new(issue)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .buffer_font(cx),
                                )
                            })
                            .child(
                                div().min_w_0().child(
                                    Label::new(card.title.clone())
                                        .size(LabelSize::Small)
                                        .weight(gpui::FontWeight::SEMIBOLD),
                                ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .when_some(card.hue, |this, hue| {
                                this.child(div().size_2().rounded_sm().bg(hue_color(hue)))
                            })
                            .child(
                                Label::new(card.worktree_name.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .buffer_font(cx)
                                    .truncate(),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .min_w_0()
                    .child(match phase.as_ref() {
                        Some((phase, source)) => source_label(*phase, source).into_any_element(),
                        None => Label::new("No phase yet")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .into_any_element(),
                    })
                    .child(div().flex_1())
                    .when_some(card.model.clone(), |this, model| {
                        this.child(model.badge(
                            SharedString::from(format!("agent-card-model-{}", card.id)),
                            card.state != AgentState::Idle,
                        ))
                    }),
            )
            .when_some(question.clone(), |this, question| {
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
            .when(question.is_none(), |this| {
                this.when_some(action, |this, action| {
                    this.child(
                        div()
                            .px_1p5()
                            .py_1()
                            .rounded_sm()
                            .bg(colors.surface_background)
                            .child(
                                Label::new(action)
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .truncate(),
                            ),
                    )
                })
            })
            .when_some(running, |this, subagent| {
                this.child(
                    Label::new(format!(
                        "↳ subagent {} · {}",
                        subagent.kind,
                        age(subagent.started, SystemTime::now())
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate(),
                )
            })
            .when_some(todos, |this, (done, total, current)| {
                this.child(
                    v_flex()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .child(
                                    Label::new("Todos")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(progress_bar(
                                    done as f32 / total.max(1) as f32,
                                    colors.text_accent,
                                    colors.border_variant,
                                ))
                                .child(
                                    Label::new(format!("{done}/{total}")).size(LabelSize::XSmall),
                                ),
                        )
                        .when_some(current, |this, current| {
                            this.child(
                                Label::new(format!("▸ {current}"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        }),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Label::new(match card.changes {
                            0 => "No changes".to_owned(),
                            1 => "1 change".to_owned(),
                            changes => format!("{changes} changes"),
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                    .when_some(card.ahead.filter(|ahead| *ahead > 0), |this, ahead| {
                        this.child(
                            Label::new(format!("↑{ahead}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .when_some(card.pull_request, |this, state| {
                        this.child(
                            Label::new(format!("PR {}", state.label()))
                                .size(LabelSize::XSmall)
                                .color(pull_request_color(state)),
                        )
                    })
                    .child(div().flex_1())
                    .when_some(card.stats.as_ref(), |this, stats| {
                        this.child(context_meter(stats, cx))
                    }),
            )
            .into_any_element()
    }
}

impl Render for AgentBoardPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cards = cards(self.workspace.upgrade(), cx);
        let colors = cx.theme().colors();
        let border_variant = colors.border_variant;
        let panel_background = colors.panel_background;
        let hooks_reporting =
            AgentTracker::try_global(cx).is_some_and(|tracker| tracker.read(cx).has_hook_reports());

        let body = if cards.is_empty() {
            v_flex()
                .flex_1()
                .p_4()
                .items_center()
                .justify_center()
                .gap_1()
                .child(Label::new("No agents running"))
                .child(
                    Label::new("Start Claude Code in a terminal and it shows up here.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            let groups: Vec<AnyElement> = GROUPS
                .into_iter()
                .filter_map(|state| {
                    let in_group: Vec<&Card> =
                        cards.iter().filter(|card| card.state == state).collect();
                    (!in_group.is_empty()).then(|| self.render_group(state, &in_group, cx))
                })
                .collect();
            v_flex()
                .id("agent-board-groups")
                .flex_1()
                .min_h_0()
                .p_2()
                .gap_3()
                .overflow_y_scroll()
                .children(groups)
                .into_any_element()
        };

        v_flex()
            .key_context("AgentBoard")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(panel_background)
            .child(self.render_summary(&cards, cx))
            .when(!hooks_reporting, |this| {
                this.child(
                    v_flex()
                        .px_2()
                        .py_1p5()
                        .gap_1p5()
                        .border_b_1()
                        .border_color(border_variant)
                        .child(
                            Label::new(
                                "Working and waiting are guessed from terminal output. Claude \
                                 Code's hooks make them exact and say what an agent is asking, \
                                 where your organization allows user hooks.",
                            )
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        )
                        .child(
                            Button::new("agent-board-install-hooks", "Install Hooks")
                                .label_size(LabelSize::Small)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(agent_tracker::InstallClaudeHooks),
                                        cx,
                                    );
                                }),
                        ),
                )
            })
            .child(body)
    }
}

impl Focusable for AgentBoardPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for AgentBoardPanel {}

impl Panel for AgentBoardPanel {
    fn persistent_name() -> &'static str {
        "AgentBoardPanel"
    }

    fn panel_key() -> &'static str {
        "AgentBoardPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(300.)
    }

    /// Out in one worktree is out in the next; see the worktree panel's
    /// `starts_open` for why a dock's open state has to follow the window
    /// rather than the worktree in Bench.
    fn starts_open(&self, _window: &Window, cx: &App) -> bool {
        self.showing_in_another_worktree(cx)
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        if active {
            cx.notify();
        }
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ZedAgent)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Agent Board")
    }

    fn icon_label(&self, _window: &Window, cx: &App) -> Option<String> {
        let needs_input = AgentTracker::try_global(cx)?
            .read(cx)
            .agents()
            .into_iter()
            .filter(|agent| agent.state == AgentState::NeedsInput)
            .count();
        (needs_input > 0).then(|| needs_input.to_string())
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    /// Right after the worktree panel's `0`, so its button sits next to that
    /// one's. Bench docks the project panel (`1`) on the right, which leaves
    /// the worktree panel alone in the left dock; past every other panel's
    /// number keeps this one unique wherever those are moved.
    fn activation_priority(&self) -> u32 {
        11
    }
}

/// Shows an agent's detail in the worktree it is running in, or goes to the
/// detail where it is already open, so an agent never has more than one.
///
/// The worktree is the one the agent's directory is in rather than the one
/// holding its terminal: a terminal opened in one worktree can run an agent in
/// another, and an agent left running after its terminal was closed has none.
/// A worktree of the directory that is not open is opened first, and the
/// detail shown once it is. The terminal decides only when no worktree holds
/// the directory.
fn open_detail(
    multi_workspace: Entity<MultiWorkspace>,
    fallback: WeakEntity<Workspace>,
    agent: SharedString,
    directory: Option<&Path>,
    terminal: Option<EntityId>,
    window: &mut Window,
    cx: &mut App,
) {
    let workspaces: Vec<Entity<Workspace>> =
        multi_workspace.read(cx).workspaces().cloned().collect();
    let existing = workspaces.iter().find_map(|workspace| {
        workspace
            .read(cx)
            .items_of_type::<AgentDetail>(cx)
            .find(|detail| detail.read(cx).agent == agent)
            .map(|detail| (workspace.clone(), detail))
    });
    let target = existing
        .as_ref()
        .map(|(workspace, _)| workspace.clone())
        .or_else(|| {
            let directory = directory?;
            // The longest root, so an agent in a worktree nested in its
            // checkout goes to the worktree.
            workspaces
                .iter()
                .filter_map(|workspace| {
                    let root = workspace
                        .read(cx)
                        .root_paths(cx)
                        .into_iter()
                        .filter(|root| directory.starts_with(root))
                        .max_by_key(|root| root.as_os_str().len())?;
                    Some((root.as_os_str().len(), workspace))
                })
                .max_by_key(|(length, _)| *length)
                .map(|(_, workspace)| workspace.clone())
        });
    if target.is_none()
        && let Some(directory) = directory
        && let Some(panel) = fallback
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<WorktreePanel>(cx))
    {
        let opening = panel.update(cx, |panel, cx| {
            panel.open_worktree_containing(
                directory,
                Box::new(OpenAgentDetail {
                    agent: agent.clone(),
                }),
                window,
                cx,
            )
        });
        if opening {
            return;
        }
    }
    let target = target
        .or_else(|| {
            let terminal = terminal?;
            workspaces
                .iter()
                .find(|workspace| terminal_view_in(workspace.read(cx), terminal, cx).is_some())
                .cloned()
        })
        .or_else(|| fallback.upgrade());
    let Some(workspace) = target else {
        return;
    };
    // Worked out here, where nothing is leased; see `AgentDetail::card`.
    let card = existing
        .is_none()
        .then(|| {
            cards(Some(workspace.clone()), cx)
                .into_iter()
                .find(|card| card.id == agent)
        })
        .flatten();

    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.activate(workspace.clone(), None, window, cx);
    });
    let handle = workspace.downgrade();
    workspace.update(cx, |workspace, cx| match existing {
        Some((_, detail)) => {
            workspace.activate_item(&detail, true, true, window, cx);
        }
        None => {
            let multi_workspace = workspace.multi_workspace().cloned();
            let detail = cx.new(|cx| AgentDetail::new(agent, card, handle, multi_workspace, cx));
            workspace.add_item_to_active_pane(Box::new(detail), None, true, window, cx);
        }
    });
}

/// The board's cards, worked out afresh from the tracker and the worktree
/// panel: both already hold everything, and keeping a copy here would only be
/// a copy to keep in step.
fn cards(workspace: Option<Entity<Workspace>>, cx: &App) -> Vec<Card> {
    let Some(tracker) = AgentTracker::try_global(cx) else {
        return Vec::new();
    };
    let worktrees: Vec<WorktreeListing> = workspace
        .and_then(|workspace| workspace.read(cx).panel::<WorktreePanel>(cx))
        .map(|panel| {
            panel
                .read(cx)
                .projects(cx)
                .into_iter()
                .flat_map(|project| project.worktrees)
                .collect()
        })
        .unwrap_or_default();

    let mut cards: Vec<Card> = tracker
        .read(cx)
        .agents()
        .into_iter()
        .map(|agent| {
            // The worktree with the longest root the agent is inside, so an
            // agent in a worktree nested in its checkout belongs to the
            // worktree.
            let worktree = agent.directory.and_then(|directory| {
                worktrees
                    .iter()
                    .filter(|worktree| {
                        worktree
                            .root
                            .as_ref()
                            .is_some_and(|root| directory.starts_with(root))
                    })
                    .max_by_key(|worktree| {
                        worktree
                            .root
                            .as_ref()
                            .map_or(0, |root| root.as_os_str().len())
                    })
            });
            let directory_name: SharedString = agent
                .directory
                .and_then(|directory| directory.file_name())
                .map(|name| name.to_string_lossy().into_owned().into())
                .unwrap_or_else(|| agent.command.clone());
            let title = worktree
                .and_then(|worktree| worktree.title.clone())
                .or_else(|| agent.activity.and_then(|activity| activity.prompt.clone()))
                .unwrap_or_else(|| directory_name.clone());
            Card {
                id: agent.id,
                directory: agent.directory.map(Path::to_path_buf),
                terminal: agent.terminal,
                state: agent.state,
                model: agent.model,
                title,
                worktree_name: worktree
                    .map(|worktree| worktree.name.clone())
                    .unwrap_or(directory_name),
                hue: worktree.and_then(|worktree| worktree.hue),
                issue: worktree.and_then(|worktree| worktree.issue.clone()),
                pull_request: worktree.and_then(|worktree| worktree.pull_request),
                changes: worktree.map_or(0, |worktree| worktree.changes),
                ahead: worktree.and_then(|worktree| worktree.ahead),
                question: agent.question,
                activity: agent.activity.cloned(),
                stats: agent.stats.cloned(),
            }
        })
        .collect();
    cards.sort_by(|a, b| {
        urgency(a.state)
            .cmp(&urgency(b.state))
            .then_with(|| a.worktree_name.cmp(&b.worktree_name))
            .then_with(|| a.id.cmp(&b.id))
    });
    cards
}

/// Brings the terminal an agent is running in forward, switching to the
/// worktree it is in first when that is not the one showing.
fn focus_terminal(
    multi_workspace: Option<&WeakEntity<MultiWorkspace>>,
    terminal: EntityId,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(multi_workspace) = multi_workspace.and_then(WeakEntity::upgrade) else {
        return;
    };
    let found = multi_workspace.read(cx).workspaces().find_map(|workspace| {
        terminal_view_in(workspace.read(cx), terminal, cx).map(|found| (workspace.clone(), found))
    });
    let Some((workspace, (index, pane))) = found else {
        return;
    };
    // After the caller's update has let go: activating a workspace re-renders
    // the window, the caller's pane or dock included.
    window.defer(cx, move |window, cx| {
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
        });
        workspace.update(cx, |workspace, cx| {
            reveal(workspace, &pane, index, window, cx);
        });
    });
}

/// One agent's detail: what it was asked, its phases, todos, subagents and
/// recent tool calls.
pub struct AgentDetail {
    /// The agent, by [`agent_tracker::AgentEntry::id`].
    agent: SharedString,
    /// The agent as last worked out, kept rather than worked out when asked
    /// for: working it out reads the workspace, and the workspace asks for
    /// the tab's text and tooltip from inside its own update — the status
    /// bar's file name does, as the tab comes forward — which made reading it
    /// there a double lease and a crash.
    card: Option<Card>,
    workspace: WeakEntity<Workspace>,
    multi_workspace: Option<WeakEntity<MultiWorkspace>>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl AgentDetail {
    fn new(
        agent: SharedString,
        card: Option<Card>,
        workspace: WeakEntity<Workspace>,
        multi_workspace: Option<WeakEntity<MultiWorkspace>>,
        cx: &mut Context<Self>,
    ) -> Self {
        // The tracker notifies when what an agent is doing moves on, which
        // is most of what the detail shows, and an observer runs once every
        // update has let go, so the workspace is free to read. The tracker is
        // a global entity outside the dock and pane tree, so this closes no
        // notify ring.
        let mut subscriptions = Vec::new();
        if let Some(tracker) = AgentTracker::try_global(cx) {
            subscriptions.push(cx.observe(&tracker, |this, _, cx| this.refresh(cx)));
        }
        Self {
            agent,
            card,
            workspace,
            multi_workspace,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let card = cards(self.workspace.upgrade(), cx)
            .into_iter()
            .find(|card| card.id == self.agent);
        let Some(card) = card else {
            // Closed only for an agent it has shown: one it never found has
            // nothing to say it has gone.
            if self.card.is_some() {
                cx.emit(ItemEvent::CloseItem);
            }
            return;
        };
        let tab_changed = self.card.as_ref().is_none_or(|previous| {
            previous.worktree_name != card.worktree_name || previous.title != card.title
        });
        self.card = Some(card);
        if tab_changed {
            cx.emit(ItemEvent::UpdateTab);
        }
        cx.notify();
    }

    fn focus_terminal(&mut self, terminal: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        focus_terminal(self.multi_workspace.as_ref(), terminal, window, cx);
    }

    fn open_issue(
        &mut self,
        identifier: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace
            .update(cx, |workspace, cx| {
                linear::open_issue(workspace, identifier, window, cx);
            })
            .log_err();
    }

    fn render_detail(&self, card: &Card, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let now = SystemTime::now();
        let activity = card.activity.as_ref();
        let phase = card.phase();

        let mut sections: Vec<AnyElement> = Vec::new();
        let terminal = card.terminal;
        let issue = card.issue.clone();
        sections.push(
            section(None, colors.border_variant)
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Label::new(match &card.issue {
                                Some(issue) => format!("{issue} · {}", card.worktree_name),
                                None => card.worktree_name.to_string(),
                            })
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx),
                        )
                        .child(div().flex_1())
                        .when_some(card.model.clone(), |this, model| {
                            this.child(model.badge("agent-detail-model", true))
                        }),
                )
                .child(Label::new(card.title.clone()).weight(gpui::FontWeight::SEMIBOLD))
                .child(
                    h_flex()
                        .gap_1p5()
                        .flex_wrap()
                        .child(state_pill(card.state))
                        .when_some(phase.as_ref(), |this, (phase, source)| {
                            this.child(source_label(*phase, source))
                        }),
                )
                .when_some(
                    activity.and_then(|activity| activity.prompt.clone()),
                    |this, prompt| {
                        this.child(
                            div()
                                .pl_2()
                                .border_l_2()
                                .border_color(colors.border_variant)
                                .child(
                                    Label::new(prompt)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .buffer_font(cx),
                                ),
                        )
                    },
                )
                .child(
                    h_flex()
                        .gap_1()
                        .when_some(terminal, |this, terminal| {
                            this.child(
                                Button::new("agent-detail-focus", "Focus Terminal")
                                    .label_size(LabelSize::Small)
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.focus_terminal(terminal, window, cx);
                                    })),
                            )
                        })
                        .when_some(issue, |this, issue| {
                            this.child(
                                Button::new("agent-detail-issue", "Open Issue")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_issue(issue.clone(), window, cx);
                                    })),
                            )
                        }),
                )
                .into_any_element(),
        );

        if card.state == AgentState::NeedsInput {
            let question = card
                .question
                .clone()
                .unwrap_or_else(|| "It has stopped to ask something.".into());
            sections.push(
                section(Some("Needs You"), colors.border_variant)
                    .child(
                        v_flex()
                            .p_2()
                            .gap_1p5()
                            .rounded_md()
                            .border_1()
                            .border_color(status.warning_border)
                            .bg(status.warning_background)
                            .child(Label::new(question).size(LabelSize::Small))
                            .when_some(terminal, |this, terminal| {
                                this.child(
                                    Button::new("agent-detail-answer", "Answer in Terminal")
                                        .label_size(LabelSize::Small)
                                        .style(ButtonStyle::Filled)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.focus_terminal(terminal, window, cx);
                                        })),
                                )
                            }),
                    )
                    .into_any_element(),
            );
        }

        if let Some(activity) = activity.filter(|activity| !activity.timeline.is_empty()) {
            sections.push(render_timeline(activity, now, colors.border_variant).into_any_element());
        }

        if let Some(activity) = activity.filter(|activity| !activity.todos.is_empty()) {
            sections.push(
                section(Some("Todos"), colors.border_variant)
                    .children(activity.todos.iter().map(|todo| {
                        let (mark, color) = match todo.status {
                            TodoStatus::Completed => ("●", Color::Success),
                            TodoStatus::InProgress => ("◐", Color::Accent),
                            TodoStatus::Pending => ("○", Color::Muted),
                        };
                        h_flex()
                            .gap_1p5()
                            .items_start()
                            .child(Label::new(mark).size(LabelSize::XSmall).color(color))
                            .child(
                                Label::new(todo.content.clone())
                                    .size(LabelSize::Small)
                                    .color(if todo.status == TodoStatus::Completed {
                                        Color::Muted
                                    } else {
                                        Color::Default
                                    })
                                    .when(todo.status == TodoStatus::InProgress, |label| {
                                        label.weight(gpui::FontWeight::MEDIUM)
                                    }),
                            )
                    }))
                    .into_any_element(),
            );
        }

        if let Some(activity) = activity.filter(|activity| !activity.subagents.is_empty()) {
            sections.push(
                section(Some("Subagents"), colors.border_variant)
                    .children(activity.subagents.iter().rev().map(|subagent| {
                        let state = match subagent.finished {
                            Some(finished) => format!(
                                "done · {}",
                                duration(
                                    finished
                                        .duration_since(subagent.started)
                                        .unwrap_or_default()
                                )
                            ),
                            None => format!("running · {}", age(subagent.started, now)),
                        };
                        h_flex()
                            .gap_2()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        Label::new(format!("↳ {}", subagent.kind))
                                            .size(LabelSize::Small)
                                            .buffer_font(cx),
                                    )
                                    .when(!subagent.description.is_empty(), |this| {
                                        this.child(
                                            Label::new(subagent.description.clone())
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .truncate(),
                                        )
                                    }),
                            )
                            .child(
                                Label::new(state)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                    }))
                    .into_any_element(),
            );
        }

        if let Some(activity) = activity.filter(|activity| !activity.feed.is_empty()) {
            sections.push(
                section(Some("Activity"), colors.border_variant)
                    .children(activity.feed.iter().take(25).map(|entry| {
                        h_flex()
                            .gap_2()
                            .items_start()
                            .child(
                                div().w(px(36.)).flex_none().child(
                                    Label::new(age(entry.at, now))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .buffer_font(cx),
                                ),
                            )
                            .child(
                                div()
                                    .mt_1()
                                    .flex_none()
                                    .size_1p5()
                                    .rounded_full()
                                    .bg(entry.phase.map_or(colors.border, phase_color)),
                            )
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .flex_wrap()
                                    .child(
                                        Label::new(entry.tool.clone())
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .buffer_font(cx),
                                    )
                                    .child(
                                        Label::new(entry.detail.clone())
                                            .size(LabelSize::XSmall)
                                            .when(entry.marked, |label| {
                                                label.weight(gpui::FontWeight::SEMIBOLD)
                                            }),
                                    ),
                            )
                    }))
                    .into_any_element(),
            );
        }

        if let Some(stats) = card.stats.as_ref() {
            sections.push(
                section(Some("Session"), colors.border_variant)
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_x_4()
                            .gap_y_2()
                            .child(figure(
                                "Tokens in",
                                tokens(
                                    stats.input_tokens
                                        + stats.cache_read_tokens
                                        + stats.cache_write_tokens,
                                ),
                            ))
                            .child(figure("Tokens out", tokens(stats.output_tokens)))
                            .child(figure(
                                "Cache hits",
                                stats.cache_hit_rate().map_or("—".to_owned(), |rate| {
                                    format!("{:.0}%", rate * 100.)
                                }),
                            ))
                            .child(figure(
                                "Context",
                                format!("{:.0}%", stats.context_fraction() * 100.),
                            ))
                            .child(figure("Compactions", stats.compactions.to_string()))
                            .child(figure("Prompts", stats.prompts.to_string())),
                    )
                    .into_any_element(),
            );
        }

        if activity.is_none() {
            sections.push(
                section(None, colors.border_variant)
                    .child(
                        Label::new(
                            "No Claude Code transcript has been found for this agent yet, so \
                             there is no phase, prompt or activity to show.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }

        v_flex()
            .id("agent-detail")
            .size_full()
            .items_center()
            .overflow_y_scroll()
            .child(v_flex().w_full().max_w(px(DETAIL_WIDTH)).children(sections))
            .into_any_element()
    }
}

impl Render for AgentDetail {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.card.as_ref() {
            Some(card) => self.render_detail(card, cx),
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Label::new("This agent is no longer running.").color(Color::Muted))
                .into_any_element(),
        };
        v_flex()
            .key_context("AgentDetail")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(body)
    }
}

impl Focusable for AgentDetail {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for AgentDetail {}

impl Item for AgentDetail {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.card
            .as_ref()
            .map_or_else(|| "Agent".into(), |card| card.worktree_name.clone())
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        self.card.as_ref().map(|card| card.title.clone())
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ZedAgent))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}

/// One agent as the board and its detail draw it.
struct Card {
    id: SharedString,
    directory: Option<PathBuf>,
    terminal: Option<EntityId>,
    state: AgentState,
    model: Option<ClaudeModel>,
    title: SharedString,
    worktree_name: SharedString,
    hue: Option<u8>,
    issue: Option<SharedString>,
    pull_request: Option<PullRequestState>,
    changes: usize,
    ahead: Option<u32>,
    question: Option<SharedString>,
    activity: Option<Activity>,
    stats: Option<TranscriptStats>,
}

impl Card {
    fn phase(&self) -> Option<(Phase, PhaseSource)> {
        self.activity.as_ref()?.phase()
    }
}

/// The terminal view showing `terminal` in a workspace, wherever it is: the
/// terminal panel's own panes or the center.
fn terminal_view_in(
    workspace: &Workspace,
    terminal: EntityId,
    cx: &App,
) -> Option<(usize, Entity<Pane>)> {
    let in_panel = workspace
        .panel::<TerminalPanel>(cx)
        .map(|panel| panel.read(cx).terminal_views(cx))
        .unwrap_or_default()
        .into_iter();
    let in_center = workspace.panes().iter().flat_map(|pane| {
        pane.read(cx)
            .items()
            .enumerate()
            .filter_map(|(index, item)| {
                Some((index, pane.clone(), item.act_as::<TerminalView>(cx)?))
            })
            .collect::<Vec<_>>()
    });
    in_panel
        .chain(in_center)
        .find(|(_, _, view)| view.read(cx).terminal().entity_id() == terminal)
        .map(|(index, pane, _)| (index, pane))
}

/// Activates a terminal's tab, bringing the terminal dock forward when that
/// is where it lives.
fn reveal(
    workspace: &mut Workspace,
    pane: &Entity<Pane>,
    index: usize,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let in_dock = workspace.panel::<TerminalPanel>(cx).is_some_and(|panel| {
        panel
            .read(cx)
            .panes()
            .iter()
            .any(|panel_pane| panel_pane.entity_id() == pane.entity_id())
    });
    if in_dock {
        workspace.focus_panel::<TerminalPanel>(window, cx);
    }
    pane.update(cx, |pane, cx| {
        pane.activate_item(index, true, true, window, cx)
    });
}

/// Agents waiting on you first, then working, then idle.
fn urgency(state: AgentState) -> u8 {
    match state {
        AgentState::NeedsInput => 0,
        AgentState::Working => 1,
        AgentState::Idle => 2,
    }
}

/// A colour per phase, the same in every theme, for the same reason as the
/// model badge's: to be told apart at a glance.
fn phase_color(phase: Phase) -> Hsla {
    let hue = match phase {
        Phase::Planning => 0.72,
        Phase::Exploring => 0.53,
        Phase::Coding => 0.61,
        Phase::Testing => 0.1,
        Phase::Reviewing => 0.92,
        Phase::Shipping => 0.4,
        Phase::Debugging => 0.01,
    };
    hsla(hue, 0.6, 0.58, 1.)
}

fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::NeedsInput => "Needs You",
        AgentState::Working => "Working",
        AgentState::Idle => "Idle",
    }
}

fn state_pill(state: AgentState) -> impl IntoElement {
    let color = agent_state_color(state);
    h_flex()
        .gap_1()
        .child(ui::Indicator::dot().color(color))
        .child(
            Label::new(state_label(state))
                .size(LabelSize::XSmall)
                .color(color),
        )
}

/// What the agent's phase was read from: filled for something the session said,
/// hollow for a guess.
fn source_label(phase: Phase, source: &PhaseSource) -> impl IntoElement {
    let mark = if source.is_explicit() { "◆" } else { "◇" };
    h_flex()
        .gap_1()
        .min_w_0()
        .child(
            Label::new(mark)
                .size(LabelSize::XSmall)
                .color(Color::Custom(phase_color(phase))),
        )
        .child(
            Label::new(source.describe())
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .truncate(),
        )
}

fn progress_bar(fraction: f32, fill: Hsla, track: Hsla) -> impl IntoElement {
    div().flex_1().h_1().rounded_sm().bg(track).child(
        div()
            .h_full()
            .rounded_sm()
            .w(relative(fraction.clamp(0., 1.)))
            .bg(fill),
    )
}

fn context_meter(stats: &TranscriptStats, cx: &App) -> AnyElement {
    let fraction = stats.context_fraction();
    let colors = cx.theme().colors();
    let fill = if fraction >= 0.75 {
        cx.theme().status().warning
    } else {
        colors.text_muted
    };
    h_flex()
        .gap_1()
        .child(
            Label::new(format!("{:.0}% ctx", fraction * 100.))
                .size(LabelSize::XSmall)
                .color(if fraction >= 0.75 {
                    Color::Warning
                } else {
                    Color::Muted
                }),
        )
        .child(
            div()
                .w(px(40.))
                .child(progress_bar(fraction, fill, colors.border_variant)),
        )
        .into_any_element()
}

fn render_timeline(activity: &Activity, now: SystemTime, border: Hsla) -> impl IntoElement {
    let spans: Vec<(Phase, Duration)> = activity
        .timeline
        .iter()
        .enumerate()
        .map(|(index, span)| {
            let end = activity
                .timeline
                .get(index + 1)
                .map_or(now, |next| next.since);
            (
                span.phase,
                end.duration_since(span.since).unwrap_or_default(),
            )
        })
        .collect();
    let total: Duration = spans.iter().map(|(_, length)| *length).sum();
    let mut per_phase: Vec<(Phase, Duration)> = Vec::new();
    for (phase, length) in &spans {
        match per_phase.iter_mut().find(|(counted, _)| counted == phase) {
            Some((_, counted)) => *counted += *length,
            None => per_phase.push((*phase, *length)),
        }
    }
    let total_seconds = total.as_secs_f32().max(1.);

    section(Some("Phases"), border)
        .child(
            h_flex()
                .w_full()
                .h_3()
                .gap_px()
                .rounded_sm()
                .overflow_hidden()
                .children(spans.iter().map(|(phase, length)| {
                    div()
                        .h_full()
                        .min_w(px(3.))
                        .w(relative(length.as_secs_f32() / total_seconds))
                        .bg(phase_color(*phase))
                })),
        )
        .child(
            h_flex()
                .flex_wrap()
                .gap_x_3()
                .gap_y_1()
                .children(per_phase.into_iter().map(|(phase, length)| {
                    h_flex()
                        .gap_1()
                        .child(div().size_2().rounded_sm().bg(phase_color(phase)))
                        .child(
                            Label::new(format!("{} {}", phase.label(), duration(length)))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                })),
        )
}

fn section(title: Option<&'static str>, border: Hsla) -> gpui::Div {
    v_flex()
        .p_3()
        .gap_2()
        .border_b_1()
        .border_color(border)
        .when_some(title, |this, title| {
            this.child(
                Label::new(title)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
        })
}

fn figure(label: &'static str, value: String) -> impl IntoElement {
    v_flex()
        .min_w(px(90.))
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(Label::new(value).size(LabelSize::Small))
}

/// How long ago `since` was, as briefly as it can be said.
fn age(since: SystemTime, now: SystemTime) -> String {
    duration(now.duration_since(since).unwrap_or_default())
}

fn duration(length: Duration) -> String {
    let seconds = length.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// A token count as people say it: 950, 48k, 1.9M.
fn tokens(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{}k", count / 1_000),
        _ => format!("{:.1}M", count as f64 / 1_000_000.),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::sync::Arc;

    /// The workspace asks the active tab for its text and tooltip from inside
    /// its own update — the status bar's file name does, as the tab comes
    /// forward — so a detail that went back to the workspace for them was a
    /// double lease, and a crash on every click of a card.
    #[gpui::test]
    async fn a_detail_answers_for_its_tab_without_the_workspace(cx: &mut TestAppContext) {
        let (_fs, multi_workspace, workspaces, _panels, mut cx) =
            crate::tests::worktree_panels(cx, 2).await;
        cx.update(|_, cx| agent_tracker::init(cx));
        let workspace = workspaces[1].clone();

        cx.update(|window, cx| {
            open_detail(
                multi_workspace.clone(),
                workspace.downgrade(),
                "agent".into(),
                None,
                None,
                window,
                cx,
            );
        });
        workspace.update(&mut cx, |workspace, cx| {
            let item = workspace
                .active_item(cx)
                .expect("the detail is the active tab");
            assert!(item.downcast::<AgentDetail>().is_some());
            item.tab_content_text(0, cx);
            item.tab_tooltip_text(cx);
        });
    }

    /// Opened from one worktree, the detail goes to the worktree the agent's
    /// directory is in, and that worktree comes forward.
    #[gpui::test]
    async fn a_detail_opens_in_the_agents_worktree(cx: &mut TestAppContext) {
        let (_fs, multi_workspace, workspaces, _panels, mut cx) =
            crate::tests::worktree_panels(cx, 2).await;
        cx.update(|_, cx| agent_tracker::init(cx));

        cx.update(|window, cx| {
            open_detail(
                multi_workspace.clone(),
                workspaces[0].downgrade(),
                "agent".into(),
                Some(Path::new("/repo-1/src")),
                None,
                window,
                cx,
            );
        });

        let count = |workspace: &Entity<Workspace>, cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace.items_of_type::<AgentDetail>(cx).count()
            })
        };
        assert_eq!(count(&workspaces[0], &mut cx), 0);
        assert_eq!(count(&workspaces[1], &mut cx), 1);
        multi_workspace.read_with(&mut cx, |multi_workspace, _| {
            assert_eq!(multi_workspace.workspace(), &workspaces[1]);
        });
    }

    /// An agent in a worktree the window does not have open opens that
    /// worktree, and its detail is shown there once it is.
    #[gpui::test]
    async fn a_detail_opens_the_agents_worktree_first(cx: &mut TestAppContext) {
        let (fs, multi_workspace, workspaces, _panels, mut cx) =
            crate::tests::worktree_panels(cx, 1).await;
        cx.update(|_, cx| {
            agent_tracker::init(cx);
            init(cx);
        });
        fs.insert_tree(
            "/outer",
            serde_json::json!({ ".git": {}, "file.txt": "hi" }),
        )
        .await;
        fs.add_linked_worktree_for_repo(
            Path::new("/outer/.git"),
            false,
            crate::tests::worktree("/wt/fix", "fix", false),
        )
        .await;
        fs.insert_file("/wt/fix/file.txt", b"hi".to_vec()).await;
        let outer_project = project::Project::test(fs.clone(), ["/outer".as_ref()], &mut cx).await;
        multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(outer_project, window, cx);
        });
        cx.run_until_parked();
        let elsewhere = workspaces[0].clone();
        multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
            multi_workspace.activate(elsewhere.clone(), None, window, cx);
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            open_detail(
                multi_workspace.clone(),
                elsewhere.downgrade(),
                "agent".into(),
                Some(Path::new("/wt/fix/src")),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let fix = multi_workspace.read_with(&mut cx, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        });
        fix.read_with(&mut cx, |workspace, cx| {
            assert_eq!(
                workspace.root_paths(cx),
                vec![Arc::<Path>::from(Path::new("/wt/fix"))]
            );
        });

        // `open_worktree` dispatches its follow-up on the worktree's first
        // frame, which a test window never draws on its own, so it is
        // dispatched here the way that does it.
        cx.update(|window, cx| {
            let focus_handle = fix.read(cx).focus_handle(cx);
            focus_handle.dispatch_action(
                &OpenAgentDetail {
                    agent: "agent".into(),
                },
                window,
                cx,
            );
        });
        cx.run_until_parked();

        fix.read_with(&mut cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<AgentDetail>(cx).count(), 1);
        });
        elsewhere.read_with(&mut cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<AgentDetail>(cx).count(), 0);
        });
    }

    #[test]
    fn durations_and_counts_read_the_way_people_say_them() {
        assert_eq!(duration(Duration::from_secs(12)), "12s");
        assert_eq!(duration(Duration::from_secs(4 * 60 + 5)), "4m");
        assert_eq!(duration(Duration::from_secs(2 * 3600 + 5 * 60)), "2h 5m");
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(48_200), "48k");
        assert_eq!(tokens(1_940_000), "1.9M");
    }

    #[test]
    fn agents_waiting_on_you_sort_first() {
        let mut states = vec![
            AgentState::Idle,
            AgentState::NeedsInput,
            AgentState::Working,
        ];
        states.sort_by_key(|state| urgency(*state));
        assert_eq!(
            states,
            vec![
                AgentState::NeedsInput,
                AgentState::Working,
                AgentState::Idle
            ]
        );
        assert_eq!(GROUPS.to_vec(), states);
    }
}
