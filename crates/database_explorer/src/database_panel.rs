//! The database explorer in the right dock: the saved connections, and each
//! one's databases, schemas, tables and columns as far as they have been
//! opened.
//!
//! The tree is flattened into rows on every render rather than kept as a list
//! that has to be patched when a level loads. A catalog is small enough that
//! rebuilding is cheaper than keeping two representations in step.

use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AsyncWindowContext, ClipboardItem, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, MouseDownEvent, Point, Subscription, Task, UniformListScrollHandle,
    WeakEntity, Window, actions, uniform_list,
};
use ui::{ContextMenu, ListItem, ListItemSpacing, Tab, Tooltip, WithScrollbar as _, prelude::*};
use workspace::{
    MultiWorkspace, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    ConnectionId, DatabaseStore, DatabaseStoreEvent, Load, ObjectKind, ObjectPath, TableKind,
    connection_modal::ConnectionModal, describe, describe_all, query_view, quote_identifier,
    send_to_agent,
};

actions!(
    database,
    [
        /// Opens the database panel, or moves focus into it if it is already
        /// open.
        ToggleFocus,
        /// Opens or closes the database panel.
        Toggle,
        /// Adds a Postgres connection.
        NewConnection,
    ]
);

/// The height the Linear panel's rows use, so the two docked panels read as
/// one list rather than two densities.
const ROW_HEIGHT: Rems = Rems(2.25);
const INDENT: Pixels = px(12.);
/// What "View Data" asks for, which is a screenful and then some rather than
/// a table.
const PREVIEW_ROWS: usize = 500;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<DatabasePanel>(window, cx) {
                workspace.close_panel::<DatabasePanel>(window, cx);
            }
        });
        workspace.register_action(|workspace, _: &NewConnection, window, cx| {
            ConnectionModal::toggle(workspace, None, window, cx);
        });
    })
    .detach();
}

/// One line of the flattened tree.
enum Row {
    Object {
        path: ObjectPath,
        depth: usize,
        label: SharedString,
        detail: Option<SharedString>,
        icon: IconName,
        /// `None` for a leaf, which has no disclosure.
        expanded: Option<bool>,
        tooltip: Option<SharedString>,
    },
    /// "Loading…", an error, or "no tables here" under the row it belongs to.
    Message {
        depth: usize,
        text: SharedString,
        is_error: bool,
    },
}

/// The children of a level that has them, or the one line saying why it has
/// none to show: still loading, failed, or empty.
fn level<'a, T>(
    rows: &mut Vec<Row>,
    load: &'a Load<Vec<T>>,
    depth: usize,
    empty: &'static str,
) -> Option<&'a Vec<T>> {
    match load {
        Load::Unloaded | Load::Loading => rows.push(Row::Message {
            depth,
            text: "Loading…".into(),
            is_error: false,
        }),
        Load::Failed(error) => rows.push(Row::Message {
            depth,
            text: error.clone(),
            is_error: true,
        }),
        Load::Loaded(values) if values.is_empty() => rows.push(Row::Message {
            depth,
            text: empty.into(),
            is_error: false,
        }),
        Load::Loaded(values) => return Some(values),
    }
    None
}

pub struct DatabasePanel {
    workspace: WeakEntity<Workspace>,
    multi_workspace: WeakEntity<MultiWorkspace>,
    store: Entity<DatabaseStore>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    /// The menu of a row, while it is open.
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    /// The rows picked out, so that one menu can send several to the agent.
    /// Held as a set because the tree's own order is what they are sent in,
    /// and that is read back off the rows at the time.
    selected: HashSet<ObjectPath>,
    /// Where a shift-click measures from.
    anchor: Option<ObjectPath>,
    _subscriptions: Vec<Subscription>,
}

