//! Bench's git panel.
//!
//! One panel for one repository or several: a Bench worktree is often a
//! workspace repository with others kept inside it, under `repos/`, and one
//! change touches several of them. Which repositories it shows is picked in
//! the repository picker; each tab — changes, history, pull requests — groups
//! by repository when there is more than one, one message commits to each,
//! and fetch, pull and push run on every one.
//!
//! It replaces Zed's git panel in the dock. Zed's panel stays registered,
//! hidden: its commit modal, the staging controls in the project diff and the
//! `git::` actions all go through it, and keeping it is what keeps them
//! working. Everything the user sees is this panel.
//!
//! # The selection
//!
//! Which repositories are selected is remembered per worktree, across
//! restarts. The active repository is always one of them; the others are kept
//! here, by work directory, since a repository entity does not outlive its
//! project.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_settings::{AgentSettings, UserAgentsMd};
use anyhow::{Result, anyhow};
use askpass::AskPassDelegate;
use editor::Editor;
use file_icons::FileIcons;
use futures::StreamExt as _;
use git::repository::{
    CommitData, CommitOptions, DiffType, FetchOptions, LogOrder, LogSource, PushOptions,
    RepoPath, UpstreamTracking,
};
use git::status::{
    DiffTreeType, FileStatus, StageStatus, StatusCode, TrackedStatus, TreeDiffStatus,
};
use git::{GitHostingProviderRegistry, GitRemote, parse_git_remote_url};
use git_ui_core::askpass_modal::AskPassModal;
use git_ui_core::pull_request_color::pull_request_color;
use github_cli::PullRequest;
use gpui::{
    Action, Anchor, App, AsyncApp, AsyncWindowContext, DismissEvent, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, Global, MouseButton, MouseDownEvent, Pixels, Point,
    PromptLevel, Subscription, Task, WeakEntity, Window, anchored, deferred, prelude::*,
};
use language_model::{
    CompletionIntent, ConfiguredModel, LanguageModelRegistry, LanguageModelRequest,
    LanguageModelRequestMessage, Role,
};
use project::git_store::{CommitDataState, Repository, RepositoryEvent};
use project::{Project, ProjectPath};
use settings::Settings as _;
use ui::{
    Checkbox, ContextMenu, ListItem, ListItemSpacing, PopoverMenu, ToggleState, Tooltip,
    prelude::*,
};
use util::ResultExt as _;
use workspace::{
    Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

use crate::branch_diff::BranchDiff;
use crate::commit_tooltip::CommitAvatar;
use crate::commit_view::CommitView;
use crate::git_graph::{
    ChangedFileEntry, ChangedFileTreeEntry, TREE_INDENT, build_changed_file_tree_entries,
};
use crate::git_panel::{GitPanel, GitStatusEntry};
use crate::git_panel_settings::GitPanelSettings;
use crate::git_status_icon;
use crate::repository_selector::RepositorySelector;
use crate::solo_diff_view::{SoloDiffBase, SoloDiffView};

/// How many commits of each repository the history tab lists: all you would
/// scroll through for one, a summary each for several.
const HISTORY_ONE_REPOSITORY: usize = 100;
const HISTORY_PER_REPOSITORY: usize = 20;

/// How long the branch view waits after a repository changes before asking
/// git again, so a burst of file saves is one diff rather than many.
const BRANCH_DIFF_DEBOUNCE: Duration = Duration::from_millis(750);

/// The diff an AI commit message is written from, at most.
const MAX_DIFF_BYTES: usize = 20_000;

const SELECTION_NAMESPACE: &str = "git_repository_selection";
const VIEW_OPTIONS_NAMESPACE: &str = "bench_git_panel";
const VIEW_OPTIONS_KEY: &str = "view_options";

/// How the panel shows changes, kept across restarts.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ViewOptions {
    tree: bool,
    zen: bool,
}

impl ViewOptions {
    fn load(cx: &App) -> Self {
        db::kvp::KeyValueStore::global(cx)
            .scoped(VIEW_OPTIONS_NAMESPACE)
            .read(VIEW_OPTIONS_KEY)
            .log_err()
            .flatten()
            .and_then(|stored| serde_json::from_str(&stored).log_err())
            .unwrap_or_default()
    }

    fn store(&self, cx: &App) {
        let Some(stored) = serde_json::to_string(self).log_err() else {
            return;
        };
        let store = db::kvp::KeyValueStore::global(cx);
        cx.background_spawn(async move {
            store
                .scoped(VIEW_OPTIONS_NAMESPACE)
                .write(VIEW_OPTIONS_KEY.to_owned(), stored)
                .await
                .log_err();
        })
        .detach();
    }
}

/// The repositories selected beside the active one, per worktree.
#[derive(Default)]
pub struct RepositorySelection {
    /// By the worktree's root, the other selected repositories' work
    /// directories. Loaded from the database the first time a worktree is
    /// asked about.
    extra: HashMap<PathBuf, Vec<PathBuf>>,
}

pub enum SelectionChanged {
    Changed,
}

impl EventEmitter<SelectionChanged> for RepositorySelection {}

struct GlobalRepositorySelection(Entity<RepositorySelection>);

impl Global for GlobalRepositorySelection {}

impl RepositorySelection {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalRepositorySelection>() {
            return global.0.clone();
        }
        let selection = cx.new(|_| Self::default());
        cx.set_global(GlobalRepositorySelection(selection.clone()));
        selection
    }

    fn extra_for(&mut self, root: &Path, cx: &App) -> &mut Vec<PathBuf> {
        self.extra.entry(root.to_path_buf()).or_insert_with(|| {
            db::kvp::KeyValueStore::global(cx)
                .scoped(SELECTION_NAMESPACE)
                .read(&root.to_string_lossy())
                .log_err()
                .flatten()
                .and_then(|stored| serde_json::from_str(&stored).log_err())
                .unwrap_or_default()
        })
    }

    fn store(&self, root: &Path, cx: &App) {
        let Some(extra) = self.extra.get(root) else {
            return;
        };
        let Some(stored) = serde_json::to_string(extra).log_err() else {
            return;
        };
        let key = root.to_string_lossy().into_owned();
        let store = db::kvp::KeyValueStore::global(cx);
        cx.background_spawn(async move {
            store
                .scoped(SELECTION_NAMESPACE)
                .write(key, stored)
                .await
                .log_err();
        })
        .detach();
    }

    /// Adds `repository` to the selection, or takes it out. The active
    /// repository cannot be the last one out: something is always selected,
    /// and taking the active one out makes the next one active.
    pub fn toggle(
        &mut self,
        project: &Entity<Project>,
        repository: &Entity<Repository>,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = project_root(project, cx) else {
            return;
        };
        let path = work_directory(repository, cx);
        let active = project.read(cx).git_store().read(cx).active_repository();
        let is_active = active.as_ref() == Some(repository);

        let extra = self.extra_for(&root, cx);
        if is_active {
            let Some(next) = extra.first().cloned() else {
                return;
            };
            extra.remove(0);
            let next = repositories(project, cx)
                .into_iter()
                .find(|repository| work_directory(repository, cx) == next);
            if let Some(next) = next {
                next.update(cx, |next, cx| next.set_as_active_repository(cx));
            }
        } else if let Some(index) = extra.iter().position(|held| *held == path) {
            extra.remove(index);
        } else {
            extra.push(path);
        }
        self.store(&root, cx);
        cx.emit(SelectionChanged::Changed);
        cx.notify();
    }

    /// Selects the active repository alone, which is what picking one by name
    /// in the picker means.
    pub fn select_only_active(&mut self, project: &Entity<Project>, cx: &mut Context<Self>) {
        let Some(root) = project_root(project, cx) else {
            return;
        };
        let extra = self.extra_for(&root, cx);
        if extra.is_empty() {
            return;
        }
        extra.clear();
        self.store(&root, cx);
        cx.emit(SelectionChanged::Changed);
        cx.notify();
    }

    /// Every selected repository of `project`, the active one included, in
    /// the order the picker lists them.
    pub fn selected(&mut self, project: &Entity<Project>, cx: &App) -> Vec<Entity<Repository>> {
        let active = project.read(cx).git_store().read(cx).active_repository();
        let Some(root) = project_root(project, cx) else {
            return active.into_iter().collect();
        };
        let extra: HashSet<PathBuf> = self.extra_for(&root, cx).iter().cloned().collect();
        repositories(project, cx)
            .into_iter()
            .filter(|repository| {
                active.as_ref() == Some(repository)
                    || extra.contains(&work_directory(repository, cx))
            })
            .collect()
    }

    pub fn is_selected(
        &mut self,
        project: &Entity<Project>,
        repository: &Entity<Repository>,
        cx: &App,
    ) -> bool {
        self.selected(project, cx).contains(repository)
    }
}

/// Whether `project`'s git panel should show several repositories.
pub fn is_multi_repository(project: &Entity<Project>, cx: &mut App) -> bool {
    let selection = RepositorySelection::global(cx);
    selection.update(cx, |selection, cx| selection.selected(project, cx).len() > 1)
}

/// The worktree the selection is remembered for.
fn project_root(project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
    project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
}

/// Every repository of the project, in the picker's order: by name.
fn repositories(project: &Entity<Project>, cx: &App) -> Vec<Entity<Repository>> {
    let mut repositories: Vec<Entity<Repository>> = project
        .read(cx)
        .git_store()
        .read(cx)
        .repositories()
        .values()
        .cloned()
        .collect();
    repositories.sort_by_key(|repository| repository.read(cx).display_name().to_lowercase());
    repositories
}

fn work_directory(repository: &Entity<Repository>, cx: &App) -> PathBuf {
    repository
        .read(cx)
        .snapshot()
        .work_directory_abs_path
        .to_path_buf()
}


#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Changes,
    History,
    PullRequests,
}

/// What the changes tab lists.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChangesMode {
    /// What is not committed yet: staged and unstaged, as the next commit
    /// sees it.
    Working,
    /// Everything on the branch since it left its base, committed or not:
    /// what a pull request from it would contain.
    Branch,
}

enum PullRequests {
    Loading,
    Loaded(Vec<PullRequest>),
    Unavailable(SharedString),
}

/// A repository's branch compared with its base; see [`ChangesMode::Branch`].
enum BranchChanges {
    Loading,
    Loaded {
        base: SharedString,
        files: Vec<(RepoPath, FileStatus)>,
        /// Each file's blob at the base, for the ones that existed there.
        base_oids: HashMap<RepoPath, git::Oid>,
    },
    Unavailable(SharedString),
}

/// One changed file as a row shows it: where it is, what happened to it, and
/// how much of it is staged.
#[derive(Clone)]
struct Change {
    path: RepoPath,
    status: FileStatus,
    staging: StageStatus,
}

