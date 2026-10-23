use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
use futures::channel::{mpsc, oneshot};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{
    Action, AnyWindowHandle, App, AsyncApp, Bounds, ClipboardItem, Context, DismissEvent, Entity,
    EntityId, EventEmitter, FocusHandle, Focusable, Global, Pixels, Point, Render, Subscription,
    Task, WeakEntity, Window, actions, anchored, deferred, px,
};
use project::Project;
use remote::{RemoteClient, RemoteConnectionIdentity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use theme::Appearance;
use ui::{ContextMenu, Tooltip, prelude::*};
use url::Url;
use util::ResultExt as _;
use workspace::item::{Item, ItemEvent};
use workspace::{MultiWorkspace, Pane, Workspace};
use zed_actions::claude::{SendSelection, SendText};

use crate::web_view::{KeyboardFocus, PageEvent, PageSource, WebView, parse_key};
use crate::{PageElement, PageHost};

/// Opens a browser tab in Bench.
#[derive(Clone, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = browser)]
#[serde(deny_unknown_fields)]
pub struct OpenBrowser {
    /// The page to load. Without one, the tab opens with its address bar
    /// focused.
    #[serde(default)]
    pub url: Option<String>,
}

actions!(
    browser,
    [
        /// Goes back to the previous page.
        GoBack,
        /// Goes forward to the next page.
        GoForward,
        /// Loads the page again.
        Reload,
        /// Moves the keyboard to the address bar.
        FocusAddressBar,
        /// Opens the Web Inspector for the page.
        OpenDevTools,
        /// Opens the page in the system's default browser.
        OpenInDefaultBrowser,
        /// Copies the text selected in the page.
        Copy,
        /// Cuts the text selected in the field focused in the page.
        Cut,
        /// Pastes the clipboard into the field focused in the page.
        Paste,
        /// Selects everything in the page, or in the field focused in it.
        SelectAll,
        /// Undoes the last edit in the page.
        Undo,
        /// Redoes the last edit undone in the page.
        Redo,
    ]
);

const BROWSER_SCRIPT: &str = include_str!("browser.js");

/// How many console messages a tab keeps.
const MAX_CONSOLE_ENTRIES: usize = 1000;

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a page gets to draw after its tab is brought to the front before
/// it is captured or clicked.
const REVEAL_DELAY: Duration = Duration::from_millis(300);

/// How long after a click the page gets to respond before the click returns:
/// Flutter, for one, holds a tap briefly to tell it from other gestures, and
/// focuses its text input a frame after that.
const CLICK_SETTLE_DELAY: Duration = Duration::from_millis(300);

/// How long after an agent's click the page's report of a mouse down is
/// ignored, so that the click doesn't move Bench's focus to the tab.
const AGENT_CLICK_WINDOW: Duration = Duration::from_secs(1);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, action: &OpenBrowser, window, cx| {
            let pane = workspace.active_pane().clone();
            let browser = BrowserView::open(workspace, pane, true, window, cx);
            match &action.url {
                Some(url) => browser.update(cx, |browser, cx| {
                    browser.navigate(url, cx).detach_and_log_err(cx);
                }),
                None => browser.update(cx, |browser, cx| {
                    window.focus(&browser.address_bar.focus_handle(cx), cx);
                }),
            }
        });
    })
    .detach();
}

/// Something the page logged, or that happened to it.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ConsoleEntry {
    /// Increases with every entry, so a caller can ask for those after the
    /// last it saw.
    pub id: u64,
    /// `log`, `info`, `warn`, `error` or `debug` for the console; `error`
    /// for uncaught errors; `network` for failed requests; `navigation` when
    /// a page loads.
    pub level: String,
    pub text: String,
}

pub enum BrowserEvent {
    TabChanged,
}

/// A tab in Bench showing pages from the web, which agents can drive through
/// Bench's MCP server.
pub struct BrowserView {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    window_handle: AnyWindowHandle,
    web_view: Option<WebView>,
    /// Why the web view couldn't be created.
    load_error: Option<SharedString>,
    /// Why the last page couldn't be loaded, shown instead of the page.
    page_error: Option<SharedString>,
    address_bar: Entity<Editor>,
    url: String,
    title: Option<SharedString>,
    loading: bool,
    load_waiters: Vec<oneshot::Sender<()>>,
    console: VecDeque<ConsoleEntry>,
    next_console_id: u64,
    selection: String,
    page_bounds: Cell<Bounds<Pixels>>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    applied_appearance: Option<Appearance>,
    ignore_mouse_down_until: Option<Instant>,
    navigation: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
}

