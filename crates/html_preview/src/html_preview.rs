mod web_view;

use std::cell::Cell;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use file_icons::FileIcons;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    Action, App, Bounds, ClipboardItem, Context, DismissEvent, Element, ElementId, Entity,
    EntityId, EventEmitter, FocusHandle, Focusable, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, IntoElement, LayoutId, Pixels, Point, Render, Style, Subscription, Task,
    WeakEntity, Window, actions, anchored, deferred, px, relative,
};
use language::{Buffer, BufferEvent};
use project::{DisableAiSettings, Project, ProjectPath};
use regex::Regex;
use theme::{Appearance, ThemeScope};
use ui::{ContextMenu, appearance_toggle, prelude::*};
use util::ResultExt as _;
use workspace::item::Item;
use workspace::notifications::NotifyResultExt as _;
use workspace::{Pane, Workspace};
use zed_actions::claude::{
    AddComment, AddCommentOn, CommentTarget, SendLines, SendSelection, SendText,
};

use crate::web_view::{PageEvent, WebView, page_url};

pub use zed_actions::preview::html::{OpenPreview, OpenPreviewToTheSide};

actions!(
    html_preview,
    [
        /// Loads the page again, picking up changes to it and the files it uses.
        Reload,
        /// Copies the text selected in the page.
        CopySelection,
        /// Selects everything in the page.
        SelectAll,
        /// Pastes the clipboard into the field focused in the page.
        Paste,
        /// Opens the HTML file the page is showing in an editor.
        OpenSource,
        /// Opens the Web Inspector for the page.
        OpenDevTools,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace
            .register_action(|workspace, _: &OpenPreview, window, cx| {
                let Some(project_path) = active_html_path(workspace, cx) else {
                    return;
                };
                let pane = workspace.active_pane().clone();
                HtmlPreviewView::open(workspace, project_path, pane, true, window, cx)
                    .notify_err(workspace, cx);
            })
            .register_action(|workspace, _: &OpenPreviewToTheSide, window, cx| {
                let Some(project_path) = active_html_path(workspace, cx) else {
                    return;
                };
                let origin_pane = workspace.active_pane().clone();
                let pane = workspace.adjacent_pane_of(&origin_pane, window, cx);
                HtmlPreviewView::open(workspace, project_path, pane, false, window, cx)
                    .notify_err(workspace, cx);
            });
    })
    .detach();
}

/// Whether a file is a page the HTML preview shows.
pub fn is_html_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
        })
}

fn active_html_path(workspace: &Workspace, cx: &App) -> Option<ProjectPath> {
    let project_path = workspace.active_item(cx)?.project_path(cx)?;
    is_html_file(project_path.path.as_std_path()).then_some(project_path)
}

/// A tab showing an HTML file from the project in a web view.
pub struct HtmlPreviewView {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project_path: ProjectPath,
    abs_path: PathBuf,
    web_view: Option<WebView>,
    load_error: Option<SharedString>,
    /// The file's buffer, which reports when the file changes and which page
    /// selections are matched against to find the lines they come from.
    buffer: Option<Entity<Buffer>>,
    /// What is selected in the page, as the page last reported it.
    selection: String,
    /// Where the page is in the window, as last laid out.
    page_bounds: Cell<Bounds<Pixels>>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    /// Light or dark, when switched away from Bench's theme's appearance.
    appearance: Option<Appearance>,
    /// The appearance last given to the web view.
    applied_appearance: Option<Appearance>,
    _subscriptions: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
}

