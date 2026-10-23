use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use fs::Fs;
use gpui::{Pixels, Point};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::Deserialize;
use util::rel_path::RelPath;

pub(crate) use platform::{KeyboardFocus, WebView};

/// The scheme pages are served on. Serving files through it, rather than
/// loading `file://` URLs, lets a page load its scripts, styles and data from
/// the worktree the way it would from a web server.
const SCHEME: &str = "bench-preview";

/// Characters left as they are in a URL path segment: RFC 3986's unreserved
/// set.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Reports to Bench what the person does in the page that Bench acts on: what
/// they select, where they open a context menu, and when they click into it.
const PAGE_SCRIPT: &str = r#"
(() => {
  const post = (message) => window.ipc.postMessage(JSON.stringify(message));
  const selectedText = () => String(window.getSelection() ?? "");
  let lastSelection = "";
  document.addEventListener("selectionchange", () => {
    const text = selectedText();
    if (text !== lastSelection) {
      lastSelection = text;
      post({ type: "selection", text });
    }
  });
  window.addEventListener(
    "contextmenu",
    (event) => {
      event.preventDefault();
      post({ type: "context_menu", x: event.clientX, y: event.clientY, text: selectedText() });
    },
    true,
  );
  window.addEventListener("mousedown", () => post({ type: "mouse_down" }), true);
})();
"#;