impl BrowserView {
    /// Adds a new browser tab to `pane`.
    pub fn open(
        workspace: &mut Workspace,
        pane: Entity<Pane>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_entity = cx.entity();
        let project = workspace.project().clone();
        let browser = cx.new(|cx| Self::new(workspace_entity, project, window, cx));
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(browser.clone()), true, focus, None, window, cx)
        });
        browser
    }

    fn new(
        workspace: Entity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (events_sender, mut events) = mpsc::unbounded();
        let source = PageSource::Web {
            url: "about:blank".to_string(),
            script: BROWSER_SCRIPT,
        };
        let (web_view, load_error) = match WebView::new(source, events_sender, window, cx) {
            Ok(web_view) => (Some(web_view), None),
            Err(error) => {
                log::error!("opening a browser tab: {error:#}");
                (None, Some(format!("{error:#}").into()))
            }
        };

        let handle_events = cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.next().await {
                let handled = this.update_in(cx, |this, window, cx| {
                    this.handle_page_event(event, window, cx)
                });
                if handled.is_err() {
                    break;
                }
            }
        });

        let address_bar = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Enter a URL, such as localhost:3000", window, cx);
            editor
        });

        let focus_handle = cx.focus_handle();
        let subscriptions = vec![
            cx.on_focus(&focus_handle, window, |this, _, _| {
                if let Some(web_view) = &this.web_view {
                    web_view.focus();
                }
            }),
            cx.on_blur(&focus_handle, window, |this, _, _| {
                if let Some(web_view) = &this.web_view {
                    web_view.focus_parent();
                }
            }),
            cx.subscribe_in(&address_bar, window, |this, _, event, window, cx| {
                if let EditorEvent::Blurred = event {
                    this.show_url_in_address_bar(window, cx);
                }
            }),
            // Zooming happens in the workspace, and the tab has to be drawn
            // again to hide or show its page.
            cx.observe(&workspace, |_, _, cx| cx.notify()),
            cx.on_release(|this, cx| {
                if let Some(web_view) = this.web_view.take() {
                    web_view.release(cx);
                }
            }),
        ];

        Self {
            focus_handle,
            workspace: workspace.downgrade(),
            project,
            window_handle: window.window_handle(),
            web_view,
            load_error,
            page_error: None,
            address_bar,
            url: String::new(),
            title: None,
            loading: false,
            load_waiters: Vec::new(),
            console: VecDeque::new(),
            next_console_id: 1,
            selection: String::new(),
            page_bounds: Cell::new(Bounds::default()),
            context_menu: None,
            applied_appearance: None,
            ignore_mouse_down_until: None,
            navigation: None,
            _subscriptions: subscriptions,
            _tasks: vec![handle_events],
        }
    }

    /// The URL of the page the tab shows.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn title(&self) -> Option<&SharedString> {
        self.title.as_ref()
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// The console entries after `after`, oldest first.
    pub fn console_entries(&self, after: u64) -> Vec<ConsoleEntry> {
        self.console
            .iter()
            .filter(|entry| entry.id > after)
            .cloned()
            .collect()
    }

    /// The id of the last console entry the tab has had.
    pub fn last_console_id(&self) -> u64 {
        self.next_console_id - 1
    }

    pub fn clear_console(&mut self) {
        self.console.clear();
    }

    fn web_view_or_err(&self) -> Result<&WebView> {
        match &self.web_view {
            Some(web_view) => Ok(web_view),
            None => Err(anyhow::anyhow!(
                "the tab has no page: {}",
                self.load_error
                    .as_deref()
                    .unwrap_or("its web view could not be created")
            )),
        }
    }

    /// Loads `input`, a URL as a person would type it into an address bar.
    /// A page on `localhost` in a remote project is reached through a port
    /// forwarded over the project's connection. Resolves with the URL loaded.
    pub fn navigate(&mut self, input: &str, cx: &mut Context<Self>) -> Task<Result<String>> {
        let url = match normalize_url(input) {
            Ok(url) => url,
            Err(error) => return Task::ready(Err(error)),
        };
        let project = self.project.clone();
        let (result_sender, result_receiver) = oneshot::channel();
        self.navigation = Some(cx.spawn(async move |this, cx| {
            let reachable = reachable_url(url.clone(), &project, cx).await;
            let result = this.update(cx, |this, cx| {
                let loaded = reachable.and_then(|reachable| {
                    this.web_view_or_err()?.load_url(reachable.as_str())?;
                    Ok(reachable)
                });
                match &loaded {
                    Ok(reachable) => {
                        this.page_error = None;
                        this.url = reachable.to_string();
                    }
                    Err(error) => {
                        this.page_error = Some(format!("{error:#}").into());
                        this.url = url.to_string();
                        this.loading = false;
                    }
                }
                this.address_bar_needs_url(cx);
                cx.emit(BrowserEvent::TabChanged);
                cx.notify();
                loaded.map(|loaded| loaded.to_string())
            });
            result_sender.send(result.and_then(|result| result)).ok();
        }));
        cx.spawn(async move |_, _| {
            result_receiver
                .await
                .context("the navigation was replaced by another")?
        })
    }

    /// Resolves when the page that is loading, or the next one to, has
    /// loaded.
    pub fn wait_for_load(&mut self) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        self.load_waiters.push(sender);
        receiver
    }

    pub fn go_back(&mut self, cx: &mut Context<Self>) -> Result<()> {
        let web_view = self.web_view_or_err()?;
        anyhow::ensure!(web_view.can_go_back(), "there is no page to go back to");
        web_view.go_back();
        cx.notify();
        Ok(())
    }

    pub fn go_forward(&mut self, cx: &mut Context<Self>) -> Result<()> {
        let web_view = self.web_view_or_err()?;
        anyhow::ensure!(
            web_view.can_go_forward(),
            "there is no page to go forward to"
        );
        web_view.go_forward();
        cx.notify();
        Ok(())
    }

    /// Loads the page again. If it couldn't be loaded, this tries to load it
    /// again from scratch, in case its server has started since.
    pub fn reload(&mut self, cx: &mut Context<Self>) -> Task<Result<String>> {
        if self.page_error.is_some() || self.url.is_empty() {
            let url = self.url.clone();
            return self.navigate(&url, cx);
        }
        let reloaded = self
            .web_view_or_err()
            .and_then(|web_view| web_view.reload());
        Task::ready(reloaded.map(|()| self.url.clone()))
    }

    fn handle_page_event(&mut self, event: PageEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            PageEvent::SelectionChanged(selection) => self.selection = selection,
            PageEvent::ContextMenu {
                position,
                selection,
            } => {
                self.selection = selection;
                let position = self.page_bounds.get().origin + position;
                self.deploy_context_menu(position, window, cx);
            }
            PageEvent::MouseDown => {
                // The page takes clicks itself, so GPUI never sees the click
                // that would dismiss the menu or focus this tab.
                if self.context_menu.take().is_some() {
                    cx.notify();
                }
                let from_agent = self
                    .ignore_mouse_down_until
                    .is_some_and(|until| Instant::now() < until);
                if !from_agent {
                    window.focus(&self.focus_handle, cx);
                }
            }
            PageEvent::OpenExternally(url) => cx.open_url(&url),
            PageEvent::OpenInNewTab(url) => self.open_in_new_tab(url, window, cx),
            PageEvent::LoadStarted(url) => {
                self.loading = true;
                self.page_error = None;
                self.set_url(url, window, cx);
            }
            PageEvent::LoadFinished(url) => {
                self.loading = false;
                // The blank page a tab starts with isn't a load anyone waits
                // for, and can finish after the first real one has begun.
                if url != "about:blank" {
                    // A reload of a page with the same title doesn't report
                    // it again, so it's read once the page has loaded.
                    if let Some(web_view) = &self.web_view {
                        self.title = web_view
                            .title()
                            .filter(|title| !title.trim().is_empty())
                            .map(SharedString::from);
                        cx.emit(BrowserEvent::TabChanged);
                    }
                    self.add_console_entry("navigation".to_string(), format!("Loaded {url}"));
                    for waiter in self.load_waiters.drain(..) {
                        waiter.send(()).ok();
                    }
                }
                self.set_url(url, window, cx);
            }
            PageEvent::LocationChanged(url) => self.set_url(url, window, cx),
            PageEvent::TitleChanged(title) => {
                self.title = (!title.trim().is_empty()).then(|| title.into());
                cx.emit(BrowserEvent::TabChanged);
                cx.notify();
            }
            PageEvent::Console { level, text } => self.add_console_entry(level, text),
        }
    }

    fn add_console_entry(&mut self, level: String, text: String) {
        if self.console.len() == MAX_CONSOLE_ENTRIES {
            self.console.pop_front();
        }
        self.console.push_back(ConsoleEntry {
            id: self.next_console_id,
            level,
            text,
        });
        self.next_console_id += 1;
    }

    fn set_url(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        if url == "about:blank" || url == self.url {
            cx.notify();
            return;
        }
        self.url = url;
        self.show_url_in_address_bar(window, cx);
        cx.emit(BrowserEvent::TabChanged);
        cx.notify();
    }

    /// Shows the page's URL in the address bar, unless the person is typing
    /// in it.
    fn show_url_in_address_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.address_bar.focus_handle(cx).is_focused(window) {
            return;
        }
        let url = self.url.clone();
        self.address_bar
            .update(cx, |editor, cx| editor.set_text(url, window, cx));
    }

    /// Updates the address bar on the next frame, for changes made where
    /// there's no window to update it with.
    fn address_bar_needs_url(&mut self, cx: &mut Context<Self>) {
        let window = self.window_handle;
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| {
                    this.update(cx, |this, cx| this.show_url_in_address_bar(window, cx))
                })
                .log_err();
        });
    }

    fn open_in_new_tab(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let pane = workspace
            .read(cx)
            .pane_for_item_id(cx.entity_id())
            .unwrap_or_else(|| workspace.read(cx).active_pane().clone());
        // The new tab is added once this one's update has finished, since
        // adding it deactivates this one.
        window.defer(cx, move |window, cx| {
            let browser = workspace.update(cx, |workspace, cx| {
                BrowserView::open(workspace, pane, true, window, cx)
            });
            browser.update(cx, |browser, cx| {
                browser.navigate(&url, cx).detach_and_log_err(cx);
            });
        });
    }

    fn confirm_address(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.address_bar.read(cx).text(cx);
        if input.trim().is_empty() {
            return;
        }
        window.focus(&self.focus_handle, cx);
        self.navigate(&input, cx).detach_and_log_err(cx);
    }

    fn cancel_address(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        self.show_url_in_address_bar(window, cx);
    }

    fn focus_address_bar(
        &mut self,
        _: &FocusAddressBar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let address_bar = self.address_bar.clone();
        window.focus(&address_bar.focus_handle(cx), cx);
        address_bar.update(cx, |editor, cx| {
            editor.select_all(&editor::actions::SelectAll, window, cx)
        });
    }

    fn handle_go_back(&mut self, _: &GoBack, _: &mut Window, cx: &mut Context<Self>) {
        self.go_back(cx).log_err();
    }

    fn handle_go_forward(&mut self, _: &GoForward, _: &mut Window, cx: &mut Context<Self>) {
        self.go_forward(cx).log_err();
    }

    fn handle_reload(&mut self, _: &Reload, _: &mut Window, cx: &mut Context<Self>) {
        self.reload(cx).detach_and_log_err(cx);
    }

    fn open_dev_tools(&mut self, _: &OpenDevTools, _: &mut Window, _: &mut Context<Self>) {
        if let Some(web_view) = &self.web_view {
            web_view.open_devtools();
        }
    }

    fn open_in_default_browser(
        &mut self,
        _: &OpenInDefaultBrowser,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.url.is_empty() {
            cx.open_url(&self.url);
        }
    }

    /// Reads the page's selection when it's asked for, since selections in
    /// text fields aren't reported as the page's selection.
    fn selected_text(&self, cx: &mut Context<Self>) -> Task<Result<String>> {
        const SELECTED_TEXT: &str = "let element = document.activeElement;
            while (element?.shadowRoot?.activeElement) element = element.shadowRoot.activeElement;
            if (element && typeof element.selectionStart === 'number' && 'value' in element) {
              return element.value.slice(element.selectionStart, element.selectionEnd);
            }
            return String(window.getSelection() ?? '');";
        let receiver = self
            .web_view_or_err()
            .and_then(|web_view| web_view.run_script(SELECTED_TEXT));
        cx.background_spawn(async move {
            let value = receiver?.await.context("the page went away")??;
            Ok(value.as_str().unwrap_or_default().to_string())
        })
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let selected_text = self.selected_text(cx);
        cx.spawn(async move |_, cx| {
            let text = selected_text.await?;
            if !text.is_empty() {
                cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(text)));
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        let selected_text = self.selected_text(cx);
        cx.spawn(async move |this, cx| {
            let text = selected_text.await?;
            if text.is_empty() {
                return Ok(());
            }
            cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(text)));
            this.update(cx, |this, _| this.web_view_or_err()?.exec_command("delete"))?
        })
        .detach_and_log_err(cx);
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if let Some(web_view) = &self.web_view {
            web_view.insert_text(&text).log_err();
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, _: &mut Context<Self>) {
        if let Some(web_view) = &self.web_view {
            web_view.select_all().log_err();
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, _: &mut Context<Self>) {
        if let Some(web_view) = &self.web_view {
            web_view.exec_command("undo").log_err();
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, _: &mut Context<Self>) {
        if let Some(web_view) = &self.web_view {
            web_view.exec_command("redo").log_err();
        }
    }

    fn send_to_agent_action(&self) -> Option<Box<dyn Action>> {
        let selection = self.selection.trim();
        if selection.is_empty() {
            return None;
        }
        Some(Box::new(SendText {
            text: format!("From {}:\n{selection}\n", self.url),
        }))
    }

    fn send_selection(&mut self, _: &SendSelection, window: &mut Window, cx: &mut Context<Self>) {
        match self.send_to_agent_action() {
            Some(action) => window.dispatch_action(action, cx),
            None => cx.propagate(),
        }
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let send_to_agent = self.send_to_agent_action();
        let has_selection = !self.selection.is_empty();
        let (can_go_back, can_go_forward) =
            self.web_view.as_ref().map_or((false, false), |web_view| {
                (web_view.can_go_back(), web_view.can_go_forward())
            });
        let focus_handle = self.focus_handle.clone();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            menu.context(focus_handle)
                .when_some(send_to_agent, |menu, action| {
                    menu.entry(
                        "Send to Agent",
                        Some(Box::new(SendSelection)),
                        move |window, cx| window.dispatch_action(action.boxed_clone(), cx),
                    )
                    .separator()
                })
                .when(has_selection, |menu| menu.action("Copy", Box::new(Copy)))
                .action("Select All", Box::new(SelectAll))
                .separator()
                .when(can_go_back, |menu| menu.action("Back", Box::new(GoBack)))
                .when(can_go_forward, |menu| {
                    menu.action("Forward", Box::new(GoForward))
                })
                .action("Reload", Box::new(Reload))
                .separator()
                .action("Open in Default Browser", Box::new(OpenInDefaultBrowser))
                .action("Open Web Inspector", Box::new(OpenDevTools))
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, _, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (can_go_back, can_go_forward) =
            self.web_view.as_ref().map_or((false, false), |web_view| {
                (web_view.can_go_back(), web_view.can_go_forward())
            });
        let has_page = self.web_view.is_some();
        h_flex()
            .p_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().toolbar_background)
            .child(
                IconButton::new("browser-back", IconName::ArrowLeft)
                    .icon_size(IconSize::Small)
                    .disabled(!can_go_back)
                    .tooltip(Tooltip::for_action_title("Back", &GoBack))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.go_back(cx).log_err();
                    })),
            )
            .child(
                IconButton::new("browser-forward", IconName::ArrowRight)
                    .icon_size(IconSize::Small)
                    .disabled(!can_go_forward)
                    .tooltip(Tooltip::for_action_title("Forward", &GoForward))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.go_forward(cx).log_err();
                    })),
            )
            .child(if self.loading {
                IconButton::new("browser-stop", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Stop Loading"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(web_view) = &this.web_view {
                            web_view.stop_loading();
                        }
                        this.loading = false;
                        cx.notify();
                    }))
            } else {
                IconButton::new("browser-reload", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .disabled(!has_page)
                    .tooltip(Tooltip::for_action_title("Reload", &Reload))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reload(cx).detach_and_log_err(cx);
                    }))
            })
            .child(
                div()
                    .key_context("BrowserAddressBar")
                    .on_action(cx.listener(Self::confirm_address))
                    .on_action(cx.listener(Self::cancel_address))
                    .flex_1()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(self.address_bar.clone()),
            )
            .child(
                IconButton::new("browser-dev-tools", IconName::Code)
                    .icon_size(IconSize::Small)
                    .disabled(!has_page)
                    .tooltip(Tooltip::for_action_title(
                        "Open Web Inspector",
                        &OpenDevTools,
                    ))
                    .on_click(cx.listener(|this, _, _, _| {
                        if let Some(web_view) = &this.web_view {
                            web_view.open_devtools();
                        }
                    })),
            )
    }
}

