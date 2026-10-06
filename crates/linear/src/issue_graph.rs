//! The issues blocking each other, as a graph in a tab.
//!
//! The issues nothing blocks are cards down the left, and each column to the
//! right holds what is blocked by the one before it, joined by a curve from
//! the blocker to what it blocks. The panel is for picking the next thing to
//! do; this is for seeing what is in its way.
//!
//! Only blockers among the issues the filters match are drawn. With the
//! default filter, your own unfinished issues, an issue blocked by one
//! somebody else has is a card in the first column.

use std::collections::{HashMap, HashSet};

use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, PathBuilder, SharedString,
    Task, WeakEntity, Window, actions, canvas, point, prelude::*, px,
};
use ui::{ContextMenu, Tooltip, prelude::*};
use ui_input::InputField;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

use crate::issue_view::{open_issue_in, send_to_agent};
use crate::linear_panel::{FilterTarget, filters_button};
use crate::{
    CreateWorktree, GRAPH_LIMIT, GraphIssue, Issue, IssueFilters, Linear, LinearEvent, StateType,
    Team,
};

actions!(
    linear,
    [
        /// Opens the issue graph: the issues that block each other, blockers
        /// on the left and what they block to their right.
        OpenIssueGraph,
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenIssueGraph, window, cx| {
            open_issue_graph(workspace, window, cx);
        });
    })
    .detach();
}