impl DatabasePanel {
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
                    .ok_or_else(|| anyhow::anyhow!("the database panel needs a multi workspace"))?;
                let store = DatabaseStore::global(cx)
                    .ok_or_else(|| anyhow::anyhow!("`database_explorer::init` has not run"))?;
                anyhow::Ok(cx.new(|cx| Self::new(handle, multi_workspace, store, cx)))
            })?
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        multi_workspace: WeakEntity<MultiWorkspace>,
        store: Entity<DatabaseStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![cx.subscribe(&store, |_, _, _: &DatabaseStoreEvent, cx| {
            cx.notify();
        })];
        Self {
            workspace,
            multi_workspace,
            store,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            context_menu: None,
            selected: HashSet::default(),
            anchor: None,
            _subscriptions: subscriptions,
        }
    }

    /// The tree as it stands, one entry per visible line.
    fn rows(&self, cx: &App) -> Vec<Row> {
        let store = self.store.read(cx);
        let mut rows = Vec::new();
        for config in store.connections() {
            let connection = ObjectPath::connection(config.id);
            let expanded = store.is_expanded(&connection);
            rows.push(Row::Object {
                path: connection,
                depth: 0,
                label: config.name.clone(),
                detail: Some(SharedString::from(format!(
                    "{}:{}",
                    config.host, config.port
                ))),
                icon: IconName::Server,
                expanded: Some(expanded),
                tooltip: Some(SharedString::from(config.url())),
            });
            if !expanded {
                continue;
            }
            let Some(databases) = level(&mut rows, store.databases(config.id), 1, "No databases")
            else {
                continue;
            };

            for database in databases {
                let path = ObjectPath::database(config.id, database.clone());
                let expanded = store.is_expanded(&path);
                rows.push(Row::Object {
                    path,
                    depth: 1,
                    label: database.clone(),
                    detail: None,
                    icon: IconName::DatabaseZap,
                    expanded: Some(expanded),
                    tooltip: None,
                });
                if !expanded {
                    continue;
                }
                let Some(schemas) = level(
                    &mut rows,
                    store.schemas(config.id, database),
                    2,
                    "No schemas",
                ) else {
                    continue;
                };

                for schema in schemas {
                    let path = ObjectPath::schema(config.id, database.clone(), schema.clone());
                    let expanded = store.is_expanded(&path);
                    rows.push(Row::Object {
                        path,
                        depth: 2,
                        label: schema.clone(),
                        detail: None,
                        icon: IconName::Folder,
                        expanded: Some(expanded),
                        tooltip: None,
                    });
                    if !expanded {
                        continue;
                    }
                    let Some(tables) = level(
                        &mut rows,
                        store.tables(config.id, database, schema),
                        3,
                        "No tables",
                    ) else {
                        continue;
                    };

                    for table in tables {
                        let path = ObjectPath::table(
                            config.id,
                            database.clone(),
                            schema.clone(),
                            table.name.clone(),
                        );
                        let expanded = store.is_expanded(&path);
                        rows.push(Row::Object {
                            path,
                            depth: 3,
                            label: table.name.clone(),
                            detail: (table.kind != TableKind::Table)
                                .then(|| SharedString::from(table.kind.label())),
                            icon: IconName::Table,
                            expanded: Some(expanded),
                            tooltip: Some(SharedString::from(format!(
                                "{}.{} ({})",
                                schema,
                                table.name,
                                table.kind.label()
                            ))),
                        });
                        if !expanded {
                            continue;
                        }
                        let Some(columns) = level(
                            &mut rows,
                            store.columns(config.id, database, schema, &table.name),
                            4,
                            "No columns",
                        ) else {
                            continue;
                        };

                        for column in columns {
                            rows.push(Row::Object {
                                path: ObjectPath::column(
                                    config.id,
                                    database.clone(),
                                    schema.clone(),
                                    table.name.clone(),
                                    column.name.clone(),
                                ),
                                depth: 4,
                                label: column.name.clone(),
                                detail: Some(column.data_type.clone()),
                                icon: IconName::Hash,
                                expanded: None,
                                tooltip: Some(SharedString::from(column.signature())),
                            });
                        }
                    }
                }
            }
        }
        rows
    }

    fn toggle(&mut self, path: ObjectPath, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.toggle_expanded(path, cx));
    }

    /// Picks out rows the way a tree is expected to: the platform modifier
    /// adds or removes one, shift takes everything between the last pick and
    /// this one, and a plain click picks only this row.
    fn select(&mut self, path: &ObjectPath, modifiers: gpui::Modifiers, cx: &mut Context<Self>) {
        if modifiers.platform || modifiers.control {
            if !self.selected.remove(path) {
                self.selected.insert(path.clone());
            }
        } else if modifiers.shift {
            let rows = self.rows(cx);
            let paths: Vec<&ObjectPath> = rows
                .iter()
                .filter_map(|row| match row {
                    Row::Object { path, .. } => Some(path),
                    Row::Message { .. } => None,
                })
                .collect();
            let anchor = self.anchor.as_ref().unwrap_or(path);
            let index = |wanted: &ObjectPath| paths.iter().position(|path| *path == wanted);
            if let (Some(from), Some(to)) = (index(anchor), index(path)) {
                let (first, last) = if from <= to { (from, to) } else { (to, from) };
                self.selected = paths[first..=last].iter().map(|&p| p.clone()).collect();
            }
            cx.notify();
            return;
        } else {
            self.selected = HashSet::from_iter([path.clone()]);
        }
        self.anchor = Some(path.clone());
        cx.notify();
    }

    /// The picked-out rows in the order the tree shows them, so that what is
    /// sent reads top to bottom.
    fn selected_paths(&self, cx: &App) -> Vec<ObjectPath> {
        if self.selected.is_empty() {
            return Vec::new();
        }
        self.rows(cx)
            .into_iter()
            .filter_map(|row| match row {
                Row::Object { path, .. } if self.selected.contains(&path) => Some(path),
                _ => None,
            })
            .collect()
    }

    /// Opens a SQL console against the database a row sits in, with `sql`
    /// already typed, running it when `run` is set.
    fn open_console(
        &self,
        connection: ConnectionId,
        database: SharedString,
        sql: String,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        // Deferred for the reason the Linear panel gives: activating the new
        // tab walks the docks, this panel among them, while a click handler
        // still holds this panel's lease.
        window.defer(cx, move |window, cx| {
            query_view::open_console(&workspace, connection, database, sql, run, window, cx);
        });
    }

    /// Opens a console on a table's first rows, the way double-clicking one is
    /// expected to. Says whether the row was a table, since nothing else has
    /// data to show.
    fn view_data(&self, path: &ObjectPath, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (Some(schema), Some(table)) = (&path.schema, &path.table) else {
            return false;
        };
        if path.column.is_some() {
            return false;
        }
        let sql = preview_statement(schema, table, PREVIEW_ROWS);
        let database = self.database_of(path, cx);
        self.open_console(path.connection, database, sql, true, window, cx);
        true
    }

    /// The database a row belongs to: its own when it is one, the
    /// connection's when the row is above that level.
    fn database_of(&self, path: &ObjectPath, cx: &App) -> SharedString {
        path.database.clone().unwrap_or_else(|| {
            self.store
                .read(cx)
                .connection(path.connection)
                .map(|config| SharedString::from(config.database.clone()))
                .unwrap_or_default()
        })
    }

    fn deploy_menu(
        &mut self,
        path: ObjectPath,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Right-clicking outside the selection moves it, the way every tree
        // does; right-clicking inside it keeps it, so a menu can act on many.
        if !self.selected.contains(&path) {
            self.select(&path, gpui::Modifiers::default(), cx);
        }
        let chosen = self.selected_paths(cx);
        let store = self.store.read(cx);
        let Some(config) = store.connection(path.connection) else {
            return;
        };
        // Everything but "Send to Agent" acts on the row that was clicked;
        // only sending has anything sensible to do with several at once.
        let send_label = if chosen.len() > 1 {
            format!("Send {} Items to Agent", chosen.len())
        } else {
            "Send to Agent".to_owned()
        };
        let send = send_to_agent(if chosen.len() > 1 {
            describe_all(store, &chosen)
        } else {
            describe(store, &path)
        });
        let database = self.database_of(&path, cx);
        let kind = path.kind();
        let connection_name = config.name.clone();
        // The panel's own node rather than whatever has focus: `SendText` is
        // handled by the workspace, and focus is not always inside it.
        let focus_handle = self.focus_handle.clone();
        let panel = cx.entity().downgrade();

        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            menu = menu.context(focus_handle);

            if kind == ObjectKind::Table {
                let preview = match (&path.schema, &path.table) {
                    (Some(schema), Some(table)) => {
                        Some(preview_statement(schema, table, PREVIEW_ROWS))
                    }
                    _ => None,
                };
                if let Some(preview) = preview {
                    menu = menu.entry("View Data", None, {
                        let panel = panel.clone();
                        let database = database.clone();
                        let connection = path.connection;
                        move |window, cx| {
                            panel
                                .update(cx, |panel, cx| {
                                    panel.open_console(
                                        connection,
                                        database.clone(),
                                        preview.clone(),
                                        true,
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        }
                    });
                }
            }

            if kind != ObjectKind::Column {
                menu = menu.entry("New Query", None, {
                    let panel = panel.clone();
                    let database = database.clone();
                    let connection = path.connection;
                    let prefill = match (&path.schema, &path.table) {
                        (Some(schema), Some(table)) => preview_statement(schema, table, 100),
                        _ => String::new(),
                    };
                    move |window, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.open_console(
                                    connection,
                                    database.clone(),
                                    prefill.clone(),
                                    false,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    }
                });
            }

            if matches!(kind, ObjectKind::Table | ObjectKind::Column) {
                let name = match (&path.schema, &path.table, &path.column) {
                    (Some(schema), Some(table), None) => format!("{schema}.{table}"),
                    (Some(schema), Some(table), Some(column)) => {
                        format!("{schema}.{table}.{column}")
                    }
                    _ => String::new(),
                };
                menu = menu.entry("Copy Name", None, move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
                });
            }

            if kind != ObjectKind::Column {
                menu = menu.entry("Refresh", None, {
                    let panel = panel.clone();
                    let path = path.clone();
                    move |_window, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                let path = path.clone();
                                panel.store.update(cx, |store, cx| store.refresh(path, cx));
                            })
                            .ok();
                    }
                });
            }

            if kind == ObjectKind::Connection {
                let connection = path.connection;
                menu = menu.separator().entry("Edit Connection…", None, {
                    let panel = panel.clone();
                    move |window, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.edit_connection(connection, window, cx);
                            })
                            .ok();
                    }
                });
                menu = menu.entry("Remove Connection", None, {
                    let panel = panel.clone();
                    let connection_name = connection_name.clone();
                    move |window, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.confirm_remove(
                                    connection,
                                    connection_name.clone(),
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    }
                });
            }

            menu.separator().action(send_label, Box::new(send))
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

    fn edit_connection(
        &self,
        connection: ConnectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    ConnectionModal::toggle(workspace, Some(connection), window, cx);
                })
                .ok();
        });
    }

    /// Removing a connection also drops its password; worth asking first.
    fn confirm_remove(
        &self,
        connection: ConnectionId,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Remove the connection “{name}”?"),
            Some("Its saved password is removed from the keychain too."),
            &["Remove", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if prompt.await.ok() == Some(0) {
                this.update(cx, |this, cx| {
                    this.store
                        .update(cx, |store, cx| store.remove_connection(connection, cx));
                })
                .ok();
            }
        })
        .detach();
    }

    fn new_connection(&self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    ConnectionModal::toggle(workspace, None, window, cx);
                })
                .ok();
        });
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            // The height the tab bar and the other panels' headers use, so
            // that the top of the right dock lines up across them.
            .h(Tab::container_height(cx))
            .px_2()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("Databases")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                IconButton::new("new-connection", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("New Postgres Connection"))
                    .on_click(cx.listener(|this, _, window, cx| this.new_connection(window, cx))),
            )
    }

    fn render_row(&self, row: &Row, index: usize, cx: &mut Context<Self>) -> AnyElement {
        match row {
            Row::Message {
                depth,
                text,
                is_error,
            } => ListItem::new(("database-message", index))
                .spacing(ListItemSpacing::Sparse)
                .height(ROW_HEIGHT)
                .indent_level(depth + 1)
                .indent_step_size(INDENT)
                .selectable(false)
                .child(
                    Label::new(text.clone())
                        .size(LabelSize::Small)
                        .color(if *is_error {
                            Color::Error
                        } else {
                            Color::Muted
                        })
                        .single_line()
                        .truncate(),
                )
                .into_any_element(),
            Row::Object {
                path,
                depth,
                label,
                detail,
                icon,
                expanded,
                tooltip,
            } => {
                let toggle_path = path.clone();
                let click_path = path.clone();
                let menu_path = path.clone();
                let refresh_path = path.clone();
                let is_selected = self.selected.contains(path);
                let expandable = expanded.is_some();
                ListItem::new(("database-object", index))
                    .spacing(ListItemSpacing::Sparse)
                    .height(ROW_HEIGHT)
                    .indent_level(*depth)
                    .indent_step_size(INDENT)
                    .toggle(*expanded)
                    // `Disclosure` does not stop the click, so without this the
                    // row's own handler would toggle the same row straight back.
                    .on_toggle(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle(toggle_path.clone(), cx);
                    }))
                    .start_slot(Icon::new(*icon).size(IconSize::Small).color(Color::Muted))
                    .when(path.kind() == ObjectKind::Connection, |this| {
                        this.end_slot(
                            IconButton::new(("refresh-connection", index), IconName::RotateCw)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(
                                    "Reload this connection's databases, tables and columns",
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    let connection = refresh_path.connection;
                                    this.store.update(cx, |store, cx| {
                                        store.refresh_connection(connection, cx);
                                    });
                                })),
                        )
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap_1p5()
                            .child(
                                Label::new(label.clone())
                                    .size(LabelSize::Small)
                                    .single_line()
                                    .truncate(),
                            )
                            .when_some(detail.clone(), |this, detail| {
                                this.child(
                                    Label::new(detail)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .single_line()
                                        .truncate(),
                                )
                            }),
                    )
                    .when_some(tooltip.clone(), |this, tooltip| {
                        this.tooltip(Tooltip::text(tooltip))
                    })
                    // A column is a leaf: clicking it must not put it in the
                    // set of opened rows, which nothing would ever take it
                    // back out of.
                    .toggle_state(is_selected)
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            let modifiers = event.modifiers();
                            this.select(&click_path, modifiers, cx);
                            // A modifier click is building a selection, not
                            // asking for the row to open.
                            if modifiers.platform || modifiers.control || modifiers.shift {
                                return;
                            }
                            if event.click_count() >= 2 && this.view_data(&click_path, window, cx) {
                                return;
                            }
                            // A column is a leaf: expanding it would put it in
                            // the set of opened rows, which nothing would ever
                            // take it back out of.
                            if expandable {
                                this.toggle(click_path.clone(), cx);
                            }
                        }),
                    )
                    .on_secondary_mouse_down(cx.listener(
                        move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.deploy_menu(menu_path.clone(), event.position, window, cx);
                        },
                    ))
                    .into_any_element()
            }
        }
    }

    /// No button of its own: the header's + is right above this.
    fn render_empty() -> AnyElement {
        v_flex()
            .p_3()
            .child(
                Label::new("No connections yet. Add one with + above.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
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
}

impl Render for DatabasePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = Rc::new(self.rows(cx));
        let body = if rows.is_empty() {
            Self::render_empty()
        } else {
            uniform_list(
                "database-tree",
                rows.len(),
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    range
                        .filter_map(|index| {
                            let row = rows.get(index)?;
                            Some(this.render_row(row, index, cx))
                        })
                        .collect()
                }),
            )
            .track_scroll(&self.scroll_handle)
            .flex_grow(1.)
            .into_any_element()
        };

        v_flex()
            .key_context("DatabasePanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .on_action(
                cx.listener(|this, _: &NewConnection, window, cx| this.new_connection(window, cx)),
            )
            .child(self.render_header(cx))
            .child(
                v_flex()
                    .id("database-tree-body")
                    .flex_1()
                    .min_h_0()
                    .py_1()
                    .child(body)
                    .vertical_scrollbar_for(&self.scroll_handle, window, cx),
            )
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

impl Focusable for DatabasePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DatabasePanel {}

impl Panel for DatabasePanel {
    fn persistent_name() -> &'static str {
        "DatabasePanel"
    }

    fn panel_key() -> &'static str {
        "DatabasePanel"
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
    /// `starts_open` for why a dock's open state follows the window rather
    /// than the worktree in Bench.
    fn starts_open(&self, _window: &Window, cx: &App) -> bool {
        self.showing_in_another_worktree(cx)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::DatabaseZap)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Databases")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        9
    }
}

/// `SELECT * FROM "schema"."table" LIMIT n`, for the console a table opens.
pub(crate) fn preview_statement(schema: &str, table: &str, limit: usize) -> String {
    format!(
        "SELECT *\nFROM {}.{}\nLIMIT {limit};",
        quote_identifier(schema),
        quote_identifier(table)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_quotes_what_it_selects_from() {
        assert_eq!(
            preview_statement("public", "users", 500),
            "SELECT *\nFROM \"public\".\"users\"\nLIMIT 500;"
        );
    }
}
