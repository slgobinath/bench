use std::path::Path;

use editor::Editor;
use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, WeakEntity,
    Window,
};
use project::Project;
use ui::prelude::*;
use util::paths::PathStyle;
use workspace::{ModalView, Workspace};
use zed_actions::claude::CommentTarget;

use crate::CommentsPanel;
use crate::comments_panel::CommentId;

/// How many lines of the commented text the dialog shows above the comment.
const EXCERPT_PREVIEW_LINES: usize = 4;

/// A floating box for writing, or rewriting, a comment on some lines.
pub struct CommentDialog {
    workspace: WeakEntity<Workspace>,
    panel: WeakEntity<CommentsPanel>,
    target: CommentTarget,
    location: SharedString,
    /// The comment being rewritten, when this isn't a new one.
    editing: Option<CommentId>,
    editor: Entity<Editor>,
}

impl CommentDialog {
    /// Opens the dialog for a new comment on `target`, or for rewriting the
    /// comment `editing` names, starting from its text.
    pub fn open(
        workspace: &mut Workspace,
        target: CommentTarget,
        editing: Option<(CommentId, SharedString)>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(panel) = workspace.panel::<CommentsPanel>(cx) else {
            return;
        };
        let location = location_label(&target, workspace.project(), cx);
        let workspace_handle = cx.weak_entity();
        workspace.toggle_modal(window, cx, |window, cx| {
            let editor = cx.new(|cx| {
                let mut editor = Editor::auto_height(3, 12, window, cx);
                editor.set_placeholder_text("Leave a comment for the agent…", window, cx);
                if let Some((_, text)) = &editing {
                    editor.set_text(text.to_string(), window, cx);
                    editor.move_to_end(&Default::default(), window, cx);
                }
                editor
            });
            Self {
                workspace: workspace_handle,
                panel: panel.downgrade(),
                target,
                location,
                editing: editing.map(|(id, _)| id),
                editor,
            }
        });
    }

    pub(crate) fn confirm(
        &mut self,
        _: &menu::Confirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.editor.read(cx).text(cx);
        let text = text.trim();
        let added_first = self
            .panel
            .update(cx, |panel, cx| match self.editing {
                Some(id) if text.is_empty() => {
                    panel.remove(id, cx);
                    false
                }
                Some(id) => {
                    panel.set_text(id, text.to_string().into(), cx);
                    false
                }
                None if text.is_empty() => false,
                None => {
                    let was_empty = panel.is_empty();
                    panel.add(
                        self.target.clone(),
                        self.location.clone(),
                        text.to_string().into(),
                        cx,
                    );
                    was_empty
                }
            })
            .unwrap_or(false);
        cx.emit(DismissEvent);
        // The panel is where comments are kept track of, so the first one
        // brings it up; after that it is left where the person put it.
        if added_first {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.open_panel::<CommentsPanel>(window, cx)
                })
                .ok();
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

/// Where a comment is, as the person sees it: the path in the worktree, with
/// the lines.
pub(crate) fn location_label(
    target: &CommentTarget,
    project: &Entity<Project>,
    cx: &App,
) -> SharedString {
    let path = Path::new(&target.path);
    let path = project
        .read(cx)
        .find_project_path(path, cx)
        .map(|project_path| project_path.path.display(PathStyle::local()).into_owned())
        .or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| target.path.clone());
    match (target.start_line, target.end_line) {
        (Some(start), Some(end)) if end > start => format!("{path}:{start}–{end}").into(),
        (Some(start), _) => format!("{path}:{start}").into(),
        (None, _) => path.into(),
    }
}

impl ModalView for CommentDialog {}

impl EventEmitter<DismissEvent> for CommentDialog {}

impl Focusable for CommentDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for CommentDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let excerpt = self.target.excerpt.as_deref().map(|excerpt| {
            let mut lines = excerpt.trim_matches('\n').lines();
            let mut preview = lines
                .by_ref()
                .take(EXCERPT_PREVIEW_LINES)
                .collect::<Vec<_>>()
                .join("\n");
            if lines.next().is_some() {
                preview.push_str("\n…");
            }
            preview
        });
        let save_label = if self.editing.is_some() {
            "Update Comment"
        } else {
            "Add Comment"
        };

        v_flex()
            .key_context("CommentDialog")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(rems(34.))
            .elevation_3(cx)
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Chat)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(self.location.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .when_some(
                excerpt.filter(|excerpt| !excerpt.trim().is_empty()),
                |this, excerpt| {
                    this.child(
                        div().pl_2().border_l_2().border_color(colors.border).child(
                            Label::new(excerpt)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .buffer_font(cx),
                        ),
                    )
                },
            )
            .child(
                div()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_focused)
                    .bg(colors.editor_background)
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(
                        Label::new("Enter to save, Shift-Enter for a new line")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("cancel-comment", "Cancel")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel(&menu::Cancel, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("save-comment", save_label)
                                    .style(ButtonStyle::Filled)
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm(&menu::Confirm, window, cx)
                                    })),
                            ),
                    ),
            )
    }
}
