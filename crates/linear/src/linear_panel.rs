//! The Linear panel: your issues, searched and filtered, one click from their
//! own tab or from a worktree of their own.
//!
//! What it shows lives on [`Linear`], not here — see the crate docs for why —
//! so a panel is only the search box and the drawing. There is one per
//! worktree, and each keeps its search box in step with the others'.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, SharedString, Task, WeakEntity, Window, actions, prelude::*,
};
use ui::{ContextMenu, ListItem, ListItemSpacing, PopoverMenu, Tooltip, prelude::*};
use ui_input::{ErasedEditor, InputField};
use workspace::{
    MultiWorkspace, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    API_KEY_ENV_VAR, API_KEY_SETTINGS_URL, AssigneeFilter, Connection, CreateWorktree,
    CycleFilter, Issue, IssueFilters, KeySource, Linear, LinearEvent, Named, StateType,
    issue_view::open_issue_in,
};

actions!(
    linear,
    [
        /// Opens the Linear panel, or moves focus into it if it is already
        /// open.
        ToggleFocus,
        /// Opens or closes the Linear panel.
        Toggle,
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<LinearPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<LinearPanel>(window, cx) {
                workspace.close_panel::<LinearPanel>(window, cx);
            }
        });
    })
    .detach();
}

pub struct LinearPanel {
    workspace: WeakEntity<Workspace>,
    multi_workspace: WeakEntity<MultiWorkspace>,
    linear: Entity<Linear>,
    focus_handle: FocusHandle,
    /// The editor itself rather than an `InputField`, for the reason the
    /// worktree panel gives for its filter box: the header is chrome.
    search: Arc<dyn ErasedEditor>,
    api_key: Entity<InputField>,
    connect_error: Option<SharedString>,
    _connect: Option<Task<()>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl LinearPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            let handle = workspace.clone();
            workspace.update_in(cx, |workspace, window, cx| {
                let multi_workspace = workspace
                    .multi_workspace()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("the Linear panel needs a multi workspace"))?;
                let linear = Linear::global(cx)
                    .ok_or_else(|| anyhow::anyhow!("`linear::init` has not run"))?;
                anyhow::Ok(cx.new(|cx| Self::new(handle, multi_workspace, linear, window, cx)))
            })?
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        multi_workspace: WeakEntity<MultiWorkspace>,
        linear: Entity<Linear>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = (ui_input::ERASED_EDITOR_FACTORY
            .get()
            .expect("the erased editor factory, which `editor::init` sets"))(
            window, cx
        );
        search.set_placeholder_text("Search issues", window, cx);
        let query = linear.read(cx).query().to_owned();
        search.set_text(&query, window, cx);

        let mut subscriptions = vec![search.subscribe(
            Box::new({
                let linear = linear.clone();
                let search = Arc::downgrade(&search);
                move |event, _window, cx| {
                    if event != ui_input::ErasedEditorEvent::BufferEdited {
                        return;
                    }
                    let Some(search) = search.upgrade() else {
                        return;
                    };
                    let query = search.text(cx);
                    linear.update(cx, |linear, cx| linear.set_query(&query, cx));
                }
            }),
            window,
            cx,
        )];
        // Another worktree's panel searched: this one's box should say what
        // the list it draws was searched for.
        subscriptions.push(cx.subscribe_in(
            &linear,
            window,
            |this: &mut Self, linear, _: &LinearEvent, window, cx| {
                let query = linear.read(cx).query().to_owned();
                if this.search.text(cx).trim() != query {
                    this.search.set_text(&query, window, cx);
                }
                cx.notify();
            },
        ));

        let api_key = cx.new(|cx| {
            InputField::new(window, cx, "lin_api_…")
                .label("Personal API key")
                .masked(true)
        });

        Self {
            workspace,
            multi_workspace,
            linear,
            focus_handle: cx.focus_handle(),
            search,
            api_key,
            connect_error: None,
            _connect: None,
            _subscriptions: subscriptions,
        }
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.api_key.read(cx).text(cx);
        let connecting = self
            .linear
            .update(cx, |linear, cx| linear.connect(key, cx));
        self.connect_error = None;
        self._connect = Some(cx.spawn_in(window, async move |this, cx| {
            let connected = connecting.await;
            this.update_in(cx, |this, window, cx| {
                match connected {
                    Ok(()) => this
                        .api_key
                        .update(cx, |api_key, cx| api_key.clear(window, cx)),
                    Err(error) => this.connect_error = Some(format!("{error:#}").into()),
                }
                this._connect = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn disconnect(linear: &Entity<Linear>, workspace: &WeakEntity<Workspace>, cx: &mut App) {
        let disconnecting = linear.update(cx, |linear, cx| linear.disconnect(cx));
        let workspace = workspace.clone();
        cx.spawn(async move |cx| {
            if let Err(error) = disconnecting.await {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.show_error(format!("Could not disconnect Linear: {error:#}"), cx)
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// The panel's own filters, which live on [`Linear`] so that every
    /// worktree's panel shows the same list.
    fn filter_target(&self) -> FilterTarget {
        let read_linear = self.linear.clone();
        let write_linear = self.linear.clone();
        FilterTarget::new(
            move |cx| Some(read_linear.read(cx).filters().clone()),
            move |change, cx| {
                write_linear.update(cx, |linear, cx| linear.update_filters(change, cx));
            },
            IssueFilters::default(),
        )
    }

    /// Opens an issue's tab. Deferred, for the reason the worktree panel's
    /// `activate` gives: focusing the new tab walks the docks, this panel
    /// among them, and a click handler holds this panel's lease.
    fn open_issue(&self, identifier: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            open_issue_in(&workspace, identifier, window, cx);
        });
    }

    /// Whether any *other* worktree of the window has the panel out; see
    /// [`Panel::starts_open`].
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
                let dock = workspace.read(cx).dock_at_position(DockPosition::Right);
                let dock = dock.read(cx);
                dock.is_open()
                    && dock
                        .active_panel()
                        .is_some_and(|panel| panel.persistent_name() == Self::persistent_name())
            })
    }

    fn render_connect(&self, error: Option<SharedString>, cx: &mut Context<Self>) -> AnyElement {
        let error = self.connect_error.clone().or(error);
        let connecting = self._connect.is_some();
        v_flex()
            .p_3()
            .gap_3()
            .child(Headline::new("Connect Linear").size(HeadlineSize::Small))
            .child(
                Label::new(
                    "Create a personal API key in Linear's settings and paste it here. \
                     Bench keeps it in your system keychain.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                Button::new("open-api-key-settings", "Create an API Key")
                    .start_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::Small))
                    .label_size(LabelSize::Small)
                    .on_click(|_, _, cx| cx.open_url(API_KEY_SETTINGS_URL)),
            )
            .child(
                div()
                    .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                        this.connect(window, cx);
                    }))
                    .child(self.api_key.clone()),
            )
            .child(
                Button::new("connect", if connecting { "Connecting…" } else { "Connect" })
                    .style(ButtonStyle::Filled)
                    .full_width()
                    .disabled(connecting)
                    .on_click(cx.listener(|this, _, window, cx| this.connect(window, cx))),
            )
            .when_some(error, |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                Label::new(format!("Or set {API_KEY_ENV_VAR} in Bench's environment."))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let linear = self.linear.read(cx);
        let (viewer, source) = match linear.connection() {
            Connection::Connected { viewer, source, .. } => (
                viewer.as_ref().map(|viewer| viewer.display_name.clone()),
                Some(*source),
            ),
            Connection::Loading | Connection::Disconnected { .. } => (None, None),
        };
        let loading = linear.is_loading();
        let menu_linear = self.linear.clone();
        let menu_workspace = self.workspace.clone();

        h_flex()
            .w_full()
            .px_1p5()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Icon::new(IconName::MagnifyingGlass)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.search.render(window, cx)),
            )
            .child(
                IconButton::new("open-dashboard", IconName::ChartBar)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Dashboard"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        // Deferred, for the reason `open_issue` gives.
                        let workspace = this.workspace.clone();
                        window.defer(cx, move |window, cx| {
                            crate::dashboard::open_dashboard_in(&workspace, window, cx);
                        });
                    })),
            )
            .child(filters_button(
                "linear-filters",
                self.filter_target(),
                self.linear.clone(),
                *self.linear.read(cx).filters() != IssueFilters::default(),
            ))
            .child(
                IconButton::new("refresh-issues", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .disabled(loading)
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.linear.update(cx, |linear, cx| linear.refresh(cx));
                    })),
            )
            .child(
                PopoverMenu::new("linear-menu")
                    .trigger_with_tooltip(
                        IconButton::new("linear-menu-trigger", IconName::Ellipsis)
                            .icon_size(IconSize::Small),
                        Tooltip::text("Linear"),
                    )
                    .anchor(gpui::Anchor::TopRight)
                    .menu(move |window, cx| {
                        let linear = menu_linear.clone();
                        let workspace = menu_workspace.clone();
                        let viewer = viewer.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            menu.when_some(viewer, |menu, viewer| {
                                menu.header(format!("Signed in as {viewer}"))
                            })
                            .entry("Open Linear", None, |_, cx| {
                                cx.open_url("https://linear.app")
                            })
                            .separator()
                            .map(|menu| {
                                if source == Some(KeySource::Environment) {
                                    menu.toggleable_entry_disabled_when(
                                        format!("Disconnect (set by {API_KEY_ENV_VAR})"),
                                        false,
                                        true,
                                        IconPosition::Start,
                                        None,
                                        |_, _| {},
                                    )
                                } else {
                                    menu.entry("Disconnect", None, move |_, cx| {
                                        Self::disconnect(&linear, &workspace, cx);
                                    })
                                }
                            })
                        }))
                    }),
            )
    }

    fn render_issue(&self, issue: &Issue, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let identifier = issue.identifier.clone();
        let create_identifier = issue.identifier.to_string();
        // From the panel's own node rather than whatever has focus: the
        // handler is the workspace's, and focus is not always inside it.
        let focus_handle = self.focus_handle.clone();
        ListItem::new(("issue", index))
            .spacing(ListItemSpacing::Sparse)
            .height(ROW_HEIGHT)
            .start_slot(
                Icon::new(issue.state.kind.icon())
                    .size(IconSize::Small)
                    .map(|icon| match issue.state.color() {
                        Some(color) => icon.color(Color::Custom(color)),
                        None => icon.color(Color::Muted),
                    }),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        Label::new(issue.identifier.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line(),
                    )
                    .child(
                        Label::new(issue.title.clone())
                            .size(LabelSize::Small)
                            .single_line()
                            .truncate(),
                    ),
            )
            .tooltip(Tooltip::text(format!(
                "{} · {}\n{}",
                issue.identifier, issue.state.name, issue.title
            )))
            .end_slot_on_hover(
                IconButton::new(("create-worktree", index), IconName::GitBranchPlus)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Create Worktree"))
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        focus_handle.dispatch_action(
                            &CreateWorktree {
                                identifier: create_identifier.clone(),
                            },
                            window,
                            cx,
                        );
                    }),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_issue(identifier.clone(), window, cx);
            }))
    }

    fn render_issues(&self, cx: &mut Context<Self>) -> AnyElement {
        let linear = self.linear.read(cx);
        let issues = linear.issues().to_vec();
        let error = linear.list_error().cloned();
        let loading = linear.is_loading();

        if let Some(error) = error {
            return v_flex()
                .p_3()
                .gap_2()
                .child(Label::new("Could not load issues").size(LabelSize::Small))
                .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                .child(
                    Button::new("retry-issues", "Try Again")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.linear.update(cx, |linear, cx| linear.refresh(cx));
                        })),
                )
                .into_any_element();
        }
        if issues.is_empty() {
            return v_flex()
                .p_3()
                .child(
                    Label::new(if loading { "Loading…" } else { "No issues match" })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element();
        }
        let mut rows = Vec::with_capacity(issues.len());
        for (index, issue) in issues.iter().enumerate() {
            rows.push(self.render_issue(issue, index, cx).into_any_element());
        }
        v_flex()
            .id("linear-issues")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .py_1()
            .children(rows)
            .into_any_element()
    }
}

