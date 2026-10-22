//! Every agent on one board, in a column for the phase of the work it is in:
//! planning, exploring, coding, testing, reviewing, shipping or debugging.
//!
//! The worktree panel answers "which of my worktrees has an agent, and does it
//! need me". The board answers the next question — what is each of them
//! actually doing — and puts the agents side by side so that the one stuck in
//! review, or the three all still exploring, stand out. The phases are read
//! from Claude Code's hooks; see [`agent_tracker::activity`]. The worktree
//! facts on each card (its issue, pull request and changes) are the panel's
//! own, so the board asks git and GitHub nothing the panel has not already.

use std::time::{Duration, SystemTime};

use agent_tracker::{
    Activity, AgentState, AgentTracker, ClaudeModel, Phase, PhaseSource, TodoStatus,
    TranscriptStats, agent_state_color,
};
use git_ui_core::pull_request_color::pull_request_color;
use github_cli::PullRequestState;
use gpui::{
    App, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Hsla, SharedString, Subscription,
    WeakEntity, Window, actions, hsla, prelude::*, px, relative,
};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::item::{Item, ItemEvent};
use workspace::{MultiWorkspace, Pane, Workspace};
use worktree_metadata::hue_color;

use crate::{WorktreeListing, WorktreePanel};

actions!(
    worktree_panel,
    [
        /// Opens the agent board: every agent in a column for the phase of
        /// the work it is in.
        OpenAgentBoard,
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenAgentBoard, window, cx| {
            open(workspace, window, cx);
        });
    })
    .detach();
}

/// Opens the board in the workspace, or goes to it if it is already open.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace.items_of_type::<AgentBoard>(cx).next();
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let handle = cx.entity().downgrade();
    let multi_workspace = workspace.multi_workspace().cloned();
    let board = cx.new(|cx| AgentBoard::new(handle, multi_workspace, cx));
    workspace.add_item_to_active_pane(Box::new(board), None, true, window, cx);
}

const COLUMN_WIDTH: f32 = 260.;
const DETAIL_WIDTH: f32 = 380.;

/// Which agents the board shows, by what they are doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StateFilter {
    All,
    NeedsInput,
    Working,
    Idle,
}

impl StateFilter {
    const ALL: [StateFilter; 4] = [
        StateFilter::All,
        StateFilter::NeedsInput,
        StateFilter::Working,
        StateFilter::Idle,
    ];

    fn label(self) -> &'static str {
        match self {
            StateFilter::All => "All",
            StateFilter::NeedsInput => "Needs You",
            StateFilter::Working => "Working",
            StateFilter::Idle => "Idle",
        }
    }

    fn admits(self, state: AgentState) -> bool {
        match self {
            StateFilter::All => true,
            StateFilter::NeedsInput => state == AgentState::NeedsInput,
            StateFilter::Working => state == AgentState::Working,
            StateFilter::Idle => state == AgentState::Idle,
        }
    }
}