impl Render for BrowserView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let appearance = cx.theme().appearance();
        if self.applied_appearance != Some(appearance)
            && let Some(web_view) = &self.web_view
        {
            web_view.set_appearance(appearance);
            self.applied_appearance = Some(appearance);
        }
        let error = self.load_error.clone().or_else(|| self.page_error.clone());

        v_flex()
            .id("Browser")
            .key_context("Browser")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::handle_go_back))
            .on_action(cx.listener(Self::handle_go_forward))
            .on_action(cx.listener(Self::handle_reload))
            .on_action(cx.listener(Self::focus_address_bar))
            .on_action(cx.listener(Self::open_dev_tools))
            .on_action(cx.listener(Self::open_in_default_browser))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::send_selection))
            .child(self.render_toolbar(cx))
            .child(div().flex_1().min_h_0().relative().map(|this| {
                match error {
                    Some(error) => this.flex().items_center().justify_center().child(
                        h_flex()
                            .p_4()
                            .gap_2()
                            .child(Icon::new(IconName::Warning).color(Color::Warning))
                            .child(Label::new(error).color(Color::Muted)),
                    ),
                    None => this.child(PageElement { view: cx.entity() }),
                }
            }))
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .snap_to_window_with_margin(px(8.))
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl PageHost for BrowserView {
    fn web_view(&self) -> Option<&WebView> {
        self.web_view.as_ref()
    }

    fn page_bounds(&self) -> &Cell<Bounds<Pixels>> {
        &self.page_bounds
    }

    fn workspace(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }
}

