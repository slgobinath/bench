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
- How to re-apply: `gh pr diff 62379 -R zed-industries/zed > pr.diff` then
  `git apply --exclude=Cargo.lock pr.diff`, and let cargo update `Cargo.lock`.