impl HtmlPreviewView {
    /// Shows `project_path` in `pane`, in the preview already showing it there
    /// if there is one.
    pub fn open(
        workspace: &mut Workspace,
        project_path: ProjectPath,
        pane: Entity<Pane>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Result<Entity<Self>> {
        let existing = pane
            .read(cx)
            .items_of_type::<Self>()
            .find(|preview| preview.read(cx).project_path == project_path);
        if let Some(preview) = existing {
            pane.update(cx, |pane, cx| {
                if let Some(index) = pane.index_for_item(&preview) {
                    pane.activate_item(index, focus, focus, window, cx);
                }
            });
            return Ok(preview);
        }

        let project = workspace.project().read(cx);
        let root = project
            .worktree_for_id(project_path.worktree_id, cx)
            .context("the file's worktree is no longer open")?
            .read(cx)
            .abs_path()
            .to_path_buf();
        let workspace_entity = cx.entity();
        let project = workspace.project().clone();
        let preview =
            cx.new(|cx| Self::new(workspace_entity, project, project_path, root, window, cx));
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(preview.clone()), true, focus, None, window, cx)
        });
        Ok(preview)
    }

    fn new(
        workspace: Entity<Workspace>,
        project: Entity<Project>,
        project_path: ProjectPath,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let abs_path = root.join(project_path.path.as_std_path());
        let fs = project.read(cx).fs().clone();

        let (events_sender, mut events) = mpsc::unbounded();
        let (web_view, load_error) = match WebView::new(
            page_url(&project_path.path),
            root,
            fs,
            events_sender,
            window,
            cx,
        ) {
            Ok(web_view) => (Some(web_view), None),
            Err(error) => {
                log::error!("opening HTML preview: {error:#}");
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

        let open_buffer = project.update(cx, |project, cx| {
            project.open_buffer(project_path.clone(), cx)
        });
        let watch_buffer = cx.spawn_in(window, async move |this, cx| {
            let Some(buffer) = open_buffer.await.log_err() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                let subscription =
                    cx.subscribe_in(&buffer, window, |this, _, event: &BufferEvent, _, _| {
                        match event {
                            BufferEvent::Saved | BufferEvent::Reloaded => this.reload_page(),
                            _ => {}
                        }
                    });
                this._subscriptions.push(subscription);
                this.buffer = Some(buffer);
            })
            .log_err();
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
            // Zooming happens in the workspace, and the preview has to be drawn
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
            project_path,
            abs_path,
            web_view,
            load_error,
            buffer: None,
            selection: String::new(),
            page_bounds: Cell::new(Bounds::default()),
            context_menu: None,
            appearance: None,
            applied_appearance: None,
            _subscriptions: subscriptions,
            _tasks: vec![handle_events, watch_buffer],
        }
    }

    pub fn project_path(&self) -> &ProjectPath {
        &self.project_path
    }

    /// Whether another pane or a panel is zoomed over this preview. The zoomed
    /// view draws on top of the center panes, but below native views, so the
    /// page has to be hidden for it to show.
    fn is_covered_by_zoomed_item(&self, preview_id: EntityId, cx: &App) -> bool {
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };
        let workspace = workspace.read(cx);
        let Some(zoomed) = workspace.zoomed_item().and_then(|zoomed| zoomed.upgrade()) else {
            return false;
        };
        workspace
            .pane_for_item_id(preview_id)
            .is_none_or(|pane| pane.entity_id() != zoomed.entity_id())
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
                window.focus(&self.focus_handle, cx);
            }
            PageEvent::OpenExternally(url) => cx.open_url(&url),
        }
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let send_to_agent = self.send_to_agent_action(cx);
        let comment_target = self.selection_target(cx);
        let has_selection = !self.selection.is_empty();
        let focus_handle = self.focus_handle.clone();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            menu.context(focus_handle)
                .when_some(send_to_agent, |menu, action| {
                    menu.entry(
                        "Send to Agent",
                        Some(Box::new(SendSelection)),
                        move |window, cx| window.dispatch_action(action.boxed_clone(), cx),
                    )
                    .when_some(comment_target, |menu, target| {
                        menu.entry(
                            "Add Comment…",
                            Some(Box::new(AddComment)),
                            move |window, cx| {
                                window.dispatch_action(
                                    Box::new(AddCommentOn {
                                        target: target.clone(),
                                    }),
                                    cx,
                                )
                            },
                        )
                    })
                    .separator()
                })
                .when(has_selection, |menu| {
                    menu.action("Copy", Box::new(CopySelection))
                })
                .action("Select All", Box::new(SelectAll))
                .separator()
                .action("Reload", Box::new(Reload))
                .action("Open Source", Box::new(OpenSource))
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

    /// What sends the page's selection to the agent: the lines of the file it
    /// was rendered from when they can be found, and otherwise the text.
    fn send_to_agent_action(&self, cx: &App) -> Option<Box<dyn Action>> {
        let target = self.selection_target(cx)?;
        match (target.start_line, target.end_line) {
            (Some(start_line), Some(end_line)) => Some(Box::new(SendLines {
                path: target.path,
                start_line,
                end_line,
            })),
            _ => Some(Box::new(SendText {
                text: format!(
                    "From {}:\n{}\n",
                    self.project_path
                        .path
                        .display(util::paths::PathStyle::local()),
                    self.selection.trim()
                ),
            })),
        }
    }

    /// The page's selection, as the lines of the file it was rendered from
    /// when they can be found, and as its text otherwise.
    fn selection_target(&self, cx: &App) -> Option<CommentTarget> {
        let selection = self.selection.trim();
        if selection.is_empty()
            || DisableAiSettings::is_ai_disabled_for_buffer(self.buffer.as_ref(), cx)
        {
            return None;
        }
        let source_lines = self
            .buffer
            .as_ref()
            .and_then(|buffer| source_lines_for_selection(&buffer.read(cx).text(), selection));
        Some(CommentTarget {
            path: self.abs_path.to_string_lossy().into_owned(),
            start_line: source_lines.as_ref().map(|lines| *lines.start()),
            end_line: source_lines.as_ref().map(|lines| *lines.end()),
            excerpt: Some(selection.to_string()),
        })
    }

    fn add_comment(&mut self, _: &AddComment, window: &mut Window, cx: &mut Context<Self>) {
        match self.selection_target(cx) {
            Some(target) => window.dispatch_action(Box::new(AddCommentOn { target }), cx),
            None => cx.propagate(),
        }
    }

    fn send_selection(&mut self, _: &SendSelection, window: &mut Window, cx: &mut Context<Self>) {
        match self.send_to_agent_action(cx) {
            Some(action) => window.dispatch_action(action, cx),
            None => cx.propagate(),
        }
    }

    fn reload_page(&self) {
        if let Some(web_view) = &self.web_view {
            web_view.reload().log_err();
        }
    }

    fn reload(&mut self, _: &Reload, _: &mut Window, _: &mut Context<Self>) {
        self.reload_page();
    }

    fn copy_selection(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.selection.clone()));
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, _: &mut Context<Self>) {
        if let Some(web_view) = &self.web_view {
            web_view.select_all().log_err();
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if let Some(web_view) = &self.web_view {
            web_view.insert_text(&text).log_err();
        }
    }

    fn open_source(&mut self, _: &OpenSource, window: &mut Window, cx: &mut Context<Self>) {
        let project_path = self.project_path.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_path(project_path, None, true, window, cx)
                    .detach_and_log_err(cx);
            })
            .log_err();
    }

    fn open_dev_tools(&mut self, _: &OpenDevTools, _: &mut Window, _: &mut Context<Self>) {
        if let Some(web_view) = &self.web_view {
            web_view.open_devtools();
        }
    }
}

