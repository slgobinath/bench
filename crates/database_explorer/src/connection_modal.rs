//! Adding a connection, and editing one that is already saved.
//!
//! The password is the one field that does not come back when a connection is
//! reopened for editing: it is in the keychain, not here. Leaving it empty
//! keeps whatever is stored, which is what editing a port or a database name
//! wants.

use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task, Window,
    prelude::*,
};
use ui::{Modal, ModalFooter, ModalHeader, Section, prelude::*};
use ui_input::InputField;
use workspace::{DismissDecision, ModalView, Workspace};

use crate::{ConnectionConfig, ConnectionId, DatabaseStore, SslMode};

const DEFAULT_PORT: u16 = 5432;

/// What the line under the fields says, when it says anything.
enum Status {
    Testing,
    Reached(SharedString),
    Failed(SharedString),
}

pub struct ConnectionModal {
    store: Entity<DatabaseStore>,
    /// The connection being edited, or `None` when this is a new one.
    editing: Option<ConnectionId>,
    name: Entity<InputField>,
    host: Entity<InputField>,
    port: Entity<InputField>,
    user: Entity<InputField>,
    password: Entity<InputField>,
    database: Entity<InputField>,
    ssl_mode: SslMode,
    status: Option<Status>,
    /// Set once Escape, Cancel or a successful Save has asked for the modal to
    /// go; see [`ModalView::on_before_dismiss`].
    dismissing: bool,
    focus_handle: FocusHandle,
    _test: Option<Task<()>>,
}

impl ConnectionModal {
    /// Opens the modal over a workspace, filled in from `editing` when it
    /// names a saved connection.
    pub fn toggle(
        workspace: &mut Workspace,
        editing: Option<ConnectionId>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(store) = DatabaseStore::global(cx) else {
            return;
        };
        workspace.toggle_modal(window, cx, |window, cx| {
            ConnectionModal::new(store, editing, window, cx)
        });
    }

    fn new(
        store: Entity<DatabaseStore>,
        editing: Option<ConnectionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let existing = editing.and_then(|id| store.read(cx).connection(id).cloned());

        let field = |placeholder: &str,
                     label: &str,
                     value: String,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            let field =
                cx.new(|cx| InputField::new(window, cx, placeholder).label(label.to_owned()));
            if !value.is_empty() {
                let editor = field.read(cx).editor().clone();
                editor.set_text(&value, window, cx);
            }
            field
        };

        let name = field(
            "Local Postgres",
            "Name",
            existing
                .as_ref()
                .map(|config| config.name.to_string())
                .unwrap_or_default(),
            window,
            cx,
        );
        let host = field(
            "localhost",
            "Host",
            existing
                .as_ref()
                .map(|config| config.host.clone())
                .unwrap_or_else(|| "localhost".to_owned()),
            window,
            cx,
        );
        let port = field(
            "5432",
            "Port",
            existing
                .as_ref()
                .map(|config| config.port.to_string())
                .unwrap_or_else(|| DEFAULT_PORT.to_string()),
            window,
            cx,
        );
        let user = field(
            "postgres",
            "User",
            existing
                .as_ref()
                .map(|config| config.user.clone())
                .unwrap_or_else(|| "postgres".to_owned()),
            window,
            cx,
        );
        let password = cx.new(|cx| {
            InputField::new(
                window,
                cx,
                if existing.is_some() {
                    "Unchanged"
                } else {
                    "Password"
                },
            )
            .label("Password")
            .masked(true)
        });
        let database = field(
            "postgres",
            "Database",
            existing
                .as_ref()
                .map(|config| config.database.clone())
                .unwrap_or_else(|| "postgres".to_owned()),
            window,
            cx,
        );