/// Whose filters a filter menu shows and changes: the panel's, or a
/// dashboard's. Each has its own, and its own defaults to reset to.
#[derive(Clone)]
pub(crate) struct FilterTarget {
    read: Rc<dyn Fn(&App) -> Option<IssueFilters>>,
    write: Rc<dyn Fn(Box<dyn FnOnce(&mut IssueFilters)>, &mut App)>,
    defaults: IssueFilters,
}

impl FilterTarget {
    pub(crate) fn new(
        read: impl Fn(&App) -> Option<IssueFilters> + 'static,
        write: impl Fn(Box<dyn FnOnce(&mut IssueFilters)>, &mut App) + 'static,
        defaults: IssueFilters,
    ) -> Self {
        Self {
            read: Rc::new(read),
            write: Rc::new(write),
            defaults,
        }
    }

    pub(crate) fn change(&self, cx: &mut App, change: impl FnOnce(&mut IssueFilters) + 'static) {
        (self.write)(Box::new(change), cx);
    }
}

/// Every filter, in one menu behind one button: the filters are set once and
/// then left alone, and a row of them took room from what they filter.
///
/// Persistent, so it stays open while you tick through it. Projects and labels
/// are submenus because there can be hundreds of them; the projects and labels
/// on offer are the ones [`Linear`] fetched when it connected.
pub(crate) fn filters_menu(
    target: FilterTarget,
    linear: Entity<Linear>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build_persistent(window, cx, move |mut menu, _, cx| {
        let Some(filters) = (target.read)(cx) else {
            return menu;
        };
        let catalog = linear.read(cx).catalog().clone();

        menu = menu.header("Assignee");
        for (label, assignee) in [
            ("Assigned to me", AssigneeFilter::Me),
            ("Unassigned", AssigneeFilter::Unassigned),
            ("Anyone", AssigneeFilter::Anyone),
        ] {
            let target = target.clone();
            menu = menu.toggleable_entry(
                label,
                filters.assignee == assignee,
                IconPosition::Start,
                None,
                move |_, cx| target.change(cx, move |filters| filters.assignee = assignee),
            );
        }

        menu = menu.separator().header("Status");
        for state in StateType::ALL {
            let target = target.clone();
            menu = menu.toggleable_entry(
                state.label(),
                filters.states.contains(&state),
                IconPosition::Start,
                None,
                move |_, cx| target.change(cx, move |filters| toggle(&mut filters.states, state)),
            );
        }

        menu = menu.separator().header("Cycle");
        for (label, cycle) in [
            ("Any cycle", CycleFilter::Any),
            ("Current cycle", CycleFilter::Current),
            ("No cycle", CycleFilter::None),
        ] {
            let target = target.clone();
            menu = menu.toggleable_entry(
                label,
                filters.cycle == cycle,
                IconPosition::Start,
                None,
                move |_, cx| target.change(cx, move |filters| filters.cycle = cycle),
            );
        }

        let project_label = match &filters.project {
            Some(project) => format!("Project: {}", project.name),
            None => "Project: Any".to_string(),
        };
        let labels_label = match filters.labels.as_slice() {
            [] => "Labels: Any".to_string(),
            [label] => format!("Labels: {}", label.name),
            labels => format!("Labels: {} selected", labels.len()),
        };
        menu = menu
            .separator()
            .submenu(project_label, {
                let target = target.clone();
                let projects = catalog.projects.clone();
                let current = filters.project.clone();
                move |mut menu, _, _| {
                    menu = menu.toggleable_entry(
                        "Any project",
                        current.is_none(),
                        IconPosition::Start,
                        None,
                        {
                            let target = target.clone();
                            move |_, cx| target.change(cx, |filters| filters.project = None)
                        },
                    );
                    for project in projects.iter().cloned() {
                        let target = target.clone();
                        let selected = current.as_ref() == Some(&project);
                        menu = menu.toggleable_entry(
                            project.name.clone(),
                            selected,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                let project = project.clone();
                                target.change(cx, move |filters| filters.project = Some(project))
                            },
                        );
                    }
                    menu
                }
            })
            .submenu(labels_label, {
                let target = target.clone();
                let labels = catalog.labels;
                let current = filters.labels.clone();
                move |mut menu, _, _| {
                    if labels.is_empty() {
                        return menu.header("No labels");
                    }
                    for label in labels.iter().cloned() {
                        let target = target.clone();
                        let selected = current.contains(&label);
                        menu = menu.toggleable_entry(
                            label.name.clone(),
                            selected,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                let label = label.clone();
                                target.change(cx, move |filters| {
                                    toggle_named(&mut filters.labels, label)
                                })
                            },
                        );
                    }
                    menu
                }
            });

        if filters != target.defaults {
            let target = target.clone();
            menu = menu.separator().entry("Reset Filters", None, move |_, cx| {
                let defaults = target.defaults.clone();
                target.change(cx, move |filters| *filters = defaults)
            });
        }
        menu
    })
}