impl Focusable for BrowserView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<BrowserEvent> for BrowserView {}

impl Item for BrowserView {
    type Event = BrowserEvent;

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Public))
    }

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        if let Some(title) = &self.title {
            return title.clone();
        }
        Url::parse(&self.url)
            .ok()
            .and_then(|url| {
                let host = url.host_str()?;
                Some(match url.port() {
                    Some(port) => format!("{host}:{port}").into(),
                    None => SharedString::from(host.to_string()),
                })
            })
            .unwrap_or_else(|| "New Tab".into())
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        (!self.url.is_empty()).then(|| self.url.clone().into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            BrowserEvent::TabChanged => f(ItemEvent::UpdateTab),
        }
    }
}

/// Reads a URL the way an address bar does: a bare host and port, such as
/// `localhost:3000`, or just a port, is a page on this machine served over
/// HTTP; another bare host is served over HTTPS.
fn normalize_url(input: &str) -> Result<Url> {
    let input = input.trim();
    anyhow::ensure!(!input.is_empty(), "no URL was given");
    anyhow::ensure!(
        !input.contains(char::is_whitespace),
        "`{input}` is not a URL"
    );
    if input.chars().all(|character| character.is_ascii_digit()) {
        return Url::parse(&format!("http://localhost:{input}"))
            .with_context(|| format!("`{input}` is not a port"));
    }
    if input.contains("://")
        || ["about:", "data:", "blob:"]
            .iter()
            .any(|scheme| input.starts_with(scheme))
    {
        return Url::parse(input).with_context(|| format!("`{input}` is not a URL"));
    }
    let host = input.split(['/', '?', '#']).next().unwrap_or_default();
    let host_name = match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|character| character.is_ascii_digit()) => name,
        _ => host,
    };
    let is_local = host_name == "localhost"
        || host_name.ends_with(".localhost")
        || host_name.ends_with(".local")
        || host_name.ends_with(".test")
        || host_name
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok();
    anyhow::ensure!(
        is_local || host_name.contains('.'),
        "`{input}` is not a URL"
    );
    let scheme = if is_local { "http" } else { "https" };
    Url::parse(&format!("{scheme}://{input}")).with_context(|| format!("`{input}` is not a URL"))
}