/// Which list a row is in, which decides what its checkbox and menu do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Section {
    Staged,
    Unstaged,
    Branch,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Section::Staged => "Staged",
            Section::Unstaged => "Changes",
            Section::Branch => "Changed on this branch",
        }
    }
}

/// A row of a file list: a folder in tree view, or a file.
#[derive(Debug, PartialEq)]
enum FileRow {
    Folder {
        path: RepoPath,
        name: SharedString,
        depth: usize,
        collapsed: bool,
    },
    File {
        index: usize,
        depth: usize,
    },
}

/// The rows of a file list, flat or as the same tree the diff view lists
/// files in, where a folder holding only one folder shares its row. A folder
/// in `collapsed` hides what is in it.
fn file_rows(changes: &[Change], tree: bool, collapsed: &HashSet<RepoPath>) -> Vec<FileRow> {
    if !tree {
        return (0..changes.len())
            .map(|index| FileRow::File { index, depth: 0 })
            .collect();
    }
    let index_by_path: HashMap<&RepoPath, usize> = changes
        .iter()
        .enumerate()
        .map(|(index, change)| (&change.path, index))
        .collect();
    let expanded: collections::HashMap<RepoPath, bool> = collapsed
        .iter()
        .map(|path| (path.clone(), false))
        .collect();
    let files = changes
        .iter()
        .map(|change| ChangedFileEntry::new(change.path.clone(), change.status))
        .collect();
    build_changed_file_tree_entries(files, &expanded)
        .into_iter()
        .filter_map(|entry| match entry {
            ChangedFileTreeEntry::Directory(directory) => Some(FileRow::Folder {
                path: directory.path,
                name: directory.name,
                depth: directory.depth,
                collapsed: !directory.expanded,
            }),
            ChangedFileTreeEntry::File(file) => Some(FileRow::File {
                index: *index_by_path.get(&file.entry.repo_path)?,
                depth: file.depth,
            }),
        })
        .collect()
}