/// The button a filter menu hangs from, lit while anything is filtered beyond
/// the defaults (`changed`), so a short list is never a mystery.
///
/// `changed` is the caller's to say because the button is drawn as part of
/// its owner: reading the filters through `target` here would read the owner
/// while it is drawing, which is a double lease and a crash.
pub(crate) fn filters_button(
    id: &'static str,
    target: FilterTarget,
    linear: Entity<Linear>,
    changed: bool,
) -> impl IntoElement {
    PopoverMenu::new(SharedString::from(id))
        .trigger_with_tooltip(
            IconButton::new(SharedString::from(format!("{id}-trigger")), IconName::Sliders)
                .icon_size(IconSize::Small)
                .toggle_state(changed),
            Tooltip::text("Filters"),
        )
        .anchor(gpui::Anchor::TopRight)
        .menu(move |window, cx| Some(filters_menu(target.clone(), linear.clone(), window, cx)))
}

/// The height of an issue row. The same as the worktree panel's, so the two
/// read as one sidebar when you switch between them; in rems, so that it
/// follows the UI font size.
const ROW_HEIGHT: Rems = Rems(2.25);

fn toggle(states: &mut Vec<StateType>, state: StateType) {
    match states.iter().position(|held| *held == state) {
        Some(index) => {
            states.remove(index);
        }
        None => states.push(state),
    }
}

fn toggle_named(names: &mut Vec<Named>, name: Named) {
    match names.iter().position(|held| held.id == name.id) {
        Some(index) => {
            names.remove(index);
        }
        None => names.push(name),
    }
}

impl Render for LinearPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.linear.read(cx).connection() {
            Connection::Loading => v_flex()
                .p_3()
                .child(
                    Label::new("Loading…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            Connection::Disconnected { error } => {
                let error = error.clone();
                self.render_connect(error, cx)
            }
            Connection::Connected { .. } => v_flex()
                .size_full()
                .child(self.render_header(window, cx))
                .child(self.render_issues(cx))
                .into_any_element(),
        };

        v_flex()
            .key_context("LinearPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(body)
    }
}

impl Focusable for LinearPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for LinearPanel {}

impl Panel for LinearPanel {
    fn persistent_name() -> &'static str {
        "LinearPanel"
    }

    fn panel_key() -> &'static str {
        "LinearPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
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
        px(320.)
    }

    /// Out in one worktree is out in the next; see the worktree panel's
    /// `starts_open` for why a dock's open state has to follow the window
    /// rather than the worktree in Bench.
    fn starts_open(&self, _window: &Window, cx: &App) -> bool {
        self.showing_in_another_worktree(cx)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Linear")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}
