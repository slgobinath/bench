//! What colour a pull request's state is drawn in.
//!
//! Shared, because two panels say the same thing about the same branch: the
//! git panel's Pull Requests tab lists them, and the worktree panel tints a
//! worktree's icon by the state of the pull request opened from it. A branch
//! that is green in one and purple in the other would be two facts rather than
//! one.

use github_cli::PullRequestState;
use gpui::hsla;
use ui::Color;

/// GitHub's own reading of a pull request: green while it is open, purple once
/// it is merged, red when it was closed without merging, grey while it is a
/// draft.
///
/// Purple is a `Custom` colour because no theme token is purple — the nearest
/// semantic ones are the accent (blue, and already spoken for: it is how the
/// worktree panel marks the worktree you are in) and the version control
/// colours (green, red, yellow). It is one mid-lightness purple rather than a
/// light and a dark one, so that it reads on either theme without the theme
/// having to be consulted.
pub fn pull_request_color(state: PullRequestState) -> Color {
    match state {
        PullRequestState::Open => Color::Success,
        PullRequestState::Merged => Color::Custom(hsla(265. / 360., 0.85, 0.65, 1.)),
        PullRequestState::Draft => Color::Muted,
        PullRequestState::Closed => Color::Error,
    }
}
