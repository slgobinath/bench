# Unmerged upstream patches carried by Bench

Changes taken from open zed-industries/zed pull requests before they were merged. When
syncing with upstream, check each PR: if it merged, drop Bench's copy in favour of
upstream's; if it changed, re-apply the newer revision.

## gpui: Add native window composition support

- PR: https://github.com/zed-industries/zed/pull/62379 (open when applied)
- Applied revision: `840e095adddab3b7a644b137c22254fb2d111491` (branch `window-composition-tree`)
- Applied on: 2026-10-13, as a single commit titled
  "gpui: Apply upstream window composition (zed#62379)"
- Why Bench needs it: the HTML preview tab hosts a native `WKWebView`. Without composition
  the web view covers every GPUI overlay in its area, so context menus, popovers and
  tooltips would be hidden behind the page.
- Bench-only changes on top of the PR:
  - `crates/gpui/Cargo.toml`: macOS `cocoa` and `objc` dev-dependencies, which the
    `native_webview` example needs and Bench's `gpui` did not already declare.
  - `crates/gpui/src/window.rs`: `WindowComposition::show_only_when_painted` and
    `Window::paint_native_surface`. A native surface registered this way is hidden in
    frames that don't paint it, so a web view disappears when its tab is switched away
    from. The marks are kept in `Frame::painted_native_surfaces` and replayed by
    `reuse_paint`, so cached views keep their surfaces shown. Not part of the PR; keep it
    (or upstream it) when re-applying.
  - `crates/gpui_macos/src/window.rs`: an overlay surface takes the mouse only over what
    it drew last frame (`OverlayInputRegions`), not over the whole window whenever it drew
    anything. Without this a toast or notification anywhere stops a web view taking
    clicks.
  - `crates/workspace`: the modal layer, workspace notifications and toasts are wrapped in
    `deferred(...)` so they land on the composition's overlay surface, above native views.
- How to re-apply: `gh pr diff 62379 -R zed-industries/zed > pr.diff` then
  `git apply --exclude=Cargo.lock pr.diff`, and let cargo update `Cargo.lock`.
