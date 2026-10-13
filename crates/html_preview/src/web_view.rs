use std::path::{Component, Path, PathBuf};

use gpui::{Pixels, Point};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::Deserialize;
use util::rel_path::RelPath;

pub(crate) use platform::WebView;

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

pub(crate) enum PageEvent {
    SelectionChanged(String),
    /// A right click, at a position relative to the page's top left corner.
    ContextMenu {
        position: Point<Pixels>,
        selection: String,
    },
    MouseDown,
    OpenExternally(String),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PageMessage {
    Selection { text: String },
    ContextMenu { x: f32, y: f32, text: String },
    MouseDown,
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
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::{Context as _, Result};
    use fs::Fs;
    use futures::channel::mpsc::UnboundedSender;
    use gpui::{
        AnyWindowHandle, App, Bounds, DevicePixels, Pixels, Window, WindowCompositionSurface,
    };
    use http::header::{CACHE_CONTROL, CONTENT_TYPE};
    use http::{HeaderValue, StatusCode};
    use objc2_app_kit::{
        NSAppearance, NSAppearanceCustomization as _, NSAppearanceNameAqua,
        NSAppearanceNameDarkAqua, NSView,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use theme::Appearance;
    use util::ResultExt as _;
    use wry::WebViewExtMacOS as _;

    use super::{
        PAGE_SCRIPT, PageEvent, PageMessage, SCHEME, content_type, opens_externally,
        resolve_request_path,
    };

    /// A WKWebView in a native surface of the window's composition, so that
    /// GPUI's overlays (context menus, popovers, tooltips) draw above it.
    pub(crate) struct WebView {
        webview: wry::WebView,
        surface: WindowCompositionSurface,
        window: AnyWindowHandle,
        bounds: Cell<Option<Bounds<DevicePixels>>>,
    }

    impl WebView {
        pub(crate) fn new(
            url: String,
            root: PathBuf,
            fs: Arc<dyn Fs>,
            events: UnboundedSender<PageEvent>,
            window: &mut Window,
            cx: &App,
        ) -> Result<Self> {
            let executor = cx.background_executor().clone();
            let webview = wry::WebViewBuilder::new()
                .with_url(url)
                .with_initialization_script_for_main_only(PAGE_SCRIPT, true)
                .with_accept_first_mouse(true)
                .with_devtools(true)
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
                .with_ipc_handler({
                    let events = events.clone();
                    move |request| match serde_json::from_str::<PageMessage>(request.body()) {
                        Ok(message) => {
                            events.unbounded_send(message.into()).log_err();
                        }
                        Err(error) => log::warn!("unreadable message from HTML preview: {error}"),
                    }
                })
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

        pub(crate) fn select_all(&self) -> Result<()> {
            Ok(self
                .webview
                .evaluate_script("document.execCommand('selectAll')")?)
        }

        /// Types `text` into the field focused in the page.
        pub(crate) fn insert_text(&self, text: &str) -> Result<()> {
            let text = serde_json::to_string(text)?;
            Ok(self.webview.evaluate_script(&format!(
                "document.execCommand('insertText', false, {text})"
            ))?)
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
        /// window may be in the middle of an update when the preview is
        /// dropped, so this waits for the current one to finish.
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
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::Result;
    use fs::Fs;
    use futures::channel::mpsc::UnboundedSender;
    use gpui::{App, Bounds, Pixels, Window};

    use super::PageEvent;

    /// HTML previews are only supported on macOS so far.
    pub(crate) struct WebView;

    impl WebView {
        pub(crate) fn new(
            _url: String,
            _root: PathBuf,
            _fs: Arc<dyn Fs>,
            _events: UnboundedSender<PageEvent>,
            _window: &mut Window,
            _cx: &App,
        ) -> Result<Self> {
            anyhow::bail!("HTML previews are only supported on macOS")
        }

        pub(crate) fn set_bounds(&self, _bounds: Bounds<Pixels>, _scale_factor: f32) -> Result<()> {
            Ok(())
        }

        pub(crate) fn paint(&self, _window: &mut Window) {}

        pub(crate) fn reload(&self) -> Result<()> {
            Ok(())
        }

        pub(crate) fn select_all(&self) -> Result<()> {
            Ok(())
        }

        pub(crate) fn insert_text(&self, _text: &str) -> Result<()> {
            Ok(())
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
    fn page_urls_encode_each_segment() {
        let path = RelPath::from_unix_str("docs/a b#1.html").unwrap();
        assert_eq!(
            page_url(path),
            "bench-preview://localhost/docs/a%20b%231.html"
        );
    }
}