pub struct BenchGitPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    tab: Tab,
    changes_mode: ChangesMode,
    tree: bool,
    /// Whether a file opens alone and in full, rather than in the diff of
    /// every change.
    zen: bool,
    /// Folders closed in tree view, by repository and folder path.
    collapsed: HashSet<(EntityId, RepoPath)>,
    /// Repositories whose group is closed, on every tab.
    collapsed_repositories: HashSet<EntityId>,
    selected_commit: Option<(EntityId, String)>,
    commit_editor: Entity<Editor>,
    amend: bool,
    signoff: bool,
    /// The message before amend replaced it with the last commit's, to put
    /// back when amend is turned off.
    message_before_amend: Option<String>,
    _generating: Option<Task<()>>,
    generating: bool,
    branch_changes: HashMap<EntityId, BranchChanges>,
    _branch_tasks: HashMap<EntityId, Task<()>>,
    /// By work directory and branch, so a branch switch asks again.
    pull_requests: HashMap<(PathBuf, Option<String>), PullRequests>,
    _pull_request_tasks: Vec<Task<()>>,
    /// What a remote operation or commit is doing, while it is; the buttons
    /// wait for it.
    busy: Option<&'static str>,
    _busy_task: Option<Task<()>>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    repository_subscriptions: HashMap<EntityId, Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl BenchGitPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            let handle = workspace.clone();
            workspace.update_in(cx, |workspace, window, cx| {
                let project = workspace.project().clone();
                cx.new(|cx| Self::new(handle, project, window, cx))
            })
        })
    }

    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let commit_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 10, window, cx);
            editor.set_placeholder_text("Commit message", window, cx);
            editor
        });
        let selection = RepositorySelection::global(cx);
        let view_options = ViewOptions::load(cx);
        let git_store = project.read(cx).git_store().clone();
        let subscriptions = vec![
            cx.subscribe(&selection, |_, _, _: &SelectionChanged, cx| cx.notify()),
            // A repository added, removed or made active.
            cx.observe(&git_store, |_, _, cx| cx.notify()),
        ];
        Self {
            workspace,
            project,
            focus_handle: cx.focus_handle(),
            tab: Tab::Changes,
            changes_mode: ChangesMode::Working,
            tree: view_options.tree,
            zen: view_options.zen,
            collapsed: HashSet::new(),
            collapsed_repositories: HashSet::new(),
            selected_commit: None,
            commit_editor,
            amend: false,
            signoff: false,
            message_before_amend: None,
            _generating: None,
            generating: false,
            branch_changes: HashMap::new(),
            _branch_tasks: HashMap::new(),
            pull_requests: HashMap::new(),
            _pull_request_tasks: Vec::new(),
            busy: None,
            _busy_task: None,
            context_menu: None,
            repository_subscriptions: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    fn store_view_options(&self, cx: &App) {
        ViewOptions {
            tree: self.tree,
            zen: self.zen,
        }
        .store(cx);
    }

    fn selected(&self, cx: &mut App) -> Vec<Entity<Repository>> {
        let selection = RepositorySelection::global(cx);
        selection.update(cx, |selection, cx| selection.selected(&self.project, cx))
    }

    /// Redraws when any selected repository changes — a file staged, a commit
    /// made, history loaded — and, in the branch view, asks git for the
    /// branch's changes again once it settles.
    fn watch(&mut self, repositories: &[Entity<Repository>], cx: &mut Context<Self>) {
        let ids: HashSet<EntityId> = repositories
            .iter()
            .map(|repository| repository.entity_id())
            .collect();
        self.repository_subscriptions
            .retain(|id, _| ids.contains(id));
        for repository in repositories {
            self.repository_subscriptions
                .entry(repository.entity_id())
                .or_insert_with(|| {
                    // Every git job notifies the repository, including the ones the
                    // branch load runs, so reloading on any notify would cancel the
                    // load in flight over and over. Only a change to what the branch
                    // contains asks again.
                    Subscription::join(
                        cx.observe(repository, |_, _, cx| cx.notify()),
                        cx.subscribe(repository, |this, repository, event, cx| {
                            if matches!(
                                event,
                                RepositoryEvent::StatusesChanged
                                    | RepositoryEvent::HeadChanged
                                    | RepositoryEvent::BranchListChanged
                            ) && this.tab == Tab::Changes
                                && this.changes_mode == ChangesMode::Branch
                            {
                                this.load_branch_changes(&repository, true, cx);
                            }
                        }),
                    )
                });
        }
    }

    fn askpass(&self, operation: String, window: &mut Window, cx: &mut Context<Self>) -> AskPassDelegate {
        let workspace = self.workspace.clone();
        let window = window.window_handle();
        let operation: SharedString = operation.into();
        AskPassDelegate::new_with_cancellation(
            &mut cx.to_async(),
            move |prompt, tx, cancellation, cx| {
                window
                    .update(cx, |_, window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.toggle_modal(window, cx, |window, cx| {
                                    AskPassModal::new(
                                        operation.clone(),
                                        prompt.into(),
                                        tx,
                                        cancellation,
                                        window,
                                        cx,
                                    )
                                });
                            })
                            .ok();
                    })
                    .ok();
            },
        )
    }

    fn toast(&self, message: String, cx: &mut App) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<BenchGitPanel>(), message),
                    cx,
                );
            })
            .ok();
    }

    fn changes(repository: &Entity<Repository>, cx: &App) -> Vec<Change> {
        let mut changes: Vec<Change> = repository
            .read(cx)
            .cached_status()
            .map(|entry| Change {
                staging: entry.status.staging(),
                path: entry.repo_path,
                status: entry.status,
            })
            .collect();
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes
    }

    // Opening

    /// The diff of one uncommitted file, in the repository it belongs to —
    /// which need not be the active one.
    fn open_diff(
        &self,
        repository: &Entity<Repository>,
        change: &Change,
        window: &mut Window,
        cx: &mut App,
    ) {
        let entry = GitStatusEntry {
            repo_path: change.path.clone(),
            status: change.status,
            staging: change.staging,
            diff_stat: None,
        };
        SoloDiffView::open_or_focus(entry, repository.clone(), self.workspace.clone(), window, cx)
            .detach_and_log_err(cx);
    }

    /// One file alone and in full: its uncommitted changes, or with
    /// `branch_base`, everything the branch changed in it.
    fn open_zen_diff(
        &self,
        repository: &Entity<Repository>,
        branch_base: Option<&SharedString>,
        change: &Change,
        window: &mut Window,
        cx: &mut App,
    ) {
        let base = match branch_base {
            Some(base_ref) => SoloDiffBase::Branch {
                base_ref: base_ref.clone(),
                base_oid: match self.branch_changes.get(&repository.entity_id()) {
                    Some(BranchChanges::Loaded { base_oids, .. }) => {
                        base_oids.get(&change.path).copied()
                    }
                    Some(BranchChanges::Loading | BranchChanges::Unavailable(_)) | None => None,
                },
            },
            None => SoloDiffBase::Head,
        };
        let entry = GitStatusEntry {
            repo_path: change.path.clone(),
            status: change.status,
            staging: change.staging,
            diff_stat: None,
        };
        SoloDiffView::open_or_focus_with_base(
            entry,
            repository.clone(),
            base,
            Some(true),
            self.workspace.clone(),
            window,
            cx,
        )
        .detach_and_log_err(cx);
    }

    /// The branch's changes against its base, as one diff, scrolled to
    /// `change` when one is given.
    fn open_branch_diff(
        &self,
        repository: &Entity<Repository>,
        base: SharedString,
        change: Option<&Change>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let project = self.project.clone();
        let repository = repository.clone();
        let file = change.map(|change| (change.path.clone(), change.status));
        self.workspace
            .update(cx, |workspace, cx| {
                BranchDiff::deploy_branch_diff_at_file(
                    workspace, project, repository, base, None, file, window, cx,
                );
            })
            .ok();
    }

    fn open_file(&self, repository: &Entity<Repository>, path: &RepoPath, window: &mut Window, cx: &mut App) {
        let Some(project_path): Option<ProjectPath> =
            repository.read(cx).repo_path_to_project_path(path, cx)
        else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_path(project_path, None, true, window, cx)
                    .detach_and_log_err(cx);
            })
            .ok();
    }

    // Staging and discarding

    fn set_staged(
        &self,
        repository: &Entity<Repository>,
        paths: Vec<RepoPath>,
        stage: bool,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let name = repository.read(cx).display_name();
        let task = repository.update(cx, |repository, cx| {
            if stage {
                repository.stage_entries(paths, cx)
            } else {
                repository.unstage_entries(paths, cx)
            }
        });
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                this.update(cx, |this, cx| {
                    this.toast(format!("Could not change what is staged in {name}: {error:#}"), cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// Throws away the changes to `changes`, after asking: tracked files go
    /// back to what was last committed, new files go to the trash.
    fn discard(
        &mut self,
        repository: &Entity<Repository>,
        changes: Vec<Change>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if changes.is_empty() {
            return;
        }
        let title = match changes.as_slice() {
            [change] => format!(
                "Discard the changes to {}?",
                change.path.file_name().unwrap_or_default()
            ),
            changes => format!("Discard the changes to {} files?", changes.len()),
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            Some("Tracked files go back to what was last committed; new files go to the trash."),
            &["Discard", "Cancel"],
            cx,
        );
        let repository = repository.clone();
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return;
            }
            let staged: Vec<RepoPath> = changes
                .iter()
                .filter(|change| change.staging != StageStatus::Unstaged)
                .map(|change| change.path.clone())
                .collect();
            let (created, tracked): (Vec<Change>, Vec<Change>) = changes
                .into_iter()
                .partition(|change| change.status.is_created());
            let result: Result<()> = async {
                if !staged.is_empty() {
                    repository
                        .update(cx, |repository, cx| repository.unstage_entries(staged, cx))
                        .await?;
                }
                if !tracked.is_empty() {
                    let paths = tracked.into_iter().map(|change| change.path).collect();
                    repository
                        .update(cx, |repository, cx| repository.checkout_files("HEAD", paths, cx))
                        .await?;
                }
                for change in created {
                    let trashed = cx.update(|cx| {
                        let project_path = repository
                            .read(cx)
                            .repo_path_to_project_path(&change.path, cx)?;
                        project.update(cx, |project, cx| project.trash_file(project_path, cx))
                    });
                    if let Some(trashed) = trashed {
                        trashed.await?;
                    }
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    this.toast(format!("Could not discard the changes: {error:#}"), cx)
                })
                .ok();
            }
        })
        .detach();
    }

    // The branch view

    fn load_branch_changes(
        &mut self,
        repository: &Entity<Repository>,
        debounce: bool,
        cx: &mut Context<Self>,
    ) {
        let id = repository.entity_id();
        if !debounce || !self.branch_changes.contains_key(&id) {
            self.branch_changes
                .entry(id)
                .or_insert(BranchChanges::Loading);
        }
        let repository = repository.clone();
        let task = cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(BRANCH_DIFF_DEBOUNCE).await;
            }
            let changes = branch_changes(&repository, cx).await;
            this.update(cx, |this, cx| {
                this.branch_changes.insert(
                    id,
                    match changes {
                        Ok(Some((base, files, base_oids))) => BranchChanges::Loaded {
                            base,
                            files,
                            base_oids,
                        },
                        Ok(None) => BranchChanges::Unavailable(
                            "No base branch to compare with".into(),
                        ),
                        Err(error) => BranchChanges::Unavailable(format!("{error:#}").into()),
                    },
                );
                cx.notify();
            })
            .ok();
        });
        self._branch_tasks.insert(id, task);
    }

    // Committing

    fn toggle_amend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.amend = !self.amend;
        if !self.amend {
            let before = self.message_before_amend.take().unwrap_or_default();
            self.commit_editor
                .update(cx, |editor, cx| editor.set_text(before, window, cx));
            cx.notify();
            return;
        }
        let Some(repository) = self.selected(cx).into_iter().next() else {
            return;
        };
        let Some(head) = repository.read(cx).head_commit.as_ref().map(|head| head.sha.to_string())
        else {
            return;
        };
        self.message_before_amend = Some(self.commit_editor.read(cx).text(cx));
        let details = repository.update(cx, |repository, _| repository.show(head));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(details)) = details.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                if this.amend {
                    let message = details.message.trim_end().to_owned();
                    this.commit_editor
                        .update(cx, |editor, cx| editor.set_text(message, window, cx));
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Commits the message in every selected repository with something to
    /// commit. With several, it says exactly what that will be first.
    ///
    /// Each repository commits what is staged in it, or — when nothing is — its
    /// tracked changes, as Zed's "Commit Tracked" does. A repository with
    /// nothing to commit is left out. Conflicts anywhere stop the whole commit:
    /// committing the other repositories and not that one would leave the
    /// change half made.
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.commit_editor.read(cx).text(cx).trim().to_owned();
        if message.is_empty() {
            window.focus(&self.commit_editor.focus_handle(cx), cx);
            return;
        }
        let options = CommitOptions {
            amend: self.amend,
            signoff: self.signoff,
            ..CommitOptions::default()
        };
        let repositories = self.selected(cx);
        let mut plans = Vec::new();
        let mut conflicted = Vec::new();
        for repository in &repositories {
            let changes = Self::changes(repository, cx);
            let name = repository.read(cx).display_name();
            if changes.iter().any(|change| change.status.is_conflicted()) {
                conflicted.push(name);
                continue;
            }
            let staged = changes
                .iter()
                .filter(|change| change.staging != StageStatus::Unstaged)
                .count();
            if staged > 0 || options.amend {
                plans.push(CommitPlan {
                    repository: repository.clone(),
                    name,
                    stage_first: None,
                    files: staged,
                });
                continue;
            }
            let tracked: Vec<RepoPath> = changes
                .into_iter()
                .filter(|change| !change.status.is_created())
                .map(|change| change.path)
                .collect();
            if !tracked.is_empty() {
                plans.push(CommitPlan {
                    repository: repository.clone(),
                    name,
                    files: tracked.len(),
                    stage_first: Some(tracked),
                });
            }
        }

        if !conflicted.is_empty() {
            let detail = format!(
                "Resolve and stage the conflicts in {} first.",
                conflicted.join(", ")
            );
            let prompt = window.prompt(
                PromptLevel::Warning,
                "There are conflicts",
                Some(&detail),
                &["OK"],
                cx,
            );
            cx.spawn(async move |_, _| prompt.await.log_err()).detach();
            return;
        }
        if plans.is_empty() {
            let prompt = window.prompt(
                PromptLevel::Info,
                "Nothing to commit",
                Some("There are no changes to commit."),
                &["OK"],
                cx,
            );
            cx.spawn(async move |_, _| prompt.await.log_err()).detach();
            return;
        }

        // One repository is the ordinary commit, which needs no warning.
        let answer = (repositories.len() > 1).then(|| {
            let detail = plans
                .iter()
                .map(|plan| {
                    let files =
                        format!("{} file{}", plan.files, if plan.files == 1 { "" } else { "s" });
                    match plan.stage_first {
                        Some(_) => format!(
                            "• {}: {files}, every tracked change (nothing is staged)",
                            plan.name
                        ),
                        None => format!("• {}: {files} staged", plan.name),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let title = format!(
                "Commit to {} repositor{}?",
                plans.len(),
                if plans.len() == 1 { "y" } else { "ies" }
            );
            window.prompt(
                PromptLevel::Warning,
                &title,
                Some(&format!("“{}”\n\n{detail}", first_line(&message))),
                &["Commit", "Cancel"],
                cx,
            )
        });
        self.busy = Some("Committing…");
        cx.notify();
        self._busy_task = Some(cx.spawn_in(window, async move |this, cx| {
            if let Some(answer) = answer
                && answer.await.ok() != Some(0)
            {
                this.update(cx, |this, cx| {
                    this.busy = None;
                    cx.notify();
                })
                .ok();
                return;
            }
            let mut committed = Vec::new();
            let mut failed = Vec::new();
            for plan in plans {
                let Ok(askpass) = this.update_in(cx, |this, window, cx| {
                    this.askpass(format!("git commit ({})", plan.name), window, cx)
                }) else {
                    return;
                };
                match commit_one(&plan, message.clone(), options, askpass, cx).await {
                    Ok(()) => committed.push(plan.name),
                    Err(error) => failed.push(format!("{}: {error:#}", plan.name)),
                }
            }
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                if failed.is_empty() {
                    this.commit_editor
                        .update(cx, |editor, cx| editor.clear(window, cx));
                    this.amend = false;
                    this.message_before_amend = None;
                    if committed.len() > 1 {
                        this.toast(format!("Committed to {}.", committed.join(", ")), cx);
                    }
                } else {
                    this.toast(
                        format!(
                            "{}Could not commit to {}",
                            if committed.is_empty() {
                                String::new()
                            } else {
                                format!("Committed to {}. ", committed.join(", "))
                            },
                            failed.join("; ")
                        ),
                        cx,
                    );
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Writes the commit message with the configured model, from what each
    /// selected repository would commit, as Zed's panel does for one.
    fn generate_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.generating || !AgentSettings::get_global(cx).enabled(cx) {
            return;
        }
        let Some(ConfiguredModel { provider, model }) =
            LanguageModelRegistry::read_global(cx).commit_message_model(cx)
        else {
            self.toast("Set a model for commit messages to write one.".into(), cx);
            return;
        };
        let repositories = self.selected(cx);
        let Some(first) = repositories.first().cloned() else {
            return;
        };
        let several = repositories.len() > 1;
        let diffs: Vec<(SharedString, _)> = repositories
            .iter()
            .map(|repository| {
                let staged = Self::changes(repository, cx)
                    .iter()
                    .any(|change| change.staging != StageStatus::Unstaged);
                let name = repository.read(cx).display_name();
                let diff = repository.update(cx, |repository, cx| {
                    repository.diff(
                        if staged {
                            DiffType::HeadToIndex
                        } else {
                            DiffType::HeadToWorktree
                        },
                        cx,
                    )
                });
                (name, diff)
            })
            .collect();
        let temperature = AgentSettings::temperature_for_model(&model, cx);
        let include_rules = AgentSettings::get_global(cx).commit_message_include_project_rules;
        let instructions = AgentSettings::get_global(cx)
            .commit_message_instructions
            .clone();
        let project = self.project.clone();
        let work_dir = first.read(cx).work_directory_abs_path.clone();
        let subject = self
            .commit_editor
            .read(cx)
            .text(cx)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned();

        self.generating = true;
        cx.notify();
        self._generating = Some(cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                if let Some(task) = cx.update(|_, cx| {
                    (!provider.is_authenticated(cx)).then(|| provider.authenticate(cx))
                })? {
                    task.await.log_err();
                }
                let mut diff_text = String::new();
                for (name, diff) in diffs {
                    let diff = diff.await??;
                    if diff.trim().is_empty() {
                        continue;
                    }
                    if several {
                        diff_text.push_str(&format!("# Repository: {name}\n"));
                    }
                    diff_text.push_str(&diff);
                    diff_text.push('\n');
                }
                let diff_text = GitPanel::compress_commit_diff(&diff_text, MAX_DIFF_BYTES);
                let rules = if include_rules {
                    GitPanel::load_project_rules(&project, &work_dir, cx).await
                } else {
                    None
                };
                let user_agents_md = if include_rules {
                    cx.update(|_, cx| {
                        UserAgentsMd::global(cx).and_then(|agents| agents.content().cloned())
                    })?
                } else {
                    None
                };
                let content = GitPanel::build_commit_message_prompt(
                    include_str!("../src/commit_message_prompt.txt"),
                    user_agents_md.as_deref(),
                    rules.as_deref(),
                    instructions.as_deref(),
                    &subject,
                    &diff_text,
                );
                let request = LanguageModelRequest {
                    thread_id: None,
                    prompt_cache_key: None,
                    prompt_id: None,
                    intent: Some(CompletionIntent::GenerateGitCommitMessage),
                    messages: vec![LanguageModelRequestMessage {
                        role: Role::User,
                        content: vec![content.into()],
                        cache: false,
                        reasoning_details: None,
                    }],
                    tools: Vec::new(),
                    tool_choice: None,
                    stop: Vec::new(),
                    temperature,
                    thinking_allowed: false,
                    thinking_effort: None,
                    speed: None,
                    compact_at_tokens: None,
                    max_output_tokens: None,
                };
                let mut stream = model.stream_completion_text(request, cx).await?;
                let mut written = if subject.trim().is_empty() {
                    String::new()
                } else {
                    format!("{subject}\n")
                };
                while let Some(chunk) = stream.stream.next().await {
                    written.push_str(&chunk?);
                    let text = written.clone();
                    this.update_in(cx, |this, window, cx| {
                        this.commit_editor
                            .update(cx, |editor, cx| editor.set_text(text, window, cx));
                    })?;
                }
                Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                this.generating = false;
                if let Err(error) = result {
                    this.toast(format!("Could not write a commit message: {error:#}"), cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // Remote operations

    /// Runs `operation` on every selected repository in turn, and says how it
    /// went in each.
    fn run_remote(&mut self, operation: RemoteOperation, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let repositories = self.selected(cx);
        let confirmed = match operation {
            RemoteOperation::Push if repositories.len() > 1 => {
                let detail = repositories
                    .iter()
                    .map(|repository| {
                        let repository = repository.read(cx);
                        format!(
                            "• {} → {}",
                            repository.display_name(),
                            repository
                                .branch
                                .as_ref()
                                .map(|branch| branch.name().to_owned())
                                .unwrap_or_else(|| "no branch".into())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(window.prompt(
                    PromptLevel::Warning,
                    &format!("Push {} repositories?", repositories.len()),
                    Some(&detail),
                    &["Push", "Cancel"],
                    cx,
                ))
            }
            _ => None,
        };
        self.busy = Some(operation.progress());
        cx.notify();
        self._busy_task = Some(cx.spawn_in(window, async move |this, cx| {
            if let Some(confirmed) = confirmed
                && confirmed.await.ok() != Some(0)
            {
                this.update(cx, |this, cx| {
                    this.busy = None;
                    cx.notify();
                })
                .ok();
                return;
            }
            let mut done = Vec::new();
            let mut failed = Vec::new();
            for repository in repositories {
                let name = repository.read_with(cx, |repository, _| repository.display_name());
                let Ok(askpass) = this.update_in(cx, |this, window, cx| {
                    this.askpass(format!("git {} ({name})", operation.verb()), window, cx)
                }) else {
                    return;
                };
                match run_remote_one(operation, &repository, askpass, cx).await {
                    Ok(()) => done.push(name),
                    Err(error) => failed.push(format!("{name}: {error:#}")),
                }
            }
            this.update(cx, |this, cx| {
                this.busy = None;
                let message = if failed.is_empty() {
                    format!("{} {}.", operation.past(), done.join(", "))
                } else if done.is_empty() {
                    format!("Could not {}: {}", operation.verb(), failed.join("; "))
                } else {
                    format!(
                        "{} {}. Could not {}: {}",
                        operation.past(),
                        done.join(", "),
                        operation.verb(),
                        failed.join("; ")
                    )
                };
                this.toast(message, cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn load_pull_requests(&mut self, repositories: &[Entity<Repository>], cx: &mut Context<Self>) {
        for repository in repositories {
            let root = work_directory(repository, cx);
            let branch = repository
                .read(cx)
                .branch
                .as_ref()
                .map(|branch| branch.name().to_owned());
            let key = (root.clone(), branch.clone());
            if self.pull_requests.contains_key(&key) {
                continue;
            }
            self.pull_requests.insert(key.clone(), PullRequests::Loading);
            let task = cx.spawn(async move |this, cx| {
                let found = cx
                    .background_spawn(async move {
                        github_cli::pull_requests(&root, branch.as_deref(), github_cli::DEFAULT_LIMIT)
                            .await
                    })
                    .await;
                this.update(cx, |this, cx| {
                    this.pull_requests.insert(
                        key,
                        match found {
                            Ok(found) => PullRequests::Loaded(found),
                            Err(unavailable) => PullRequests::Unavailable(unavailable.message()),
                        },
                    );
                    cx.notify();
                })
                .ok();
            });
            self._pull_request_tasks.push(task);
        }
    }

    // Menus

    fn deploy_menu(
        &mut self,
        menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = menu.focus_handle(cx);
        window.defer(cx, move |window, cx| window.focus(&focus, cx));
        let subscription = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, _, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn file_menu(
        &mut self,
        repository: Entity<Repository>,
        change: Change,
        section: Section,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let absolute = work_directory(&repository, cx).join(change.path.as_unix_str());
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let entry = |this: &WeakEntity<Self>, action: fn(&mut Self, &Entity<Repository>, &Change, &mut Window, &mut Context<Self>)| {
                let this = this.clone();
                let repository = repository.clone();
                let change = change.clone();
                move |window: &mut Window, cx: &mut App| {
                    this.update(cx, |this, cx| action(this, &repository, &change, window, cx))
                        .ok();
                }
            };
            let menu = match section {
                Section::Branch => menu,
                Section::Staged | Section::Unstaged => menu
                    .entry("Open Diff", None, entry(&this, |this, repository, change, window, cx| {
                        this.open_diff(repository, change, window, cx)
                    })),
            };
            let menu = menu.entry("Open File", None, entry(&this, |this, repository, change, window, cx| {
                this.open_file(repository, &change.path, window, cx)
            }));
            let menu = match section {
                Section::Staged => menu.entry("Unstage", None, entry(&this, |this, repository, change, _, cx| {
                    this.set_staged(repository, vec![change.path.clone()], false, cx)
                })),
                Section::Unstaged => menu.entry("Stage", None, entry(&this, |this, repository, change, _, cx| {
                    this.set_staged(repository, vec![change.path.clone()], true, cx)
                })),
                Section::Branch => menu,
            };
            let menu = menu
                .separator()
                .action(
                    "Send to Claude",
                    Box::new(zed_actions::claude::SendFile {
                        path: absolute.to_string_lossy().into_owned(),
                    }),
                );
            match section {
                Section::Branch => menu,
                Section::Staged | Section::Unstaged => menu.separator().entry(
                    "Discard Changes…",
                    None,
                    entry(&this, |this, repository, change, window, cx| {
                        this.discard(repository, vec![change.clone()], window, cx)
                    }),
                ),
            }
        });
        self.deploy_menu(menu, position, window, cx);
    }

    fn commit_menu(
        &mut self,
        repository: Entity<Repository>,
        sha: String,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let repository = repository.downgrade();
            let open_sha = sha.clone();
            let copy_sha = sha.clone();
            let send_sha = sha.clone();
            menu.entry("Open Commit", None, move |window, cx| {
                CommitView::open(
                    open_sha.clone(),
                    repository.clone(),
                    workspace.clone(),
                    None,
                    None,
                    window,
                    cx,
                );
            })
            .entry("Copy SHA", None, move |_, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_sha.clone()));
            })
            .separator()
            .action(
                "Send to Claude",
                Box::new(zed_actions::claude::SendCommit { sha: send_sha }),
            )
        });
        self.deploy_menu(menu, position, window, cx);
    }

    // Drawing

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let border = colors.border;
        let active = colors.panel_background;
        let inactive = colors.tab_bar_background;
        let tabs = [
            ("tab-changes", "Changes", Tab::Changes),
            ("tab-history", "History", Tab::History),
            ("tab-pull-requests", "Pull Requests", Tab::PullRequests),
        ];
        let count = tabs.len();
        h_flex()
            .w_full()
            .flex_none()
            .children(tabs.into_iter().enumerate().map(|(index, (id, label, tab))| {
                let selected = self.tab == tab;
                div()
                    .id(id)
                    .flex_1()
                    .h(rems(2.25))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .bg(if selected { active } else { inactive })
                    .when(index + 1 < count, |this| this.border_r_1())
                    .when(!selected, |this| this.border_b_1())
                    .border_color(border)
                    .child(Label::new(label).color(if selected {
                        Color::Default
                    } else {
                        Color::Muted
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = tab;
                        cx.notify();
                    }))
            }))
    }

    fn render_repository_header(
        &self,
        repository: &Entity<Repository>,
        detail: Option<SharedString>,
        is_first: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let repository_id = repository.entity_id();
        let collapsed = self.collapsed_repositories.contains(&repository_id);
        let repository = repository.read(cx);
        let branch = repository
            .branch
            .as_ref()
            .map(|branch| branch.name().to_owned())
            .unwrap_or_else(|| "no branch".into());
        h_flex()
            .id(("repository-header", repository_id.as_u64() as usize))
            .w_full()
            .px_2()
            .pt_2()
            .pb_1()
            .when(!is_first, |this| this.mt_3())
            .gap_1p5()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        Icon::new(if collapsed {
                            IconName::Folder
                        } else {
                            IconName::FolderOpen
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(Label::new(repository.display_name()).size(LabelSize::Small))
                    .child(
                        Label::new(detail.unwrap_or_else(|| branch.into()))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line()
                            .truncate(),
                    ),
            )
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed_repositories.remove(&repository_id) {
                    this.collapsed_repositories.insert(repository_id);
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_changes_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = |id: &'static str, label: &'static str, mode: ChangesMode| {
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .toggle_state(self.changes_mode == mode)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.changes_mode = mode;
                    if mode == ChangesMode::Branch {
                        for repository in this.selected(cx) {
                            this.load_branch_changes(&repository, false, cx);
                        }
                    }
                    cx.notify();
                }))
        };
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_1()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_0p5()
                    .child(mode("changes-working", "Uncommitted", ChangesMode::Working))
                    .child(mode("changes-branch", "Branch", ChangesMode::Branch)),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("toggle-zen", IconName::Crosshair)
                            .icon_size(IconSize::Small)
                            .toggle_state(self.zen)
                            .tooltip(Tooltip::text(if self.zen {
                                "Open Files in the Diff of Every Change"
                            } else {
                                "Zen Diff: Open One Whole File at a Time"
                            }))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.zen = !this.zen;
                                this.store_view_options(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        IconButton::new("toggle-tree", IconName::ListTree)
                            .icon_size(IconSize::Small)
                            .toggle_state(self.tree)
                            .tooltip(Tooltip::text(if self.tree {
                                "Show as List"
                            } else {
                                "Show as Tree"
                            }))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.tree = !this.tree;
                                this.store_view_options(cx);
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_section_header(
        &self,
        repository: &Entity<Repository>,
        section: Section,
        changes: &[Change],
        key: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let repository = repository.clone();
        let paths: Vec<RepoPath> = changes.iter().map(|change| change.path.clone()).collect();
        let discardable = changes.to_vec();
        h_flex()
            .w_full()
            .px_2()
            .py_0p5()
            .justify_between()
            .child(
                Label::new(format!("{} ({})", section.title(), changes.len()))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .when(section == Section::Unstaged, |this| {
                        let repository = repository.clone();
                        this.child(
                            IconButton::new(("discard-all", key), IconName::Undo)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Discard All Changes…"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.discard(&repository, discardable.clone(), window, cx)
                                })),
                        )
                    })
                    .when(section != Section::Branch, |this| {
                        let stage = section == Section::Unstaged;
                        this.child(
                            Button::new(
                                ("stage-section", key),
                                if stage { "Stage All" } else { "Unstage All" },
                            )
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_staged(&repository, paths.clone(), stage, cx)
                            })),
                        )
                    }),
            )
            .into_any_element()
    }

    /// A file list: a section's changes, flat or as a tree.
    fn render_files(
        &mut self,
        repository: &Entity<Repository>,
        section: Section,
        changes: Vec<Change>,
        branch_base: Option<SharedString>,
        key: usize,
        rows: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let repository_id = repository.entity_id();
        let collapsed: HashSet<RepoPath> = self
            .collapsed
            .iter()
            .filter(|(id, _)| *id == repository_id)
            .map(|(_, path)| path.clone())
            .collect();
        for (row, file_row) in file_rows(&changes, self.tree, &collapsed).into_iter().enumerate() {
            let id = key * 100_000 + row;
            match file_row {
                FileRow::Folder {
                    path,
                    name,
                    depth,
                    collapsed,
                } => {
                    let folder_icon =
                        FileIcons::get_folder_icon(!collapsed, path.as_std_path(), cx)
                            .map(Icon::from_path)
                            .unwrap_or_else(|| {
                                Icon::new(if collapsed {
                                    IconName::Folder
                                } else {
                                    IconName::FolderOpen
                                })
                            })
                            .size(IconSize::Small)
                            .color(Color::Muted);
                    rows.push(
                        ListItem::new(("folder", id))
                            .spacing(ListItemSpacing::Sparse)
                            .indent_level(depth)
                            .indent_step_size(px(TREE_INDENT))
                            .start_slot(folder_icon)
                            .child(
                                Label::new(name)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                            .tooltip(Tooltip::text(SharedString::from(
                                path.as_unix_str().to_owned(),
                            )))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let key = (repository_id, path.clone());
                                if !this.collapsed.remove(&key) {
                                    this.collapsed.insert(key);
                                }
                                cx.notify();
                            }))
                            .into_any_element(),
                    );
                }
                FileRow::File { index, depth } => {
                    let Some(change) = changes.get(index).cloned() else {
                        continue;
                    };
                    rows.push(self.render_file(
                        repository,
                        section,
                        change,
                        branch_base.clone(),
                        depth,
                        id,
                        cx,
                    ));
                }
            }
        }
    }

    fn render_file(
        &self,
        repository: &Entity<Repository>,
        section: Section,
        change: Change,
        branch_base: Option<SharedString>,
        depth: usize,
        id: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let file_name = change
            .path
            .file_name()
            .map(|name| name.to_owned())
            .unwrap_or_default();
        let directory = (!self.tree)
            .then(|| {
                change
                    .path
                    .parent()
                    .map(|parent| parent.as_unix_str().to_owned())
                    .filter(|parent| !parent.is_empty())
            })
            .flatten();
        let checkbox = (section != Section::Branch).then(|| {
            let repository = repository.clone();
            let path = change.path.clone();
            Checkbox::new(
                ("stage", id),
                if section == Section::Staged {
                    ToggleState::Selected
                } else {
                    ToggleState::Unselected
                },
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                // Not also a click on the row, which opens the diff.
                cx.stop_propagation();
                this.set_staged(&repository, vec![path.clone()], section == Section::Unstaged, cx);
            }))
        });
        let open_repository = repository.clone();
        let open_change = change.clone();
        let menu_repository = repository.clone();
        let menu_change = change.clone();
        let full_path = SharedString::from(change.path.as_unix_str().to_owned());
        ListItem::new(("file", id))
            .spacing(ListItemSpacing::Sparse)
            .indent_level(depth)
            .indent_step_size(px(TREE_INDENT))
            .start_slot(
                h_flex()
                    .gap_1()
                    .children(checkbox)
                    .child(git_status_icon(change.status)),
            )
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1()
                    .child(
                        Label::new(file_name)
                            .size(LabelSize::Small)
                            .when(change.status.is_deleted(), Label::strikethrough)
                            .single_line(),
                    )
                    .children(directory.map(|directory| {
                        Label::new(directory)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line()
                            .truncate()
                    })),
            )
            .tooltip(Tooltip::text(full_path))
            .on_click(cx.listener(move |this, _, window, cx| match &branch_base {
                _ if this.zen => {
                    this.open_zen_diff(&open_repository, branch_base.as_ref(), &open_change, window, cx)
                }
                Some(base) => this.open_branch_diff(
                    &open_repository,
                    base.clone(),
                    Some(&open_change),
                    window,
                    cx,
                ),
                None => this.open_diff(&open_repository, &open_change, window, cx),
            }))
            .on_secondary_mouse_down(cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.file_menu(
                    menu_repository.clone(),
                    menu_change.clone(),
                    section,
                    event.position,
                    window,
                    cx,
                );
            }))
            .into_any_element()
    }

    fn render_changes(&mut self, repositories: &[Entity<Repository>], cx: &mut Context<Self>) -> AnyElement {
        let several = repositories.len() > 1;
        let mut rows = Vec::new();
        let mut content_start = None;
        for (index, repository) in repositories.iter().enumerate() {
            if let Some(start) = content_start.take() {
                indent_repository_content(&mut rows, start);
            }
            match self.changes_mode {
                ChangesMode::Working => {
                    let changes = Self::changes(repository, cx);
                    let staged: Vec<Change> = changes
                        .iter()
                        .filter(|change| change.staging != StageStatus::Unstaged)
                        .cloned()
                        .collect();
                    let unstaged: Vec<Change> = changes
                        .into_iter()
                        .filter(|change| change.staging != StageStatus::Staged)
                        .collect();
                    if several {
                        rows.push(self.render_repository_header(repository, None, index == 0, cx));
                        content_start = Some(rows.len());
                        if self.collapsed_repositories.contains(&repository.entity_id()) {
                            continue;
                        }
                    }
                    if staged.is_empty() && unstaged.is_empty() {
                        rows.push(render_note("No changes"));
                        continue;
                    }
                    for (offset, (section, changes)) in
                        [(Section::Staged, staged), (Section::Unstaged, unstaged)]
                            .into_iter()
                            .enumerate()
                    {
                        if changes.is_empty() {
                            continue;
                        }
                        let key = index * 3 + offset;
                        rows.push(self.render_section_header(repository, section, &changes, key, cx));
                        self.render_files(repository, section, changes, None, key, &mut rows, cx);
                    }
                }
                ChangesMode::Branch => {
                    if !self.branch_changes.contains_key(&repository.entity_id()) {
                        self.load_branch_changes(repository, false, cx);
                    }
                    match self.branch_changes.get(&repository.entity_id()) {
                        None | Some(BranchChanges::Loading) => {
                            if several {
                                rows.push(self.render_repository_header(repository, None, index == 0, cx));
                                content_start = Some(rows.len());
                                if self.collapsed_repositories.contains(&repository.entity_id()) {
                                    continue;
                                }
                            }
                            rows.push(render_note("Comparing with the base branch…"));
                        }
                        Some(BranchChanges::Unavailable(message)) => {
                            let message = message.clone();
                            if several {
                                rows.push(self.render_repository_header(repository, None, index == 0, cx));
                                content_start = Some(rows.len());
                                if self.collapsed_repositories.contains(&repository.entity_id()) {
                                    continue;
                                }
                            }
                            rows.push(render_note(message));
                        }
                        Some(BranchChanges::Loaded { base, files, .. }) => {
                            let base = base.clone();
                            let changes: Vec<Change> = files
                                .iter()
                                .map(|(path, status)| Change {
                                    path: path.clone(),
                                    status: *status,
                                    staging: StageStatus::Unstaged,
                                })
                                .collect();
                            rows.push(self.render_repository_header(
                                repository,
                                Some(format!("vs {base}").into()),
                                index == 0,
                                cx,
                            ));
                            content_start = Some(rows.len());
                            if self.collapsed_repositories.contains(&repository.entity_id()) {
                                continue;
                            }
                            if changes.is_empty() {
                                rows.push(render_note("Nothing changed on this branch"));
                                continue;
                            }
                            let key = index * 3 + 2;
                            self.render_files(
                                repository,
                                Section::Branch,
                                changes,
                                Some(base),
                                key,
                                &mut rows,
                                cx,
                            );
                        }
                    }
                }
            }
        }
        if let Some(start) = content_start {
            indent_repository_content(&mut rows, start);
        }
        v_flex()
            .id("bench-git-changes")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pb_2()
            .children(rows)
            .into_any_element()
    }

    fn render_history(&mut self, repositories: &[Entity<Repository>], window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let several = repositories.len() > 1;
        let limit = if several {
            HISTORY_PER_REPOSITORY
        } else {
            HISTORY_ONE_REPOSITORY
        };
        let local_offset =
            time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
        let now = time::OffsetDateTime::now_utc();
        let mut rows = Vec::new();
        let mut content_start = None;
        for (index, repository) in repositories.iter().enumerate() {
            if let Some(start) = content_start.take() {
                indent_repository_content(&mut rows, start);
            }
            if several {
                rows.push(self.render_repository_header(repository, None, index == 0, cx));
                content_start = Some(rows.len());
                if self.collapsed_repositories.contains(&repository.entity_id()) {
                    continue;
                }
            }
            let remote = git_remote(repository, cx);
            let commits = repository.update(cx, |repository, cx| {
                let Some(source) = history_source(repository) else {
                    return Vec::new();
                };
                let shas: Vec<git::Oid> = repository
                    .graph_data(source, LogOrder::DateOrder, 0..limit, cx)
                    .commits
                    .iter()
                    .map(|commit| commit.sha)
                    .collect();
                shas.into_iter()
                    .map(|sha| {
                        let data: Option<Arc<CommitData>> =
                            match repository.fetch_commit_data(sha, false, cx) {
                                CommitDataState::Loaded(data) => Some(data.clone()),
                                CommitDataState::Loading(_) => None,
                            };
                        (sha, data)
                    })
                    .collect()
            });
            if commits.is_empty() {
                rows.push(render_note("No commits"));
                continue;
            }
            let repository_id = repository.entity_id();
            for (sha, data) in commits {
                let sha = sha.to_string();
                let sha_shared: SharedString = sha.clone().into();
                let short_sha: SharedString = sha.chars().take(7).collect::<String>().into();
                let (subject, author, email, when): (SharedString, SharedString, Option<SharedString>, String) =
                    match &data {
                        Some(data) => (
                            data.subject.clone(),
                            data.author_name.clone(),
                            Some(data.author_email.clone()),
                            time::OffsetDateTime::from_unix_timestamp(data.commit_timestamp)
                                .map(|at| {
                                    time_format::format_localized_timestamp(
                                        at,
                                        now,
                                        local_offset,
                                        time_format::TimestampFormat::Relative,
                                    )
                                })
                                .unwrap_or_default(),
                        ),
                        None => ("Loading…".into(), "".into(), None, String::new()),
                    };
                let avatar = CommitAvatar::new(&sha_shared, email, remote.as_ref())
                    .size(px(14.))
                    .render(window, cx);
                let is_selected = self
                    .selected_commit
                    .as_ref()
                    .is_some_and(|(id, selected)| *id == repository_id && *selected == sha);
                let dot = || {
                    Label::new("•")
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .alpha(0.5)
                        .flex_none()
                };
                let open_repository = repository.downgrade();
                let menu_repository = repository.clone();
                let workspace = self.workspace.clone();
                let click_sha = sha.clone();
                let menu_sha = sha.clone();
                rows.push(
                    v_flex()
                        .id(SharedString::from(format!("commit-{repository_id:?}-{sha}")))
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_0p5()
                        .cursor_pointer()
                        .border_1()
                        .border_color(if is_selected {
                            cx.theme().colors().border_focused
                        } else {
                            gpui::transparent_black()
                        })
                        .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                        .child(
                            Label::new(subject)
                                .single_line()
                                .truncate(),
                        )
                        .child(
                            h_flex()
                                .min_w_0()
                                .gap_1()
                                .child(avatar)
                                .child(
                                    Label::new(author)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .single_line()
                                        .truncate(),
                                )
                                .child(dot())
                                .child(
                                    Label::new(when)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .single_line()
                                        .flex_none(),
                                )
                                .child(dot())
                                .child(
                                    Label::new(short_sha)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .flex_none(),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.selected_commit = Some((repository_id, click_sha.clone()));
                            CommitView::open(
                                click_sha.clone(),
                                open_repository.clone(),
                                workspace.clone(),
                                None,
                                None,
                                window,
                                cx,
                            );
                            cx.notify();
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                this.selected_commit = Some((repository_id, menu_sha.clone()));
                                this.commit_menu(
                                    menu_repository.clone(),
                                    menu_sha.clone(),
                                    event.position,
                                    window,
                                    cx,
                                );
                            }),
                        )
                        .into_any_element(),
                );
            }
        }
        if let Some(start) = content_start {
            indent_repository_content(&mut rows, start);
        }
        v_flex()
            .id("bench-git-history")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pb_2()
            .children(rows)
            .into_any_element()
    }

    fn render_pull_requests(&mut self, repositories: &[Entity<Repository>], cx: &mut Context<Self>) -> AnyElement {
        self.load_pull_requests(repositories, cx);
        let several = repositories.len() > 1;
        let mut rows = Vec::new();
        let mut content_start = None;
        for (index, repository) in repositories.iter().enumerate() {
            if let Some(start) = content_start.take() {
                indent_repository_content(&mut rows, start);
            }
            if several {
                rows.push(self.render_repository_header(repository, None, index == 0, cx));
                content_start = Some(rows.len());
                if self.collapsed_repositories.contains(&repository.entity_id()) {
                    continue;
                }
            }
            let key = (
                work_directory(repository, cx),
                repository
                    .read(cx)
                    .branch
                    .as_ref()
                    .map(|branch| branch.name().to_owned()),
            );
            match self.pull_requests.get(&key) {
                None | Some(PullRequests::Loading) => rows.push(render_note("Loading…")),
                Some(PullRequests::Unavailable(message)) => rows.push(render_note(message.clone())),
                Some(PullRequests::Loaded(pull_requests)) if pull_requests.is_empty() => {
                    rows.push(render_note("No pull requests from this branch"))
                }
                Some(PullRequests::Loaded(pull_requests)) => {
                    for (row, pull_request) in pull_requests.iter().enumerate() {
                        let url = pull_request.url.clone();
                        rows.push(
                            ListItem::new(("pull-request", index * 100_000 + row))
                                .spacing(ListItemSpacing::Sparse)
                                .start_slot(
                                    Icon::new(IconName::PullRequest)
                                        .size(IconSize::Small)
                                        .color(pull_request_color(pull_request.state)),
                                )
                                .child(
                                    v_flex()
                                        .min_w_0()
                                        .child(
                                            Label::new(pull_request.title.clone())
                                                .single_line()
                                                .truncate(),
                                        )
                                        .child(
                                            Label::new(format!(
                                                "#{} • {} • {}",
                                                pull_request.number,
                                                pull_request.state.label(),
                                                pull_request.author
                                            ))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                        ),
                                )
                                .on_click(move |_, _, cx| cx.open_url(&url))
                                .into_any_element(),
                        );
                    }
                }
            }
        }
        if let Some(start) = content_start {
            indent_repository_content(&mut rows, start);
        }
        v_flex()
            .id("bench-git-pull-requests")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pb_2()
            .children(rows)
            .into_any_element()
    }

    fn render_commit_box(&self, repositories: &[Entity<Repository>], cx: &mut Context<Self>) -> impl IntoElement {
        let several = repositories.len() > 1;
        let busy = self.busy;
        let nothing_staged = !repositories.iter().any(|repository| {
            Self::changes(repository, cx)
                .iter()
                .any(|change| change.staging != StageStatus::Unstaged)
        });
        let label: SharedString = if let Some(busy) = busy {
            busy.into()
        } else if self.amend {
            "Amend".into()
        } else if several {
            format!("Commit to {}", repositories.len()).into()
        } else if nothing_staged {
            "Commit Tracked".into()
        } else {
            "Commit".into()
        };
        let ai_enabled = AgentSettings::get_global(cx).enabled(cx);
        v_flex()
            .p_2()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                div()
                    .p_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.commit_editor.clone()),
            )
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_0p5()
                            .when(ai_enabled, |this| {
                                this.child(
                                    IconButton::new("generate-message", IconName::AiEdit)
                                        .icon_size(IconSize::Small)
                                        .disabled(self.generating)
                                        .tooltip(Tooltip::text(if self.generating {
                                            "Writing a commit message…"
                                        } else {
                                            "Write a Commit Message"
                                        }))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.generate_message(window, cx)
                                        })),
                                )
                            })
                            .child(
                                Button::new("toggle-amend", "Amend")
                                    .label_size(LabelSize::Small)
                                    .toggle_state(self.amend)
                                    // Amending rewrites one repository's last
                                    // commit; across several it would be a
                                    // different commit in each.
                                    .disabled(several)
                                    .tooltip(Tooltip::text("Amend the Last Commit"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_amend(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("toggle-signoff", "Sign-off")
                                    .label_size(LabelSize::Small)
                                    .toggle_state(self.signoff)
                                    .tooltip(Tooltip::text("Add a Signed-off-by Line"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.signoff = !this.signoff;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        Button::new("commit", label)
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Filled)
                            .disabled(busy.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.commit(window, cx))),
                    ),
            )
    }

    fn render_footer(&self, repositories: &[Entity<Repository>], cx: &mut Context<Self>) -> impl IntoElement {
        let project = self.project.clone();
        let busy = self.busy.is_some();
        let (label, branch): (SharedString, Option<SharedString>) = match repositories {
            [one] => {
                let one = one.read(cx);
                (
                    one.display_name(),
                    one.branch
                        .as_ref()
                        .map(|branch| SharedString::from(branch.name().to_owned())),
                )
            }
            many => (format!("{} repositories", many.len()).into(), None),
        };
        let remote = |id: &'static str, icon: IconName, tooltip: &'static str, operation: RemoteOperation| {
            IconButton::new(id, icon)
                .icon_size(IconSize::Small)
                .disabled(busy)
                .tooltip(Tooltip::text(tooltip))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.run_remote(operation, window, cx);
                }))
        };
        h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .justify_between()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1()
                    .child(
                        PopoverMenu::new("bench-repository-switcher")
                            .menu(move |window, cx| {
                                let project = project.clone();
                                Some(cx.new(|cx| RepositorySelector::new(project, rems(20.), window, cx)))
                            })
                            .trigger(
                                Button::new("bench-repository-selector", label)
                                    .label_size(LabelSize::Small)
                                    .truncate(true)
                                    .start_icon(
                                        Icon::new(IconName::GitBranch).size(IconSize::Small),
                                    ),
                            )
                            .anchor(Anchor::BottomLeft),
                    )
                    .children(branch.map(|branch| {
                        Label::new(branch)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line()
                            .truncate()
                    })),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_0p5()
                    .child(remote(
                        "bench-fetch",
                        IconName::ArrowCircle,
                        "Fetch",
                        RemoteOperation::Fetch,
                    ))
                    .child(remote(
                        "bench-pull",
                        IconName::ArrowDown,
                        "Pull",
                        RemoteOperation::Pull,
                    ))
                    .child(remote(
                        "bench-push",
                        IconName::ArrowUp,
                        "Push",
                        RemoteOperation::Push,
                    )),
            )
    }

    /// The dock badge's count: the active repository's changes. The badge is
    /// drawn from `&App`, which cannot ask the selection for the others.
    fn change_count(&self, cx: &App) -> usize {
        let active = self.project.read(cx).git_store().read(cx).active_repository();
        active
            .map(|repository| repository.read(cx).status_summary().count)
            .unwrap_or(0)
    }
}