/// What a web view shows.
pub(crate) enum PageSource {
    /// A file from a worktree, served on [`SCHEME`] from `root`. Links that
    /// leave the worktree open in the person's browser.
    Worktree {
        url: String,
        root: PathBuf,
        fs: Arc<dyn Fs>,
    },
    /// Pages from the web, in Bench's browser. `script` runs at the start of
    /// every page it loads, before the page's own scripts.
    Web { url: String, script: &'static str },
}

pub(crate) enum PageEvent {
    SelectionChanged(String),
    /// A right click, at a position relative to the page's top left corner.
    ContextMenu {
        position: Point<Pixels>,
        selection: String,
    },
    MouseDown,
    OpenExternally(String),
    /// The page asked for a URL to open in a new window.
    OpenInNewTab(String),
    LoadStarted(String),
    LoadFinished(String),
    TitleChanged(String),
    /// The page's URL changed without a new page loading, as it does when a
    /// single-page app routes.
    LocationChanged(String),
    Console {
        level: String,
        text: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PageMessage {
    Selection { text: String },
    ContextMenu { x: f32, y: f32, text: String },
    MouseDown,
    Location { url: String },
    Console { level: String, text: String },
}

impl From<PageMessage> for PageEvent {
    fn from(message: PageMessage) -> Self {
        match message {
            PageMessage::Selection { text } => PageEvent::SelectionChanged(text),
            PageMessage::ContextMenu { x, y, text } => PageEvent::ContextMenu {
                position: gpui::point(gpui::px(x), gpui::px(y)),
                selection: text,
            },
            PageMessage::MouseDown => PageEvent::MouseDown,
            PageMessage::Location { url } => PageEvent::LocationChanged(url),
            PageMessage::Console { level, text } => PageEvent::Console { level, text },
        }
    }
}

/// The URL a file in the worktree is served at.
pub(crate) fn page_url(path: &RelPath) -> String {
    let mut url = format!("{SCHEME}://localhost");
    for segment in path.components() {
        url.push('/');
        url.extend(utf8_percent_encode(segment, PATH_SEGMENT));
    }
    url
}

/// The file a request for `request_path` is served from, which is always
/// inside `root`.
fn resolve_request_path(root: &Path, request_path: &str) -> Option<PathBuf> {
    let decoded = percent_decode_str(request_path).decode_utf8().ok()?;
    let relative = decoded.trim_start_matches('/');
    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if relative.is_empty() || relative.ends_with('/') {
        path.push("index.html");
    }
    Some(path)
}

/// Whether a navigation leaves the worktree, and so opens in the person's
/// browser instead of replacing the page.
fn opens_externally(url: &str) -> bool {
    ["http:", "https:", "mailto:"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
}

/// Whether Bench's browser loads a URL itself. Anything else, such as a
/// `mailto:` link or an app's own scheme, is handed to the system.
fn browser_loads(url: &str) -> bool {
    ["http:", "https:", "about:", "data:", "blob:"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
}

/// Wraps a script an agent runs in the page, which is the body of an async
/// function, so that what it returns comes back as JSON text. A value JSON
/// can't express, such as a function, comes back as `null`.
fn script_function_body(body: &str) -> String {
    format!(
        "const value = await (async () => {{\n{body}\n}})();\n\
         return JSON.stringify(value === undefined ? null : value) ?? \"null\";"
    )
}

/// A key press, as AppKit describes one.
#[derive(Debug, PartialEq)]
pub(crate) struct KeyStroke {
    characters: String,
    characters_ignoring_modifiers: String,
    key_code: u16,
    shift: bool,
    control: bool,
    option: bool,
    command: bool,
}

/// Reads a key such as `Enter`, `a` or `Shift+Tab`. Modifiers are `Shift`,
/// `Control`, `Alt` and `Meta`, and the key is a single character or one of
/// the names `KeyboardEvent.key` uses for keys that don't type one.
pub(crate) fn parse_key(spec: &str) -> Result<KeyStroke> {
    let (modifiers, key) = match spec.strip_suffix("++") {
        Some(modifiers) => (modifiers, "+"),
        None => match spec.rsplit_once('+') {
            Some((modifiers, key)) if !key.is_empty() => (modifiers, key),
            _ => ("", spec),
        },
    };
    let mut stroke = KeyStroke {
        characters: String::new(),
        characters_ignoring_modifiers: String::new(),
        key_code: 0,
        shift: false,
        control: false,
        option: false,
        command: false,
    };
    for modifier in modifiers.split('+').filter(|modifier| !modifier.is_empty()) {
        match modifier.to_ascii_lowercase().as_str() {
            "shift" => stroke.shift = true,
            "control" | "ctrl" => stroke.control = true,
            "alt" | "option" => stroke.option = true,
            "meta" | "cmd" | "command" => stroke.command = true,
            _ => anyhow::bail!("unknown modifier `{modifier}` in `{spec}`"),
        }
    }

    // Function keys type the characters AppKit reserves for them, such as
    // NSUpArrowFunctionKey.
    let named = match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => Some(("\r", 36)),
        "tab" => Some(("\t", 48)),
        "escape" | "esc" => Some(("\u{1b}", 53)),
        "backspace" => Some(("\u{7f}", 51)),
        "delete" => Some(("\u{f728}", 117)),
        "space" => Some((" ", 49)),
        "arrowup" | "up" => Some(("\u{f700}", 126)),
        "arrowdown" | "down" => Some(("\u{f701}", 125)),
        "arrowleft" | "left" => Some(("\u{f702}", 123)),
        "arrowright" | "right" => Some(("\u{f703}", 124)),
        "home" => Some(("\u{f729}", 115)),
        "end" => Some(("\u{f72b}", 119)),
        "pageup" => Some(("\u{f72c}", 116)),
        "pagedown" => Some(("\u{f72d}", 121)),
        _ => None,
    };
    if let Some((characters, key_code)) = named {
        stroke.characters = characters.to_string();
        stroke.characters_ignoring_modifiers = characters.to_string();
        stroke.key_code = key_code;
        return Ok(stroke);
    }

    let mut characters = key.chars();
    let (Some(character), None) = (characters.next(), characters.next()) else {
        anyhow::bail!("unknown key `{key}` in `{spec}`");
    };
    let unshifted = character.to_lowercase().to_string();
    stroke.characters = if stroke.shift {
        character.to_uppercase().to_string()
    } else {
        character.to_string()
    };
    stroke.characters_ignoring_modifiers = unshifted.clone();
    // Pages read the physical key, `KeyboardEvent.code`, from the key code,
    // so characters on the US layout get theirs.
    stroke.key_code = unshifted
        .chars()
        .next()
        .and_then(us_layout_key_code)
        .unwrap_or(0);
    Ok(stroke)
}

fn us_layout_key_code(character: char) -> Option<u16> {
    const KEYS: &str = "asdfhgzxcv\0bqweryt123465=97-80]ou[ip\0lj'k;\\,/nm.\0\0`";
    KEYS.chars()
        .position(|key| key == character && key != '\0')
        .and_then(|position| u16::try_from(position).ok())
}

fn content_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("wasm") => "application/wasm",
        Some("xml") => "application/xml",
        Some("txt" | "md" | "csv") => "text/plain; charset=utf-8",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;

    use anyhow::{Context as _, Result};
    use block2::RcBlock;
    use fs::Fs;
    use futures::channel::{mpsc::UnboundedSender, oneshot};
    use gpui::{
        AnyWindowHandle, App, Bounds, DevicePixels, Pixels, Point, Size, Window,
        WindowCompositionSurface,
    };
    use http::header::{CACHE_CONTROL, CONTENT_TYPE};
    use http::{HeaderValue, StatusCode};
    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{
        NSAppearance, NSAppearanceCustomization as _, NSAppearanceNameAqua,
        NSAppearanceNameDarkAqua, NSEvent, NSEventModifierFlags, NSEventType, NSImage, NSResponder,
        NSView,
    };
    use objc2_foundation::{NSError, NSPoint, NSProcessInfo, NSRect, NSSize, NSString};
    use objc2_web_kit::{WKContentWorld, WKSnapshotConfiguration};
    use theme::Appearance;
    use util::ResultExt as _;
    use wry::WebViewExtMacOS as _;

    use super::{
        KeyStroke, PAGE_SCRIPT, PageEvent, PageMessage, PageSource, SCHEME, browser_loads,
        content_type, opens_externally, resolve_request_path, script_function_body,
    };

    /// A WKWebView in a native surface of the window's composition, so that
    /// GPUI's overlays (context menus, popovers, tooltips) draw above it.
    pub(crate) struct WebView {
        webview: wry::WebView,
        surface: WindowCompositionSurface,
        window: AnyWindowHandle,
        bounds: Cell<Option<Bounds<DevicePixels>>>,
    }

    /// Whatever had the keyboard in the window, to give it back after input
    /// sent to the page moved it there.
    pub(crate) struct KeyboardFocus(Option<Retained<NSResponder>>);

    impl WebView {
        pub(crate) fn new(
            source: PageSource,
            events: UnboundedSender<PageEvent>,
            window: &mut Window,
            cx: &App,
        ) -> Result<Self> {
            let builder = wry::WebViewBuilder::new()
                .with_initialization_script_for_main_only(PAGE_SCRIPT, true)
                .with_accept_first_mouse(true)
                .with_devtools(true)
                .with_ipc_handler({
                    let events = events.clone();
                    move |request| match serde_json::from_str::<PageMessage>(request.body()) {
                        Ok(message) => {
                            events.unbounded_send(message.into()).log_err();
                        }
                        Err(error) => log::warn!("unreadable message from a web view: {error}"),
                    }
                });
            let builder = match source {
                PageSource::Worktree { url, root, fs } => {
                    let executor = cx.background_executor().clone();
                    builder
                        .with_url(url)
                        .with_asynchronous_custom_protocol(
                            SCHEME.to_string(),
                            move |_, request, responder| {
                                let path = resolve_request_path(&root, request.uri().path());
                                let fs = fs.clone();
                                executor
                                    .spawn(async move {
                                        responder.respond(serve(path, fs.as_ref()).await);
                                    })
                                    .detach();
                            },
                        )
                        .with_navigation_handler({
                            let events = events.clone();
                            move |url| {
                                if opens_externally(&url) {
                                    events
                                        .unbounded_send(PageEvent::OpenExternally(url))
                                        .log_err();
                                    return false;
                                }
                                true
                            }
                        })
                        .with_new_window_req_handler(move |url, _| {
                            events
                                .unbounded_send(PageEvent::OpenExternally(url))
                                .log_err();
                            wry::NewWindowResponse::Deny
                        })
                }
                PageSource::Web { url, script } => builder
                    .with_url(url)
                    .with_initialization_script_for_main_only(script, true)
                    .with_back_forward_navigation_gestures(true)
                    .with_navigation_handler({
                        let events = events.clone();
                        move |url| {
                            if browser_loads(&url) {
                                return true;
                            }
                            events
                                .unbounded_send(PageEvent::OpenExternally(url))
                                .log_err();
                            false
                        }
                    })
                    .with_on_page_load_handler({
                        let events = events.clone();
                        move |event, url| {
                            let event = match event {
                                wry::PageLoadEvent::Started => PageEvent::LoadStarted(url),
                                wry::PageLoadEvent::Finished => PageEvent::LoadFinished(url),
                            };
                            events.unbounded_send(event).log_err();
                        }
                    })
                    .with_document_title_changed_handler({
                        let events = events.clone();
                        move |title| {
                            events
                                .unbounded_send(PageEvent::TitleChanged(title))
                                .log_err();
                        }
                    })
                    .with_new_window_req_handler(move |url, _| {
                        events
                            .unbounded_send(PageEvent::OpenInNewTab(url))
                            .log_err();
                        wry::NewWindowResponse::Deny
                    }),
            };
            let webview = builder
                .build_as_child(&*window)
                .context("creating the web view")?;

            let composition = window.enable_window_composition()?;
            let surface = composition.create_native_surface()?;
            composition.show_only_when_painted(surface.id())?;
            let container = *surface
                .platform_surface()?
                .platform_handle()?
                .downcast::<usize>()
                .map_err(|_| anyhow::anyhow!("native surface is not an AppKit view"))?;
            anyhow::ensure!(container != 0, "native surface has no AppKit view");
            // SAFETY: the container is the live NSView the composition surface
            // owns, and this runs on the main thread, as AppKit requires.
            unsafe { (*(container as *const NSView)).addSubview(&webview.webview()) };

            Ok(Self {
                webview,
                surface,
                window: window.window_handle(),
                bounds: Cell::new(None),
            })
        }

        pub(crate) fn set_bounds(&self, bounds: Bounds<Pixels>, scale_factor: f32) -> Result<()> {
            let device_bounds = bounds.to_device_pixels(scale_factor);
            if self.bounds.get() == Some(device_bounds) {
                return Ok(());
            }
            self.surface.platform_surface()?.set_bounds(device_bounds)?;
            // The web view's frame is relative to the surface's view, which
            // the composition positions in the window.
            self.webview.webview().setFrame(NSRect::new(
                NSPoint::new(0., 0.),
                NSSize::new(
                    f64::from(f32::from(bounds.size.width)),
                    f64::from(f32::from(bounds.size.height)),
                ),
            ));
            self.bounds.set(Some(device_bounds));
            Ok(())
        }

        pub(crate) fn paint(&self, window: &mut Window) {
            window.paint_native_surface(self.surface.id());
        }

        pub(crate) fn reload(&self) -> Result<()> {
            Ok(self.webview.reload()?)
        }

        pub(crate) fn load_url(&self, url: &str) -> Result<()> {
            Ok(self.webview.load_url(url)?)
        }

        pub(crate) fn title(&self) -> Option<String> {
            // SAFETY: called on the main thread, where the web view lives.
            unsafe { self.webview.webview().title() }.map(|title| title.to_string())
        }

        pub(crate) fn can_go_back(&self) -> bool {
            // SAFETY: called on the main thread, where the web view lives.
            unsafe { self.webview.webview().canGoBack() }
        }

        pub(crate) fn can_go_forward(&self) -> bool {
            // SAFETY: called on the main thread, where the web view lives.
            unsafe { self.webview.webview().canGoForward() }
        }

        pub(crate) fn go_back(&self) {
            // SAFETY: called on the main thread, where the web view lives.
            unsafe { self.webview.webview().goBack() };
        }

        pub(crate) fn go_forward(&self) {
            // SAFETY: called on the main thread, where the web view lives.
            unsafe { self.webview.webview().goForward() };
        }

        pub(crate) fn stop_loading(&self) {
            // SAFETY: called on the main thread, where the web view lives.
            unsafe { self.webview.webview().stopLoading() };
        }

        /// The page's size, in CSS pixels.
        pub(crate) fn page_size(&self) -> Size<Pixels> {
            let frame = self.webview.webview().frame();
            gpui::size(
                gpui::px(frame.size.width as f32),
                gpui::px(frame.size.height as f32),
            )
        }

        pub(crate) fn select_all(&self) -> Result<()> {
            Ok(self
                .webview
                .evaluate_script("document.execCommand('selectAll')")?)
        }

        /// Runs an editing command, such as `undo` or `delete`, on whatever
        /// has focus in the page.
        pub(crate) fn exec_command(&self, command: &str) -> Result<()> {
            let command = serde_json::to_string(command)?;
            Ok(self
                .webview
                .evaluate_script(&format!("document.execCommand({command})"))?)
        }

        /// Types `text` into the field focused in the page.
        pub(crate) fn insert_text(&self, text: &str) -> Result<()> {
            let text = serde_json::to_string(text)?;
            Ok(self.webview.evaluate_script(&format!(
                "document.execCommand('insertText', false, {text})"
            ))?)
        }

        /// Runs `body` as the body of an async function in the page, and
        /// resolves with what it returns. It runs in the page's own world, so
        /// it sees the page's globals.
        pub(crate) fn run_script(
            &self,
            body: &str,
        ) -> Result<oneshot::Receiver<Result<serde_json::Value>>> {
            let main_thread = MainThreadMarker::new().context("scripts run on the main thread")?;
            let (sender, receiver) = oneshot::channel();
            let sender = RefCell::new(Some(sender));
            let handler = RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
                // SAFETY: WebKit passes either a live error or a live value,
                // each null when absent.
                let result = unsafe { script_result(value, error) };
                if let Some(sender) = sender.borrow_mut().take() {
                    sender.send(result).ok();
                }
            });
            let source = NSString::from_str(&script_function_body(body));
            // SAFETY: on the main thread, with no arguments, in the main
            // frame; WebKit calls the handler once.
            unsafe {
                self.webview
                    .webview()
                    .callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
                        &source,
                        None,
                        None,
                        &WKContentWorld::pageWorld(main_thread),
                        Some(&handler),
                    );
            }
            Ok(receiver)
        }

