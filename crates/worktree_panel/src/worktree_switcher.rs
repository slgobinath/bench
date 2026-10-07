//! A palette over the worktrees the window has open, most recently shown
//! first, for switching without the worktree panel on screen.

use std::sync::Arc;

use agent_tracker::{AgentSummary, ClaudeModel, agent_state_color};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task,
    WeakEntity, Window, rems,
};
use picker::{Picker, PickerDelegate};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, MultiWorkspace, Workspace};

use crate::WorktreePanel;

struct SwitcherEntry {
    workspace: Entity<Workspace>,
    name: SharedString,
    /// The title, branch and repository, whichever the worktree has.
    detail: SharedString,
    agents: AgentSummary,
    models: Vec<ClaudeModel>,
    is_active: bool,
}

/// The window's open worktrees, most recently shown first. Worktrees the
/// window has no workspace for are left out: they have no "recently" to
/// order them by, and opening one is the panel's job.
fn switcher_entries(panel: &WorktreePanel, cx: &App) -> Vec<SwitcherEntry> {
    let Some(multi_workspace) = panel.multi_workspace.upgrade() else {
        return Vec::new();
    };
    let multi_workspace = multi_workspace.read(cx);

    let mut entries: Vec<(Option<u64>, SwitcherEntry)> = Vec::new();
    for repository in panel.tree(cx) {
        for row in repository.worktrees {
            let Some(workspace) = row.workspace else {
                continue;
            };
            let detail = [
                row.title.map(|title| title.to_string()),
                row.branch.map(|branch| branch.to_string()),
                Some(repository.name.to_string()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            entries.push((
                multi_workspace.activation_stamp(&workspace),
                SwitcherEntry {
                    workspace,
                    name: row.name,
                    detail: detail.into(),
                    agents: row.agents,
                    models: row.models,
                    is_active: row.is_active,
                },
            ));
        }
    }
    // `None` sorts below every `Some`, which puts never-shown workspaces last.
    entries.sort_by_key(|(stamp, _)| std::cmp::Reverse(*stamp));
    entries.into_iter().map(|(_, entry)| entry).collect()
}

/// Opens the switcher over `workspace`.
///
/// Deferred, because building the entries reads every workspace of the window
/// and an action handler holds this one's lease; see
/// [`WorktreePanel::activate`].
pub(crate) fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(panel) = workspace.panel::<WorktreePanel>(cx) else {
        return;
    };
    let workspace = cx.entity();
    window.defer(cx, move |window, cx| {
        let multi_workspace = panel.read(cx).multi_workspace.clone();
        let entries = switcher_entries(panel.read(cx), cx);
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                WorktreeSwitcher::new(multi_workspace, entries, window, cx)
            });
        });
    });
}

struct WorktreeSwitcher {
    picker: Entity<Picker<SwitcherDelegate>>,
}

impl WorktreeSwitcher {
    fn new(
        multi_workspace: WeakEntity<MultiWorkspace>,
        entries: Vec<SwitcherEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = SwitcherDelegate::new(cx.entity().downgrade(), multi_workspace, entries);
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl ModalView for WorktreeSwitcher {}

impl EventEmitter<DismissEvent> for WorktreeSwitcher {}

impl Focusable for WorktreeSwitcher {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for WorktreeSwitcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("WorktreeSwitcher")
            .elevation_3(cx)
            .w(rems(34.))
            .child(self.picker.clone())
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
    }
}

struct SwitcherDelegate {
    switcher: WeakEntity<WorktreeSwitcher>,
    multi_workspace: WeakEntity<MultiWorkspace>,
    entries: Vec<SwitcherEntry>,
    candidates: Vec<StringMatchCandidate>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl SwitcherDelegate {
    fn new(
        switcher: WeakEntity<WorktreeSwitcher>,
        multi_workspace: WeakEntity<MultiWorkspace>,
        entries: Vec<SwitcherEntry>,
    ) -> Self {
        let candidates: Vec<StringMatchCandidate> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                StringMatchCandidate::new(index, &format!("{} {}", entry.name, entry.detail))
            })
            .collect();
        let matches = all_matches(&candidates);
        Self {
            switcher,
            multi_workspace,
            selected_index: previous_worktree_index(matches.len()),
            entries,
            candidates,
            matches,
        }
    }
}

fn all_matches(candidates: &[StringMatchCandidate]) -> Vec<StringMatch> {
    candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| StringMatch {
            candidate_id: index,
            string: candidate.string.clone(),
            positions: Vec::new(),
            score: 0.0,
        })
        .collect()
}

/// The first entry is the worktree already showing, so with nothing typed the
/// one to land on is the one before it, which makes the shortcut and `enter`
/// a swap between the last two.
fn previous_worktree_index(match_count: usize) -> usize {
    usize::from(match_count > 1)
}

impl PickerDelegate for SwitcherDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "worktree switcher"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Switch to worktree…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self.candidates.clone();
        cx.spawn_in(window, async move |picker, cx| {
            let query = query.trim().to_string();
            let matches = if query.is_empty() {
                all_matches(&candidates)
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
                    delegate.selected_index = if query.is_empty() {
                        previous_worktree_index(matches.len())
                    } else {
                        0
                    };
                    delegate.matches = matches;
                    cx.notify();
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(workspace) = self
            .matches
            .get(self.selected_index)
            .and_then(|found| self.entries.get(found.candidate_id))
            .map(|entry| entry.workspace.clone())
        else {
            return;
        };
        self.dismissed(window, cx);
        // After the modal is gone and this picker's lease is dropped; see
        // `WorktreePanel::activate` for why activating cannot happen inside.
        let multi_workspace = self.multi_workspace.clone();
        window.defer(cx, move |window, cx| {
            multi_workspace
                .update(cx, |multi_workspace, cx| {
                    multi_workspace.activate(workspace, None, window, cx);
                })
                .log_err();
        });
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.switcher
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let found = self.matches.get(ix)?;
        let entry = self.entries.get(found.candidate_id)?;
        // The candidate leads with the name, so positions inside it are the
        // name's; the rest are in the detail, which is drawn plain.
        let name_positions: Vec<usize> = found
            .positions
            .iter()
            .copied()
            .filter(|position| *position < entry.name.len())
            .collect();
        let working = entry.agents.working + entry.agents.needs_input > 0;

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(
                            HighlightedLabel::new(entry.name.clone(), name_positions)
                                .flex_none(),
                        )
                        .child(
                            div().min_w_0().flex_1().child(
                                Label::new(entry.detail.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                        )
                        .when(entry.is_active, |row| {
                            row.child(
                                Label::new("current")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .flex_none(),
                            )
                        }),
                )
                .end_slot(
                    h_flex()
                        .gap_1p5()
                        .children(entry.agents.state().map(|state| {
                            Icon::new(IconName::AiClaude)
                                .size(IconSize::Small)
                                .color(agent_state_color(state))
                        }))
                        .children(entry.models.iter().enumerate().map(|(position, model)| {
                            model.badge(("switcher-model", ix * 8 + position), working)
                        })),
                ),
        )
    }
}