impl Render for BenchGitPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let repositories = self.selected(cx);
        self.watch(&repositories, cx);
        let body = if repositories.is_empty() {
            v_flex()
                .flex_1()
                .p_3()
                .child(
                    Label::new("No git repository in this project")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            match self.tab {
                Tab::Changes => v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_changes_toolbar(cx))
                    .child(self.render_changes(&repositories, cx))
                    .when(self.changes_mode == ChangesMode::Working, |this| {
                        this.child(self.render_commit_box(&repositories, cx))
                    })
                    .into_any_element(),
                Tab::History => self.render_history(&repositories, window, cx),
                Tab::PullRequests => self.render_pull_requests(&repositories, cx),
            }
        };
        v_flex()
            .key_context("BenchGitPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_tabs(cx))
            .child(body)
            .when(!repositories.is_empty(), |this| {
                this.child(self.render_footer(&repositories, cx))
            })
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl Focusable for BenchGitPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for BenchGitPanel {}

impl Panel for BenchGitPanel {
    fn persistent_name() -> &'static str {
        "BenchGitPanel"
    }

    fn panel_key() -> &'static str {
        "BenchGitPanel"
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        GitPanelSettings::get_global(cx).dock
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _position: DockPosition, _: &mut Window, _cx: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        GitPanelSettings::get_global(cx).default_width
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<IconName> {
        GitPanelSettings::get_global(cx)
            .button
            .then_some(IconName::GitBranch)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Git")
    }

    fn icon_label(&self, _: &Window, cx: &App) -> Option<String> {
        if !GitPanelSettings::get_global(cx).show_count_badge {
            return None;
        }
        let count = self.change_count(cx);
        (count > 0).then(|| count.to_string())
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(zed_actions::git_panel::ToggleFocus)
    }

    fn starts_open(&self, _: &Window, cx: &App) -> bool {
        GitPanelSettings::get_global(cx).starts_open
    }

    /// Zed's git panel, hidden in the same dock, holds 3; two panels of one
    /// dock with one priority is an assertion failure in debug builds.
    fn activation_priority(&self) -> u32 {
        4
    }
}