/// Whether a URL is a page served on the machine it's loaded on, which for a
/// remote project is the remote host.
fn is_loopback(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && match url.host() {
            Some(url::Host::Domain(domain)) => {
                domain == "localhost" || domain.ends_with(".localhost")
            }
            Some(url::Host::Ipv4(address)) => address.is_loopback() || address.is_unspecified(),
            Some(url::Host::Ipv6(address)) => address.is_loopback() || address.is_unspecified(),
            None => false,
        }
}

/// The URL to load for `url`. A page on the remote host of a remote project
/// is reached through a forwarded port; a page on this machine is checked to
/// have a server, since WebKit shows a blank page when there's none.
async fn reachable_url(mut url: Url, project: &Entity<Project>, cx: &mut AsyncApp) -> Result<Url> {
    if !is_loopback(&url) {
        return Ok(url);
    }
    let port = url
        .port_or_known_default()
        .with_context(|| format!("{url} has no port"))?;
    let remote_client = project.read_with(cx, |project, _| project.remote_client());
    if let Some(remote_client) = remote_client {
        let local_port = forward_port(&remote_client, port, cx).await?;
        if local_port != port {
            url.set_port(Some(local_port))
                .map_err(|()| anyhow::anyhow!("{url} can't have a port"))?;
        }
        return Ok(url);
    }
    let listening = cx
        .background_spawn(async move { accepts_connections(port, Duration::from_secs(1)) })
        .await;
    anyhow::ensure!(
        listening,
        "Nothing is listening on port {port} on this machine. Is the dev server running?"
    );
    Ok(url)
}

