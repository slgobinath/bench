//! A Linear issue in a tab: what it is, where it stands, and what has been
//! said about it. Read-only — changing an issue is done in Linear, one click
//! away through "Open in Linear".

use gpui::{
    App, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle, SharedString, Task,
    WeakEntity, Window, prelude::*,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use project::Project;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use time_format::TimestampFormat;
use ui::{Divider, Tooltip, prelude::*};
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

use crate::{CreateWorktree, Issue, IssueDetail, Linear, LinearEvent};

/// The link, with a trailing space so it doesn't fuse with what is typed next.
pub fn send_to_agent(url: &str) -> zed_actions::claude::SendText {
    zed_actions::claude::SendText {
        text: format!("{url} "),
    }
}

/// Opens an issue's tab in the workspace, or goes to it if it is already open.
pub fn open_issue(
    workspace: &mut Workspace,
    identifier: SharedString,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<IssueView>(cx)
        .find(|view| view.read(cx).identifier == identifier);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let project = workspace.project().clone();
    let view = cx.new(|cx| IssueView::new(identifier, project, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

enum Loaded {
    Loading,
    Failed(SharedString),
    Issue {
        detail: IssueDetail,
        description: Option<Entity<Markdown>>,
        comments: Vec<(SharedString, SharedString, Entity<Markdown>)>,
    },
}

pub struct IssueView {
    identifier: SharedString,
    /// What the tab is titled with before the issue has loaded, and after.
    title: Option<SharedString>,
    /// Known once the issue has loaded; the tab can only be sent to the agent
    /// from then on.
    url: Option<SharedString>,
    project: Entity<Project>,
    loaded: Loaded,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    _load: Task<()>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl IssueView {
    fn new(
        identifier: SharedString,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();
        // Connecting after the tab was opened — it was restored, or the key
        // was just pasted — is a reason to try again.
        if let Some(linear) = Linear::global(cx) {
            subscriptions.push(cx.subscribe_in(
                &linear,
                window,
                |this: &mut Self, linear, _: &LinearEvent, _window, cx| {
                    if matches!(this.loaded, Loaded::Failed(_)) && linear.read(cx).is_connected()
                    {
                        this.reload(cx);
                    }
                },
            ));
        }
        let mut this = Self {
            identifier,
            title: None,
            url: None,
            project,
            loaded: Loaded::Loading,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            _load: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(linear) = Linear::global(cx) else {
            self.loaded = Loaded::Failed("Linear is not available.".into());
            return;
        };
        let identifier = self.identifier.clone();
        let detail = linear.read_with(cx, |linear, cx| linear.issue_detail(&identifier, cx));
        self._load = cx.spawn(async move |this, cx| {
            let detail = detail.await;
            this.update(cx, |this, cx| {
                this.loaded = match detail {
                    Ok(detail) => this.loaded_issue(detail, cx),
                    Err(error) => Loaded::Failed(format!("{error:#}").into()),
                };
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })
            .ok();
        });
    }

    fn loaded_issue(&mut self, detail: IssueDetail, cx: &mut Context<Self>) -> Loaded {
        let languages = self.project.read(cx).languages().clone();
        let markdown = |source: &str, cx: &mut Context<Self>| {
            let source = SharedString::from(source.to_owned());
            let languages = languages.clone();
            cx.new(|cx| Markdown::new(source, Some(languages), None, cx))
        };
        self.title = Some(detail.issue.title.clone());
        self.url = Some(detail.issue.url.clone());
        let description = detail
            .description
            .as_deref()
            .filter(|description| !description.trim().is_empty())
            .map(|description| markdown(description, cx));
        let now = OffsetDateTime::now_utc();
        let comments = detail
            .comments
            .iter()
            .map(|comment| {
                let when = OffsetDateTime::parse(&comment.created_at, &Rfc3339)
                    .map(|at| {
                        time_format::format_local_timestamp(at, now, TimestampFormat::Relative)
                    })
                    .unwrap_or_default();
                (
                    comment.author(),
                    SharedString::from(when),
                    markdown(&comment.body, cx),
                )
            })
            .collect();
        Loaded::Issue {
            detail,
            description,
            comments,
        }
    }

    fn render_header(&self, issue: &Issue, cx: &mut Context<Self>) -> impl IntoElement {
        let url = issue.url.clone();
        let identifier = issue.identifier.to_string();
        // From the tab's own node rather than whatever has focus: the handler
        // is the workspace's, and focus is not always inside it.
        let focus_handle = self.focus_handle.clone();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(
                        Label::new(issue.identifier.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("create-worktree", "Create Worktree")
                                    .start_icon(
                                        Icon::new(IconName::GitBranchPlus).size(IconSize::Small),
                                    )
                                    .label_size(LabelSize::Small)
                                    .on_click(move |_, window, cx| {
                                        focus_handle.dispatch_action(
                                            &CreateWorktree {
                                                identifier: identifier.clone(),
                                                start_agent: false,
                                            },
                                            window,
                                            cx,
                                        );
                                    }),
                            )
                            .child(
                                Button::new("open-in-linear", "Open in Linear")
                                    .start_icon(
                                        Icon::new(IconName::ArrowUpRight).size(IconSize::Small),
                                    )
                                    .label_size(LabelSize::Small)
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                            .child(
                                IconButton::new("reload-issue", IconName::RotateCw)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Reload"))
                                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                            ),
                    ),
            )
            .child(Headline::new(issue.title.clone()).size(HeadlineSize::Large))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_x_3()
                    .gap_y_1()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Icon::new(issue.state.kind.icon())
                                    .size(IconSize::Small)
                                    .map(|icon| match issue.state.color() {
                                        Some(color) => icon.color(Color::Custom(color)),
                                        None => icon.color(Color::Muted),
                                    }),
                            )
                            .child(Label::new(issue.state.name.clone()).size(LabelSize::Small)),
                    )
                    .child(property(
                        IconName::Person,
                        issue
                            .assignee
                            .as_ref()
                            .map(|assignee| assignee.display_name.clone())
                            .unwrap_or_else(|| "Unassigned".into()),
                    ))
                    .when(issue.priority > 0., |this| {
                        this.child(property(
                            IconName::SignalHigh,
                            issue.priority_label.clone(),
                        ))
                    })
                    .child(property(IconName::UserGroup, issue.team.name.clone()))
                    .when_some(issue.project.as_ref(), |this, project| {
                        this.child(property(IconName::Box, project.name.clone()))
                    })
                    .when_some(issue.cycle.as_ref(), |this, cycle| {
                        this.child(property(IconName::ArrowCircle, cycle.label()))
                    })
                    .children(issue.labels.iter().map(|label| {
                        h_flex()
                            .gap_1()
                            .child(
                                div().size_2().rounded_full().bg(crate::hex_color(&label.color)
                                    .unwrap_or_else(|| Color::Muted.color(cx))),
                            )
                            .child(Label::new(label.name.clone()).size(LabelSize::Small))
                    })),
            )
    }
}

fn property(icon: IconName, text: SharedString) -> impl IntoElement {
    h_flex()
        .gap_1()
        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
        .child(Label::new(text).size(LabelSize::Small))
}

fn markdown_element(markdown: &Entity<Markdown>, window: &Window, cx: &App) -> MarkdownElement {
    MarkdownElement::new(
        markdown.clone(),
        MarkdownStyle::themed(MarkdownFont::Preview, window, cx),
    )
    .on_url_click(|url, _, cx| cx.open_url(&url))
}

impl Render for IssueView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.loaded {
            Loaded::Loading => v_flex()
                .child(Label::new("Loading…").color(Color::Muted))
                .into_any_element(),
            Loaded::Failed(error) => v_flex()
                .gap_2()
                .child(Label::new(format!("Could not load {}", self.identifier)))
                .child(
                    Label::new(error.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
                .child(
                    Button::new("retry", "Try Again")
                        .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                )
                .into_any_element(),
            Loaded::Issue {
                detail,
                description,
                comments,
            } => v_flex()
                .gap_4()
                .child(self.render_header(&detail.issue, cx))
                .child(Divider::horizontal())
                .child(match description {
                    Some(description) => markdown_element(description, window, cx).into_any_element(),
                    None => Label::new("No description")
                        .color(Color::Muted)
                        .into_any_element(),
                })
                .when(!comments.is_empty(), |this| {
                    this.child(Divider::horizontal()).child(
                        v_flex()
                            .gap_3()
                            .child(Label::new("Comments").color(Color::Muted))
                            .children(comments.iter().map(|(author, when, body)| {
                                v_flex()
                                    .gap_1()
                                    .p_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(cx.theme().colors().border_variant)
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(
                                                Label::new(author.clone())
                                                    .size(LabelSize::Small)
                                                    .weight(gpui::FontWeight::SEMIBOLD),
                                            )
                                            .child(
                                                Label::new(when.clone())
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                            ),
                                    )
                                    .child(markdown_element(body, window, cx))
                            })),
                    )
                })
                .into_any_element(),
        };

        div()
            .id("linear-issue")
            .key_context("LinearIssue")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .bg(cx.theme().colors().editor_background)
            .child(
                // A readable measure rather than the pane's full width: an
                // issue is prose, and prose across a wide monitor is hard to
                // follow from one line to the next.
                div()
                    .w_full()
                    .max_w(rems(48.))
                    .mx_auto()
                    .px_6()
                    .py_6()
                    .child(content),
            )
    }
}

impl Focusable for IssueView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for IssueView {}

impl Item for IssueView {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        match &self.title {
            Some(title) => format!("{} {title}", self.identifier).into(),
            None => self.identifier.clone(),
        }
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.tab_content_text(0, cx))
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTodo))
    }

    fn tab_extra_context_menu_actions(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Vec<(SharedString, Box<dyn gpui::Action>)> {
        let Some(url) = &self.url else {
            return Vec::new();
        };
        vec![
            (
                "Create Worktree and Start Agent".into(),
                Box::new(CreateWorktree {
                    identifier: self.identifier.to_string(),
                    start_agent: true,
                }),
            ),
            ("Send to Agent".into(), Box::new(send_to_agent(url))),
        ]
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let identifier = self.identifier.clone();
        let project = self.project.clone();
        Task::ready(Some(
            cx.new(|cx| IssueView::new(identifier, project, window, cx)),
        ))
    }
}

/// Opens an issue's tab in the workspace behind a weak handle; for callers
/// that hold one rather than a lease on the workspace.
pub(crate) fn open_issue_in(
    workspace: &WeakEntity<Workspace>,
    identifier: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| open_issue(workspace, identifier, window, cx))
        .ok();
}