/// The one-based lines of `source` that `selection`, text selected in the
/// page rendered from it, was found on. Each selected line is looked for in
/// turn, with its words allowed any whitespace between them, since the page
/// joins and wraps lines. Lines that markup splits up, such as a word in bold,
/// are skipped; the range runs from the first line found to the last.
fn source_lines_for_selection(source: &str, selection: &str) -> Option<RangeInclusive<u32>> {
    let body_start = Regex::new(r"(?i)<body[\s>]")
        .ok()
        .and_then(|body| body.find(source))
        .map_or(0, |body| body.start());
    let mut search_from = body_start;
    let mut start = None;
    let mut end = None;
    for line in selection.lines() {
        let words = line
            .split_whitespace()
            .map(regex::escape)
            .collect::<Vec<_>>();
        if words.is_empty() {
            continue;
        }
        let Ok(pattern) = Regex::new(&words.join(r"\s+")) else {
            continue;
        };
        if let Some(found) = pattern.find_at(source, search_from) {
            start.get_or_insert(found.start());
            end = Some(found.end());
            search_from = found.end();
        }
    }
    let line_of = |offset: usize| {
        let newlines = source
            .get(..offset)
            .map_or(0, |text| text.matches('\n').count());
        u32::try_from(newlines)
            .unwrap_or(u32::MAX)
            .saturating_add(1)
    };
    Some(line_of(start?)..=line_of(end?))
}

