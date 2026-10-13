use std::path::PathBuf;

use anyhow::Result;
use editor::Editor;
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    ScrollHandle, SharedString, Task, WeakEntity, Window,
};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::{OpenOptions, Workspace};
use zed_actions::claude::{AddComment, CommentTarget, ReviewComment, SendComments};

use crate::{ClearAll, CommentDialog, SendAll, ToggleFocus, trimmed_excerpt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommentId(usize);

struct Comment {
    id: CommentId,
    target: CommentTarget,
    location: SharedString,
    text: SharedString,
}

/// The comments written so far in a worktree, waiting to be sent to the agent.
pub struct CommentsPanel {
    workspace: WeakEntity<Workspace>,
    comments: Vec<Comment>,
    next_id: usize,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
}

impl CommentsPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            let handle = workspace.clone();
            workspace.update(cx, |_, cx| cx.new(|cx| Self::new(handle, cx)))
        })
    }

    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        Self {
            workspace,
            comments: Vec::new(),
            next_id: 0,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.comments.is_empty()
    }

    pub(crate) fn add(
        &mut self,
        mut target: CommentTarget,
        location: SharedString,
        text: SharedString,
        cx: &mut Context<Self>,
    ) {
        target.excerpt = trimmed_excerpt(target.excerpt.as_deref());
        let id = CommentId(self.next_id);
        self.next_id += 1;
        self.comments.push(Comment {
            id,
            target,
            location,
            text,
        });
        self.scroll_handle.scroll_to_bottom();
        cx.notify();
    }

    pub(crate) fn set_text(&mut self, id: CommentId, text: SharedString, cx: &mut Context<Self>) {
        if let Some(comment) = self.comments.iter_mut().find(|comment| comment.id == id) {
            comment.text = text;
            cx.notify();
        }
    }

    pub(crate) fn remove(&mut self, id: CommentId, cx: &mut Context<Self>) {
        self.comments.retain(|comment| comment.id != id);
        cx.notify();
    }

    fn clear_all(&mut self, _: &ClearAll, _: &mut Window, cx: &mut Context<Self>) {
        self.comments.clear();
        cx.notify();
    }

    fn send_all(&mut self, _: &SendAll, window: &mut Window, cx: &mut Context<Self>) {
        if self.comments.is_empty() {
            return;
        }
        let comments = self
            .comments
            .drain(..)
            .map(|comment| ReviewComment {
                target: comment.target,
                comment: comment.text.to_string(),
            })
            .collect();
        window.dispatch_action(Box::new(SendComments { comments }), cx);
        cx.notify();
    }

    fn edit(&mut self, id: CommentId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(comment) = self.comments.iter().find(|comment| comment.id == id) else {
            return;
        };
        let target = comment.target.clone();
        let text = comment.text.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                CommentDialog::open(workspace, target, Some((id, text)), window, cx);
            })
            .log_err();
    }

    /// Opens the file a comment is on, at its first line. A comment on a
    /// preview opens the file the preview was rendered from.
    fn open_location(&mut self, id: CommentId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(comment) = self.comments.iter().find(|comment| comment.id == id) else {
            return;
        };
        let path = PathBuf::from(&comment.target.path);
        let line = comment.target.start_line;
        self.workspace
            .update(cx, |workspace, cx| {
                let open = workspace.open_abs_path(
                    path,
                    OpenOptions {
                        focus: Some(true),
                        ..Default::default()
                    },
                    window,
                    cx,
                );
                cx.spawn_in(window, async move |_, cx| {
                    let item = open.await?;
                    if let Some(line) = line {
                        cx.update(|window, cx| {
                            if let Some(editor) = item.act_as::<Editor>(cx) {
                                editor.update(cx, |editor, cx| {
                                    editor.go_to_singleton_buffer_point(
                                        text::Point::new(line.saturating_sub(1), 0),
                                        window,
                                        cx,
                                    );
                                });
                            }
                        })?;
                    }
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            })
            .log_err();
    }

    fn render_comment(&self, comment: &Comment, cx: &mut Context<Self>) -> impl IntoElement {
        let id = comment.id;
        let border = cx.theme().colors().border_variant;
        let background = cx.theme().colors().editor_background;
        let group = SharedString::from(format!("comment-{}", id.0));
        let excerpt = comment
            .target
            .excerpt
            .as_deref()
            .and_then(|excerpt| excerpt.lines().find(|line| !line.trim().is_empty()))
            .map(|line| line.trim().to_string());

        v_flex()
            .id(("comment", id.0))
            .group(group.clone())
            .p_2()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(background)
            .child(
                h_flex()
                    .justify_between()
                    .gap_1()
                    .child(
                        div()
                            .id(("comment-location", id.0))
                            .min_w_0()
                            .cursor_pointer()
                            .tooltip(Tooltip::text("Go to Lines"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_location(id, window, cx)
                            }))
                            .child(
                                Label::new(comment.location.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Accent)
                                    .truncate(),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .visible_on_hover(group)
                            .child(
                                IconButton::new(("edit-comment", id.0), IconName::Pencil)
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text("Edit Comment"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.edit(id, window, cx)
                                    })),
                            )
                            .child(
                                IconButton::new(("delete-comment", id.0), IconName::Trash)
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text("Delete Comment"))
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.remove(id, cx)),
                                    ),
                            ),
                    ),
            )
            .when_some(excerpt, |this, excerpt| {
                this.child(
                    Label::new(excerpt)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .buffer_font(cx)
                        .single_line()
                        .truncate(),
                )
            })
            .child(
                div()
                    .id(("comment-text", id.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| this.edit(id, window, cx)))
                    .child(Label::new(comment.text.clone()).size(LabelSize::Small)),
            )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .p_6()
            .gap_2()
            .items_center()
            .justify_center()
            .child(Icon::new(IconName::Chat).color(Color::Muted))
            .child(Label::new("No comments yet").color(Color::Muted))
            .child(
                Label::new(
                    "Select lines in an editor or a Markdown or HTML preview and add a comment. \
                     Comments collect here until you send them to the agent together.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                Button::new("add-comment-hint", "Add Comment")
                    .label_size(LabelSize::Small)
                    .key_binding(ui::KeyBinding::for_action(&AddComment, cx))
                    .disabled(true),
            )
    }
}