/// What committing does in one repository; see [`BenchGitPanel::commit`].
struct CommitPlan {
    repository: Entity<Repository>,
    name: SharedString,
    /// Nothing is staged, so the tracked changes are staged first, as Zed's
    /// "Commit Tracked" does.
    stage_first: Option<Vec<RepoPath>>,
    files: usize,
}

async fn commit_one(
    plan: &CommitPlan,
    message: String,
    options: CommitOptions,
    askpass: AskPassDelegate,
    cx: &mut AsyncApp,
) -> Result<()> {
    if let Some(paths) = plan.stage_first.clone() {
        plan.repository
            .update(cx, |repository, cx| repository.stage_entries(paths, cx))
            .await?;
    }
    plan.repository
        .update(cx, |repository, cx| {
            repository.commit(message.into(), None, options, askpass, cx)
        })
        .await?
}

/// Everything on a repository's branch since its base: what has been
/// committed on it and what has not, against where it left the base branch.
/// `None` when there is no base to compare with.
async fn branch_changes(
    repository: &Entity<Repository>,
    cx: &mut AsyncApp,
) -> Result<
    Option<(
        SharedString,
        Vec<(RepoPath, FileStatus)>,
        HashMap<RepoPath, git::Oid>,
    )>,
> {
    let base = repository
        .update(cx, |repository, _| repository.default_branch(true))
        .await??;
    let Some(base) = base else {
        return Ok(None);
    };
    let tree = repository
        .update(cx, |repository, cx| {
            repository.diff_tree(DiffTreeType::MergeBaseWithWorktree { base: base.clone() }, cx)
        })
        .await??;
    let base_oids = tree
        .entries
        .iter()
        .filter_map(|(path, status)| match status {
            TreeDiffStatus::Added => None,
            TreeDiffStatus::Modified { old } | TreeDiffStatus::Deleted { old } => {
                Some((path.clone(), *old))
            }
        })
        .collect();
    let mut files: BTreeMap<RepoPath, FileStatus> = tree
        .entries
        .into_iter()
        .map(|(path, status)| {
            let code = match status {
                TreeDiffStatus::Added => StatusCode::Added,
                TreeDiffStatus::Modified { .. } => StatusCode::Modified,
                TreeDiffStatus::Deleted { .. } => StatusCode::Deleted,
            };
            (
                path,
                FileStatus::Tracked(TrackedStatus {
                    index_status: code,
                    worktree_status: StatusCode::Unmodified,
                }),
            )
        })
        .collect();
    // New files git does not track yet are on the branch too, as far as a
    // review is concerned; the tree diff only knows tracked ones.
    repository.read_with(cx, |repository, _| {
        for entry in repository.cached_status() {
            if entry.status == FileStatus::Untracked {
                files.entry(entry.repo_path).or_insert(FileStatus::Untracked);
            }
        }
    });
    Ok(Some((base, files.into_iter().collect(), base_oids)))
}

