//! Review comments: comments on lines of code and of Markdown and HTML pages,
//! collected in a panel to be read over and edited, then sent to the agent
//! together as one prompt.

mod comment_dialog;
mod comments_panel;

use editor::Editor;
use gpui::{App, Focusable as _, Window, actions};
use project::DisableAiSettings;
use workspace::Workspace;
use zed_actions::claude::{AddComment, AddCommentOn, CommentTarget};

pub use crate::comment_dialog::CommentDialog;
pub use crate::comments_panel::CommentsPanel;

/// How much of the text a comment is on is kept to show with it. The lines
/// say where it is; this is only a reminder of what it says.
const MAX_EXCERPT_CHARS: usize = 2_000;

actions!(
    review_comments,
    [
        /// Opens the comments panel, or moves focus into it if it is already
        /// open.
        ToggleFocus,
        /// Sends every comment in the comments panel to the agent as one
        /// prompt, and clears the panel.
        SendAll,
        /// Removes every comment from the comments panel.
        ClearAll,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|workspace, _: &ToggleFocus, window, cx| {
                workspace.toggle_panel_focus::<CommentsPanel>(window, cx);
            })
            // Previews handle this themselves, with the lines their selection
            // came from; what reaches the workspace is from an editor.
            .register_action(|workspace, _: &AddComment, window, cx| {
                if let Some(target) = editor_comment_target(workspace, window, cx) {
                    CommentDialog::open(workspace, target, None, window, cx);
                }
            })
            .register_action(|workspace, action: &AddCommentOn, window, cx| {
                CommentDialog::open(workspace, action.target.clone(), None, window, cx);
            });
    })
    .detach();
}

/// The lines selected in the focused editor, or the line the cursor is on.
fn editor_comment_target(
    workspace: &Workspace,
    window: &Window,
    cx: &mut App,
) -> Option<CommentTarget> {
    let editor = workspace.active_item_as::<Editor>(cx)?;
    if !editor.focus_handle(cx).is_focused(window) {
        return None;
    }
    editor.update(cx, |editor, cx| {
        let selection = editor
            .selections
            .newest_adjusted(&editor.display_snapshot(cx));
        let multi_buffer = editor.buffer().read(cx);
        let (buffer, start) = multi_buffer.point_to_buffer_point(selection.start, cx)?;
        // A selection running into another file of a multi-file editor is
        // taken as far as the end of the first one's line.
        let end = multi_buffer
            .point_to_buffer_point(selection.end, cx)
            .filter(|(end_buffer, _)| end_buffer.entity_id() == buffer.entity_id())
            .map_or(start, |(_, end)| end);
        if DisableAiSettings::is_ai_disabled_for_buffer(Some(&buffer), cx) {
            return None;
        }
        let buffer = buffer.read(cx);
        let path = buffer.file()?.as_local()?.abs_path(cx);
        // A selection that runs up to the start of a line does not take it.
        let end_row = if end.row > start.row && end.column == 0 {
            end.row - 1
        } else {
            end.row
        };
        let excerpt = if start == end {
            buffer.text_for_range(
                text::Point::new(start.row, 0)
                    ..text::Point::new(start.row, buffer.line_len(start.row)),
            )
        } else {
            buffer.text_for_range(start..end)
        }
        .collect::<String>();
        Some(CommentTarget {
            path: path.to_string_lossy().into_owned(),
            start_line: Some(start.row + 1),
            end_line: Some(end_row + 1),
            excerpt: Some(excerpt),
        })
    })
}

/// `excerpt`, cut down to what is worth keeping with a comment.
fn trimmed_excerpt(excerpt: Option<&str>) -> Option<String> {
    let excerpt = excerpt?.trim_matches('\n');
    if excerpt.trim().is_empty() {
        return None;
    }
    Some(match excerpt.char_indices().nth(MAX_EXCERPT_CHARS) {
        Some((end, _)) => format!("{}…", &excerpt[..end]),
        None => excerpt.to_string(),
    })
}
