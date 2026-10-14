use std::sync::Arc;

use collections::HashSet;
use editor::Editor;
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity,
    Window, actions,
};
use language::Buffer;
use lsp::LanguageServerSelector;
use picker::{Picker, PickerDelegate};
use project::{LanguageServerChoice, Project};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

actions!(
    lsp_tool,
    [
        /// Picks one of the current file's language servers and starts it, or
        /// restarts it if it is already running. Starts a server that was
        /// stopped by hand, too.
        StartLanguageServer
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &StartLanguageServer, window, cx| {
            LanguageServerPicker::toggle(workspace, window, cx);
        });
    })
    .detach();
}

pub struct LanguageServerPicker {
    picker: Entity<Picker<LanguageServerPickerDelegate>>,
}

impl LanguageServerPicker {
    fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let Some(buffer) = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
            .and_then(|editor| editor.read(cx).active_buffer(cx))
        else {
            return;
        };
        let project = workspace.project().clone();
        let choices = project.read(cx).lsp_store().update(cx, |lsp_store, cx| {
            lsp_store.language_server_choices_for_local_buffer(&buffer, cx)
        });
        let file_name = buffer
            .read(cx)
            .file()
            .map(|file| file.file_name(cx).to_string());
        workspace.toggle_modal(window, cx, move |window, cx| {
            let delegate = LanguageServerPickerDelegate {
                picker: cx.entity().downgrade(),
                buffer,
                project,
                file_name,
                choices,
                matches: Vec::new(),
                selected_index: 0,
            };
            Self {
                picker: cx.new(|cx| Picker::uniform_list(delegate, window, cx)),
            }
        });
    }
}

impl Render for LanguageServerPicker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("LanguageServerPicker")
            .w(rems(28.))
            .child(self.picker.clone())
    }
}

impl Focusable for LanguageServerPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for LanguageServerPicker {}

impl ModalView for LanguageServerPicker {}

pub struct LanguageServerPickerDelegate {
    picker: WeakEntity<LanguageServerPicker>,
    buffer: Entity<Buffer>,
    project: Entity<Project>,
    file_name: Option<String>,
    choices: Vec<LanguageServerChoice>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl LanguageServerPickerDelegate {
    /// Starts the server for every open file of the active file's language,
    /// so that they all get its help, not only the one it was picked from.
    fn start(&self, choice: &LanguageServerChoice, cx: &mut App) {
        let language = self
            .buffer
            .read(cx)
            .language()
            .map(|language| language.name());
        let buffers = self
            .project
            .read(cx)
            .opened_buffers(cx)
            .into_iter()
            .filter(|buffer| buffer.read(cx).language().map(|language| language.name()) == language)
            .collect::<Vec<_>>();
        self.project.update(cx, |project, cx| {
            project.restart_language_servers_for_buffers(
                buffers,
                HashSet::from_iter([LanguageServerSelector::Name(choice.name.clone())]),
                true,
                cx,
            );
        });
    }
}

fn status(choice: &LanguageServerChoice) -> (&'static str, Color) {
    match (choice.running, choice.stopped) {
        (Some(_), _) => ("Running", Color::Success),
        (None, true) => ("Stopped", Color::Warning),
        (None, false) => ("Not running", Color::Muted),
    }
}

impl PickerDelegate for LanguageServerPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "language server picker"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        match &self.file_name {
            Some(file_name) => format!("Start a language server for {file_name}…").into(),
            None => "Start a language server…".into(),
        }
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some(if self.choices.is_empty() {
            "No language servers are set up for this file".into()
        } else {
            "No matching language servers".into()
        })
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates = self
            .choices
            .iter()
            .enumerate()
            .map(|(index, choice)| StringMatchCandidate::new(index, choice.name.as_ref()))
            .collect::<Vec<_>>();
        let background = cx.background_executor().clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background,
                )
                .await
            };
            picker
                .update(cx, |picker, cx| {
                    let delegate = &mut picker.delegate;
                    // Starting what isn't running is what this is for, so
                    // the first such server is where the selection starts.
                    delegate.selected_index = matches
                        .iter()
                        .position(|mat| {
                            delegate
                                .choices
                                .get(mat.candidate_id)
                                .is_some_and(|choice| choice.running.is_none())
                        })
                        .unwrap_or(0);
                    delegate.matches = matches;
                    cx.notify();
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let choice = self
            .matches
            .get(self.selected_index)
            .and_then(|mat| self.choices.get(mat.candidate_id))
            .cloned();
        if let Some(choice) = choice {
            self.start(&choice, cx);
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;
        let choice = self.choices.get(mat.candidate_id)?;
        let (status, color) = status(choice);
        let action = if choice.running.is_some() {
            "Restart"
        } else {
            "Start"
        };
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .gap_2()
                        .child(HighlightedLabel::new(
                            mat.string.clone(),
                            mat.positions.clone(),
                        ))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Label::new(status).size(LabelSize::Small).color(color))
                                .when(selected, |this| {
                                    this.child(
                                        Label::new(action)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                }),
                        ),
                ),
        )
    }
}