/// The hosting provider of a repository's default remote, which is where
/// commit avatars come from.
fn git_remote(repository: &Entity<Repository>, cx: &App) -> Option<GitRemote> {
    let remote_url = repository.read(cx).default_remote_url()?;
    // Absent before any provider is registered, which only tests see.
    let registry = GitHostingProviderRegistry::try_global(cx)?;
    let (provider, parsed) = parse_git_remote_url(registry, &remote_url)?;
    Some(GitRemote {
        host: provider,
        owner: parsed.owner.into(),
        repo: parsed.repo.into(),
    })
}

#[derive(Clone, Copy)]
enum RemoteOperation {
    Fetch,
    Pull,
    Push,
}

impl RemoteOperation {
    fn verb(self) -> &'static str {
        match self {
            RemoteOperation::Fetch => "fetch",
            RemoteOperation::Pull => "pull",
            RemoteOperation::Push => "push",
        }
    }

    fn past(self) -> &'static str {
        match self {
            RemoteOperation::Fetch => "Fetched",
            RemoteOperation::Pull => "Pulled",
            RemoteOperation::Push => "Pushed",
        }
    }

    fn progress(self) -> &'static str {
        match self {
            RemoteOperation::Fetch => "Fetching…",
            RemoteOperation::Pull => "Pulling…",
            RemoteOperation::Push => "Pushing…",
        }
    }
}