impl Render for CommentsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel_background = cx.theme().colors().panel_background;
        let border = cx.theme().colors().border;
        let count = self.comments.len();
        let is_empty = count == 0;
        let comments = self
            .comments
            .iter()
            .map(|comment| self.render_comment(comment, cx).into_any_element())
            .collect::<Vec<_>>();

        v_flex()
            .key_context("CommentsPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::send_all))
            .on_action(cx.listener(Self::clear_all))
            .size_full()
            .bg(panel_background)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(border)
                    .child(Label::new("Comments"))
                    .when(!is_empty, |this| {
                        this.child(Label::new(count.to_string()).color(Color::Muted))
                    }),
            )
            .map(|this| {
                if is_empty {
                    this.child(self.render_empty(cx))
                } else {
                    this.child(
                        v_flex()
                            .id("comments")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .p_2()
                            .gap_2()
                            .children(comments),
                    )
                }
            })
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .justify_end()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        Button::new("clear-comments", "Clear")
                            .label_size(LabelSize::Small)
                            .disabled(is_empty)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(ClearAll.boxed_clone(), cx)
                            }),
                    )
                    .child(
                        Button::new("send-comments", "Send All to Agent")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Send).size(IconSize::Small))
                            .disabled(is_empty)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.send_all(&SendAll, window, cx)
                            })),
                    ),
            )
    }
}

impl Focusable for CommentsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for CommentsPanel {}

impl Panel for CommentsPanel {
    fn persistent_name() -> &'static str {
        "CommentsPanel"
    }

    fn panel_key() -> &'static str {
        "CommentsPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Chat)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Comments")
    }

    fn icon_label(&self, _: &Window, _: &App) -> Option<String> {
        (!self.comments.is_empty()).then(|| self.comments.len().to_string())
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use util::path;
    use workspace::{AppState, MultiWorkspace};

    use super::*;

    fn target(path: &str, lines: Option<(u32, u32)>, excerpt: &str) -> CommentTarget {
        CommentTarget {
            path: path.to_string(),
            start_line: lines.map(|(start, _)| start),
            end_line: lines.map(|(_, end)| end),
            excerpt: Some(excerpt.to_string()),
        }
    }

    fn write_comment(
        workspace: &Entity<Workspace>,
        target: CommentTarget,
        text: &str,
        cx: &mut VisualTestContext,
    ) {
        workspace.update_in(cx, |workspace, window, cx| {
            CommentDialog::open(workspace, target, None, window, cx)
        });
        cx.simulate_input(text);
        let dialog = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<CommentDialog>(cx)
            })
            .expect("the comment dialog is open");
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.confirm(&menu::Confirm, window, cx)
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn comments_collect_in_the_panel_until_sent_together(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "main.rs": "fn main() {}\n" }))
            .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let sent = Rc::new(RefCell::new(Vec::new()));
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let sent = sent.clone();
            workspace.register_action(move |_, action: &SendComments, _, _| {
                sent.borrow_mut().push(action.comments.clone());
            });
            let workspace_handle = cx.weak_entity();
            let panel = cx.new(|cx| CommentsPanel::new(workspace_handle, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        let main_rs = path!("/project/main.rs");
        write_comment(
            &workspace,
            target(main_rs, Some((1, 1)), "fn main() {}"),
            "Add a doc comment",
            cx,
        );
        write_comment(&workspace, target(main_rs, None, "Pricing"), "   ", cx);
        write_comment(&workspace, target(main_rs, None, "Pricing"), "Bigger", cx);

        let labels = panel.read_with(cx, |panel, _| {
            panel
                .comments
                .iter()
                .map(|comment| (comment.location.to_string(), comment.text.to_string()))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            labels,
            [
                ("main.rs:1".to_string(), "Add a doc comment".to_string()),
                ("main.rs".to_string(), "Bigger".to_string()),
            ],
            "an empty comment should be dropped, and the rest listed in order"
        );

        panel.update_in(cx, |panel, window, cx| panel.send_all(&SendAll, window, cx));
        cx.run_until_parked();

        let sent = sent.borrow();
        assert_eq!(
            sent.len(),
            1,
            "every comment should go to the agent in one message"
        );
        assert_eq!(
            sent[0]
                .iter()
                .map(|comment| comment.comment.as_str())
                .collect::<Vec<_>>(),
            ["Add a doc comment", "Bigger"]
        );
        assert!(panel.read_with(cx, |panel, _| panel.is_empty()));
    }
}