pub struct AgentBoard {
    workspace: WeakEntity<Workspace>,
    multi_workspace: Option<WeakEntity<MultiWorkspace>>,
    /// The card whose detail is open, by [`agent_tracker::AgentEntry::id`].
    selected: Option<SharedString>,
    filter: StateFilter,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl AgentBoard {
    fn new(
        workspace: WeakEntity<Workspace>,
        multi_workspace: Option<WeakEntity<MultiWorkspace>>,
        cx: &mut Context<Self>,
    ) -> Self {
        // An observer rather than a subscription: the tracker notifies when
        // what an agent is doing moves on, which is most of what the board
        // shows, and emits `AgentsChanged` only when an agent's state does. The
        // tracker is a global entity outside the dock and pane tree, so this
        // closes no notify ring.
        let mut subscriptions = Vec::new();
        if let Some(tracker) = AgentTracker::try_global(cx) {
            subscriptions.push(cx.observe(&tracker, |_, _, cx| cx.notify()));
        }
        Self {
            workspace,
            multi_workspace,
            selected: None,
            filter: StateFilter::All,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    /// The board's cards, worked out afresh from the tracker and the worktree
    /// panel: both already hold everything, and keeping a copy here would
    /// only be a copy to keep in step.
    fn cards(&self, cx: &App) -> Vec<Card> {
        let Some(tracker) = AgentTracker::try_global(cx) else {
            return Vec::new();
        };
        let worktrees: Vec<WorktreeListing> = self
            .workspace
            .upgrade()
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
                    .or_else(|| {
                        agent
                            .activity
                            .and_then(|activity| activity.prompt.clone())
                    })
                    .unwrap_or_else(|| directory_name.clone());
                Card {
                    id: agent.id,
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

    fn select(&mut self, id: SharedString, cx: &mut Context<Self>) {
        self.selected = if self.selected.as_ref() == Some(&id) {
            None
        } else {
            Some(id)
        };
        cx.notify();
    }

    /// Brings the terminal an agent is running in forward, switching to the
    /// worktree it is in first when that is not the one showing.
    fn focus_terminal(&mut self, terminal: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.as_ref().and_then(WeakEntity::upgrade)
        else {
            return;
        };
        let found = multi_workspace
            .read(cx)
            .workspaces()
            .find_map(|workspace| {
                terminal_view_in(workspace.read(cx), terminal, cx)
                    .map(|found| (workspace.clone(), found))
            });
        let Some((workspace, (index, pane))) = found else {
            return;
        };
        // After this update has let go of the board: activating a workspace
        // re-renders the window, the board's pane included.
        window.defer(cx, move |window, cx| {
            multi_workspace
                .update(cx, |multi_workspace, cx| {
                    multi_workspace.activate(workspace.clone(), None, window, cx);
                });
            workspace.update(cx, |workspace, cx| {
                reveal(workspace, &pane, index, window, cx);
            });
        });
    }

    fn open_issue(&mut self, identifier: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                linear::open_issue(workspace, identifier, window, cx);
            })
            .log_err();
    }

    fn render_summary(&self, cards: &[Card], cx: &mut Context<Self>) -> AnyElement {
        let count = |state| cards.iter().filter(|card| card.state == state).count();
        let needs_input = count(AgentState::NeedsInput);
        let working = count(AgentState::Working);
        let idle = count(AgentState::Idle);
        let used: u64 = cards
            .iter()
            .filter_map(|card| card.stats.as_ref())
            .map(|stats| stats.input_tokens + stats.cache_read_tokens + stats.cache_write_tokens)
            .sum();
        let colors = cx.theme().colors();

        h_flex()
            .flex_wrap()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(stat(
                "Agents",
                cards.len().to_string(),
                Some(format!("{working} working · {idle} idle")),
                Color::Default,
                colors.border_variant,
            ))
            .child(stat(
                "Needs You",
                needs_input.to_string(),
                None,
                if needs_input > 0 {
                    Color::Warning
                } else {
                    Color::Default
                },
                colors.border_variant,
            ))
            .child(stat(
                "Tokens In",
                tokens(used),
                Some("across these sessions".to_owned()),
                Color::Default,
                colors.border_variant,
            ))
            .into_any_element()
    }

    fn render_toolbar(&self, cards: &[Card], cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        h_flex()
            .px_3()
            .py_1p5()
            .gap_1()
            .flex_wrap()
            .border_b_1()
            .border_color(colors.border_variant)
            .children(StateFilter::ALL.iter().map(|filter| {
                let filter = *filter;
                let count = cards
                    .iter()
                    .filter(|card| filter.admits(card.state))
                    .count();
                Button::new(
                    SharedString::from(format!("agent-board-filter-{}", filter.label())),
                    format!("{} {count}", filter.label()),
                )
                .label_size(LabelSize::Small)
                .toggle_state(self.filter == filter)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.filter = filter;
                    cx.notify();
                }))
            }))
            .child(div().flex_1())
            .children(Phase::ALL.iter().map(|phase| {
                h_flex()
                    .gap_1()
                    .child(div().size_2().rounded_sm().bg(phase_color(*phase)))
                    .child(
                        Label::new(phase.label())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
            }))
            .into_any_element()
    }

    fn render_board(&self, cards: &[Card], cx: &mut Context<Self>) -> AnyElement {
        let shown: Vec<&Card> = cards
            .iter()
            .filter(|card| self.filter.admits(card.state))
            .collect();
        let unknown: Vec<&Card> = shown
            .iter()
            .copied()
            .filter(|card| card.phase().is_none())
            .collect();

        let mut columns = Vec::new();
        if !unknown.is_empty() {
            columns.push(self.render_column(None, &unknown, cx).into_any_element());
        }
        for phase in Phase::ALL {
            let in_phase: Vec<&Card> = shown
                .iter()
                .copied()
                .filter(|card| card.phase().map(|(card_phase, _)| card_phase) == Some(phase))
                .collect();
            columns.push(self.render_column(Some(phase), &in_phase, cx).into_any_element());
        }

        h_flex()
            .id("agent-board-columns")
            .flex_1()
            .min_w_0()
            .h_full()
            .items_start()
            .p_3()
            .gap_2p5()
            .overflow_x_scroll()
            // Without this a container that scrolls only sideways turns a
            // vertical swipe into a sideways one, so swiping down a column
            // would slide the whole board as well as scroll the column.
            .restrict_scroll_to_axis()
            .children(columns)
            .into_any_element()
    }

    fn render_column(
        &self,
        phase: Option<Phase>,
        cards: &[&Card],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let label: SharedString = phase.map_or("No Phase Yet".into(), |phase| phase.label().into());
        let accent = phase.map_or(colors.border, phase_color);
        let needs_input = cards
            .iter()
            .filter(|card| card.state == AgentState::NeedsInput)
            .count();
        let id = SharedString::from(format!("agent-board-column-{label}"));

        v_flex()
            .id(id)
            .flex_none()
            .w(px(COLUMN_WIDTH))
            .max_h_full()
            .gap_2()
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1p5()
                    .rounded_md()
                    .border_t_2()
                    .border_color(accent)
                    .bg(colors.surface_background)
                    .child(Label::new(label.clone()).size(LabelSize::Small).weight(gpui::FontWeight::SEMIBOLD))
                    .child(
                        Label::new(cards.len().to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .when(needs_input > 0, |this| {
                        this.child(
                            Label::new(format!("{needs_input} need you"))
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                    }),
            )
            .when(cards.is_empty(), |this| {
                this.child(
                    div()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_dashed()
                        .border_color(colors.border_variant)
                        .child(
                            Label::new(format!("No agents {}", label.to_lowercase()))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .children(cards.iter().map(|card| self.render_card(card, cx)).collect::<Vec<_>>())
            .into_any_element()
    }

    fn render_card(&self, card: &Card, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let selected = self.selected.as_ref() == Some(&card.id);
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

        v_flex()
            .id(SharedString::from(format!("agent-card-{}", card.id)))
            .w_full()
            .p_2()
            .gap_1p5()
            .rounded_md()
            .border_1()
            .cursor_pointer()
            .bg(colors.elevated_surface_background)
            .border_color(if selected {
                colors.border_focused
            } else if card.state == AgentState::NeedsInput {
                status.warning_border
            } else {
                colors.border_variant
            })
            .hover(|style| style.border_color(colors.border))
            .on_click(cx.listener(move |this, _, _, cx| this.select(id.clone(), cx)))
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
                    .child(state_pill(card.state))
                    .child(div().flex_1())
                    .when_some(card.model.clone(), |this, model| {
                        this.child(model.badge(
                            SharedString::from(format!("agent-card-model-{}", card.id)),
                            card.state != AgentState::Idle,
                        ))
                    }),
            )
            .when_some(phase.as_ref(), |this, (phase, source)| {
                this.child(source_label(*phase, source))
            })
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
                    Label::new(format!("↳ subagent {} · {}", subagent.kind, age(subagent.started, SystemTime::now())))
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
                                .child(Label::new("Todos").size(LabelSize::XSmall).color(Color::Muted))
                                .child(progress_bar(done as f32 / total.max(1) as f32, colors.text_accent, colors.border_variant))
                                .child(Label::new(format!("{done}/{total}")).size(LabelSize::XSmall)),
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
                        })
                        .child(
                            IconButton::new("agent-detail-close", IconName::Close)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Close Detail"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.selected = None;
                                    cx.notify();
                                })),
                        ),
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
                                duration(finished.duration_since(subagent.started).unwrap_or_default())
                            ),
                            None => format!("running · {}", age(subagent.started, now)),
                        };
                        h_flex()
                            .gap_2()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Label::new(format!("↳ {}", subagent.kind)).size(LabelSize::Small).buffer_font(cx))
                                    .when(!subagent.description.is_empty(), |this| {
                                        this.child(
                                            Label::new(subagent.description.clone())
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .truncate(),
                                        )
                                    }),
                            )
                            .child(Label::new(state).size(LabelSize::XSmall).color(Color::Muted))
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
                            .child(figure("Tokens in", tokens(stats.input_tokens + stats.cache_read_tokens + stats.cache_write_tokens)))
                            .child(figure("Tokens out", tokens(stats.output_tokens)))
                            .child(figure(
                                "Cache hits",
                                stats
                                    .cache_hit_rate()
                                    .map_or("—".to_owned(), |rate| format!("{:.0}%", rate * 100.)),
                            ))
                            .child(figure("Context", format!("{:.0}%", stats.context_fraction() * 100.)))
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
            .id("agent-board-detail")
            .flex_none()
            .w(px(DETAIL_WIDTH))
            .h_full()
            .border_l_1()
            .border_color(colors.border_variant)
            .bg(colors.panel_background)
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            .children(sections)
            .into_any_element()
    }
}