/// One repository's fetch, pull or push, to its branch's own remote — the one
/// git would use from a terminal. The panel asks which remote when there is a
/// choice; asking once per repository would be a dialog per repository, so
/// the default is taken.
async fn run_remote_one(
    operation: RemoteOperation,
    repository: &Entity<Repository>,
    askpass: AskPassDelegate,
    cx: &mut AsyncApp,
) -> Result<()> {
    if let RemoteOperation::Fetch = operation {
        let output = repository
            .update(cx, |repository, cx| {
                repository.fetch(FetchOptions::All, askpass, cx)
            })
            .await??;
        log::info!("git fetch: {}", output.stdout);
        return Ok(());
    }

    let branch = repository
        .read_with(cx, |repository, _| repository.branch.clone())
        .ok_or_else(|| anyhow!("no branch is checked out"))?;
    let is_push = matches!(operation, RemoteOperation::Push);
    let remote = repository
        .update(cx, |repository, _| {
            repository.get_remotes(Some(branch.name().to_owned()), is_push)
        })
        .await??
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no remote"))?;

    let output = if is_push {
        let options = match &branch.upstream {
            Some(upstream) if !matches!(upstream.tracking, UpstreamTracking::Gone) => None,
            _ => Some(PushOptions::SetUpstream),
        };
        let remote_branch: SharedString = branch
            .upstream
            .as_ref()
            .filter(|upstream| matches!(upstream.tracking, UpstreamTracking::Tracked(_)))
            .and_then(|upstream| upstream.branch_name())
            .unwrap_or_else(|| branch.name())
            .to_owned()
            .into();
        repository
            .update(cx, |repository, cx| {
                repository.push(
                    branch.name().to_owned().into(),
                    remote_branch,
                    remote.name.clone(),
                    options,
                    askpass,
                    cx,
                )
            })
            .await??
    } else {
        let branch_name = branch
            .upstream
            .is_none()
            .then(|| branch.name().to_owned().into());
        repository
            .update(cx, |repository, cx| {
                repository.pull(branch_name, remote.name.clone(), false, askpass, cx)
            })
            .await??
    };
    log::info!("git {}: {}", operation.verb(), output.stdout);
    Ok(())
}

/// What the history of a repository is read from: its branch, or the commit
/// its detached head is on. `None` for a repository with no commits.
fn history_source(repository: &Repository) -> Option<LogSource> {
    let head = repository.head_commit.as_ref()?;
    match repository.branch.as_ref() {
        Some(branch) => Some(LogSource::Branch(branch.name().to_string().into())),
        None => Some(LogSource::Sha(head.sha.as_ref().parse().ok()?)),
    }
}

/// Sets a repository's rows, everything after its header, in from the header.
fn indent_repository_content(rows: &mut Vec<AnyElement>, start: usize) {
    if start >= rows.len() {
        return;
    }
    let content = rows.split_off(start);
    rows.push(v_flex().w_full().pl_4().pr_2().children(content).into_any_element());
}

fn render_note(message: impl Into<SharedString>) -> AnyElement {
    div()
        .px_3()
        .py_1()
        .child(
            Label::new(message)
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .into_any_element()
}

fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or(message)
}