        Self {
            ssl_mode: existing
                .as_ref()
                .map(|config| config.ssl_mode)
                .unwrap_or_default(),
            store,
            editing,
            name,
            host,
            port,
            user,
            password,
            database,
            status: None,
            dismissing: false,
            focus_handle: cx.focus_handle(),
            _test: None,
        }
    }

    /// The fields as a connection, or what is wrong with them. The password
    /// comes back separately because `None` there means something — leave the
    /// keychain's alone — that an empty string does not.
    fn read_fields(&self, cx: &App) -> Result<(ConnectionConfig, Option<String>), SharedString> {
        let name = self.name.read(cx).text(cx).trim().to_owned();
        let host = self.host.read(cx).text(cx).trim().to_owned();
        let port = self.port.read(cx).text(cx).trim().to_owned();
        let user = self.user.read(cx).text(cx).trim().to_owned();
        let database = self.database.read(cx).text(cx).trim().to_owned();
        let password = self.password.read(cx).text(cx);

        if host.is_empty() {
            return Err("A host is required.".into());
        }
        if database.is_empty() {
            return Err("A database is required.".into());
        }
        let port = port
            .parse::<u16>()
            .map_err(|_| SharedString::from("The port must be a number between 1 and 65535."))?;

        let config = ConnectionConfig {
            // Zero asks the store for an id; a saved connection keeps its own.
            id: self.editing.unwrap_or(ConnectionId(0)),
            name: if name.is_empty() {
                SharedString::from(format!("{host}:{port}/{database}"))
            } else {
                SharedString::from(name)
            },
            host,
            port,
            user,
            database,
            ssl_mode: self.ssl_mode,
        };
        // An empty password on an edit means "leave the stored one alone";
        // on a new connection there is nothing to leave.
        let password = if password.is_empty() && self.editing.is_some() {
            None
        } else {
            Some(password)
        };
        Ok((config, password))
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (config, password) = match self.read_fields(cx) {
            Ok(fields) => fields,
            Err(error) => {
                self.status = Some(Status::Failed(error));
                cx.notify();
                return;
            }
        };
        self.store.update(cx, |store, cx| {
            store.save_connection(config, password, cx);
        });
        window.focus(&self.focus_handle, cx);
        self.dismiss(cx);
    }

    /// Reaches the server with what is typed, without saving any of it.
    fn test(&mut self, cx: &mut Context<Self>) {
        let (config, password) = match self.read_fields(cx) {
            Ok(fields) => fields,
            Err(error) => {
                self.status = Some(Status::Failed(error));
                cx.notify();
                return;
            }
        };
        let test = self
            .store
            .update(cx, |store, cx| store.test_connection(config, password, cx));
        self.status = Some(Status::Testing);
        cx.notify();
        self._test = Some(cx.spawn(async move |this, cx| {
            let outcome = test.await;
            this.update(cx, |this, cx| {
                this.status = Some(match outcome {
                    Ok(version) => Status::Reached(version),
                    Err(error) => Status::Failed(format!("{error:#}").into()),
                });
                cx.notify();
            })
            .ok();
        }));
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.dismissing = true;
        cx.emit(DismissEvent);
    }

    fn render_status(&self) -> Option<Label> {
        let (text, color) = match self.status.as_ref()? {
            Status::Testing => ("Connecting…".into(), Color::Muted),
            Status::Reached(version) => (
                SharedString::from(format!("Connected. PostgreSQL {version}.")),
                Color::Success,
            ),
            Status::Failed(error) => (error.clone(), Color::Error),
        };
        Some(Label::new(text).size(LabelSize::Small).color(color))
    }

    fn render_ssl_mode(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .child(Label::new("SSL").size(LabelSize::Small).color(Color::Muted))
            .children(SslMode::ALL.into_iter().enumerate().map(|(index, mode)| {
                Button::new(("ssl-mode", index), mode.label())
                    .label_size(LabelSize::Small)
                    .toggle_state(self.ssl_mode == mode)
                    .selected_style(ButtonStyle::Tinted(ui::TintColor::Accent))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.ssl_mode = mode;
                        cx.notify();
                    }))
            }))
    }
}

impl ModalView for ConnectionModal {
    /// A click outside must not throw away a half-typed connection, so only
    /// Escape, Cancel and a successful Save close this one.
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> DismissDecision {
        DismissDecision::Dismiss(self.dismissing)
    }
}

impl EventEmitter<DismissEvent> for ConnectionModal {}

impl Focusable for ConnectionModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.read(cx).focus_handle(cx)
    }
}

impl Render for ConnectionModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .elevation_3(cx)
            .w(rems(30.))
            .key_context("DatabaseConnectionModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| this.dismiss(cx)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.confirm(window, cx)))
            .child(
                Modal::new("database-connection", None)
                    .header(ModalHeader::new().headline(if self.editing.is_some() {
                        "Edit Connection"
                    } else {
                        "New Postgres Connection"
                    }))
                    .section(
                        Section::new().child(
                            v_flex()
                                .gap_2()
                                .child(self.name.clone())
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(div().flex_1().child(self.host.clone()))
                                        .child(div().w(rems(6.)).child(self.port.clone())),
                                )
                                .child(self.user.clone())
                                .child(self.password.clone())
                                .child(self.database.clone())
                                .child(self.render_ssl_mode(cx))
                                .children(self.render_status()),
                        ),
                    )
                    .footer(
                        ModalFooter::new()
                            .start_slot(
                                Button::new("test", "Test Connection")
                                    .disabled(matches!(self.status, Some(Status::Testing)))
                                    .on_click(cx.listener(|this, _, _, cx| this.test(cx))),
                            )
                            .end_slot(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("cancel", "Cancel").on_click(
                                            cx.listener(|this, _, _, cx| this.dismiss(cx)),
                                        ),
                                    )
                                    .child(
                                        Button::new("save", "Save")
                                            .style(ButtonStyle::Filled)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.confirm(window, cx)
                                            })),
                                    ),
                            ),
                    ),
            )
    }
}
