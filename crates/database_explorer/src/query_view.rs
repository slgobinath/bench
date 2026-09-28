//! The SQL console: a tab holding an editor, a Run button and the rows that
//! came back.
//!
//! A console belongs to one database of one connection — the one the tree row
//! it was opened from sits in — so that "run this" never has to ask where.

use std::collections::BTreeSet;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use editor::Editor;
use gpui::{
    AnyElement, App, ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    MouseButton, MouseDownEvent, Point, Subscription, Task, WeakEntity, Window, actions,
    prelude::*,
};
use ui::{
    ColumnWidthConfig, ContextMenu, ResizableColumnsState, Table, TableInteractionState,
    TableResizeBehavior, Tooltip, UncheckedTableRow, prelude::*,
};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

use crate::{ConnectionId, DatabaseStore, MAX_ROWS, QueryResult, describe_rows, send_to_agent};

actions!(
    database,
    [
        /// Runs the SQL console's statement, or the selection when there is
        /// one.
        RunQuery,
    ]
);

/// How wide a result column starts out. Postgres says nothing useful about
/// how wide a value will be, so every column starts the same and is dragged
/// from there.
const COLUMN_WIDTH: f32 = 160.;
const ROW_NUMBER_WIDTH: f32 = 52.;
/// Tall enough to read a row at a glance rather than to fit the most rows on
/// screen; a grid packed to the line height is hard to track across.
const RESULT_ROW_HEIGHT: Rems = Rems(1.75);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &RunQuery, window, cx| {
            if let Some(console) = workspace.active_item_as::<QueryView>(cx) {
                console.update(cx, |console, cx| console.run(window, cx));
            }
        });
    })
    .detach();
}