/// Opens the graph in the workspace, or goes to it if it is already open.
pub fn open_issue_graph(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<IssueGraphView>(cx).next();
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let Some(linear) = Linear::global(cx) else {
        return;
    };
    let handle = cx.entity().downgrade();
    let view = cx.new(|cx| IssueGraphView::new(linear, handle, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

pub(crate) fn open_issue_graph_in(
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| open_issue_graph(workspace, window, cx))
        .ok();
}

const CARD_WIDTH: f32 = 264.;
const CARD_HEIGHT: f32 = 84.;
const COLUMN_GAP: f32 = 88.;
const ROW_GAP: f32 = 12.;
const MARGIN: f32 = 20.;

/// What the graph starts filtered to: your own unfinished issues, as the
/// panel does.
fn default_filters() -> IssueFilters {
    IssueFilters::default()
}

enum Loaded {
    Loading,
    Failed(SharedString),
    Graph {
        issues: Vec<GraphIssue>,
        layout: Layout,
    },
}

pub struct IssueGraphView {
    linear: Entity<Linear>,
    workspace: WeakEntity<Workspace>,
    filters: IssueFilters,
    /// The team the graph was fetched for, to tell when to fetch again.
    team: Option<Team>,
    loaded: Loaded,
    focus_handle: FocusHandle,
    context_menu: Option<(Entity<ContextMenu>, gpui::Point<Pixels>, gpui::Subscription)>,
    search: Entity<InputField>,
    /// What the search box says, trimmed. Empty draws every issue.
    query: String,
    _load: Task<()>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl IssueGraphView {
    fn new(
        linear: Entity<Linear>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Connecting after the tab was opened, or the panel being scoped to
        // another project's team, is a reason to look again.
        let search = cx.new(|cx| {
            InputField::new(window, cx, "Find an issue and what it is tied to…")
                .start_icon(IconName::MagnifyingGlass)
        });
        let view = cx.entity().downgrade();
        let editor = search.read(cx).editor().clone();
        let typing = editor.subscribe(
            Box::new(move |event, _window, cx| {
                if event == ui_input::ErasedEditorEvent::BufferEdited {
                    view.update(cx, |this, cx| {
                        this.query = this.search.read(cx).text(cx).trim().to_owned();
                        cx.notify();
                    })
                    .ok();
                }
            }),
            window,
            cx,
        );
        let mut subscriptions = vec![typing];
        subscriptions.push(cx.subscribe_in(
            &linear,
            window,
            |this: &mut Self, linear, _: &LinearEvent, _window, cx| {
                let linear = linear.read(cx);
                let failed = matches!(this.loaded, Loaded::Failed(_)) && linear.is_connected();
                if failed || linear.team() != this.team.as_ref() {
                    this.reload(cx);
                }
                cx.notify();
            },
        ));
        let mut this = Self {
            linear,
            workspace,
            filters: default_filters(),
            team: None,
            loaded: Loaded::Loading,
            focus_handle: cx.focus_handle(),
            context_menu: None,
            search,
            query: String::new(),
            _load: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let linear = self.linear.read(cx);
        self.team = linear.team().cloned();
        let fetched = linear.issue_graph(&self.filters, cx);
        self.loaded = Loaded::Loading;
        self._load = cx.spawn(async move |this, cx| {
            let fetched = fetched.await;
            this.update(cx, |this, cx| {
                this.loaded = match fetched {
                    Ok(issues) => Loaded::Graph {
                        layout: layout(&issues),
                        issues,
                    },
                    Err(error) => Loaded::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    /// The menu of an issue's card, with what the panel's own offers for it.
    fn deploy_issue_menu(
        &mut self,
        issue: &Issue,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let create_worktree = CreateWorktree {
            identifier: issue.identifier.to_string(),
            start_agent: false,
        };
        let create_worktree_and_start_agent = CreateWorktree {
            start_agent: true,
            ..create_worktree.clone()
        };
        let send = send_to_agent(&issue.url);
        // From the tab's own node rather than whatever has focus: the
        // handlers are the workspace's, and focus is not always inside it.
        let focus_handle = self.focus_handle.clone();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            menu.context(focus_handle)
                .action("Create Worktree", Box::new(create_worktree))
                .action(
                    "Create Worktree and Start Agent",
                    Box::new(create_worktree_and_start_agent),
                )
                .action("Send to Agent", Box::new(send))
        });
        let focus = menu.focus_handle(cx);
        window.defer(cx, move |window, cx| window.focus(&focus, cx));
        let subscription = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, _, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn filter_target(&self, cx: &Context<Self>) -> FilterTarget {
        let read = cx.entity().downgrade();
        let write = read.clone();
        FilterTarget::new(
            move |cx| read.upgrade().map(|view| view.read(cx).filters.clone()),
            move |change, cx| {
                write
                    .update(cx, |this, cx| {
                        let before = this.filters.clone();
                        change(&mut this.filters);
                        if this.filters != before {
                            this.reload(cx);
                        }
                    })
                    .ok();
            },
            default_filters(),
        )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let scope = self.team.as_ref().map(|team| team.key.clone());
        h_flex()
            .w_full()
            .justify_between()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(Headline::new("Issue Graph").size(HeadlineSize::Large))
                    .children(scope.map(|key| Label::new(key).color(Color::Muted))),
            )
            .child(div().flex_1().max_w(rems(28.)).child(self.search.clone()))
            .child(
                h_flex()
                    .gap_1()
                    .child(filters_button(
                        "graph-filters",
                        self.filter_target(cx),
                        self.linear.clone(),
                        self.filters != default_filters(),
                    ))
                    .child(
                        IconButton::new("reload-graph", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .disabled(matches!(self.loaded, Loaded::Loading))
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    ),
            )
    }

    fn render_card(
        &self,
        graph: &GraphIssue,
        column: usize,
        row: usize,
        highlighted: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let issue = &graph.issue;
        let identifier = issue.identifier.clone();
        let menu_issue = issue.clone();
        let workspace = self.workspace.clone();
        let colors = cx.theme().colors();
        let in_progress = issue.state.kind == StateType::Started;
        let (left, top) = card_origin(column, row);

        v_flex()
            .id(SharedString::from(format!(
                "graph-card-{}",
                issue.identifier
            )))
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(CARD_WIDTH))
            .h(px(CARD_HEIGHT))
            .px_2p5()
            .py_2()
            .gap_1()
            .justify_between()
            .overflow_hidden()
            .rounded_lg()
            .border_1()
            .border_color(if highlighted {
                colors.text_accent
            } else if in_progress {
                colors.border_focused
            } else {
                colors.border_variant
            })
            .bg(if highlighted {
                colors.element_selected
            } else {
                colors.elevated_surface_background
            })
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .tooltip(Tooltip::text(format!(
                "{} · {}\n{}",
                issue.identifier, issue.state.name, issue.title
            )))
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.deploy_issue_menu(&menu_issue, event.position, window, cx);
                }),
            )
            .on_click(move |_, window, cx| {
                let workspace = workspace.clone();
                let identifier = identifier.clone();
                // Deferred, for the reason `open_issue` gives.
                window.defer(cx, move |window, cx| {
                    open_issue_in(&workspace, identifier, window, cx);
                });
            })
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(issue.state.kind.icon())
                            .size(IconSize::Small)
                            .map(|icon| match issue.state.color() {
                                Some(color) => icon.color(Color::Custom(color)),
                                None => icon.color(Color::Muted),
                            }),
                    )
                    .child(
                        Label::new(issue.identifier.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line(),
                    )
                    .when(issue.priority > 0., |this| {
                        this.child(
                            Label::new(issue.priority_label.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line(),
                        )
                    }),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(colors.text)
                    .line_clamp(2)
                    .child(issue.title.clone()),
            )
            .children(issue.assignee.as_ref().map(|assignee| {
                Label::new(assignee.display_name.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .single_line()
            }))
    }

    fn render_graph(
        &self,
        issues: &[GraphIssue],
        layout: &Layout,
        highlighted: &HashSet<SharedString>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let columns = layout.columns.len().max(1);
        let rows = layout
            .columns
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .max(1);
        let width = MARGIN * 2. + columns as f32 * CARD_WIDTH + (columns - 1) as f32 * COLUMN_GAP;
        let height = MARGIN * 2. + rows as f32 * CARD_HEIGHT + (rows - 1) as f32 * ROW_GAP;

        let mut slots: HashMap<usize, (usize, usize)> = HashMap::new();
        for (column, members) in layout.columns.iter().enumerate() {
            for (row, index) in members.iter().enumerate() {
                slots.insert(*index, (column, row));
            }
        }
        let links: Vec<((f32, f32), (f32, f32))> = layout
            .edges
            .iter()
            .filter_map(|(blocker, blocked)| {
                let (from_column, from_row) = *slots.get(blocker)?;
                let (to_column, to_row) = *slots.get(blocked)?;
                let (from_left, from_top) = card_origin(from_column, from_row);
                let (to_left, to_top) = card_origin(to_column, to_row);
                Some((
                    (from_left + CARD_WIDTH, from_top + CARD_HEIGHT / 2.),
                    (to_left, to_top + CARD_HEIGHT / 2.),
                ))
            })
            .collect();
        let line = cx.theme().colors().text_muted;

        let cards: Vec<_> = layout
            .columns
            .iter()
            .enumerate()
            .flat_map(|(column, members)| {
                members
                    .iter()
                    .enumerate()
                    .filter_map(move |(row, index)| Some((column, row, issues.get(*index)?)))
            })
            .map(|(column, row, graph)| {
                let highlighted = highlighted.contains(&graph.issue.id);
                self.render_card(graph, column, row, highlighted, cx)
                    .into_any_element()
            })
            .collect();

        div()
            .id("issue-graph-scroll")
            .size_full()
            .overflow_scroll()
            .child(
                div()
                    .relative()
                    .w(px(width))
                    .h(px(height))
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _| {
                                for ((from_x, from_y), (to_x, to_y)) in &links {
                                    let start = point(
                                        bounds.origin.x + px(*from_x),
                                        bounds.origin.y + px(*from_y),
                                    );
                                    let end = point(
                                        bounds.origin.x + px(*to_x),
                                        bounds.origin.y + px(*to_y),
                                    );
                                    let bend = (end.x - start.x) / 2.;
                                    let mut path = PathBuilder::stroke(px(2.));
                                    path.move_to(start);
                                    path.cubic_bezier_to(
                                        end,
                                        point(start.x + bend, start.y),
                                        point(end.x - bend, end.y),
                                    );
                                    match path.build() {
                                        Ok(path) => window.paint_path(path, line),
                                        Err(error) => log::debug!("drawing a graph link: {error}"),
                                    }
                                }
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
                    .children(cards),
            )
    }
}

/// Where a card's top left corner is, from the graph's own.
fn card_origin(column: usize, row: usize) -> (f32, f32) {
    (
        MARGIN + column as f32 * (CARD_WIDTH + COLUMN_GAP),
        MARGIN + row as f32 * (CARD_HEIGHT + ROW_GAP),
    )
}

impl Render for IssueGraphView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if !self.linear.read(cx).is_connected() {
            Label::new("Connect Linear in the Linear panel to see the graph.")
                .color(Color::Muted)
                .into_any_element()
        } else {
            match &self.loaded {
                Loaded::Loading => Label::new("Loading…")
                    .color(Color::Muted)
                    .into_any_element(),
                Loaded::Failed(error) => v_flex()
                    .gap_2()
                    .child(Label::new("Could not load the graph"))
                    .child(
                        Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Button::new("retry-graph", "Try Again")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    )
                    .into_any_element(),
                Loaded::Graph { issues, .. } if issues.is_empty() => {
                    Label::new("No issues match the filters.")
                        .color(Color::Muted)
                        .into_any_element()
                }
                Loaded::Graph { issues, .. }
                    if !self.query.is_empty()
                        && focus(issues, &self.query)
                            .is_none_or(|focus| focus.matched.is_empty()) =>
                {
                    Label::new(format!("No issues match “{}”.", self.query))
                        .color(Color::Muted)
                        .into_any_element()
                }
                Loaded::Graph {
                    issues,
                    layout: whole,
                } => {
                    // Searching redraws just what the match is tied to, laid
                    // out afresh so it is not left scattered across the gaps
                    // of the whole graph.
                    let (shown, shown_layout, highlighted) = match focus(issues, &self.query) {
                        Some(focus) => {
                            let shown: Vec<GraphIssue> = focus
                                .shown
                                .iter()
                                .map(|&index| issues[index].clone())
                                .collect();
                            let highlighted = focus
                                .matched
                                .iter()
                                .map(|&index| issues[index].issue.id.clone())
                                .collect();
                            let shown_layout = layout(&shown);
                            (Some(shown), Some(shown_layout), highlighted)
                        }
                        None => (None, None, HashSet::new()),
                    };
                    let issues = shown.as_deref().unwrap_or(issues);
                    let layout = shown_layout.as_ref().unwrap_or(whole);
                    v_flex()
                        .size_full()
                        .min_h_0()
                        .gap_2()
                        .child(self.render_graph(issues, layout, &highlighted, cx))
                        .when(issues.len() >= GRAPH_LIMIT, |this| {
                            this.child(
                                Label::new(format!(
                                    "Showing the {GRAPH_LIMIT} most recently updated issues."
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                        })
                        .into_any_element()
                }
            }
        };

        v_flex()
            .id("issue-graph")
            .key_context("IssueGraph")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .gap_3()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().size_full().child(body))
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                gpui::deferred(
                    gpui::anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl Focusable for IssueGraphView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for IssueGraphView {}

impl Item for IssueGraphView {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Linear Issue Graph".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Workflow))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}

/// Which column and row each issue is drawn in, and which links are drawn.
#[derive(Debug, PartialEq)]
pub(crate) struct Layout {
    /// Indices into the issues, a column of them at a time, top to bottom.
    columns: Vec<Vec<usize>>,
    /// Pairs of indices: the blocker, and what it blocks.
    edges: Vec<(usize, usize)>,
}

/// Pairs of indices: a blocker, and an issue it blocks. Blocking something
/// that is not among `issues`, or itself, is not a link.
fn links(issues: &[GraphIssue]) -> Vec<(usize, usize)> {
    let index: HashMap<&SharedString, usize> = issues
        .iter()
        .enumerate()
        .map(|(position, graph)| (&graph.issue.id, position))
        .collect();

    let mut links: Vec<(usize, usize)> = Vec::new();
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    for (blocker, graph) in issues.iter().enumerate() {
        for id in &graph.blocks {
            if let Some(&blocked) = index.get(id)
                && blocked != blocker
                && seen.insert((blocker, blocked))
            {
                links.push((blocker, blocked));
            }
        }
    }
    links
}

/// What a search draws: the issues it matches, and everything they are tied
/// to, in both directions.
#[derive(Debug, PartialEq)]
pub(crate) struct Focus {
    /// Indices of the issues to draw, in order.
    shown: Vec<usize>,
    /// Indices of the ones the search matched, which are drawn highlighted.
    matched: HashSet<usize>,
}

/// The issues whose identifier or title contains `query`, with every issue
/// that blocks them, and every issue they block, down the chain, as long as
/// it is not closed. A closed issue ends the chain: what it blocked is no
/// longer waiting on anything through it. `None` for an empty query, which
/// is everything.
pub(crate) fn focus(issues: &[GraphIssue], query: &str) -> Option<Focus> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return None;
    }
    let matched: HashSet<usize> = issues
        .iter()
        .enumerate()
        .filter(|(_, graph)| {
            graph.issue.identifier.to_lowercase().contains(&query)
                || graph.issue.title.to_lowercase().contains(&query)
        })
        .map(|(index, _)| index)
        .collect();

    let links = links(issues);
    let mut shown: HashSet<usize> = matched.clone();
    for downstream in [true, false] {
        let mut pending: Vec<usize> = matched.iter().copied().collect();
        while let Some(current) = pending.pop() {
            for &(blocker, blocked) in &links {
                let (from, next) = if downstream {
                    (blocker, blocked)
                } else {
                    (blocked, blocker)
                };
                if from == current
                    && !issues[next].issue.state.kind.is_closed()
                    && shown.insert(next)
                {
                    pending.push(next);
                }
            }
        }
    }

    let mut shown: Vec<usize> = shown.into_iter().collect();
    shown.sort_unstable();
    Some(Focus { shown, matched })
}

/// Puts every issue one column to the right of the furthest of its blockers,
/// so a link always runs left to right.
///
/// Linear lets issues block each other in a ring. There is no left to right
/// for that, so the issue met first is placed as if nothing blocked it and
/// the links that would have run backwards are not drawn.
pub(crate) fn layout(issues: &[GraphIssue]) -> Layout {
    let mut edges = links(issues);

    let count = issues.len();
    let mut blocked_by = vec![0usize; count];
    let mut blocking: Vec<Vec<usize>> = vec![Vec::new(); count];
    for &(blocker, blocked) in &edges {
        blocked_by[blocked] += 1;
        blocking[blocker].push(blocked);
    }

    let mut column_of = vec![0usize; count];
    let mut placed = vec![false; count];
    let mut ready: Vec<usize> = (0..count).filter(|&issue| blocked_by[issue] == 0).collect();
    ready.reverse();
    let mut placed_count = 0;
    while placed_count < count {
        let next = match ready.pop() {
            Some(next) => next,
            None => match (0..count).find(|&issue| !placed[issue]) {
                Some(next) => next,
                None => break,
            },
        };
        if placed[next] {
            continue;
        }
        placed[next] = true;
        placed_count += 1;
        for &blocked in &blocking[next] {
            if placed[blocked] {
                continue;
            }
            column_of[blocked] = column_of[blocked].max(column_of[next] + 1);
            blocked_by[blocked] = blocked_by[blocked].saturating_sub(1);
            if blocked_by[blocked] == 0 {
                ready.push(blocked);
            }
        }
    }

    edges.retain(|&(blocker, blocked)| column_of[blocker] < column_of[blocked]);

    let column_count = column_of.iter().copied().max().map_or(0, |last| last + 1);
    let mut columns: Vec<Vec<usize>> = vec![Vec::new(); column_count];
    for issue in 0..count {
        columns[column_of[issue]].push(issue);
    }

    let blocks_something: HashSet<usize> = edges.iter().map(|&(blocker, _)| blocker).collect();
    if let Some(first) = columns.first_mut() {
        first.sort_by_key(|&issue| {
            (
                !blocks_something.contains(&issue),
                issues[issue].issue.state.kind != StateType::Started,
                issue,
            )
        });
    }

    let mut row_of = vec![0usize; count];
    for (row, &issue) in columns.first().into_iter().flatten().enumerate() {
        row_of[issue] = row;
    }
    for column in columns.iter_mut().skip(1) {
        // Beside the blockers it hangs off, which keeps the links short and
        // mostly apart.
        let average_row = |issue: usize| -> f32 {
            let rows: Vec<f32> = edges
                .iter()
                .filter(|&&(_, blocked)| blocked == issue)
                .map(|&(blocker, _)| row_of[blocker] as f32)
                .collect();
            rows.iter().sum::<f32>() / rows.len().max(1) as f32
        };
        column.sort_by(|&a, &b| average_row(a).total_cmp(&average_row(b)).then(a.cmp(&b)));
        for (row, &issue) in column.iter().enumerate() {
            row_of[issue] = row;
        }
    }

    Layout { columns, edges }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::tests::issue_json;

    fn issue(identifier: &str, blocks: &[&str]) -> GraphIssue {
        GraphIssue {
            issue: Arc::new(serde_json::from_value(issue_json(identifier)).expect("an issue")),
            blocks: blocks.iter().map(|id| format!("id-{id}").into()).collect(),
        }
    }

    fn identifiers(issues: &[GraphIssue], layout: &Layout) -> Vec<Vec<String>> {
        layout
            .columns
            .iter()
            .map(|column| {
                column
                    .iter()
                    .map(|&index| issues[index].issue.identifier.to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn what_is_blocked_goes_right_of_its_blocker() {
        let issues = [
            issue("A-3", &[]),
            issue("A-1", &["A-2"]),
            issue("A-2", &["A-3"]),
        ];
        let layout = layout(&issues);
        assert_eq!(
            identifiers(&issues, &layout),
            vec![vec!["A-1"], vec!["A-2"], vec!["A-3"]]
        );
        assert_eq!(layout.edges, vec![(1, 2), (2, 0)]);
    }

    #[test]
    fn an_issue_sits_right_of_its_furthest_blocker() {
        let issues = [
            issue("A-1", &["A-2", "A-3"]),
            issue("A-2", &["A-3"]),
            issue("A-3", &[]),
        ];
        let layout = layout(&issues);
        assert_eq!(
            identifiers(&issues, &layout),
            vec![vec!["A-1"], vec!["A-2"], vec!["A-3"]]
        );
        assert_eq!(layout.edges.len(), 3);
    }

    #[test]
    fn issues_nothing_blocks_are_the_first_column_blockers_first() {
        let issues = [
            issue("A-1", &[]),
            issue("A-2", &["A-4"]),
            issue("A-3", &[]),
            issue("A-4", &[]),
        ];
        let layout = layout(&issues);
        assert_eq!(
            identifiers(&issues, &layout),
            vec![vec!["A-2", "A-1", "A-3"], vec!["A-4"]]
        );
    }

    #[test]
    fn a_ring_of_blockers_is_laid_out_and_drops_the_backward_link() {
        let issues = [
            issue("A-1", &["A-2"]),
            issue("A-2", &["A-3"]),
            issue("A-3", &["A-1"]),
        ];
        let layout = layout(&issues);
        assert_eq!(
            identifiers(&issues, &layout),
            vec![vec!["A-1"], vec!["A-2"], vec!["A-3"]]
        );
        assert_eq!(layout.edges, vec![(0, 1), (1, 2)]);
    }

    #[test]
    fn blockers_outside_the_set_and_self_blocks_are_ignored() {
        let issues = [issue("A-1", &["A-9", "A-1"])];
        let layout = layout(&issues);
        assert_eq!(identifiers(&issues, &layout), vec![vec!["A-1"]]);
        assert!(layout.edges.is_empty());
    }

    fn closed(identifier: &str, blocks: &[&str]) -> GraphIssue {
        let mut json = issue_json(identifier);
        json["state"]["type"] = serde_json::json!("completed");
        GraphIssue {
            issue: Arc::new(serde_json::from_value(json).expect("an issue")),
            blocks: blocks.iter().map(|id| format!("id-{id}").into()).collect(),
        }
    }

    fn shown_identifiers(issues: &[GraphIssue], query: &str) -> Vec<String> {
        focus(issues, query)
            .expect("a search")
            .shown
            .into_iter()
            .map(|index| issues[index].issue.identifier.to_string())
            .collect()
    }

    #[test]
    fn a_match_brings_everything_upstream_and_downstream() {
        let issues = [
            issue("A-1", &["A-2"]),
            issue("A-2", &["A-3"]),
            issue("A-3", &[]),
            issue("A-4", &[]),
        ];
        assert_eq!(shown_identifiers(&issues, "a-2"), vec!["A-1", "A-2", "A-3"]);
        let found = focus(&issues, "a-2").expect("a search");
        assert_eq!(found.matched, HashSet::from([1]));
    }

    #[test]
    fn a_match_does_not_bring_what_its_blockers_also_block() {
        let issues = [
            issue("A-1", &["A-2", "A-3"]),
            issue("A-2", &[]),
            issue("A-3", &[]),
        ];
        assert_eq!(shown_identifiers(&issues, "a-2"), vec!["A-1", "A-2"]);
    }

    #[test]
    fn a_closed_issue_ends_the_chain_in_either_direction() {
        let issues = [
            issue("A-1", &["A-2"]),
            closed("A-2", &["A-3"]),
            issue("A-3", &["A-4"]),
            issue("A-4", &[]),
        ];
        assert_eq!(shown_identifiers(&issues, "a-3"), vec!["A-3", "A-4"]);
        assert_eq!(shown_identifiers(&issues, "a-1"), vec!["A-1"]);
    }

    #[test]
    fn a_search_reads_titles_and_ignores_case() {
        let issues = [issue("A-1", &[])];
        assert_eq!(shown_identifiers(&issues, "  FIX LOGIN "), vec!["A-1"]);
        assert!(focus(&issues, "nothing like it").is_some_and(|found| found.matched.is_empty()));
        assert_eq!(focus(&issues, "  "), None);
    }

    #[test]
    fn no_issues_is_no_columns() {
        let layout = layout(&[]);
        assert!(layout.columns.is_empty());
    }
}