        /// Captures what the page shows, as TIFF data the size of the page in
        /// screen pixels.
        pub(crate) fn snapshot(&self) -> Result<oneshot::Receiver<Result<Vec<u8>>>> {
            let main_thread =
                MainThreadMarker::new().context("snapshots are taken on the main thread")?;
            let (sender, receiver) = oneshot::channel();
            let sender = RefCell::new(Some(sender));
            let handler = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
                // SAFETY: WebKit passes either a live image or a live error.
                let result = unsafe { snapshot_result(image, error) };
                if let Some(sender) = sender.borrow_mut().take() {
                    sender.send(result).ok();
                }
            });
            // SAFETY: on the main thread; WebKit calls the handler once.
            unsafe {
                let configuration = WKSnapshotConfiguration::new(main_thread);
                configuration.setAfterScreenUpdates(true);
                self.webview
                    .webview()
                    .takeSnapshotWithConfiguration_completionHandler(
                        Some(&configuration),
                        &handler,
                    );
            }
            Ok(receiver)
        }

        /// Whether keys typed in the window go to the page.
        pub(crate) fn has_keyboard_focus(&self) -> bool {
            let webview = self.webview.webview();
            webview.window().is_some_and(|window| {
                window.firstResponder().is_some_and(|responder| {
                    std::ptr::eq(
                        Retained::as_ptr(&responder).cast::<AnyObject>(),
                        Retained::as_ptr(&webview).cast::<AnyObject>(),
                    )
                })
            })
        }

        pub(crate) fn keyboard_focus(&self) -> KeyboardFocus {
            KeyboardFocus(
                self.webview
                    .webview()
                    .window()
                    .and_then(|window| window.firstResponder()),
            )
        }

        pub(crate) fn restore_keyboard_focus(&self, focus: KeyboardFocus) {
            if let Some(window) = self.webview.webview().window() {
                window.makeFirstResponder(focus.0.as_deref());
            }
        }

        /// Clicks the page at `position`, relative to its top left corner,
        /// with AppKit mouse events, so the page sees the trusted pointer
        /// events a person's click makes.
        pub(crate) fn click(&self, position: Point<Pixels>, click_count: isize) -> Result<()> {
            let webview = self.webview.webview();
            let window = webview.window().context("the page is not in a window")?;
            anyhow::ensure!(
                !webview.isHiddenOrHasHiddenAncestor(),
                "the page is not showing"
            );
            // The web view is flipped, so this point is from its top left.
            let location = webview.convertPoint_toView(
                NSPoint::new(
                    f64::from(f32::from(position.x)),
                    f64::from(f32::from(position.y)),
                ),
                None,
            );
            let timestamp = NSProcessInfo::processInfo().systemUptime();
            for count in 1..=click_count.max(1) {
                for (event_type, pressure) in [
                    (NSEventType::LeftMouseDown, 1.),
                    (NSEventType::LeftMouseUp, 0.),
                ] {
                    let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                        event_type,
                        location,
                        NSEventModifierFlags::empty(),
                        timestamp,
                        window.windowNumber(),
                        None,
                        0,
                        count,
                        pressure,
                    )
                    .context("creating a mouse event")?;
                    // The window doesn't deliver mouse events it didn't get
                    // from the system to the web view, so they go to it
                    // directly.
                    if event_type == NSEventType::LeftMouseDown {
                        webview.mouseDown(&event);
                    } else {
                        webview.mouseUp(&event);
                    }
                }
            }
            Ok(())
        }

        /// Presses a key in the page with AppKit key events. The page has the
        /// keyboard only for the key press, unless it had it already.
        pub(crate) fn press_key(&self, key: &KeyStroke) -> Result<()> {
            let webview = self.webview.webview();
            let window = webview.window().context("the page is not in a window")?;
            let previous_focus = (!self.has_keyboard_focus()).then(|| self.keyboard_focus());
            if previous_focus.is_some() {
                window.makeFirstResponder(Some(&webview));
            }
            let mut modifiers = NSEventModifierFlags::empty();
            if key.shift {
                modifiers |= NSEventModifierFlags::Shift;
            }
            if key.control {
                modifiers |= NSEventModifierFlags::Control;
            }
            if key.option {
                modifiers |= NSEventModifierFlags::Option;
            }
            if key.command {
                modifiers |= NSEventModifierFlags::Command;
            }
            let characters = NSString::from_str(&key.characters);
            let characters_ignoring_modifiers =
                NSString::from_str(&key.characters_ignoring_modifiers);
            let timestamp = NSProcessInfo::processInfo().systemUptime();
            let mut result = Ok(());
            for event_type in [NSEventType::KeyDown, NSEventType::KeyUp] {
                let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                    event_type,
                    NSPoint::new(0., 0.),
                    modifiers,
                    timestamp,
                    window.windowNumber(),
                    None,
                    &characters,
                    &characters_ignoring_modifiers,
                    false,
                    key.key_code,
                );
                match event {
                    Some(event) => window.sendEvent(&event),
                    None => {
                        result = Err(anyhow::anyhow!("creating a key event"));
                        break;
                    }
                }
            }
            if let Some(previous_focus) = previous_focus {
                self.restore_keyboard_focus(previous_focus);
            }
            result
        }

        pub(crate) fn open_devtools(&self) {
            self.webview.open_devtools();
        }

        /// Shows the page light or dark: pages that style themselves with
        /// `prefers-color-scheme` follow the web view's appearance.
        pub(crate) fn set_appearance(&self, appearance: Appearance) {
            // SAFETY: the appearance names are constant strings AppKit exports.
            let name = unsafe {
                match appearance {
                    Appearance::Light => NSAppearanceNameAqua,
                    Appearance::Dark => NSAppearanceNameDarkAqua,
                }
            };
            let appearance = NSAppearance::appearanceNamed(name);
            self.webview.webview().setAppearance(appearance.as_deref());
        }

        /// Gives the page the keyboard, so that keys typed into it reach it.
        pub(crate) fn focus(&self) {
            if self.webview.webview().window().is_some() {
                self.webview.focus().log_err();
            }
        }

        /// Gives the keyboard back to GPUI, which AppKit leaves with the page
        /// after a click in it until something takes it.
        pub(crate) fn focus_parent(&self) {
            self.webview.focus_parent().log_err();
        }

        /// Removes the web view's surface from the window's composition. The
        /// window may be in the middle of an update when the view is dropped,
        /// so this waits for the current one to finish.
        pub(crate) fn release(self, cx: &mut App) {
            let window = self.window;
            cx.defer(move |cx| {
                let surface_id = self.surface.id();
                drop(self);
                window
                    .update(cx, |_, window, _| {
                        window
                            .enable_window_composition()?
                            .remove_surface(surface_id)
                    })
                    .log_err();
            });
        }
    }

    /// # Safety
    ///
    /// `value` and `error` are each null or a live object.
    unsafe fn script_result(
        value: *mut AnyObject,
        error: *mut NSError,
    ) -> Result<serde_json::Value> {
        // SAFETY: the caller guarantees the error is null or live.
        if let Some(error) = unsafe { error.as_ref() } {
            let message = error
                .userInfo()
                .objectForKey(&NSString::from_str("WKJavaScriptExceptionMessage"))
                .and_then(|message| {
                    message
                        .downcast_ref::<NSString>()
                        .map(|message| message.to_string())
                })
                .unwrap_or_else(|| error.localizedDescription().to_string());
            anyhow::bail!("{message}");
        }
        // SAFETY: the caller guarantees the value is null or live.
        let Some(value) = (unsafe { value.as_ref() }) else {
            return Ok(serde_json::Value::Null);
        };
        let json = value
            .downcast_ref::<NSString>()
            .context("the script's result was not JSON text")?
            .to_string();
        serde_json::from_str(&json).context("reading the script's result")
    }

    /// # Safety
    ///
    /// `image` and `error` are each null or a live object.
    unsafe fn snapshot_result(image: *mut NSImage, error: *mut NSError) -> Result<Vec<u8>> {
        // SAFETY: the caller guarantees the error is null or live.
        if let Some(error) = unsafe { error.as_ref() } {
            anyhow::bail!("{}", error.localizedDescription());
        }
        // SAFETY: the caller guarantees the image is null or live.
        let image = unsafe { image.as_ref() }.context("WebKit returned no snapshot")?;
        let data = image
            .TIFFRepresentation()
            .context("the snapshot could not be encoded")?;
        Ok(data.to_vec())
    }

    async fn serve(path: Option<PathBuf>, fs: &dyn Fs) -> http::Response<Vec<u8>> {
        let Some(path) = path else {
            return response(StatusCode::FORBIDDEN, "text/plain", Vec::new());
        };
        match fs.load_bytes(&path).await {
            Ok(body) => response(StatusCode::OK, content_type(&path), body),
            Err(error) => {
                log::debug!("HTML preview could not read {}: {error:#}", path.display());
                response(StatusCode::NOT_FOUND, "text/plain", Vec::new())
            }
        }
    }

    fn response(status: StatusCode, content_type: &str, body: Vec<u8>) -> http::Response<Vec<u8>> {
        let mut response = http::Response::new(body);
        *response.status_mut() = status;
        let headers = response.headers_mut();
        if let Ok(value) = HeaderValue::from_str(content_type) {
            headers.insert(CONTENT_TYPE, value);
        }
        // Reloading after an agent edits a stylesheet or script must pick up
        // the change rather than a cached copy.
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use anyhow::Result;
    use futures::channel::{mpsc::UnboundedSender, oneshot};
    use gpui::{App, Bounds, Pixels, Point, Size, Window};

    use super::{KeyStroke, PageEvent, PageSource};

    /// Web views are only supported on macOS so far.
    pub(crate) struct WebView;

    pub(crate) struct KeyboardFocus;

    impl WebView {
        pub(crate) fn new(
            _source: PageSource,
            _events: UnboundedSender<PageEvent>,
            _window: &mut Window,
            _cx: &App,
        ) -> Result<Self> {
            anyhow::bail!("web pages are only supported on macOS")
        }

        pub(crate) fn set_bounds(&self, _bounds: Bounds<Pixels>, _scale_factor: f32) -> Result<()> {
            Ok(())
        }

        pub(crate) fn paint(&self, _window: &mut Window) {}

        pub(crate) fn reload(&self) -> Result<()> {
            Ok(())
        }

        pub(crate) fn load_url(&self, _url: &str) -> Result<()> {
            Ok(())
        }

        pub(crate) fn title(&self) -> Option<String> {
            None
        }

        pub(crate) fn can_go_back(&self) -> bool {
            false
        }

        pub(crate) fn can_go_forward(&self) -> bool {
            false
        }

        pub(crate) fn go_back(&self) {}

        pub(crate) fn go_forward(&self) {}

        pub(crate) fn stop_loading(&self) {}

        pub(crate) fn page_size(&self) -> Size<Pixels> {
            Size::default()
        }

        pub(crate) fn select_all(&self) -> Result<()> {
            Ok(())
        }

        pub(crate) fn exec_command(&self, _command: &str) -> Result<()> {
            Ok(())
        }

        pub(crate) fn insert_text(&self, _text: &str) -> Result<()> {
            Ok(())
        }

        pub(crate) fn run_script(
            &self,
            _body: &str,
        ) -> Result<oneshot::Receiver<Result<serde_json::Value>>> {
            anyhow::bail!("web pages are only supported on macOS")
        }

        pub(crate) fn snapshot(&self) -> Result<oneshot::Receiver<Result<Vec<u8>>>> {
            anyhow::bail!("web pages are only supported on macOS")
        }

        pub(crate) fn has_keyboard_focus(&self) -> bool {
            false
        }

        pub(crate) fn keyboard_focus(&self) -> KeyboardFocus {
            KeyboardFocus
        }

        pub(crate) fn restore_keyboard_focus(&self, _focus: KeyboardFocus) {}

        pub(crate) fn click(&self, _position: Point<Pixels>, _click_count: isize) -> Result<()> {
            anyhow::bail!("web pages are only supported on macOS")
        }

        pub(crate) fn press_key(&self, _key: &KeyStroke) -> Result<()> {
            anyhow::bail!("web pages are only supported on macOS")
        }

        pub(crate) fn open_devtools(&self) {}

        pub(crate) fn set_appearance(&self, _appearance: theme::Appearance) {}

        pub(crate) fn focus(&self) {}

        pub(crate) fn focus_parent(&self) {}

        pub(crate) fn release(self, _cx: &mut App) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_resolve_inside_the_root() {
        let root = Path::new("/work/site");
        assert_eq!(
            resolve_request_path(root, "/docs/a%20b.html"),
            Some(PathBuf::from("/work/site/docs/a b.html"))
        );
        assert_eq!(
            resolve_request_path(root, "/"),
            Some(PathBuf::from("/work/site/index.html"))
        );
        assert_eq!(
            resolve_request_path(root, "/docs/"),
            Some(PathBuf::from("/work/site/docs/index.html"))
        );
        assert_eq!(resolve_request_path(root, "/../secret.txt"), None);
        assert_eq!(
            resolve_request_path(root, "/docs/%2E%2E/%2E%2E/secret"),
            None
        );
    }

    #[test]
    fn the_browser_loads_web_pages_and_hands_off_other_schemes() {
        assert!(browser_loads("http://localhost:3000/"));
        assert!(browser_loads("https://example.com"));
        assert!(browser_loads("about:blank"));
        assert!(!browser_loads("mailto:someone@example.com"));
        assert!(!browser_loads("vscode://file/a"));
    }

    #[test]
    fn keys_are_parsed_with_their_modifiers() {
        let enter = parse_key("Enter").unwrap();
        assert_eq!((enter.characters.as_str(), enter.key_code), ("\r", 36));
        assert!(!enter.shift && !enter.command);

        let shift_tab = parse_key("Shift+Tab").unwrap();
        assert!(shift_tab.shift);
        assert_eq!(shift_tab.key_code, 48);

        let select_all = parse_key("Meta+a").unwrap();
        assert!(select_all.command);
        assert_eq!(
            (select_all.characters.as_str(), select_all.key_code),
            ("a", 0)
        );

        let shifted = parse_key("Shift+k").unwrap();
        assert_eq!(shifted.characters, "K");
        assert_eq!(shifted.characters_ignoring_modifiers, "k");
        assert_eq!(shifted.key_code, 40);

        let plus = parse_key("Control++").unwrap();
        assert!(plus.control);
        assert_eq!(plus.characters, "+");

        assert_eq!(parse_key("`").unwrap().key_code, 50);
        assert_eq!(parse_key("p").unwrap().key_code, 35);
        assert!(parse_key("Hyper+a").is_err());
        assert!(parse_key("NotAKey").is_err());
    }

    #[test]
    fn page_urls_encode_each_segment() {
        let path = RelPath::from_unix_str("docs/a b#1.html").unwrap();
        assert_eq!(
            page_url(path),
            "bench-preview://localhost/docs/a%20b%231.html"
        );
    }
}