/// One agent as the board draws it.
struct Card {
    id: SharedString,
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

impl Render for AgentBoard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cards = self.cards(cx);
        let editor_background = cx.theme().colors().editor_background;
        let border_variant = cx.theme().colors().border_variant;
        let hooks_reporting = AgentTracker::try_global(cx)
            .is_some_and(|tracker| tracker.read(cx).has_hook_reports());
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| !cards.iter().any(|card| &card.id == selected))
        {
            self.selected = None;
        }
        let selected = self
            .selected
            .as_ref()
            .and_then(|selected| cards.iter().find(|card| &card.id == selected));

        let body = if cards.is_empty() {
            v_flex()
                .flex_1()
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
            h_flex()
                .flex_1()
                .min_h_0()
                .items_start()
                .child(self.render_board(&cards, cx))
                .when_some(selected.map(|card| self.render_detail(card, cx)), |this, detail| {
                    this.child(detail)
                })
                .into_any_element()
        };

        v_flex()
            .key_context("AgentBoard")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(editor_background)
            .child(self.render_summary(&cards, cx))
            .when(!hooks_reporting, |this| {
                this.child(
                    h_flex()
                        .px_3()
                        .py_1p5()
                        .gap_2()
                        .border_b_1()
                        .border_color(border_variant)
                        .child(
                            Icon::new(IconName::Info)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(
                                "Working and waiting are guessed from terminal output. Claude \
                                 Code's hooks make them exact and say what an agent is asking, \
                                 where your organization allows user hooks.",
                            )
                            .size(LabelSize::Small)
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
            .child(self.render_toolbar(&cards, cx))
            .child(body)
    }
}

impl Focusable for AgentBoard {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for AgentBoard {}

impl Item for AgentBoard {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Agents".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ZedAgent))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
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
            .filter_map(|(index, item)| Some((index, pane.clone(), item.act_as::<TerminalView>(cx)?)))
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
    pane.update(cx, |pane, cx| pane.activate_item(index, true, true, window, cx));
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

fn state_pill(state: AgentState) -> impl IntoElement {
    let (label, color) = match state {
        AgentState::NeedsInput => ("Needs you", agent_state_color(state)),
        AgentState::Working => ("Working", agent_state_color(state)),
        AgentState::Idle => ("Idle", agent_state_color(state)),
    };
    h_flex()
        .gap_1()
        .child(ui::Indicator::dot().color(color))
        .child(Label::new(label).size(LabelSize::XSmall).color(color))
}

/// Why the card is in its column: filled for something the session said,
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
    div()
        .flex_1()
        .h_1()
        .rounded_sm()
        .bg(track)
        .child(
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
        .child(div().w(px(40.)).child(progress_bar(fraction, fill, colors.border_variant)))
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
            (span.phase, end.duration_since(span.since).unwrap_or_default())
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

fn stat(
    label: &'static str,
    value: String,
    detail: Option<String>,
    color: Color,
    border: Hsla,
) -> impl IntoElement {
    v_flex()
        .px_4()
        .py_2()
        .gap_0p5()
        .min_w(px(140.))
        .border_r_1()
        .border_color(border)
        .child(Label::new(label).size(LabelSize::XSmall).color(Color::Muted))
        .child(
            h_flex()
                .gap_1p5()
                .items_baseline()
                .child(Label::new(value).size(LabelSize::Large).color(color))
                .when_some(detail, |this, detail| {
                    this.child(Label::new(detail).size(LabelSize::XSmall).color(Color::Muted))
                }),
        )
}

fn figure(label: &'static str, value: String) -> impl IntoElement {
    v_flex()
        .min_w(px(90.))
        .child(Label::new(label).size(LabelSize::XSmall).color(Color::Muted))
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
    fn the_state_filter_admits_what_it_names() {
        assert!(StateFilter::All.admits(AgentState::Idle));
        assert!(StateFilter::NeedsInput.admits(AgentState::NeedsInput));
        assert!(!StateFilter::NeedsInput.admits(AgentState::Working));
        assert!(StateFilter::Idle.admits(AgentState::Idle));
        assert!(!StateFilter::Working.admits(AgentState::Idle));
    }

    #[test]
    fn agents_waiting_on_you_sort_first() {
        let mut states = vec![AgentState::Idle, AgentState::NeedsInput, AgentState::Working];
        states.sort_by_key(|state| urgency(*state));
        assert_eq!(
            states,
            vec![AgentState::NeedsInput, AgentState::Working, AgentState::Idle]
        );
    }
}