struct PortForward {
    local_port: u16,
    /// The `ssh -N -L` that set the forward up. Over a multiplexed connection
    /// the forward belongs to the connection and `ssh` exits once it's set
    /// up; otherwise this process holds it open until Bench quits.
    _process: util::command::Child,
}

/// The ports forwarded from this machine to remote hosts, by host and remote
/// port. A forward lasts as long as the connection it was made over.
#[derive(Default)]
struct PortForwards(HashMap<(RemoteConnectionIdentity, u16), PortForward>);

impl Global for PortForwards {}

/// Whether something accepts connections on the port on this machine.
fn accepts_connections(port: u16, timeout: Duration) -> bool {
    [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ]
    .iter()
    .any(|address| TcpStream::connect_timeout(address, timeout).is_ok())
}

/// The port on this machine that reaches `remote_port` on the host, which is
/// the same port when it's free here.
async fn forward_port(
    remote_client: &Entity<RemoteClient>,
    remote_port: u16,
    cx: &mut AsyncApp,
) -> Result<u16> {
    let identity = remote_client.read_with(cx, |client, _| {
        RemoteConnectionIdentity::from(&client.connection_options())
    });
    let key = (identity, remote_port);
    let existing = cx.update(|cx| {
        cx.try_global::<PortForwards>()
            .and_then(|forwards| forwards.0.get(&key))
            .map(|forward| forward.local_port)
    });
    if let Some(local_port) = existing {
        // A forward ends when the connection it was made over does, such as
        // when the project reconnects.
        let alive = cx
            .background_spawn(
                async move { accepts_connections(local_port, Duration::from_millis(500)) },
            )
            .await;
        if alive {
            return Ok(local_port);
        }
        cx.update(|cx| cx.default_global::<PortForwards>().0.remove(&key));
    }

    let local_port = match TcpListener::bind((Ipv4Addr::LOCALHOST, remote_port)) {
        Ok(_) => remote_port,
        Err(_) => TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.local_addr())
            .map(|address| address.port())
            .context("finding a free port to forward")?,
    };
    let command = remote_client.read_with(cx, |client, _| {
        client.build_forward_ports_command(vec![(local_port, "localhost".to_string(), remote_port)])
    })?;
    let mut process = util::command::new_command(&command.program)
        .args(&command.args)
        .envs(&command.env)
        .stdin(util::command::Stdio::null())
        .stdout(util::command::Stdio::null())
        .stderr(util::command::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("starting ssh to forward the port")?;

    // Nothing listens on the local port until the forward is set up.
    let mut ready = false;
    for _ in 0..50 {
        let listening = cx
            .background_spawn(
                async move { accepts_connections(local_port, Duration::from_millis(200)) },
            )
            .await;
        if listening {
            ready = true;
            break;
        }
        if let Ok(Some(status)) = process.try_status()
            && !status.success()
        {
            anyhow::bail!(
                "ssh could not forward port {remote_port} from the remote host ({status})"
            );
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
    anyhow::ensure!(
        ready,
        "timed out forwarding port {remote_port} from the remote host"
    );
    cx.update(|cx| {
        cx.default_global::<PortForwards>().0.insert(
            key,
            PortForward {
                local_port,
                _process: process,
            },
        );
    });
    Ok(local_port)
}

// What agents do in the browser, through Bench's MCP server.

/// What an agent's click is aimed at.
pub enum ClickTarget {
    /// An element from a snapshot, by its ref, such as `{"ref": "e3"}`, or a
    /// CSS selector, such as `{"selector": "#submit"}`.
    Element(serde_json::Value),
    /// A point relative to the page's top left corner, in CSS pixels.
    Point(Point<Pixels>),
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ClickOutcome {
    /// What was clicked, such as `button "Sign in"`, when it was an element.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub element: Option<String>,
    pub x: f32,
    pub y: f32,
    /// What was over the element's center and so took the click instead,
    /// when something was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covered_by: Option<String>,
    /// Whether the click left a text field focused, which gives the page the
    /// keyboard as a person's click would.
    pub text_field_focused: bool,
}

#[derive(Deserialize)]
struct ClickPoint {
    x: f32,
    y: f32,
    element: String,
    covered: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Screenshot {
    #[serde(skip)]
    pub png: Vec<u8>,
    /// The image's width, which is the page's width in CSS pixels, so a
    /// point in the image is a point to click.
    pub width: u32,
    pub height: u32,
}

/// Runs `body` as the body of an async function in the page and returns
/// what it returns.
pub async fn run_script(
    browser: &Entity<BrowserView>,
    body: &str,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    let receiver =
        browser.read_with(cx, |browser, _| browser.web_view_or_err()?.run_script(body))?;
    let timeout = cx.background_executor().timer(SCRIPT_TIMEOUT);
    futures::select_biased! {
        result = receiver.fuse() => result.context("the page went away before the script finished")?,
        _ = timeout.fuse() => anyhow::bail!("the script did not finish within {} seconds", SCRIPT_TIMEOUT.as_secs()),
    }
}

/// Brings the tab to the front of its pane, if it isn't, so the page is
/// drawn and can be captured and clicked.
pub async fn reveal(browser: &Entity<BrowserView>, cx: &mut AsyncApp) -> Result<()> {
    let (window, workspace) = browser.read_with(cx, |browser, _| {
        (browser.window_handle, browser.workspace.clone())
    });
    let browser_id = browser.entity_id();
    let revealed = window.update(cx, |root, window, cx| {
        let workspace = workspace
            .upgrade()
            .context("the tab's workspace was closed")?;
        if let Ok(multi_workspace) = root.downcast::<MultiWorkspace>() {
            anyhow::ensure!(
                multi_workspace.read(cx).workspace() == &workspace,
                "the browser tab's project isn't the one showing in its window"
            );
        }
        reveal_item(&workspace, browser_id, window, cx)
    })??;
    if revealed {
        cx.background_executor().timer(REVEAL_DELAY).await;
    }
    Ok(())
}

/// Activates the item in its pane, returning whether it wasn't active.
fn reveal_item(
    workspace: &Entity<Workspace>,
    item_id: EntityId,
    window: &mut Window,
    cx: &mut App,
) -> Result<bool> {
    let pane = workspace
        .read(cx)
        .pane_for_item_id(item_id)
        .context("the browser tab is not in a pane")?;
    let is_active = pane
        .read(cx)
        .active_item()
        .is_some_and(|item| item.item_id() == item_id);
    if is_active {
        return Ok(false);
    }
    pane.update(cx, |pane, cx| {
        let index = pane
            .items()
            .position(|item| item.item_id() == item_id)
            .context("the browser tab is not in its pane")?;
        pane.activate_item(index, false, false, window, cx);
        anyhow::Ok(())
    })?;
    Ok(true)
}

pub async fn screenshot(browser: &Entity<BrowserView>, cx: &mut AsyncApp) -> Result<Screenshot> {
    reveal(browser, cx).await?;
    let (receiver, size) = browser.read_with(cx, |browser, _| {
        let web_view = browser.web_view_or_err()?;
        anyhow::Ok((web_view.snapshot()?, web_view.page_size()))
    })?;
    let timeout = cx.background_executor().timer(SCRIPT_TIMEOUT);
    let tiff = futures::select_biased! {
        result = receiver.fuse() => result.context("the page went away before it was captured")??,
        _ = timeout.fuse() => anyhow::bail!("capturing the page timed out"),
    };
    let width = f32::from(size.width).round() as u32;
    let height = f32::from(size.height).round() as u32;
    cx.background_spawn(async move { encode_screenshot(&tiff, width, height) })
        .await
}

/// Scales the capture, which is in screen pixels, to CSS pixels, and encodes
/// it as PNG.
fn encode_screenshot(tiff: &[u8], width: u32, height: u32) -> Result<Screenshot> {
    let image = image::load_from_memory_with_format(tiff, image::ImageFormat::Tiff)
        .context("reading the page's capture")?;
    let image = if width > 0 && height > 0 && image.width() > width {
        image.resize_exact(width, height, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let image = image::DynamicImage::ImageRgb8(image.to_rgb8());
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .context("encoding the page's capture")?;
    Ok(Screenshot {
        png,
        width: image.width(),
        height: image.height(),
    })
}

/// Clicks in the page with AppKit mouse events, as a person would. The page
/// keeps the keyboard afterwards only when the click focused a text field,
/// so that typing into it works; otherwise the keyboard goes back to where
/// the person had it.
pub async fn click(
    browser: &Entity<BrowserView>,
    target: ClickTarget,
    click_count: isize,
    cx: &mut AsyncApp,
) -> Result<ClickOutcome> {
    reveal(browser, cx).await?;
    let (point, element, covered_by) = match target {
        ClickTarget::Point(point) => (point, None, None),
        ClickTarget::Element(target) => {
            let target = serde_json::to_string(&target)?;
            let value = run_script(
                browser,
                &format!("return await window.__bench.clickPoint({target});"),
                cx,
            )
            .await?;
            let ClickPoint {
                x,
                y,
                element,
                covered,
            } = serde_json::from_value(value).context("reading where to click")?;
            (gpui::point(px(x), px(y)), Some(element), covered)
        }
    };

    let window = browser.read_with(cx, |browser, _| browser.window_handle);
    let previous_focus = window.update(cx, |_, window, cx| {
        browser.update(cx, |browser, cx| -> Result<Option<KeyboardFocus>> {
            browser.ignore_mouse_down_until = Some(Instant::now() + AGENT_CLICK_WINDOW);
            let had_focus = browser.focus_handle.is_focused(window);
            let web_view = browser.web_view_or_err()?;
            let previous_focus = (!had_focus).then(|| web_view.keyboard_focus());
            web_view.click(point, click_count)?;
            cx.notify();
            Ok(previous_focus)
        })
    })??;

    cx.background_executor().timer(CLICK_SETTLE_DELAY).await;
    let text_field_focused = run_script(browser, "return window.__bench.focusedEditable();", cx)
        .await
        .log_err()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    window.update(cx, |_, window, cx| {
        browser.update(cx, |browser, cx| {
            if text_field_focused {
                window.focus(&browser.focus_handle, cx);
            } else if let Some(previous_focus) = previous_focus
                && let Some(web_view) = &browser.web_view
            {
                web_view.restore_keyboard_focus(previous_focus);
            }
        })
    })?;

    Ok(ClickOutcome {
        element,
        x: f32::from(point.x),
        y: f32::from(point.y),
        covered_by,
        text_field_focused,
    })
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct TypeOutcome {
    /// The field typed into.
    pub element: String,
    /// The field's text afterwards.
    pub value: String,
}

/// Types `text` into the text field focused in the page, after clicking
/// `target` to focus it when there is one.
pub async fn type_text(
    browser: &Entity<BrowserView>,
    target: Option<serde_json::Value>,
    text: &str,
    clear: bool,
    submit: bool,
    cx: &mut AsyncApp,
) -> Result<TypeOutcome> {
    if let Some(target) = target {
        click(browser, ClickTarget::Element(target), 1, cx).await?;
    }
    let text = serde_json::to_string(text)?;
    let value = run_script(
        browser,
        &format!("return window.__bench.typeText({text}, {clear});"),
        cx,
    )
    .await?;
    let outcome: TypeOutcome = serde_json::from_value(value).context("reading what was typed")?;
    if submit {
        press_key(browser, "Enter", cx).await?;
    }
    Ok(outcome)
}

/// Presses a key, such as `Enter` or `Shift+Tab`, in the page.
pub async fn press_key(browser: &Entity<BrowserView>, key: &str, cx: &mut AsyncApp) -> Result<()> {
    let key = parse_key(key)?;
    reveal(browser, cx).await?;
    browser.read_with(cx, |browser, _| browser.web_view_or_err()?.press_key(&key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_read_like_an_address_bar() {
        let url = |input: &str| normalize_url(input).map(|url| url.to_string()).ok();
        assert_eq!(
            url("localhost:3000").as_deref(),
            Some("http://localhost:3000/")
        );
        assert_eq!(url("3000").as_deref(), Some("http://localhost:3000/"));
        assert_eq!(
            url("127.0.0.1:8080/app?x=1").as_deref(),
            Some("http://127.0.0.1:8080/app?x=1")
        );
        assert_eq!(url("example.com").as_deref(), Some("https://example.com/"));
        assert_eq!(
            url("http://example.com/a").as_deref(),
            Some("http://example.com/a")
        );
        assert_eq!(url("myapp.test").as_deref(), Some("http://myapp.test/"));
        assert_eq!(url("not a url"), None);
        assert_eq!(url("word"), None);
        assert_eq!(url(""), None);
    }

    #[test]
    fn loopback_urls_are_recognized() {
        let loopback = |input: &str| is_loopback(&Url::parse(input).unwrap());
        assert!(loopback("http://localhost:3000"));
        assert!(loopback("http://127.0.0.1:5173/"));
        assert!(loopback("http://[::1]:8080/"));
        assert!(loopback("http://0.0.0.0:8080/"));
        assert!(loopback("http://app.localhost/"));
        assert!(!loopback("https://example.com"));
        assert!(!loopback("about:blank"));
    }
}