impl Render for HtmlPreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let appearance = self.appearance.unwrap_or_else(|| cx.theme().appearance());
        if self.applied_appearance != Some(appearance)
            && let Some(web_view) = &self.web_view
        {
            web_view.set_appearance(appearance);
            self.applied_appearance = Some(appearance);
        }
        let show_toggle =
            self.web_view.is_some() && !self.is_covered_by_zoomed_item(cx.entity_id(), cx);

        v_flex()
            .id("HtmlPreview")
            .key_context("HtmlPreview")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::send_selection))
            .on_action(cx.listener(Self::add_comment))
            .on_action(cx.listener(Self::reload))
            .on_action(cx.listener(Self::copy_selection))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::open_source))
            .on_action(cx.listener(Self::open_dev_tools))
            .map(|this| match &self.load_error {
                Some(error) => this.items_center().justify_center().child(
                    h_flex()
                        .p_4()
                        .gap_2()
                        .child(Icon::new(IconName::Warning).color(Color::Warning))
                        .child(Label::new(error.clone()).color(Color::Muted)),
                ),
                None => this.child(PageElement { view: cx.entity() }),
            })
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .snap_to_window_with_margin(px(8.))
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
            .when(show_toggle, |this| {
                this.child(self.render_appearance_toggle(appearance, cx))
            })
    }
}

impl HtmlPreviewView {
    /// Deferred, as the page is drawn above everything that isn't.
    fn render_appearance_toggle(
        &self,
        appearance: Appearance,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = (appearance != cx.theme().appearance())
            .then(|| theme_settings::theme_for_appearance(appearance, None, cx));
        let colors = theme.as_ref().unwrap_or_else(|| cx.theme()).colors();
        let toggle = div()
            .absolute()
            .top_2()
            .right_4()
            .p_0p5()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background)
            .shadow_sm()
            .child(appearance_toggle(
                "toggle-appearance",
                appearance,
                cx.listener(|this, appearance: &Appearance, _, cx| {
                    this.appearance = Some(*appearance);
                    cx.notify();
                }),
            ));
        deferred(ThemeScope::new(theme, toggle)).with_priority(0)
    }
}

/// Places the native web view where the page is laid out.
struct PageElement {
    view: Entity<HtmlPreviewView>,
}

impl IntoElement for PageElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PageElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let view = self.view.read(cx);
        view.page_bounds.set(bounds);
        if view.is_covered_by_zoomed_item(self.view.entity_id(), cx) {
            return None;
        }
        let web_view = view.web_view.as_ref()?;
        web_view.set_bounds(bounds, window.scale_factor()).log_err();
        // AppKit sends the clicks over the page to the page, so GPUI content
        // under it must not act as though it were hovered.
        Some(window.insert_hitbox(bounds, HitboxBehavior::BlockMouse))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if hitbox.is_some()
            && let Some(web_view) = &self.view.read(cx).web_view
        {
            web_view.paint(window);
        }
    }
}

impl Focusable for HtmlPreviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for HtmlPreviewView {}

impl Item for HtmlPreviewView {
    type Event = ();

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        FileIcons::get_icon(self.project_path.path.as_std_path(), cx)
            .map(Icon::from_path)
            .or_else(|| Some(Icon::new(IconName::Public)))
    }

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        self.project_path
            .path
            .file_name()
            .map(|name| SharedString::from(name.to_string()))
            .unwrap_or_else(|| "HTML Preview".into())
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some(self.abs_path.to_string_lossy().into_owned().into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(_: &Self::Event, _: &mut dyn FnMut(workspace::item::ItemEvent)) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "<!doctype html>
<html>
<head><title>Pricing</title></head>
<body>
  <h1>Pricing</h1>
  <p>Simple plans for
     every team.</p>
  <ul>
    <li>Starter</li>
    <li>Team <b>plus</b></li>
    <li>Enterprise</li>
  </ul>
</body>
</html>
";

    #[test]
    fn a_selected_heading_is_found_in_the_body_not_the_title() {
        assert_eq!(source_lines_for_selection(PAGE, "Pricing"), Some(5..=5));
    }

    #[test]
    fn a_paragraph_wrapped_in_the_source_is_found() {
        assert_eq!(
            source_lines_for_selection(PAGE, "Simple plans for every team."),
            Some(6..=7)
        );
    }

    #[test]
    fn several_lines_span_from_the_first_to_the_last_found() {
        assert_eq!(
            source_lines_for_selection(PAGE, "Starter\nTeam plus\nEnterprise"),
            Some(9..=11)
        );
    }

    #[test]
    fn text_not_in_the_source_is_not_found() {
        assert_eq!(
            source_lines_for_selection(PAGE, "Rendered by a script"),
            None
        );
    }
}