#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use project::Fs as _;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;
    use settings::SettingsStore;
    use theme::LoadThemes;
    use util::path;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    /// A workspace repository with another kept inside it, as a Bench
    /// worktree with a `repos/` folder is: `outer` has a staged change,
    /// `inner` an unstaged one to a tracked file.
    async fn two_repositories(
        cx: &mut TestAppContext,
    ) -> (Arc<FakeFs>, Entity<Project>, Entity<Workspace>, VisualTestContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/root/outer"),
            json!({
                ".git": {},
                "a.txt": "new",
                "src": { "lib.rs": "new" },
                "repos": { "inner": { ".git": {}, "b.txt": "new" } },
            }),
        )
        .await;
        let outer = Path::new(path!("/root/outer/.git"));
        let inner = Path::new(path!("/root/outer/repos/inner/.git"));
        fs.set_branch_name(outer, Some("main"));
        fs.set_branch_name(inner, Some("main"));
        fs.set_head_for_repo(
            outer,
            &[("a.txt", "old".into()), ("src/lib.rs", "old".into())],
            "outer-head",
        );
        fs.set_index_for_repo(outer, &[("a.txt", "new".into()), ("src/lib.rs", "old".into())]);
        fs.set_head_and_index_for_repo(inner, &[("b.txt", "old".into())]);

        let project = Project::test(fs.clone(), [path!("/root/outer").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        // The selection is remembered, and the tests share one database, so
        // every test starts from the active repository alone.
        cx.update(|_, cx| {
            RepositorySelection::global(cx)
                .update(cx, |selection, cx| selection.select_only_active(&project, cx))
        });
        (fs, project, workspace, cx)
    }

    fn select_all(project: &Entity<Project>, cx: &mut VisualTestContext) {
        cx.update(|_, cx| {
            let selection = RepositorySelection::global(cx);
            for repository in repositories(project, cx) {
                selection.update(cx, |selection, cx| {
                    if !selection.is_selected(project, &repository, cx) {
                        selection.toggle(project, &repository, cx);
                    }
                });
            }
        });
        cx.run_until_parked();
    }

    fn head(fs: &FakeFs, dot_git: &str) -> Option<String> {
        fs.with_git_state(Path::new(dot_git), false, |state| {
            state.refs.get("HEAD").cloned()
        })
        .ok()
        .flatten()
    }

    fn panel_in_window(
        project: &Entity<Project>,
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<BenchGitPanel> {
        let panel = cx.new_window_entity(|window, cx| {
            BenchGitPanel::new(workspace.downgrade(), project.clone(), window, cx)
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
            workspace.open_panel::<BenchGitPanel>(window, cx);
        });
        cx.run_until_parked();
        panel
    }

    fn change(path: &str) -> Change {
        Change {
            path: RepoPath::new(path).expect("a repository path"),
            status: FileStatus::Untracked,
            staging: StageStatus::Unstaged,
        }
    }

    #[test]
    fn files_are_listed_flat_or_as_a_tree() {
        let changes = [
            change("src/b.rs"),
            change("a.txt"),
            change("src/deep/c.rs"),
            change("java/io/app/Main.java"),
        ];
        let folder = |path: &str, name: &str, depth: usize, collapsed: bool| FileRow::Folder {
            path: RepoPath::new(path).expect("a repository path"),
            name: SharedString::from(name.to_owned()),
            depth,
            collapsed,
        };
        assert_eq!(
            file_rows(&changes, false, &HashSet::new()),
            (0..4)
                .map(|index| FileRow::File { index, depth: 0 })
                .collect::<Vec<_>>()
        );
        assert_eq!(
            file_rows(&changes, true, &HashSet::new()),
            vec![
                folder("java/io/app", "java/io/app", 0, false),
                FileRow::File { index: 3, depth: 1 },
                folder("src", "src", 0, false),
                folder("src/deep", "deep", 1, false),
                FileRow::File { index: 2, depth: 2 },
                FileRow::File { index: 0, depth: 1 },
                FileRow::File { index: 1, depth: 0 },
            ],
            "a folder holding only one folder shares its row"
        );
        let collapsed: HashSet<RepoPath> = [RepoPath::new("src").expect("a repository path")]
            .into_iter()
            .collect();
        assert_eq!(
            file_rows(&changes, true, &collapsed),
            vec![
                folder("java/io/app", "java/io/app", 0, false),
                FileRow::File { index: 3, depth: 1 },
                folder("src", "src", 0, true),
                FileRow::File { index: 1, depth: 0 },
            ],
            "a closed folder hides what is in it"
        );
    }

    /// Every tab and every way of listing changes draws, for one repository
    /// and for two — drawing is where a lease taken twice would crash.
    #[gpui::test]
    async fn the_panel_draws_every_tab_and_view(cx: &mut TestAppContext) {
        let (_fs, project, workspace, mut cx) = two_repositories(cx).await;
        let panel = panel_in_window(&project, &workspace, &mut cx);
        for several in [false, true] {
            if several {
                select_all(&project, &mut cx);
            }
            for (tab, mode, tree) in [
                (Tab::Changes, ChangesMode::Working, false),
                (Tab::Changes, ChangesMode::Working, true),
                (Tab::Changes, ChangesMode::Branch, false),
                (Tab::Changes, ChangesMode::Branch, true),
                (Tab::History, ChangesMode::Working, false),
                (Tab::PullRequests, ChangesMode::Working, false),
            ] {
                panel.update(&mut cx, |panel, cx| {
                    panel.tab = tab;
                    panel.changes_mode = mode;
                    panel.tree = tree;
                    cx.notify();
                });
                cx.run_until_parked();
            }
        }
    }

    #[gpui::test]
    async fn tree_and_zen_are_kept_for_the_next_panel(cx: &mut TestAppContext) {
        let (_fs, project, workspace, mut cx) = two_repositories(cx).await;
        let panel = panel_in_window(&project, &workspace, &mut cx);
        panel.update(&mut cx, |panel, cx| {
            assert!(!panel.tree && !panel.zen);
            panel.tree = true;
            panel.zen = true;
            panel.store_view_options(cx);
        });
        cx.run_until_parked();

        let next_panel = cx.new_window_entity(|window, cx| {
            BenchGitPanel::new(workspace.downgrade(), project.clone(), window, cx)
        });
        next_panel.update(&mut cx, |panel, _| {
            assert!(panel.tree && panel.zen);
        });
    }

    #[gpui::test]
    async fn switching_to_the_branch_view_finishes_loading(cx: &mut TestAppContext) {
        let (_fs, project, workspace, mut cx) = two_repositories(cx).await;
        let panel = panel_in_window(&project, &workspace, &mut cx);
        select_all(&project, &mut cx);
        panel.update(&mut cx, |panel, cx| {
            panel.changes_mode = ChangesMode::Branch;
            for repository in panel.selected(cx) {
                panel.load_branch_changes(&repository, false, cx);
            }
            cx.notify();
        });
        cx.run_until_parked();
        panel.update(&mut cx, |panel, cx| {
            let repositories = panel.selected(cx);
            assert_eq!(repositories.len(), 2);
            for repository in repositories {
                assert!(
                    matches!(
                        panel.branch_changes.get(&repository.entity_id()),
                        Some(BranchChanges::Loaded { .. })
                    ),
                    "the branch view should finish loading every repository"
                );
            }
        });
    }

    #[gpui::test]
    async fn one_repository_commits_without_a_warning(cx: &mut TestAppContext) {
        let (fs, project, workspace, mut cx) = two_repositories(cx).await;
        let panel = panel_in_window(&project, &workspace, &mut cx);
        panel.update_in(&mut cx, |panel, window, cx| {
            panel
                .commit_editor
                .update(cx, |editor, cx| editor.set_text("Fix it", window, cx));
            panel.commit(window, cx);
        });
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        let active = cx.update(|_, cx| {
            project
                .read(cx)
                .git_store()
                .read(cx)
                .active_repository()
                .map(|repository| work_directory(&repository, cx))
        });
        let dot_git = active.expect("an active repository").join(".git");
        assert_eq!(
            head(&fs, &dot_git.to_string_lossy()).as_deref(),
            Some("fake-commit-1")
        );
    }

    #[gpui::test]
    async fn one_message_commits_to_every_repository_after_a_warning(cx: &mut TestAppContext) {
        let (fs, project, workspace, mut cx) = two_repositories(cx).await;
        select_all(&project, &mut cx);
        let panel = panel_in_window(&project, &workspace, &mut cx);
        panel.update_in(&mut cx, |panel, window, cx| {
            panel
                .commit_editor
                .update(cx, |editor, cx| editor.set_text("Connect locations", window, cx));
            panel.commit(window, cx);
        });
        cx.run_until_parked();

        let (title, detail) = cx.pending_prompt().expect("a warning before committing");
        assert_eq!(title, "Commit to 2 repositories?");
        assert!(detail.contains("outer: 1 file staged"), "{detail}");
        assert!(
            detail.contains("inner: 1 file, every tracked change (nothing is staged)"),
            "{detail}"
        );
        cx.simulate_prompt_answer("Commit");
        cx.run_until_parked();

        assert_eq!(head(&fs, path!("/root/outer/.git")).as_deref(), Some("fake-commit-1"));
        assert_eq!(
            head(&fs, path!("/root/outer/repos/inner/.git")).as_deref(),
            Some("fake-commit-1"),
            "the repository with nothing staged commits its tracked changes"
        );
    }

    #[gpui::test]
    async fn cancelling_the_warning_commits_nothing(cx: &mut TestAppContext) {
        let (fs, project, workspace, mut cx) = two_repositories(cx).await;
        select_all(&project, &mut cx);
        let panel = panel_in_window(&project, &workspace, &mut cx);
        panel.update_in(&mut cx, |panel, window, cx| {
            panel
                .commit_editor
                .update(cx, |editor, cx| editor.set_text("Connect locations", window, cx));
            panel.commit(window, cx);
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        assert_eq!(head(&fs, path!("/root/outer/.git")).as_deref(), Some("outer-head"));
        panel.read_with(&cx, |panel, _| assert!(panel.busy.is_none()));
    }

    /// Discarding asks first; a new file then goes to the trash. (Putting a
    /// tracked file back is `git checkout`, which the fake repository does not
    /// implement.)
    #[gpui::test]
    async fn discarding_asks_and_trashes_a_new_file(cx: &mut TestAppContext) {
        let (fs, project, workspace, mut cx) = two_repositories(cx).await;
        fs.insert_file(path!("/root/outer/repos/inner/scratch.txt"), b"x".to_vec())
            .await;
        cx.run_until_parked();
        let panel = panel_in_window(&project, &workspace, &mut cx);
        let inner = cx.update(|_, cx| {
            repositories(&project, cx)
                .into_iter()
                .find(|repository| work_directory(repository, cx).ends_with("inner"))
                .expect("the inner repository")
        });
        let change = cx.update(|_, cx| {
            BenchGitPanel::changes(&inner, cx)
                .into_iter()
                .find(|change| change.path.as_unix_str() == "scratch.txt")
                .expect("the new file")
        });
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.discard(&inner, vec![change.clone()], window, cx);
        });
        cx.run_until_parked();
        let (title, _) = cx.pending_prompt().expect("a question first");
        assert_eq!(title, "Discard the changes to scratch.txt?");
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(fs.is_file(Path::new(path!("/root/outer/repos/inner/scratch.txt"))).await);

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.discard(&inner, vec![change], window, cx);
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("Discard");
        cx.run_until_parked();
        assert!(!fs.is_file(Path::new(path!("/root/outer/repos/inner/scratch.txt"))).await);
    }

    /// The branch view lists what changed since the base, committed or not,
    /// and new files git does not track yet.
    #[gpui::test]
    async fn the_branch_view_lists_everything_since_the_base(cx: &mut TestAppContext) {
        let (fs, project, workspace, mut cx) = two_repositories(cx).await;
        fs.insert_file(path!("/root/outer/new.txt"), b"new".to_vec()).await;
        cx.run_until_parked();
        let outer = cx.update(|_, cx| {
            repositories(&project, cx)
                .into_iter()
                .find(|repository| work_directory(repository, cx).ends_with("outer"))
                .expect("the outer repository")
        });
        let found = cx
            .spawn(|mut cx| async move { branch_changes(&outer, &mut cx).await })
            .await;
        let (base, files, _) = found.expect("the branch diff").expect("a base branch");
        assert_eq!(base.as_ref(), "origin/main");
        let listed = |name: &str| files.iter().any(|(path, _)| path.as_unix_str() == name);
        assert!(listed("a.txt"), "a change on the branch: {files:?}");
        assert!(
            files
                .iter()
                .any(|(path, status)| path.as_unix_str() == "new.txt"
                    && *status == FileStatus::Untracked),
            "a new file git does not track yet: {files:?}"
        );
        drop(workspace);
    }
}