/// Opens a console in the workspace's active pane.
pub(crate) fn open_console(
    workspace: &WeakEntity<Workspace>,
    connection: ConnectionId,
    database: SharedString,
    sql: String,
    run: bool,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| {
            let Some(store) = DatabaseStore::global(cx) else {
                return;
            };
            let languages = workspace.project().read(cx).languages().clone();
            let console = cx.new(|cx| {
                QueryView::new(
                    store,
                    connection,
                    database,
                    sql,
                    Some(languages),
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(console.clone()), None, true, window, cx);
            if run {
                console.update(cx, |console, cx| console.run(window, cx));
            }
        })
        .log_err();
}

/// One width per result column, behind the row-number column at the front.
/// `ResizableColumnsState` wants a width and a behaviour for every column, so
/// this is the only place that decides how many there are.
fn column_widths(columns: usize, cx: &mut App) -> Entity<ResizableColumnsState> {
    let count = columns + 1;
    let mut widths = vec![px(ROW_NUMBER_WIDTH)];
    widths.extend(std::iter::repeat_n(px(COLUMN_WIDTH), columns));
    let behavior = std::iter::repeat_n(TableResizeBehavior::MinSize(2.), count).collect();
    cx.new(|_| ResizableColumnsState::new(count, widths, behavior))
}

enum State {
    Idle,
    Running,
    Done(QueryResult),
    Failed(SharedString),
}

pub struct QueryView {
    store: Entity<DatabaseStore>,
    connection: ConnectionId,
    database: SharedString,
    editor: Entity<Editor>,
    state: State,
    table_interaction: Entity<TableInteractionState>,
    /// Rebuilt whenever a result's column count changes, since the widths are
    /// per column.
    column_widths: Entity<ResizableColumnsState>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    /// Which result rows are picked out, by index, kept ordered so that what
    /// is sent to the agent reads in the grid's own order.
    selected: BTreeSet<usize>,
    /// Where a shift-click measures from.
    anchor: Option<usize>,
    focus_handle: FocusHandle,
    _run: Option<Task<()>>,
}

impl QueryView {
    fn new(
        store: Entity<DatabaseStore>,
        connection: ConnectionId,
        database: SharedString,
        sql: String,
        languages: Option<Arc<language::LanguageRegistry>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text("SELECT …", window, cx);
            if !sql.is_empty() {
                editor.set_text(sql, window, cx);
            }
            editor
        });

        // SQL highlighting comes from the SQL extension when it is installed;
        // without it the console is plain text rather than broken.
        if let Some(languages) = languages {
            cx.spawn({
                let editor = editor.clone();
                async move |_, cx| {
                    // Not having the SQL extension installed is not a
                    // failure; the console is plain text then.
                    let Ok(sql) = languages.language_for_name("SQL").await else {
                        return;
                    };
                    editor.update(cx, |editor, cx| {
                        if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                            buffer.update(cx, |buffer, cx| buffer.set_language(Some(sql), cx));
                        }
                    });
                }
            })
            .detach();
        }

        Self {
            store,
            connection,
            database,
            editor,
            state: State::Idle,
            table_interaction: cx.new(|cx| TableInteractionState::new(cx)),
            column_widths: column_widths(0, cx),
            context_menu: None,
            selected: BTreeSet::new(),
            anchor: None,
            focus_handle: cx.focus_handle(),
            _run: None,
        }
    }

    /// The selection when there is one, the whole console when there is not —
    /// the way a SQL console is expected to behave.
    fn sql_to_run(&self, cx: &App) -> String {
        let editor = self.editor.read(cx);
        let selection = *editor.selections.newest_anchor();
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let selected: String = snapshot
            .text_for_range(selection.start..selection.end)
            .collect();
        if selected.trim().is_empty() {
            editor.text(cx)
        } else {
            selected
        }
    }

    pub fn run(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.state, State::Running) {
            return;
        }
        let sql = self.sql_to_run(cx);
        if sql.trim().is_empty() {
            return;
        }
        let execute = self.store.update(cx, |store, cx| {
            store.execute(self.connection, self.database.clone(), sql, cx)
        });
        self.state = State::Running;
        cx.notify();
        self._run = Some(cx.spawn(async move |this, cx| {
            let outcome = execute.await;
            this.update(cx, |this, cx| {
                match outcome {
                    Ok(result) => {
                        // The widths are per column, so a result with a
                        // different shape starts from the default ones.
                        this.column_widths = column_widths(result.columns.len(), cx);
                        // The rows a selection pointed at are gone.
                        this.selected.clear();
                        this.anchor = None;
                        this.state = State::Done(result);
                    }
                    Err(error) => this.state = State::Failed(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn result(&self) -> Option<&QueryResult> {
        match &self.state {
            State::Done(result) => Some(result),
            _ => None,
        }
    }

    /// Picks out a row the way a list is expected to: plain click replaces the
    /// selection, the platform modifier adds or removes one row, and shift
    /// takes everything between the last click and this one.
    fn select_row(&mut self, row: usize, modifiers: gpui::Modifiers, cx: &mut Context<Self>) {
        if modifiers.platform || modifiers.control {
            if !self.selected.remove(&row) {
                self.selected.insert(row);
            }
            self.anchor = Some(row);
        } else if modifiers.shift {
            let anchor = self.anchor.unwrap_or(row);
            let (first, last) = if anchor <= row {
                (anchor, row)
            } else {
                (row, anchor)
            };
            self.selected = (first..=last).collect();
        } else {
            self.selected = BTreeSet::from([row]);
            self.anchor = Some(row);
        }
        cx.notify();
    }

    /// The selected rows in the order the result has them, so what is sent
    /// reads the way the grid does.
    fn selected_rows(&self) -> Vec<Vec<Option<SharedString>>> {
        let Some(result) = self.result() else {
            return Vec::new();
        };
        self.selected
            .iter()
            .filter_map(|row| result.rows.get(*row).cloned())
            .collect()
    }

    fn deploy_row_menu(
        &mut self,
        row: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Right-clicking outside the selection moves it, the way every list
        // does; right-clicking inside it keeps it, so a menu can act on many.
        if !self.selected.contains(&row) {
            self.select_row(row, gpui::Modifiers::default(), cx);
        }
        let Some(result) = self.result() else {
            return;
        };
        let columns = result.columns.clone();
        let rows = self.selected_rows();
        if rows.is_empty() {
            return;
        }
        let heading = match (rows.len(), self.selected.iter().next()) {
            (1, Some(first)) => format!(
                "Row {} of a Postgres result from database `{}`:",
                first + 1,
                self.database
            ),
            (count, _) => format!(
                "{count} rows of a Postgres result from database `{}`:",
                self.database
            ),
        };
        let described = describe_rows(&heading, &columns, &rows);
        let copied = described.clone();
        let label = if rows.len() == 1 {
            ("Copy Row".to_owned(), "Send Row to Agent".to_owned())
        } else {
            (
                format!("Copy {} Rows", rows.len()),
                format!("Send {} Rows to Agent", rows.len()),
            )
        };
        let focus_handle = self.focus_handle.clone();

        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            menu.context(focus_handle)
                .entry(label.0, None, move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                })
                .separator()
                .action(label.1, Box::new(send_to_agent(described)))
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

    fn connection_name(&self, cx: &App) -> SharedString {
        self.store
            .read(cx)
            .connection(self.connection)
            .map(|config| config.name.clone())
            .unwrap_or_else(|| "Removed connection".into())
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = matches!(self.state, State::Running);
        let status: Option<SharedString> = match &self.state {
            State::Idle => None,
            State::Running => Some("Running…".into()),
            State::Failed(_) => None,
            State::Done(result) => Some(
                format!(
                    "{} {} · {} ms{}",
                    if result.columns.is_empty() {
                        result.rows_affected as usize
                    } else {
                        result.rows.len()
                    },
                    if result.columns.is_empty() {
                        "affected"
                    } else {
                        "rows"
                    },
                    result.elapsed.as_millis(),
                    if result.truncated {
                        format!(" · first {MAX_ROWS}")
                    } else {
                        String::new()
                    }
                )
                .into(),
            ),
        };

        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_1p5()
                    .min_w_0()
                    .child(
                        Icon::new(IconName::DatabaseZap)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("{} · {}", self.connection_name(cx), self.database))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line()
                            .truncate(),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .when_some(status, |this, status| {
                        this.child(
                            Label::new(status)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        Button::new("run", if running { "Running…" } else { "Run" })
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::Small))
                            .disabled(running)
                            .tooltip(Tooltip::text("Run the statement, or the selection"))
                            .on_click(cx.listener(|this, _, window, cx| this.run(window, cx))),
                    ),
            )
    }

    fn render_results(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.state {
            State::Idle => Self::render_notice("Run a statement to see its rows.", false, cx),
            State::Running => Self::render_notice("Running…", false, cx),
            State::Failed(error) => Self::render_notice(error.clone(), true, cx),
            State::Done(result) if result.columns.is_empty() => Self::render_notice(
                format!("{} row(s) affected.", result.rows_affected),
                false,
                cx,
            ),
            State::Done(result) if result.rows.is_empty() => {
                Self::render_notice("No rows.", false, cx)
            }
            State::Done(result) => self.render_table(result, cx),
        }
    }

    fn render_notice(
        text: impl Into<SharedString>,
        is_error: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .p_3()
            .bg(cx.theme().colors().editor_background)
            .size_full()
            .child(
                Label::new(text.into())
                    .size(LabelSize::Small)
                    .color(if is_error { Color::Error } else { Color::Muted }),
            )
            .into_any_element()
    }

    fn render_table(&self, result: &QueryResult, cx: &mut Context<Self>) -> AnyElement {
        let columns = Rc::new(result.columns.clone());
        let rows = Rc::new(result.rows.clone());
        let count = columns.len() + 1;

        let mut headers: Vec<AnyElement> = Vec::with_capacity(count);
        headers.push(
            Label::new("#")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        );
        for column in columns.iter() {
            headers.push(
                Label::new(column.clone())
                    .size(LabelSize::Small)
                    .single_line()
                    .truncate()
                    .into_any_element(),
            );
        }

        let null_color = cx.theme().colors().text_disabled;
        let line_number_color = cx.theme().colors().editor_line_number;
        let selected_background = cx.theme().colors().element_selected;
        let selected = Rc::new(self.selected.clone());
        let this = cx.entity().downgrade();

        Table::new(count)
            .interactable(&self.table_interaction)
            .width_config(ColumnWidthConfig::Resizable(self.column_widths.clone()))
            .header(headers)
            .striped()
            .pin_cols(1)
            .uniform_list("query-results", rows.len(), {
                let rows = rows.clone();
                move |range: Range<usize>, _window, _cx| {
                    range
                        .filter_map(|index| {
                            let values = rows.get(index)?;
                            let mut cells: UncheckedTableRow<AnyElement> =
                                Vec::with_capacity(columns.len() + 1);
                            cells.push(
                                Label::new((index + 1).to_string())
                                    .size(LabelSize::Small)
                                    .color(Color::Custom(line_number_color))
                                    .into_any_element(),
                            );
                            for column in 0..columns.len() {
                                cells.push(match values.get(column) {
                                    Some(Some(value)) => Label::new(value.clone())
                                        .size(LabelSize::Small)
                                        .single_line()
                                        .truncate()
                                        .into_any_element(),
                                    _ => Label::new("NULL")
                                        .size(LabelSize::Small)
                                        .color(Color::Custom(null_color))
                                        .into_any_element(),
                                });
                            }
                            Some(cells)
                        })
                        .collect()
                }
            })
            .map_row(move |(index, row), _window, _cx| {
                let menu_handle = this.clone();
                let click_handle = this.clone();
                row.h(RESULT_ROW_HEIGHT)
                    .items_center()
                    .when(selected.contains(&index), |row| row.bg(selected_background))
                    .on_click(move |event: &gpui::ClickEvent, _window, cx| {
                        let modifiers = event.modifiers();
                        click_handle
                            .update(cx, |this, cx| this.select_row(index, modifiers, cx))
                            .ok();
                    })
                    .on_mouse_down(
                        MouseButton::Right,
                        move |event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            menu_handle
                                .update(cx, |this, cx| {
                                    this.deploy_row_menu(index, event.position, window, cx);
                                })
                                .ok();
                        },
                    )
                    .into_any_element()
            })
            .into_any_element()
    }
}

impl Render for QueryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("SqlConsole")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(|this, _: &RunQuery, window, cx| this.run(window, cx)))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .h(relative(0.4))
                    .min_h(rems(6.))
                    .w_full()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.editor.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.render_results(cx)),
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

impl Focusable for QueryView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<ItemEvent> for QueryView {}

impl Item for QueryView {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        format!("{} · {}", self.connection_name(cx), self.database).into()
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.tab_content_text(0, cx))
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::DatabaseZap))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn can_split(&self) -> bool {
        false
    }
}
