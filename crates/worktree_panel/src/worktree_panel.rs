//! The worktree panel: every worktree of every open repository as a row.
//!
//! Bench's window already holds one [`Workspace`] per open worktree — that is
//! [`MultiWorkspace`], and it is Zed's, not ours. What Zed does not do is show
//! them: its sidebar lists *projects*, and a project's other worktrees live
//! behind an overflow menu, one checkmark deep. This panel is the same data
//! drawn as a tree, so switching between four worktrees of one repository is a
//! click rather than a menu.
//!
//! The rows are not limited to what the window has open, in either direction.
//! Which projects and worktrees exist is the question the panel answers, so an
//! unopened one is a row too — dimmed, and clicking it opens it. The projects
//! are the window's own project groups, which outlive the workspaces that were
//! opened for them and survive a restart. A project's worktrees come off its
//! repository snapshot while it has a workspace open, and off git itself while
//! it does not; see [`WorktreePanel::discover`].
//!
//! It is a dock [`Panel`] rather than a bespoke sidebar, which is what keeps it
//! cheap to carry: position, resizing, width persistence, focus and
//! serialization are all Zed's, and this crate touches `workspace` not at all.
//!
//! The tree is derived on every render rather than cached. `MultiWorkspace` is
//! the only owner of the workspace and project list, and a second copy of it
//! here would be a second thing to keep honest.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use git::repository::{CreateWorktreeTarget, Worktree as GitWorktree};
use agent_tracker::{AgentState, AgentSummary, AgentTracker, AgentsChanged, agent_state_color};
use git_ui_core::pull_request_color::pull_request_color;
use github_cli::{self, PullRequest, PullRequestState};
use linear::{Issue, Linear, LinearEvent};
use worktree_metadata::{
    HUE_NAMES, HUES, LinkedIssue, MetadataChanged, WorktreeMetadataStore, hue_color,
};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AsyncWindowContext, ClipboardItem, Context,
    DismissEvent,
    Entity,
    EventEmitter, FocusHandle, Focusable, Global, Task, Transformation, WeakEntity, Window,
    actions, percentage, prelude::*, pulsating_between, svg,
};
use project::{
    Fs, ProjectGroupKey, discover_root_repo_common_dir, git_store::Repository,
    git_store::linked_worktree_short_name, repo_identity_path_if_local,
};
use ui::{
    CommonAnimationExt as _, ContextMenu, Label, ListItem, ListItemSpacing, Tooltip,
    prelude::*,
};
use ui_input::{ErasedEditor, InputField};
use settings::Settings as _;
use util::path_list::PathList;
use util::paths::home_dir;
use util::ResultExt as _;
use workspace::{
    ModalView, MultiWorkspace, MultiWorkspaceEvent, OpenMode, RemovalIntent, Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelSizeState},
};

actions!(
    worktree_panel,
    [
        /// Opens the worktree panel, or moves focus into it if it is already
        /// open.
        ToggleFocus,
        /// Opens or closes the worktree panel.
        Toggle,
    ]
);

/// Which linked worktrees the panel lists. See the `worktree_panel` section of
/// the settings file.
#[derive(Clone, Debug, PartialEq, settings::RegisterSetting)]
pub struct WorktreePanelSettings {
    pub include: Vec<PathBuf>,
    pub exclude: Vec<PathBuf>,
}

impl WorktreePanelSettings {
    /// Whether a linked worktree at `path` is one the settings allow.
    ///
    /// Other tools keep worktrees of the same repositories in directories of
    /// their own, and every one of them is in `git worktree list`. These lists
    /// are how a user keeps those out of a panel that is about Bench's.
    fn allows(&self, path: &Path) -> bool {
        let included =
            self.include.is_empty() || self.include.iter().any(|root| path.starts_with(root));
        included && !self.exclude.iter().any(|root| path.starts_with(root))
    }
}

impl settings::Settings for WorktreePanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let panel = content.worktree_panel.clone().unwrap_or_default();
        Self {
            include: expand_paths(panel.include.unwrap_or_default()),
            exclude: expand_paths(panel.exclude.unwrap_or_default()),
        }
    }
}

fn expand_paths(paths: Vec<String>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(|path| match path.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                home_dir().join(rest.trim_start_matches('/'))
            }
            _ => PathBuf::from(path),
        })
        .collect()
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<WorktreePanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<WorktreePanel>(window, cx) {
                workspace.close_panel::<WorktreePanel>(window, cx);
            }
        });
        // Deferred because working out which projects there are to create
        // in reads every workspace of the window, this one included, and an
        // action handler holds this one's lease.
        workspace.register_action(|workspace, action: &linear::CreateWorktree, window, cx| {
            let Some(panel) = workspace.panel::<WorktreePanel>(cx) else {
                return;
            };
            let identifier = SharedString::from(action.identifier.clone());
            let start_agent = action.start_agent;
            window.defer(cx, move |window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.add_worktree_for_issue(identifier, start_agent, window, cx);
                });
            });
        });
    })
    .detach();
}

/// One repository and every worktree of it, open or not.
struct RepositoryRow {
    key: ProjectGroupKey,
    name: SharedString,
    /// Whether the window has a workspace open for any of its worktrees. A
    /// project that is only a row still belongs in the panel — it is one the
    /// user added — but it is not one of the window's own.
    is_open: bool,
    /// The repository new worktrees are made in, which only an open project
    /// has: making one is something the repository does, and a project that is
    /// just a row has no repository entity behind it.
    repository: Option<Entity<Repository>>,
    worktrees: Vec<WorktreeRow>,
}

/// One worktree of a repository.
struct WorktreeRow {
    /// The project group the worktree belongs to, which is what opens it when
    /// its project has no workspace to switch from.
    key: ProjectGroupKey,
    /// The [`Workspace`] showing this worktree, when the window has it open.
    /// `None` is a worktree that exists in the repository but is not open —
    /// clicking it opens it.
    workspace: Option<Entity<Workspace>>,
    /// The linked worktree's own directory name, or `main` for the repository's
    /// original checkout.
    name: SharedString,
    /// This worktree's root, absent for a workspace holding no folder.
    root: Option<PathBuf>,
    /// A workspace already showing this repository, which is what opening an
    /// unopened worktree switches from. Every group has one: the groups are
    /// built out of the window's open workspaces.
    switch_from: Option<Entity<Workspace>>,
    /// The repository this worktree belongs to, which is what deletes it.
    /// Absent for a row the repository's own list did not account for.
    repository: Option<Entity<Repository>>,
    /// The repository's own checkout cannot be deleted — git refuses, and it
    /// is the thing the others are linked to — so it gets no delete button.
    is_main: bool,
    is_active: bool,
    /// What became of the pull request opened from this worktree's branch,
    /// when there is one; see [`WorktreePanel::scan_pull_requests`].
    pull_request: Option<PullRequestState>,
    /// The agents running in this worktree's terminals, counted by what they
    /// are doing.
    agents: AgentSummary,
    /// The branch this worktree has checked out; see [`RowPlan::branch`].
    branch: Option<SharedString>,
    /// The issue Bench made this worktree for, as it stored it. It outlives
    /// the issue's identifier, which the branch name only has a copy of.
    linked_issue: Option<LinkedIssue>,
    /// The worktree's Linear issue, once Linear has said which that is: the
    /// one it was made for, or else the one its branch is named after. See
    /// [`Linear::look_up_ids`] and [`Linear::look_up_branches`].
    issue: Option<Arc<Issue>>,
    /// The title stored for the worktree, which its card leads with; see
    /// [`worktree_metadata::WorktreeMetadata::title`].
    title: Option<SharedString>,
    /// What git says about the worktree's files and branch; see
    /// [`WorktreePanel::scan_statuses`]. Default until it has said.
    status: WorktreeStatus,
}

/// What is known about one project's pull requests; see
/// [`WorktreePanel::scan_pull_requests`].
enum PullRequestScan {
    /// The first scan of this project is running. Nothing is drawn from it
    /// until it lands, which is why a refresh keeps the old answer instead of
    /// going back through here.
    Pending { _scan: Task<()> },
    Found {
        /// The state of each branch's pull request, by branch name. A branch
        /// with no pull request is absent rather than present and empty.
        by_branch: HashMap<SharedString, PullRequestState>,
        /// When this was asked, which is what [`PULL_REQUEST_REFRESH`] is
        /// measured from.
        at: Instant,
        /// A refresh in flight. The answer already found stays on screen until
        /// it lands, because a colour that blinks off every five minutes would
        /// be worse than one that is briefly five minutes old.
        _refresh: Option<Task<()>>,
    },
}

/// How long a project's pull request states are trusted before they are asked
/// for again.
///
/// A pull request merges while Bench is open, and the icon that said "open"
/// should not keep saying it for the rest of the session. Long enough that the
/// panel is not a source of `gh` processes, short enough that a merge shows up
/// while you still remember making it.
const PULL_REQUEST_REFRESH: Duration = Duration::from_secs(300);

/// A [`Workspace`] in the window, with the worktree root it is showing.
struct OpenWorktree {
    workspace: Entity<Workspace>,
    root: Option<PathBuf>,
}

/// What the panel knows about the worktrees of a project the window has no
/// workspace open for; see [`WorktreePanel::discover`].
enum Discovery {
    /// The scan is running.
    Pending { _scan: Task<()> },
    /// What git reported. Empty means the project is not a git repository, or
    /// that git could not be asked.
    Found {
        worktrees: Vec<GitWorktree>,
        at: Instant,
    },
}

/// How long git's answer about a project's worktrees is trusted. Worktrees are
/// made and removed outside Bench as well as in it, and a project that is only
/// a row has no repository entity to watch.
const DISCOVERY_REFRESH: Duration = Duration::from_secs(60);

/// What has been scanned about each project, for the whole application.
///
/// A panel belongs to one worktree — there is a workspace each — but a
/// project's worktrees and pull requests are facts about the project, not
/// about the window you are looking through. Holding them per panel meant two
/// things, both bad: a panel drawn for the first time had nothing to show and
/// filled in a moment later, which is the flicker you see when clicking
/// through worktrees, and every panel ran its own `git worktree list` and
/// `gh pr list` over the same projects.
/// What the panel shows that is not derived from the window: which projects
/// are collapsed, and which chevron is mid-turn.
///
/// Shared for the same reason the scans are. There is a panel per worktree,
/// and "is this project collapsed" is a fact about the project — collapsing it
/// in one worktree and finding it open in the next is the panel disagreeing
/// with itself.
#[derive(Default)]
struct PanelView {
    collapsed: Vec<ProjectGroupKey>,
    /// The project whose chevron is turning, and when it started.
    ///
    /// The animation has to play on a click and *only* on a click. A panel is
    /// a fresh element tree every time its worktree comes forward, and
    /// `with_animation` replays whenever its element is mounted, so without
    /// this every chevron in the panel spins on every switch.
    turning: Option<(ProjectGroupKey, Instant)>,
}

impl Global for PanelView {}

#[derive(Default)]
struct ProjectScans {
    discovered: HashMap<PathBuf, Discovery>,
    pull_requests: HashMap<PathBuf, PullRequestScan>,
    /// By worktree root rather than by project: each worktree has files of
    /// its own.
    statuses: HashMap<PathBuf, StatusScan>,
    /// The worktrees being pushed or pulled, by root, so that a second click
    /// while one runs does nothing and every panel shows it running.
    syncing: HashSet<PathBuf>,
    /// When each repository's own checkout was last fetched; see
    /// [`FETCH_EVERY`].
    fetched: HashMap<PathBuf, Instant>,
}

/// What is known about one worktree's status; see
/// [`WorktreePanel::scan_statuses`].
enum StatusScan {
    Pending { _scan: Task<()> },
    Found {
        status: WorktreeStatus,
        checked: Instant,
        /// A refresh in flight, while the answer already found stays on
        /// screen.
        _refresh: Option<Task<()>>,
    },
}

/// What a worktree's card says about its files and branch, from one
/// `git status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WorktreeStatus {
    /// When a file last changed; see [`worktree_status`].
    last_edit: Option<SystemTime>,
    /// How many entries `git status` lists: changed files, and untracked
    /// files or directories.
    changes: usize,
    /// How far the branch is ahead of and behind its upstream, as of the last
    /// fetch. `None` for a branch with no upstream, or no branch at all.
    ahead_behind: Option<(u32, u32)>,
}

/// How long a worktree's status is trusted before git is asked again. Short,
/// because an agent at work edits all the time and "edited an hour ago" on a
/// worktree it is busy in reads as wrong; each ask is one `git status`.
const STATUS_REFRESH: Duration = Duration::from_secs(30);

/// How often a repository's own checkout is fetched, so that its card knows
/// what there is to pull. Git only knows the upstream as of the last fetch,
/// and nothing else in Bench fetches, so without this the sync button would
/// never offer a teammate's commits. Fetching updates the remote branches every
/// worktree of the repository shares, so the main checkout's is enough.
const FETCH_EVERY: Duration = Duration::from_secs(300);

impl Global for ProjectScans {}

/// Puts a scan's result where every panel can see it, and asks the windows to
/// draw: the panel that started a scan is not necessarily the one on screen
/// when it lands.
fn record_scan(cx: &mut App, record: impl FnOnce(&mut ProjectScans)) {
    record(cx.default_global::<ProjectScans>());
    cx.refresh_windows();
}

pub struct WorktreePanel {
    /// The workspace this panel belongs to — one of the window's many, and the
    /// one whose dock it sits in. Held to tell it apart from its siblings in
    /// [`Self::showing_in_another_worktree`], not to read.
    workspace: WeakEntity<Workspace>,
    multi_workspace: WeakEntity<MultiWorkspace>,
    focus_handle: FocusHandle,
    /// The filter box at the top of the panel; see [`Self::filter`].
    ///
    /// The editor itself rather than an `InputField`, which draws a filled,
    /// bordered box: the panel's header is chrome, and a box with a border
    /// around it there competes with the rows for attention.
    filter: Arc<dyn ErasedEditor>,
    /// The menu of a worktree row, while it is open.
    context_menu: Option<(Entity<ContextMenu>, gpui::Point<gpui::Pixels>, gpui::Subscription)>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl WorktreePanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            let handle = workspace.clone();
            workspace.update_in(cx, |workspace, window, cx| {
                let multi_workspace = workspace
                    .multi_workspace()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("the worktree panel needs a multi workspace"))?;
                anyhow::Ok(cx.new(|cx| Self::new(handle, multi_workspace, window, cx)))
            })?
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        multi_workspace: WeakEntity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();
        // The panel has no state of its own to keep in step — it re-derives the
        // tree on every render — so all it needs is to be told to render again.
        //
        // It must be these four events and not `cx.observe`, because a panel's
        // notify is not a leaf: the dock observes its panels, the sidebar
        // observes every dock, and `MultiWorkspace` observes the sidebar. An
        // observer here closes that ring, and every notify then goes round it
        // forever — the window never finishes flushing effects and never
        // appears. Subscribing leaves the ring open: these events fire when
        // the workspace list actually changes, not when anything in it
        // redraws.
        if let Some(multi_workspace) = multi_workspace.upgrade() {
            subscriptions.push(cx.subscribe(
                &multi_workspace,
                |_, _, event: &MultiWorkspaceEvent, cx| match event {
                    MultiWorkspaceEvent::ActiveWorkspaceChanged { .. }
                    | MultiWorkspaceEvent::WorkspaceAdded(_)
                    | MultiWorkspaceEvent::WorkspaceRemoved(_)
                    | MultiWorkspaceEvent::ProjectGroupsChanged => cx.notify(),
                },
            ));
        }
        // The tracker sweeps on its own clock and says so only when something
        // actually moved, which is why this is a subscription rather than an
        // observer: a redraw per second, forever, is not what a panel that is
        // usually idle should cost.
        if let Some(tracker) = AgentTracker::try_global(cx) {
            subscriptions.push(cx.subscribe(&tracker, |_, _, _: &AgentsChanged, cx| cx.notify()));
        }
        // A subscription for the same reason: Linear says when an issue a row
        // shows has been looked up or has moved.
        if let Some(linear) = Linear::global(cx) {
            subscriptions.push(cx.subscribe(&linear, |_, _, _: &LinearEvent, cx| cx.notify()));
        }
        let metadata = WorktreeMetadataStore::global(cx);
        subscriptions.push(cx.subscribe(&metadata, |_, _, _: &MetadataChanged, cx| cx.notify()));
        // Only when this panel's own settings changed: the settings store
        // changes on every edit of the settings file, and redrawing for all of
        // them is the per-change cost the subscriptions above avoid.
        let mut settings = WorktreePanelSettings::get_global(cx).clone();
        subscriptions.push(cx.observe_global::<settings::SettingsStore>(move |_, cx| {
            let current = WorktreePanelSettings::get_global(cx);
            if *current != settings {
                settings = current.clone();
                cx.notify();
            }
        }));

        let filter = (ui_input::ERASED_EDITOR_FACTORY
            .get()
            .expect("the erased editor factory, which `editor::init` sets"))(
            window, cx
        );
        filter.set_placeholder_text("Filter", window, cx);
        // Every keystroke changes which rows there are, and the rows are
        // derived on each draw, so there is nothing to keep in step — only to
        // redraw.
        subscriptions.push(filter.subscribe(
            Box::new({
                let panel = cx.entity().downgrade();
                move |event, _window, cx| {
                    if event == ui_input::ErasedEditorEvent::BufferEdited {
                        panel.update(cx, |_, cx| cx.notify()).ok();
                    }
                }
            }),
            window,
            cx,
        ));

        Self {
            workspace,
            multi_workspace,
            focus_handle: cx.focus_handle(),
            filter,
            context_menu: None,
            _subscriptions: subscriptions,
        }
    }

    /// The tree: every project the window holds, and every worktree of each.
    ///
    /// Grouping is by [`ProjectGroupKey`], which is exactly the identity
    /// `MultiWorkspace` itself groups by — every linked worktree of a
    /// repository shares one, so this is the repository, not the checkout.
    ///
    /// The projects are the window's project groups, not its open workspaces:
    /// a group outlives the workspaces opened for it and is what a restart
    /// brings back, so a project the user added is a row until the user removes
    /// it. The window's workspaces are folded in as well, because the workspace
    /// being displayed is not always pinned to a group yet — but only the ones
    /// that have a folder in them.
    ///
    /// A group's rows are every worktree the repository has, not only the ones
    /// this window has open: which worktrees exist is the question the panel is
    /// there to answer, and a worktree you cannot see is one you cannot switch
    /// to.
    fn tree(&self, cx: &App) -> Vec<RepositoryRow> {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return Vec::new();
        };
        let multi_workspace = multi_workspace.read(cx);
        let active = multi_workspace.workspace().clone();

        let mut groups: Vec<(ProjectGroupKey, Vec<OpenWorktree>)> = Vec::new();
        for workspace in multi_workspace.workspaces() {
            let key = workspace.read(cx).project_group_key(cx);
            // A workspace holding no folder is not a project. The window has
            // one of those before anything is opened, and the panel lists
            // projects the user added — `MultiWorkspace` draws the same line,
            // refusing to make a project group for a key with no paths.
            if key.path_list().is_empty() {
                continue;
            }
            let open = OpenWorktree {
                root: workspace_root(workspace, cx),
                workspace: workspace.clone(),
            };
            match groups.iter_mut().find(|(held, _)| held.matches(&key)) {
                Some((_, open_worktrees)) => open_worktrees.push(open),
                None => groups.push((key, vec![open])),
            }
        }
        for key in multi_workspace.project_group_keys() {
            if !groups.iter().any(|(held, _)| held.matches(&key)) {
                groups.push((key, Vec::new()));
            }
        }

        let mut rows: Vec<RepositoryRow> = groups
            .into_iter()
            .map(|(key, open_worktrees)| RepositoryRow {
                name: key.display_name(&HashMap::new()),
                is_open: !open_worktrees.is_empty(),
                repository: group_repository(&key, &open_worktrees, cx),
                worktrees: self.worktree_rows(&key, &open_worktrees, &active, cx),
                key,
            })
            .collect();

        hide_worktrees_inside_other_checkouts(&mut rows);

        // Ordered by name so a row does not move when an unrelated worktree is
        // opened or closed.
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// Every worktree of one project group, open or not.
    ///
    /// An open project's worktrees come off its repository snapshot, so that
    /// half stays a derivation with no git I/O and nothing cached. A project
    /// with nothing open has no repository entity to ask, so its worktrees are
    /// the ones [`Self::discover`] found; until that lands there is nothing
    /// honest to draw, and the project is a row on its own.
    fn worktree_rows(
        &self,
        key: &ProjectGroupKey,
        open_worktrees: &[OpenWorktree],
        active: &Entity<Workspace>,
        cx: &App,
    ) -> Vec<WorktreeRow> {
        let linked = linked_worktrees(open_worktrees, cx);
        let mut worktrees: Vec<GitWorktree> = linked
            .iter()
            .map(|(worktree, _)| worktree.clone())
            .collect();
        let mut anchor = linked
            .first()
            .and_then(|(_, repository)| repository_anchor(repository, cx));

        if open_worktrees.is_empty() {
            let root = project_root(key);
            let discovered = root
                .as_ref()
                .zip(cx.try_global::<ProjectScans>())
                .and_then(|(root, scans)| scans.discovered.get(root));
            match discovered {
                Some(Discovery::Found {
                    worktrees: found, ..
                }) => worktrees = found.clone(),
                Some(Discovery::Pending { .. }) | None => return Vec::new(),
            }
            anchor = root;
        }

        let open_roots: Vec<Option<PathBuf>> = open_worktrees
            .iter()
            .map(|open| open.root.clone())
            .collect();
        let settings = WorktreePanelSettings::get_global(cx);
        // What the window has open stays listed whatever the settings say, and
        // so does the repository's own checkout, which is what a project is.
        worktrees.retain(|worktree| {
            worktree.is_main
                || Some(worktree.path.as_path()) == anchor.as_deref()
                || open_roots
                    .iter()
                    .any(|root| root.as_deref() == Some(worktree.path.as_path()))
                || settings.allows(&worktree.path)
        });
        let switch_from = open_worktrees.first().map(|open| open.workspace.clone());
        let pull_requests = self.scanned_pull_requests(key, cx);
        let tracker = AgentTracker::try_global(cx);
        let linear = Linear::global(cx);
        let metadata = WorktreeMetadataStore::try_global(cx);

        plan_rows(&worktrees, &open_roots, anchor.as_deref())
            .into_iter()
            .filter_map(|plan| {
                let open = match plan.open {
                    Some(index) => Some(open_worktrees.get(index)?),
                    None => None,
                };
                let stored = plan
                    .root
                    .as_deref()
                    .zip(metadata.as_ref())
                    .map(|(root, metadata)| metadata.read(cx).get(root, cx));
                Some(WorktreeRow {
                    key: key.clone(),
                    switch_from: switch_from.clone(),
                    repository: plan.root.as_ref().and_then(|root| {
                        linked
                            .iter()
                            .find(|(worktree, _)| &worktree.path == root)
                            .map(|(_, repository)| repository.clone())
                    }),
                    is_main: plan.is_main,
                    name: match (&plan.name, open) {
                        // A workspace the worktree list did not account for is
                        // named from the project instead; see `plan_rows`.
                        (None, Some(open)) => self.unlisted_workspace_name(&open.workspace, cx),
                        // Neither open nor in a worktree list: a project git
                        // knows nothing about, which its own directory names.
                        (None, None) => plan
                            .root
                            .as_deref()
                            .and_then(directory_name)
                            .unwrap_or_else(|| "main".into()),
                        (Some(name), _) => name.clone(),
                    },
                    is_active: open.is_some_and(|open| &open.workspace == active),
                    workspace: open.map(|open| open.workspace.clone()),
                    pull_request: plan.branch.as_ref().and_then(|branch| {
                        pull_requests?.get(branch).copied()
                    }),
                    agents: plan
                        .root
                        .as_deref()
                        .zip(tracker.as_ref())
                        .map(|(root, tracker)| tracker.read(cx).summary_for(root))
                        .unwrap_or_default(),
                    linked_issue: stored.as_ref().and_then(|stored| stored.issue.clone()),
                    title: stored
                        .as_ref()
                        .and_then(|stored| stored.shown_title())
                        .map(|title| SharedString::from(title.to_string())),
                    status: plan
                        .root
                        .as_ref()
                        .and_then(|root| {
                            match cx.try_global::<ProjectScans>()?.statuses.get(root)? {
                                StatusScan::Found { status, .. } => Some(*status),
                                StatusScan::Pending { .. } => None,
                            }
                        })
                        .unwrap_or_default(),
                    issue: None,
                    branch: plan.branch,
                    root: plan.root,
                })
            })
            .map(|mut row| {
                let linear = linear.as_ref().map(|linear| linear.read(cx));
                row.issue = linear.and_then(|linear| {
                    row.linked_issue
                        .as_ref()
                        .and_then(|linked| linear.issue_for_id(&linked.id))
                        .or_else(|| {
                            row.branch
                                .as_ref()
                                .and_then(|branch| linear.issue_for_branch(branch))
                        })
                });
                row
            })
            .collect()
    }

    /// The root of every project the window holds no workspace for.
    fn closed_project_roots(&self, cx: &App) -> Vec<PathBuf> {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return Vec::new();
        };
        let multi_workspace = multi_workspace.read(cx);
        let open: Vec<ProjectGroupKey> = multi_workspace
            .workspaces()
            .map(|workspace| workspace.read(cx).project_group_key(cx))
            .collect();

        multi_workspace
            .project_group_keys()
            .into_iter()
            .filter(|key| !open.iter().any(|held| held.matches(key)))
            .filter_map(|key| project_root(&key))
            .collect()
    }

    /// Asks git for the worktrees of the projects nothing is open for.
    ///
    /// An open project's worktrees are already in the window, on the repository
    /// snapshot. A project that is only a row — added in an earlier session and
    /// brought back by one, or added here without being opened — has no
    /// repository entity behind it, and the panel exists to say which worktrees
    /// it has. So it asks git: once per project, and again if the project is
    /// closed after having been open.
    ///
    /// Started from `render`, which is the one place that knows the panel is
    /// being looked at. It is cheap to call repeatedly: a project already
    /// scanned or being scanned is skipped, so each one costs a single
    /// `git worktree list`.
    fn discover(&mut self, roots: &[PathBuf], cx: &mut Context<Self>) {
        let Some(fs) = self.fs(cx) else {
            return;
        };
        for root in roots {
            match cx.default_global::<ProjectScans>().discovered.get(root) {
                Some(Discovery::Pending { .. }) => continue,
                Some(Discovery::Found { at, .. }) if at.elapsed() < DISCOVERY_REFRESH => continue,
                _ => {}
            }

            let scan = cx.spawn({
                let fs = fs.clone();
                let root = root.clone();
                async move |_, cx| {
                    let found = cx
                        .background_spawn(worktrees_on_disk(fs, root.clone()))
                        .await;
                    cx.update(|cx| {
                        record_scan(cx, |scans| {
                            scans.discovered.insert(
                                root,
                                Discovery::Found {
                                    worktrees: found,
                                    at: Instant::now(),
                                },
                            );
                        });
                    });
                }
            });
            cx.default_global::<ProjectScans>()
                .discovered
                .insert(root.clone(), Discovery::Pending { _scan: scan });
        }
    }

    /// Forgets what git said about a project, so that the next draw asks
    /// again. Used when Bench itself has just changed a project's worktrees.
    fn rediscover(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(root) = root else {
            return;
        };
        cx.default_global::<ProjectScans>().discovered.remove(&root);
        cx.notify();
    }

    fn scan_pull_requests(&mut self, roots: &[PathBuf], cx: &mut Context<Self>) {
        for root in roots {
            match cx.default_global::<ProjectScans>().pull_requests.get(root) {
                Some(PullRequestScan::Pending { .. }) => continue,
                Some(PullRequestScan::Found { at, _refresh, .. })
                    if _refresh.is_some() || at.elapsed() < PULL_REQUEST_REFRESH =>
                {
                    continue;
                }
                _ => {}
            }

            let scan = cx.spawn({
                let root = root.clone();
                async move |_, cx| {
                    let found = cx
                        .background_spawn(pull_requests_by_branch(root.clone()))
                        .await;
                    cx.update(|cx| {
                        record_scan(cx, |scans| {
                            scans.pull_requests.insert(
                                root,
                                PullRequestScan::Found {
                                    by_branch: found,
                                    at: Instant::now(),
                                    _refresh: None,
                                },
                            );
                        });
                    });
                }
            });

            let scans = cx.default_global::<ProjectScans>();
            match scans.pull_requests.get_mut(root) {
                Some(PullRequestScan::Found { _refresh, .. }) => *_refresh = Some(scan),
                _ => {
                    scans
                        .pull_requests
                        .insert(root.clone(), PullRequestScan::Pending { _scan: scan });
                }
            }
        }
    }

    /// What is known about the pull requests of one project's branches.
    fn scanned_pull_requests<'a>(
        &self,
        key: &ProjectGroupKey,
        cx: &'a App,
    ) -> Option<&'a HashMap<SharedString, PullRequestState>> {
        match cx
            .try_global::<ProjectScans>()?
            .pull_requests
            .get(&project_root(key)?)
        {
            Some(PullRequestScan::Found { by_branch, .. }) => Some(by_branch),
            Some(PullRequestScan::Pending { .. }) | None => None,
        }
    }

    /// Asks git about each worktree's files and branch, for its card: when a
    /// file last changed, how many have, and how the branch stands against
    /// its upstream. Started from `render`, like the other scans, and as cheap
    /// to call on every draw: a worktree asked about recently is skipped.
    ///
    /// A root marked to fetch is the repository's own checkout, fetched first
    /// when [`FETCH_EVERY`] has passed.
    fn scan_statuses(&mut self, roots: &[(PathBuf, bool)], cx: &mut Context<Self>) {
        let Some(fs) = self.fs(cx) else {
            return;
        };
        for (root, may_fetch) in roots {
            match cx.default_global::<ProjectScans>().statuses.get(root) {
                Some(StatusScan::Pending { .. }) => continue,
                Some(StatusScan::Found {
                    checked, _refresh, ..
                }) if _refresh.is_some() || checked.elapsed() < STATUS_REFRESH => continue,
                _ => {}
            }

            let scans = cx.default_global::<ProjectScans>();
            let fetch = *may_fetch
                && scans
                    .fetched
                    .get(root)
                    .is_none_or(|at| at.elapsed() >= FETCH_EVERY);
            if fetch {
                // Before the fetch rather than after, so that one that fails
                // waits its turn like one that worked instead of being retried
                // on every draw.
                scans.fetched.insert(root.clone(), Instant::now());
            }

            let scan = cx.spawn({
                let fs = fs.clone();
                let root = root.clone();
                async move |_, cx| {
                    let status = cx
                        .background_spawn({
                            let root = root.clone();
                            async move {
                                if fetch {
                                    fetch_upstream(&root).await.log_err();
                                }
                                worktree_status(fs, root).await
                            }
                        })
                        .await;
                    cx.update(|cx| {
                        record_scan(cx, |scans| {
                            scans.statuses.insert(
                                root,
                                StatusScan::Found {
                                    status,
                                    checked: Instant::now(),
                                    _refresh: None,
                                },
                            );
                        });
                    });
                }
            });

            let scans = cx.default_global::<ProjectScans>();
            match scans.statuses.get_mut(root) {
                Some(StatusScan::Found { _refresh, .. }) => *_refresh = Some(scan),
                _ => {
                    scans
                        .statuses
                        .insert(root.clone(), StatusScan::Pending { _scan: scan });
                }
            }
        }
    }

    fn fs(&self, cx: &App) -> Option<Arc<dyn Fs>> {
        let multi_workspace = self.multi_workspace.upgrade()?;
        let workspace = multi_workspace.read(cx).workspace().read(cx);
        Some(workspace.project().read(cx).fs().clone())
    }

    /// The name for a workspace the repository's worktree list does not
    /// account for; see [`Self::worktree_rows`].
    fn unlisted_workspace_name(&self, workspace: &Entity<Workspace>, cx: &App) -> SharedString {
        let project = workspace.read(cx).project().read(cx);
        // `ordered_pairs` yields (the repository's own checkout, this
        // worktree's directory), which is exactly what the short-name helper
        // compares. It returns `None` for the main worktree — the case we want
        // to name rather than leave blank.
        project
            .worktree_paths(cx)
            .ordered_pairs()
            .next()
            .and_then(|(main, own)| linked_worktree_short_name(main, own))
            .unwrap_or_else(|| "main".into())
    }

    /// Makes a worktree's workspace the one the window is showing.
    ///
    /// Deferred onto the window, and deliberately not through anything that
    /// hands out `&mut Self`. Activating a workspace ends by placing focus,
    /// and to find a focus handle `Workspace::fallback_focus_handle` reads
    /// every panel in every dock — this panel among them. Any lease on it
    /// still being held at that point is a double lease, which is a panic, and
    /// the lease is exactly what `cx.listener` and `cx.defer_in` give you. So
    /// the work goes to `window.defer`, whose callback takes no entity, and it
    /// runs once this panel's lease has been dropped.
    fn activate(
        &mut self,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multi_workspace = self.multi_workspace.clone();
        window.defer(cx, move |window, cx| {
            multi_workspace
                .update(cx, |multi_workspace, cx| {
                    multi_workspace.activate(workspace, None, window, cx);
                })
                .ok();
        });
    }

    /// Opens a worktree the window does not have open yet, under the project
    /// it belongs to.
    ///
    /// Opening one takes a moment — its project is created, its saved layout
    /// read back, its terminals reattached — and for all of that the window
    /// would otherwise go on showing the worktree you were in. That reads as a
    /// click that did nothing, or worse, as having arrived: the terminal you
    /// type into next is still the old worktree's. So the window says what it
    /// is doing straight away, with a dialog over the worktree you are leaving
    /// that takes the keyboard until the new one is showing.
    ///
    /// It opens the worktree directly rather than through another worktree of
    /// the same repository, which is what used to make the window flash to
    /// that one first. Naming the project group keeps it under the row it was
    /// clicked in.
    ///
    /// Like [`Self::activate`], this runs on `window.defer` and takes no
    /// `&mut Self` — see there for why a lease on this panel across an
    /// activation is a panic.
    ///
    /// `then` is dispatched in the worktree once it is showing, which is how a
    /// worktree made to start an agent in gets its agent.
    fn open_worktree(
        &mut self,
        key: ProjectGroupKey,
        root: PathBuf,
        name: SharedString,
        then: Option<Box<dyn gpui::Action>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multi_workspace = self.multi_workspace.clone();
        window.defer(cx, move |window, cx| {
            let window_handle = window.window_handle();
            let Some(multi_workspace) = multi_workspace.upgrade() else {
                return;
            };
            let leaving = multi_workspace.read(cx).workspace().clone();
            let opening = leaving.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |_, cx| OpeningWorktree::new(name.clone(), cx));
                workspace.active_modal::<OpeningWorktree>(cx)
            });
            let opened = multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.find_or_create_local_workspace(
                    PathList::new(&[root]),
                    Some(key),
                    None,
                    OpenMode::Activate,
                    None,
                    window,
                    cx,
                )
            });
            cx.spawn(async move |cx| {
                let opened = opened.await;
                if let Some(opening) = opening {
                    opening.update(cx, |_, cx| cx.emit(DismissEvent));
                }
                match opened {
                    Ok(workspace) => {
                        let Some(action) = then else {
                            return;
                        };
                        // After a frame, once the worktree's workspace is the
                        // one drawn: its actions are only reachable from there.
                        window_handle
                            .update(cx, |_, window, _| {
                                window.on_next_frame(move |window, cx| {
                                    let focus_handle = workspace.read(cx).focus_handle(cx);
                                    focus_handle.dispatch_action(&*action, window, cx);
                                });
                            })
                            .log_err();
                    }
                    Err(error) => {
                        log::error!("opening the worktree {name}: {error:#}");
                        leaving.update(cx, |workspace, cx| {
                            workspace
                                .show_error(format!("Could not open “{name}”: {error:#}"), cx);
                        });
                    }
                }
            })
            .detach();
        });
    }

    /// Adds a project to the window: asks for folders, then opens them.
    ///
    /// A project, not a worktree — the panel groups by repository, so this is
    /// how a second repository gets into the list at all.
    ///
    /// The window keeps showing what it was showing. The panel is the list of
    /// every project, and adding one adds a row to that list; taking the user
    /// away from the worktree they were in is a separate decision, which the
    /// rows themselves are there to make. The exception is a window that has
    /// nothing open, where there is nothing to take the user away from and the
    /// project is opened as usual.
    fn add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let multi_workspace = self.multi_workspace.clone();
        let folders = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: None,
        });
        cx.spawn_in(window, async move |_, cx| {
            let Some(folders) = folders.await?? else {
                return anyhow::Ok(());
            };
            if folders.is_empty() {
                return anyhow::Ok(());
            }
            multi_workspace
                .update_in(cx, |multi_workspace, window, cx| {
                    // Holding more than one project at a time is what
                    // `OpenMode::Add` needs and what a multi workspace is; with
                    // it turned off, adding a project replaces the window's.
                    if multi_workspace.multi_workspace_enabled(cx)
                        && !is_empty_workspace(multi_workspace.workspace(), cx)
                    {
                        multi_workspace.find_or_create_local_workspace(
                            PathList::new(&folders),
                            None,
                            None,
                            OpenMode::Add,
                            None,
                            window,
                            cx,
                        )
                    } else {
                        multi_workspace.open_project(folders, OpenMode::Activate, window, cx)
                    }
                })?
                .await?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Removes a project from the window: its row goes, and so do the
    /// workspaces the window has open for its worktrees.
    ///
    /// Nothing is deleted. A removed project is one the panel stops listing,
    /// which is the counterpart of [`Self::add_project`] — deleting a worktree
    /// from disk is [`Self::delete_worktree`], on the worktree's own row.
    ///
    /// Closing a workspace can stop to ask about unsaved changes, and the
    /// removal is abandoned if the user says no. Deferred and without
    /// `&mut Self` for the reason given on [`Self::activate`]: removing the
    /// project the window is showing activates a replacement.
    fn remove_project(
        &mut self,
        key: ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multi_workspace = self.multi_workspace.clone();
        window.defer(cx, move |window, cx| {
            multi_workspace
                .update(cx, |multi_workspace, cx| {
                    multi_workspace
                        .remove_project_group(&key, window, cx)
                        .detach_and_log_err(cx);
                })
                .ok();
        });
    }

    /// Keeps this worktree's panel at the width every worktree shares.
    ///
    /// Checked on every draw rather than once, because a panel is drawn again
    /// each time its worktree comes forward and that is exactly when it must
    /// agree with the one you just left. Doing it once — when the panel is
    /// first built — misses every drag that happens afterwards, which is most
    /// of them.
    ///
    /// It costs a comparison: the shared width is held in memory, and the
    /// dock's own is a field read. Nothing is written unless they differ.
    fn match_shared_width(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mine = self.my_width(cx);
        let Some(shared) = shared_width(cx) else {
            // Nothing has been dragged yet, in this session or any before it.
            // Whatever the first panel to draw is wearing becomes the width
            // the others take, so they agree from the start rather than from
            // the first drag.
            if let Some(mine) = mine {
                set_shared_width(mine, cx);
            }
            return;
        };
        if mine == Some(shared) {
            return;
        }

        let workspace = self.workspace.clone();
        let panel = cx.entity();
        // Deferred, per `activate`: this runs inside a draw, and a dock cannot
        // be updated while it is being rendered.
        window.defer(cx, move |_window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            let dock = workspace
                .read(cx)
                .dock_at_position(DockPosition::Left)
                .clone();
            dock.update(cx, |dock, cx| {
                dock.set_panel_size_state(&panel, shared, cx);
            });
            // Persisted for this workspace too, so the next session starts at
            // the shared width rather than flickering to it on the first draw.
            workspace.update(cx, |workspace, cx| {
                workspace.persist_panel_size_state(WorktreePanel::panel_key(), shared, cx);
            });
        });
    }

    /// What this worktree's panel is currently wearing.
    fn my_width(&self, cx: &Context<Self>) -> Option<PanelSizeState> {
        let workspace = self.workspace.upgrade()?;
        let dock = workspace
            .read(cx)
            .dock_at_position(DockPosition::Left)
            .clone();
        let width = dock.read(cx).stored_panel_size_state(&cx.entity());
        width
    }

    /// Creates a worktree of one repository: asks for a name, and that name is
    /// the whole answer — it names the directory and the branch.
    ///
    /// Where it goes is Bench's decision rather than a question: every worktree
    /// of every project lives under `~/bench`; see [`worktrees_directory`].
    ///
    /// The modal belongs to the workspace it is opened on, so the repository's
    /// own workspace is activated first — the panel lists every project, and
    /// a modal opened on a workspace the window is not showing is a modal
    /// nobody sees. Deferred and without `&mut Self` for the reason given on
    /// [`Self::activate`].
    ///
    /// With an `issue`, the modal opens already linked to it and named after
    /// its branch; otherwise it offers to link one.
    fn add_worktree(
        &mut self,
        from: Entity<Workspace>,
        repository: Entity<Repository>,
        key: ProjectGroupKey,
        issue: Option<Arc<Issue>>,
        start_agent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multi_workspace = self.multi_workspace.clone();
        let panel = cx.entity().downgrade();
        let directory = worktrees_directory(&key);
        window.defer(cx, move |window, cx| {
            multi_workspace
                .update(cx, |multi_workspace, cx| {
                    multi_workspace.activate(from.clone(), None, window, cx);
                })
                .ok();

            let host = from.clone();
            host.update(cx, |host, cx| {
                host.toggle_modal(window, cx, move |window, cx| {
                    NameWorktree::new(directory, issue, window, cx, move |name, issue, window, cx| {
                        let from = from.clone();
                        let repository = repository.clone();
                        let key = key.clone();
                        panel
                            .update(cx, |panel, cx| {
                                panel.create_worktree(
                                    from,
                                    repository,
                                    key,
                                    name,
                                    issue,
                                    start_agent,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    })
                });
            });
        });
    }

    /// Creates a worktree for a Linear issue: asks which project it is for
    /// when the window has more than one, then asks for the name as
    /// [`Self::add_worktree`] does, with the issue already linked.
    fn add_worktree_for_issue(
        &mut self,
        identifier: SharedString,
        start_agent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(linear) = Linear::global(cx) else {
            return;
        };
        // Only projects a worktree can be made in: an open one, since making
        // one is something the repository does.
        let targets: Vec<ProjectTarget> = self
            .tree(cx)
            .into_iter()
            .filter_map(|row| {
                Some(ProjectTarget {
                    from: row
                        .worktrees
                        .iter()
                        .find_map(|worktree| worktree.switch_from.clone())?,
                    repository: row.repository?,
                    name: row.name,
                    key: row.key,
                })
            })
            .collect();
        let issue = linear.read_with(cx, |linear, cx| linear.issue(&identifier, cx));
        let workspace = self.workspace.clone();

        cx.spawn_in(window, async move |this, cx| {
            let issue = match issue.await {
                Ok(issue) => issue,
                Err(error) => {
                    workspace.update(cx, |workspace, cx| {
                        workspace.show_error(
                            format!("Could not find the Linear issue {identifier}: {error:#}"),
                            cx,
                        );
                    })?;
                    return anyhow::Ok(());
                }
            };
            this.update_in(cx, |this, window, cx| {
                let mut targets = targets;
                match targets.len() {
                    0 => {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.show_error(
                                    "Open a project to create a worktree in.".to_string(),
                                    cx,
                                );
                            })
                            .ok();
                    }
                    1 => {
                        let target = targets.remove(0);
                        this.add_worktree(
                            target.from,
                            target.repository,
                            target.key,
                            Some(issue),
                            start_agent,
                            window,
                            cx,
                        );
                    }
                    _ => this.choose_project(targets, issue, start_agent, window, cx),
                }
            })
        })
        .detach_and_log_err(cx);
    }

    /// Asks which project a worktree for `issue` goes in. Opened on the
    /// workspace the window is showing, deferred for the reason given on
    /// [`Self::activate`].
    fn choose_project(
        &mut self,
        targets: Vec<ProjectTarget>,
        issue: Arc<Issue>,
        start_agent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multi_workspace = self.multi_workspace.clone();
        let panel = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            let Some(host) = multi_workspace
                .upgrade()
                .map(|multi_workspace| multi_workspace.read(cx).workspace().clone())
            else {
                return;
            };
            host.update(cx, |host, cx| {
                host.toggle_modal(window, cx, move |_window, cx| {
                    ChooseProject::new(targets, issue.clone(), cx, move |target, window, cx| {
                        let issue = issue.clone();
                        panel
                            .update(cx, |panel, cx| {
                                panel.add_worktree(
                                    target.from,
                                    target.repository,
                                    target.key,
                                    Some(issue),
                                    start_agent,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    })
                });
            });
        });
    }

    /// Makes the worktree the user named, then opens it.
    ///
    /// The branch is created, not chosen: a worktree named `fix-login` is a
    /// branch named `fix-login`, off whatever the repository has checked out.
    /// Git refuses a branch that already exists, and refusing is the honest
    /// answer — two worktrees cannot share one branch — so it is shown rather
    /// than worked around.
    ///
    /// `git worktree add` creates the directories on the way to the worktree,
    /// so the project's directory under `~/bench` need not exist yet.
    ///
    /// A worktree made for a Linear issue also tells Linear that work on it has
    /// begun; see [`Linear::start_issue`]. The worktree is made first, and a
    /// failure there is reported without undoing it.
    fn create_worktree(
        &mut self,
        from: Entity<Workspace>,
        repository: Entity<Repository>,
        key: ProjectGroupKey,
        name: SharedString,
        issue: Option<Arc<Issue>>,
        start_agent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (path, target) = new_worktree(&key, &name);
        // The issue linked in the dialog, which need not be the one the
        // worktree was asked for: it could have been changed or unlinked.
        let then: Option<Box<dyn gpui::Action>> = start_agent.then(|| match &issue {
            Some(issue) => Box::new(linear::send_to_agent(&issue.url)) as Box<dyn gpui::Action>,
            None => Box::new(zed_actions::claude::NewTerminal),
        });
        // The project's other worktrees, whose colours the new one avoids.
        let siblings: Vec<PathBuf> = self
            .tree(cx)
            .into_iter()
            .filter(|row| row.key.matches(&key))
            .flat_map(|row| row.worktrees)
            .filter_map(|worktree| worktree.root)
            .collect();
        let linked = issue.as_ref().map(|issue| LinkedIssue {
            id: issue.id.to_string(),
            identifier: issue.identifier.to_string(),
        });
        let title = issue.as_ref().map(|issue| issue.title.to_string());
        cx.spawn_in(window, async move |this, cx| {
            let created = repository
                .update(cx, |repository, _| {
                    repository.create_worktree(target, path.clone())
                })
                .await?;

            match created {
                Ok(()) => {
                    // Stored before the worktree opens, so its title bar is
                    // in its own colour from the first frame.
                    cx.update(|_, cx| {
                        WorktreeMetadataStore::global(cx).update(cx, |store, cx| {
                            let hue = store.least_used_hue(&siblings, cx);
                            store.update(
                                &path,
                                |metadata| {
                                    metadata.hue = Some(hue);
                                    metadata.issue = linked;
                                    metadata.title = title;
                                },
                                cx,
                            );
                        });
                    })?;
                    this.update_in(cx, |this, window, cx| {
                        this.rediscover(project_root(&key), cx);
                        this.open_worktree(key, path, name, then, window, cx);
                    })?;
                    let started = issue.and_then(|issue| {
                        let linear = cx.update(|_, cx| Linear::global(cx)).ok()??;
                        let identifier = issue.identifier.clone();
                        let started =
                            linear.update(cx, |linear, cx| linear.start_issue(issue, cx));
                        Some((identifier, started))
                    });
                    if let Some((identifier, started)) = started
                        && let Err(error) = started.await
                    {
                        log::error!("starting the Linear issue {identifier}: {error:#}");
                        from.update(cx, |workspace, cx| {
                            workspace.show_error(
                                format!(
                                    "Created the worktree, but could not update {identifier} in Linear: {error:#}"
                                ),
                                cx,
                            );
                        });
                    }
                }
                Err(refused) => {
                    log::error!("creating the worktree {}: {refused:#}", path.display());
                    from.update(cx, |workspace, cx| {
                        workspace.show_error(
                            format!("Could not create the worktree “{name}”: {refused}"),
                            cx,
                        );
                    });
                }
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Deletes a worktree: closes the workspaces showing anything inside it,
    /// removes the repositories nested in it, then removes the worktree.
    ///
    /// It asks first, because this deletes a directory. It asks a second time
    /// when git refuses — which is what git does when a worktree has changes
    /// that removing it would lose — naming what git refused and why, rather
    /// than forcing straight away.
    ///
    /// A worktree of a project that keeps clones in `repos/` holds a worktree
    /// of each clone's repository. Those go first, each through its own
    /// repository: to the outer repository they are untracked files, so git
    /// refuses the outer worktree while they are there, and deleting them with
    /// it would leave each clone's repository holding a worktree that no
    /// longer exists, with its branch locked to it.
    ///
    /// The workspaces go before the directory does: a workspace whose folder
    /// has just stopped existing shows an empty tree and errors on every file
    /// watch. Every workspace inside the worktree goes, not only the one its
    /// row shows — one opened at a nested repository, say. And once the
    /// directory is gone, anything still showing it is closed too, and any
    /// project made of it removed: a workspace of a deleted folder no longer
    /// belongs to its repository, so it would otherwise turn up as a project
    /// of its own.
    fn delete_worktree(
        &mut self,
        repository: Entity<Repository>,
        root: PathBuf,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(fs) = self.fs(cx) else {
            return;
        };
        let multi_workspace = self.multi_workspace.clone();
        // The project git will be asked about again once this worktree is
        // gone; see `rediscover`.
        let project = repository_anchor(&repository, cx);

        cx.spawn_in(window, async move |this, cx| {
            let nested = cx
                .background_spawn(nested_worktrees(fs.clone(), root.clone()))
                .await;
            let mut detail = format!("{} will be removed from disk.", root.display());
            if !nested.is_empty() {
                detail.push_str(&format!(
                    "\n\nThe repositories inside it are removed from their own repositories \
                     first: {}.",
                    nested
                        .iter()
                        .map(|nested| nested.label(&root))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let confirmed = cx.update(|window, cx| {
                window.prompt(
                    gpui::PromptLevel::Warning,
                    &format!("Delete the worktree “{name}”?"),
                    Some(&detail),
                    &["Delete", "Cancel"],
                    cx,
                )
            })?;
            if confirmed.await? != 0 {
                return anyhow::Ok(());
            }

            // Closing can stop to ask about unsaved changes, and answering no
            // means the worktree stays. Deleting the directory anyway would
            // throw away the very work the prompt was protecting.
            let closed = multi_workspace
                .update_in(cx, |multi_workspace, window, cx| {
                    close_workspaces_inside(multi_workspace, &root, window, cx)
                })?
                .await?;
            if !closed {
                return anyhow::Ok(());
            }

            let git = which::which("git").ok();
            let mut refused: Vec<(String, anyhow::Error)> = Vec::new();
            let mut refused_nested = Vec::new();
            for nested in &nested {
                match remove_nested(&fs, git.as_deref(), nested, false).await {
                    Ok(()) => {}
                    Err(error) => {
                        refused.push((nested.label(&root), error));
                        refused_nested.push(nested.clone());
                    }
                }
            }
            // Only once everything inside it is gone: with a nested
            // repository refused, the outer worktree still holds it, and
            // nothing is deleted until the user says so.
            let mut outer_removed = false;
            if refused.is_empty() {
                let removed = repository
                    .update(cx, |repository, _| {
                        repository.remove_worktree(root.clone(), false)
                    })
                    .await?;
                match removed {
                    Ok(()) => outer_removed = true,
                    Err(error) => refused.push((name.to_string(), error)),
                }
            }

            if !refused.is_empty() {
                for (what, error) in &refused {
                    log::warn!("git refused to remove {what}: {error:#}");
                }
                let reasons = refused
                    .iter()
                    .map(|(what, error)| format!("• {what}: {}", git_reason(error)))
                    .collect::<Vec<_>>()
                    .join("\n");
                let forced = cx.update(|window, cx| {
                    window.prompt(
                        gpui::PromptLevel::Warning,
                        &format!("Could not delete “{name}”."),
                        Some(&format!(
                            "{reasons}\n\nDeleting it anyway discards whatever is in it."
                        )),
                        &["Delete Anyway", "Cancel"],
                        cx,
                    )
                })?;
                if forced.await? != 0 {
                    this.update(cx, |this, cx| this.rediscover(project.clone(), cx))
                        .ok();
                    return anyhow::Ok(());
                }
                for nested in &refused_nested {
                    remove_nested(&fs, git.as_deref(), nested, true).await?;
                }
                if !outer_removed {
                    repository
                        .update(cx, |repository, _| repository.remove_worktree(root.clone(), true))
                        .await??;
                }
            }

            multi_workspace
                .update_in(cx, |multi_workspace, window, cx| {
                    clear_deleted(multi_workspace, &root, window, cx);
                })
                .ok();
            cx.update(|_, cx| {
                WorktreeMetadataStore::global(cx)
                    .update(cx, |store, cx| store.remove(&root, cx));
            })?;
            this.update(cx, |this, cx| this.rediscover(project.clone(), cx))
                .ok();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// The menu of a worktree row: opening its Linear issue when it has one,
    /// editing its title, copying its path, its colour — the colours a worktree can be, the one
    /// it is ticked, and going back to the one worked out from its folder —
    /// and deleting it, which every worktree but the repository's own
    /// checkout offers.
    fn deploy_row_menu(
        &mut self,
        root: PathBuf,
        issue: Option<SharedString>,
        delete: Option<(Entity<Repository>, SharedString)>,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = WorktreeMetadataStore::global(cx);
        let metadata = store.read(cx).get(&root, cx);
        let current = metadata.hue_for(&root);
        let chosen = metadata.hue.is_some();
        let this = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let path = root.to_string_lossy().into_owned();
            let delete_root = root.clone();
            menu.when_some(issue.clone(), |menu, identifier| {
                let this = this.clone();
                menu.entry(format!("Open {identifier}"), None, move |window, cx| {
                    this.update(cx, |this, cx| this.open_issue(identifier.clone(), window, cx))
                        .ok();
                })
            })
            .entry("Edit Title…", None, {
                let this = this.clone();
                let root = root.clone();
                move |window, cx| {
                    this.update(cx, |this, cx| this.edit_title(root.clone(), window, cx))
                        .ok();
                }
            })
            .entry("Copy Path", None, move |_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
            })
            .separator()
            .submenu("Colour", move |mut menu, _, _| {
                for hue in 0..HUES {
                    let name = HUE_NAMES.get(usize::from(hue)).copied().unwrap_or_default();
                    let store = store.clone();
                    let root = root.clone();
                    menu = menu.custom_entry(
                        move |_, _| {
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(div().size_3().rounded_full().bg(hue_color(hue)))
                                .child(Label::new(name))
                                .when(hue == current, |this| {
                                    this.child(
                                        div().flex_1().flex().justify_end().child(
                                            Icon::new(IconName::Check)
                                                .size(IconSize::Small)
                                                .color(Color::Accent),
                                        ),
                                    )
                                })
                                .into_any_element()
                        },
                        move |_, cx| {
                            store.update(cx, |store, cx| {
                                store.update(&root, |metadata| metadata.hue = Some(hue), cx)
                            });
                        },
                    );
                }
                let store = store.clone();
                let root = root.clone();
                menu.separator().toggleable_entry(
                    "Automatic",
                    !chosen,
                    IconPosition::Start,
                    None,
                    move |_, cx| {
                        store.update(cx, |store, cx| {
                            store.update(&root, |metadata| metadata.hue = None, cx)
                        });
                    },
                )
            })
            .when_some(delete, |menu, (repository, name)| {
                let this = this.clone();
                menu.separator()
                    .entry("Delete Worktree", None, move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.delete_worktree(
                                repository.clone(),
                                delete_root.clone(),
                                name.clone(),
                                window,
                                cx,
                            );
                        })
                        .ok();
                    })
            })
        });
        let focus = menu.focus_handle(cx);
        window.defer(cx, move |window, cx| window.focus(&focus, cx));
        let subscription = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, _, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn toggle_collapsed(&mut self, key: &ProjectGroupKey, cx: &mut Context<Self>) {
        let view = cx.default_global::<PanelView>();
        match view.collapsed.iter().position(|held| held.matches(key)) {
            Some(index) => {
                view.collapsed.remove(index);
            }
            None => view.collapsed.push(key.clone()),
        }
        // Only the project that was clicked turns, and only from now.
        view.turning = Some((key.clone(), Instant::now()));
        cx.notify();
    }

    /// Whether the dock is open on this panel, which is what its own button in
    /// the activity bar draws: Zed's sidebar icon, filled while the panel is
    /// out and hollow while it is not. Asked of the workspace the window is
    /// showing, because that is the one whose dock is on screen.
    fn is_showing(&self, cx: &App) -> bool {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return false;
        };
        let workspace = multi_workspace.read(cx).workspace().clone();
        let dock = workspace
            .read(cx)
            .dock_at_position(DockPosition::Left)
            .clone();
        let dock = dock.read(cx);
        dock.is_open()
            && dock
                .active_panel()
                .is_some_and(|panel| panel.persistent_name() == Self::persistent_name())
    }

    /// Whether any *other* worktree of the window has the panel out.
    ///
    /// Every workspace but this panel's own: this one is the one being built —
    /// [`Panel::starts_open`] is asked while the workspace is mid-update — so
    /// reading it here is a double lease and a panic. It is also the one with
    /// nothing to say, since the question is what the rest of the window is
    /// doing.
    fn showing_in_another_worktree(&self, cx: &App) -> bool {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return false;
        };
        let own = self.workspace.entity_id();
        let workspaces: Vec<Entity<Workspace>> = multi_workspace
            .read(cx)
            .workspaces()
            .filter(|workspace| workspace.entity_id() != own)
            .cloned()
            .collect();
        workspaces.into_iter().any(|workspace| {
            let dock = workspace
                .read(cx)
                .dock_at_position(DockPosition::Left)
                .clone();
            let dock = dock.read(cx);
            dock.is_open()
                && dock
                    .active_panel()
                    .is_some_and(|panel| panel.persistent_name() == Self::persistent_name())
        })
    }

    /// What the filter box says, trimmed and lowercased, or nothing when it is
    /// empty.
    fn filter(&self, cx: &App) -> Option<String> {
        let filter = self.filter.text(cx).trim().to_lowercase();
        (!filter.is_empty()).then_some(filter)
    }

    fn is_collapsed(&self, key: &ProjectGroupKey, cx: &App) -> bool {
        cx.try_global::<PanelView>()
            .is_some_and(|view| view.collapsed.iter().any(|held| held.matches(key)))
    }

    /// Whether this project's chevron should be drawn mid-turn, which is true
    /// only just after it was clicked.
    fn is_turning(&self, key: &ProjectGroupKey, cx: &App) -> bool {
        cx.try_global::<PanelView>()
            .and_then(|view| view.turning.as_ref())
            .is_some_and(|(turning, at)| turning.matches(key) && at.elapsed() < CHEVRON_TURN)
    }

    fn render_repository(
        &self,
        row: &RepositoryRow,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let collapsed = self.is_collapsed(&row.key, cx);
        let turning = self.is_turning(&row.key, cx);
        let key = row.key.clone();
        // Any workspace of this repository will do to create against: it only
        // needs the project, and every worktree of one repository shares it.
        let add = row
            .worktrees
            .iter()
            .find_map(|worktree| worktree.switch_from.clone())
            .zip(row.repository.clone());
        let add_key = row.key.clone();

        let remove_key = row.key.clone();

        ListItem::new(("repository", index))
            .spacing(ListItemSpacing::Sparse)
            .height(ROW_HEIGHT)
            .start_slot(
                h_flex()
                    .gap_1()
                    .child(render_chevron(index, collapsed, turning, cx))
                    // A project is a directory; a worktree is a branch. The
                    // rows are told apart by their icons before they are read.
                    .child(
                        Icon::new(IconName::Folder)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                Label::new(row.name.clone())
                    .single_line()
                    .truncate()
                    // A project the window has nothing open for reads as one of
                    // the user's rather than one of the window's.
                    .color(if row.is_open {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .end_slot(
                h_flex()
                    .gap_0p5()
                    // Creating a worktree is the repository's own doing, and
                    // there is no repository to ask while the project is
                    // closed.
                    .when_some(add, |this, (from, repository)| {
                        this.child(
                            IconButton::new(("add-worktree", index), IconName::Plus)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("New Worktree"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.add_worktree(
                                        from.clone(),
                                        repository.clone(),
                                        add_key.clone(),
                                        None,
                                        false,
                                        window,
                                        cx,
                                    );
                                })),
                        )
                    })
                    .child(
                        IconButton::new(("remove-project", index), IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Remove Project"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.remove_project(remove_key.clone(), window, cx);
                            })),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_collapsed(&key, cx)))
    }

    /// A worktree as a card: what it is for above, which worktree it is below.
    ///
    /// The card leads with the worktree's stored title — its issue's, unless
    /// that was edited — and the worktree's name goes under it. Without a
    /// title the name leads, and the line under it holds only what else there
    /// is to say. The icon is always the branch, since every worktree is one;
    /// where its issue stands is on the issue's pill.
    fn render_worktree(
        &self,
        row: &WorktreeRow,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_open = row.workspace.is_some();
        // Deleting needs the repository that owns the worktree, and git will
        // not remove the repository's own checkout.
        let delete = (!row.is_main)
            .then(|| row.repository.clone())
            .flatten()
            .map(|repository| (repository, row.name.clone()));
        // What a click does: go to the worktree if the window has it open,
        // otherwise open it.
        let activate = row.workspace.clone();
        let open = row
            .root
            .clone()
            .filter(|_| !is_open)
            .map(|root| (row.key.clone(), root));
        let open_name = row.name.clone();
        // Only the repository's own checkout: it is where the branch everyone
        // else starts from lives, and keeping it level with its upstream is
        // the chore. A worktree's branch is pushed when its work is done.
        let sync = match (&row.root, row.status.ahead_behind) {
            (Some(root), Some((ahead, behind))) if row.is_main && (ahead > 0 || behind > 0) => {
                Some((root.clone(), ahead, behind))
            }
            _ => None,
        };
        let is_syncing = row.root.as_ref().is_some_and(|root| {
            cx.try_global::<ProjectScans>()
                .is_some_and(|scans| scans.syncing.contains(root))
        });

        let name = match &row.issue {
            Some(issue) => name_without_identifier(&row.name, &issue.identifier),
            None => row.name.clone(),
        };
        let (heading, subheading) = match row.title.clone() {
            Some(title) => (title, Some(name)),
            // Without a title the name leads, and the line under it names the
            // branch — which says something only when it is not the name
            // again: the repository's own checkout switched to a feature
            // branch, or a worktree whose branch was renamed.
            None => {
                let branch = row.branch.clone().filter(|branch| *branch != name);
                (name, branch)
            }
        };
        let text_color = if is_open {
            Color::Default
        } else {
            Color::Muted
        };
        let colors = cx.theme().colors();

        v_flex()
            .id(("worktree", index))
            .ml_3()
            .mr_1p5()
            .my_0p5()
            .px_2()
            .py_1p5()
            .gap_0p5()
            .rounded_md()
            .border_1()
            .cursor_pointer()
            .map(|card| {
                if row.is_active {
                    card.border_color(colors.border_selected)
                        .bg(colors.ghost_element_selected)
                } else {
                    card.border_color(colors.border_variant)
                        .bg(colors.ghost_element_background)
                        .hover(|card| card.bg(colors.ghost_element_hover))
                }
            })
            .when_some(row_tooltip(row), |this, tooltip| {
                this.tooltip(Tooltip::text(tooltip))
            })
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::GitBranch)
                            .size(IconSize::Small)
                            .color(if row.is_active {
                                // The worktree the window is showing. One
                                // accented icon says which of them you are in
                                // from across the tree.
                                Color::Accent
                            } else if is_open {
                                Color::Muted
                            } else {
                                // A worktree that exists but is not open in
                                // this window: the card is there to be
                                // clicked, and should not read as one of the
                                // window's own.
                                Color::Ignored
                            }),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                // Truncated: a long title would otherwise
                                // widen the card past the panel.
                                div().min_w_0().child(
                                    Label::new(heading)
                                        .single_line()
                                        .truncate()
                                        .color(text_color),
                                ),
                            )
                            .children(sync.map(|(root, ahead, behind)| {
                                if is_syncing {
                                    Icon::new(IconName::ArrowCircle)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted)
                                        .with_rotate_animation(2)
                                        .into_any_element()
                                } else {
                                    IconButton::new(("sync", index), IconName::ArrowCircle)
                                        .icon_size(IconSize::XSmall)
                                        .icon_color(Color::Accent)
                                        .tooltip(Tooltip::text(sync_description(ahead, behind)))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.sync_worktree(
                                                root.clone(),
                                                ahead > 0,
                                                behind > 0,
                                                window,
                                                cx,
                                            );
                                        }))
                                        .into_any_element()
                                }
                            })),
                    )
                    .children(row.status.last_edit.and_then(|at| {
                        let ago = SystemTime::now().duration_since(at).ok()?;
                        Some(
                            Label::new(edited_ago(ago))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .flex_none(),
                        )
                    }))
                    .children(render_agents(index, &row.agents)),
            )
            // Always drawn, empty or not, so that every card is the same
            // height: a card with nothing more to say — the repository's own
            // checkout, usually — would otherwise sit shorter than the rest.
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .h_5()
                    .gap_1p5()
                    // Holds the icon's room, so the name sits under the
                    // title rather than under the icon.
                    .child(div().flex_none().size(IconSize::Small.rems()))
                    .child(div().flex_1().min_w_0().children(subheading.map(|name| {
                        Label::new(name)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line()
                            .truncate()
                    })))
                    .children(render_changes(index, row.status.changes))
                    .children(row.status.ahead_behind.and_then(|(ahead, behind)| {
                        render_ahead_behind(index, ahead, behind)
                    }))
                    .children(
                        row.pull_request
                            .map(|state| render_pull_request(state, index)),
                    )
                    .children(row.issue.as_deref().map(render_issue_pill)),
            )
            .when_some(row.root.clone(), |this, root| {
                let issue = row.issue.as_ref().map(|issue| issue.identifier.clone());
                this.on_mouse_down(
                    gpui::MouseButton::Right,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.deploy_row_menu(
                            root.clone(),
                            issue.clone(),
                            delete.clone(),
                            event.position,
                            window,
                            cx,
                        );
                    }),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                match (activate.clone(), open.clone()) {
                    (Some(workspace), _) => this.activate(workspace, window, cx),
                    (None, Some((key, root))) => {
                        this.open_worktree(key, root, open_name.clone(), None, window, cx)
                    }
                    (None, None) => {}
                }
            }))
    }
}

impl WorktreePanel {
    /// Pulls the commits the worktree's branch is behind by, then pushes the
    /// ones it is ahead by. Pulling only fast-forwards: a branch that has
    /// gone its own way needs a merge or a rebase, and which is not for a
    /// button to decide, so git's refusal is shown instead.
    fn sync_worktree(
        &mut self,
        root: PathBuf,
        push: bool,
        pull: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let started = cx.default_global::<ProjectScans>().syncing.insert(root.clone());
        if !started {
            return;
        }
        cx.refresh_windows();
        cx.spawn_in(window, async move |_, cx| {
            let result = cx
                .background_spawn(sync_with_upstream(root.clone(), push, pull))
                .await;
            let refused = cx.update(|window, cx| {
                record_scan(cx, |scans| {
                    scans.syncing.remove(&root);
                    // Due again, so the counts the sync changed are asked for
                    // on the next draw. A refresh already in flight started
                    // before the sync and would land with the old counts.
                    if let Some(StatusScan::Found {
                        checked, _refresh, ..
                    }) = scans.statuses.get_mut(&root)
                    {
                        if let Some(due) = Instant::now().checked_sub(STATUS_REFRESH) {
                            *checked = due;
                        }
                        *_refresh = None;
                    }
                });
                result.err().map(|error| {
                    log::warn!("could not sync {}: {error:#}", root.display());
                    window.prompt(
                        gpui::PromptLevel::Warning,
                        "Could not sync with the upstream branch.",
                        Some(&git_reason(&error)),
                        &["OK"],
                        cx,
                    )
                })
            })?;
            if let Some(refused) = refused {
                refused.await?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Asks for a worktree's title, starting from the one it has. Opened on
    /// the workspace the window is showing, which is the one whose panel was
    /// clicked, and deferred for the reason given on [`Self::activate`].
    fn edit_title(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let multi_workspace = self.multi_workspace.clone();
        let title = WorktreeMetadataStore::global(cx)
            .read(cx)
            .get(&root, cx)
            .title
            .unwrap_or_default();
        window.defer(cx, move |window, cx| {
            let Some(workspace) = multi_workspace
                .upgrade()
                .map(|multi_workspace| multi_workspace.read(cx).workspace().clone())
            else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, move |window, cx| {
                    EditTitle::new(root, &title, window, cx)
                });
            });
        });
    }

    /// Opens an issue's tab in the worktree the window is showing. Deferred
    /// for the reason given on [`Self::activate`]: focusing the tab walks the
    /// docks, and a click handler holds this panel's lease.
    fn open_issue(&mut self, identifier: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let multi_workspace = self.multi_workspace.clone();
        window.defer(cx, move |window, cx| {
            let Some(workspace) = multi_workspace
                .upgrade()
                .map(|multi_workspace| multi_workspace.read(cx).workspace().clone())
            else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                linear::open_issue(workspace, identifier, window, cx);
            });
        });
    }
}

/// A Linear issue's state, as Linear draws it: its group's icon in the colour
/// the team gave the state.
fn render_issue_state(issue: &Issue) -> Icon {
    Icon::new(issue.state.kind.icon())
        .size(IconSize::Small)
        .map(|icon| match issue.state.color() {
            Some(color) => icon.color(Color::Custom(color)),
            None => icon.color(Color::Muted),
        })
}

/// What became of the pull request opened from a worktree's branch. At the end
/// of the row, with the agents: both are what is happening to the worktree,
/// where the icon at the start says what it is.
fn render_pull_request(state: PullRequestState, index: usize) -> impl IntoElement {
    div()
        .id(("pull-request", index))
        .child(
            Icon::new(IconName::PullRequest)
                .size(IconSize::XSmall)
                .color(pull_request_color(state)),
        )
        .tooltip(Tooltip::text(format!("Pull request: {}", state.label())))
}

/// What a worktree row says on hover: the branch in full, since a linked row
/// shows it shortened, and the issue it is for.
fn row_tooltip(row: &WorktreeRow) -> Option<String> {
    let issue = row.issue.as_ref()?;
    let branch = row.branch.as_ref().unwrap_or(&row.name);
    Some(format!(
        "{branch}\n{} · {}: {}",
        issue.identifier, issue.state.name, issue.title
    ))
}

/// A linked worktree's name with the issue identifier it starts with taken
/// off, since the row already shows the identifier: `RB-146-connect-extras`
/// is `connect-extras`, and so is `dev-rb-146-connect-extras`, whose
/// directory name flattened a `dev/` branch prefix. A name that is nothing but
/// the identifier, or does not carry it, is left whole.
fn name_without_identifier(name: &str, identifier: &str) -> SharedString {
    let lowercase_name = name.to_ascii_lowercase();
    let lowercase_identifier = identifier.to_ascii_lowercase();
    let rest = lowercase_name
        .find(&lowercase_identifier)
        .filter(|start| *start == 0 || name[..*start].ends_with(['-', '/']))
        .and_then(|start| name.get(start + identifier.len()..))
        .map(|rest| rest.trim_start_matches(['-', '_', '/']))
        .filter(|rest| !rest.is_empty());
    SharedString::from(rest.unwrap_or(name).to_string())
}

/// The height of a row, project and worktree alike.
///
/// Taller than what the list item's own padding gives it, which is sized for a
/// long list read at a glance. This one is short — a handful of projects and
/// their worktrees — and its rows are click targets you aim at, so they are
/// given the room. In rems, so that it follows the UI font size.
const ROW_HEIGHT: Rems = Rems(2.25);

/// How long the chevron takes to turn. Short enough that expanding still feels
/// like a direct response to the click, long enough to be seen.
const CHEVRON_TURN: Duration = Duration::from_millis(120);

/// Whether an agent is at work in the worktree, and what it is doing: Claude's
/// mark, in the colour of whatever wants you most, breathing while it works.
/// How many agents there are is left to the tooltip; the card only needs to
/// say that one is there and whether it is waiting on you.
fn render_agents(index: usize, agents: &AgentSummary) -> Option<AnyElement> {
    let state = agents.state()?;
    let total = agents.total();
    let summary = if agents.needs_input > 0 {
        format!("{} of {total} waiting for you", agents.needs_input)
    } else if agents.working > 0 {
        format!("{} of {total} working", agents.working)
    } else {
        format!("{total} idle")
    };

    let mark = div()
        .id(("agents", index))
        .flex_none()
        .child(
            Icon::new(IconName::AiClaude)
                .size(IconSize::Small)
                .color(agent_state_color(state)),
        )
        .tooltip(Tooltip::text(format!(
            "{total} agent{}: {summary}",
            if total == 1 { "" } else { "s" }
        )));
    Some(match state {
        AgentState::Working => mark
            .with_animation(
                ("agents-working", index),
                Animation::new(AGENT_BREATH)
                    .repeat()
                    .with_easing(pulsating_between(0.35, 1.)),
                |mark, delta| mark.opacity(delta),
            )
            .into_any_element(),
        AgentState::Idle | AgentState::NeedsInput => mark.into_any_element(),
    })
}

/// How long one breath of a working agent's mark takes. Slow, because it runs
/// for as long as the agent does, at the edge of your eye.
const AGENT_BREATH: Duration = Duration::from_secs(2);

/// A worktree's Linear issue: where it stands, as Linear draws it, and its
/// identifier. Plain, with no box around it, because the title above is what
/// the card is about and this is a detail of it.
fn render_issue_pill(issue: &Issue) -> impl IntoElement {
    h_flex()
        .flex_none()
        .gap_0p5()
        .child(render_issue_state(issue).size(IconSize::XSmall))
        .child(
            Label::new(issue.identifier.clone())
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
}

/// How many changes a worktree has that are not committed, when it has any.
fn render_changes(index: usize, changes: usize) -> Option<impl IntoElement> {
    (changes > 0).then(|| {
        div()
            .id(("changes", index))
            .flex_none()
            .child(
                Label::new(format!("{changes} changed"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .tooltip(Tooltip::text(format!(
                "{changes} uncommitted change{}",
                if changes == 1 { "" } else { "s" }
            )))
    })
}

/// How the branch stands against its upstream, when it is anywhere but level
/// with it: commits to push, and commits to pull as of the last fetch.
fn render_ahead_behind(index: usize, ahead: u32, behind: u32) -> Option<impl IntoElement> {
    if ahead == 0 && behind == 0 {
        return None;
    }
    let mut counts = Vec::new();
    let mut explained = Vec::new();
    if ahead > 0 {
        counts.push(format!("↑{ahead}"));
        explained.push(format!("{ahead} to push"));
    }
    if behind > 0 {
        counts.push(format!("↓{behind}"));
        explained.push(format!("{behind} to pull, as of the last fetch"));
    }
    Some(
        div()
            .id(("ahead-behind", index))
            .flex_none()
            .child(
                Label::new(counts.join(" "))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .tooltip(Tooltip::text(format!("Commits {}", explained.join("; ")))),
    )
}

/// What the sync button will do, for its tooltip.
fn sync_description(ahead: u32, behind: u32) -> String {
    let commits = |count: u32| format!("{count} commit{}", if count == 1 { "" } else { "s" });
    match (ahead, behind) {
        (0, behind) => format!("Pull {}", commits(behind)),
        (ahead, 0) => format!("Push {}", commits(ahead)),
        (ahead, behind) => format!("Pull {} and push {}", commits(behind), commits(ahead)),
    }
}

/// How long ago a worktree was last edited, as briefly as a card has room for.
fn edited_ago(elapsed: Duration) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const MONTH: u64 = 30 * DAY;
    const YEAR: u64 = 365 * DAY;

    let seconds = elapsed.as_secs();
    match seconds {
        0..MINUTE => "just now".to_string(),
        MINUTE..HOUR => format!("{}m ago", seconds / MINUTE),
        HOUR..DAY => format!("{}h ago", seconds / HOUR),
        DAY..WEEK => format!("{}d ago", seconds / DAY),
        WEEK..MONTH => format!("{}w ago", seconds / WEEK),
        MONTH..YEAR => format!("{}mo ago", seconds / MONTH),
        _ => format!("{}y ago", seconds / YEAR),
    }
}

/// The disclosure chevron, turning a quarter circle as the repository opens and
/// closes.
///
/// It animates only when `turning` — when this project's own chevron was just
/// clicked. Every other time it is drawn already at rest: `with_animation`
/// replays whenever its element is mounted, and a panel is mounted afresh each
/// time its worktree comes forward, so animating unconditionally made every
/// chevron in the panel spin on every switch between worktrees.
fn render_chevron(index: usize, collapsed: bool, turning: bool, cx: &App) -> AnyElement {
    // Drawn as an `svg` rather than a `ui::Icon` because rotating an `Icon`
    // needs a trait that `ui` keeps to itself, and this needs no upstream
    // change to reach.
    let size = IconSize::XSmall.rems();
    let at_rest = if collapsed { 0. } else { 0.25 };
    let chevron = svg()
        .size(size)
        .path(IconName::ChevronRight.path())
        .text_color(Color::Muted.color(cx));
    if !turning {
        return chevron
            .with_transformation(Transformation::rotate(percentage(at_rest)))
            .into_any_element();
    }
    chevron
        .with_animation(
            // Two ids, one per state, so a toggle remounts the element.
            ("chevron", 2 * index + usize::from(collapsed)),
            Animation::new(CHEVRON_TURN),
            move |chevron, delta| {
                let quarter_turn = if collapsed { 1. - delta } else { delta } * 0.25;
                chevron.with_transformation(Transformation::rotate(percentage(quarter_turn)))
            },
        )
        .into_any_element()
}

/// The path the repository's worktree names are relative to: its own checkout.
///
/// Derived from the common git directory rather than the work directory, so a
/// project opened at a linked worktree still anchors on the repository.
fn repository_anchor(repository: &Entity<Repository>, cx: &App) -> Option<PathBuf> {
    let snapshot = repository.read(cx).snapshot();
    repo_identity_path_if_local(&snapshot.common_dir_abs_path, snapshot.path_style)
        .map(Path::to_path_buf)
}

/// One row of a repository group, decided but not yet dressed in entities.
struct RowPlan {
    /// `None` for a row that has no worktree to take a name from, which is a
    /// workspace the repository's list did not account for.
    name: Option<SharedString>,
    /// The branch this worktree has checked out, which is what a pull request
    /// is keyed by. `None` for a detached head, and for a row git said nothing
    /// about — the directory's name is not a branch name, so guessing one
    /// would colour rows by coincidence.
    branch: Option<SharedString>,
    root: Option<PathBuf>,
    /// Index into the group's open workspaces, when this worktree is one.
    open: Option<usize>,
    /// The repository's own checkout, which git will not let us remove.
    is_main: bool,
}

/// Which rows a repository group has, in the order they are drawn.
///
/// Kept apart from the entities so the decisions are testable on their own:
/// every worktree appears whether or not it is open, an open workspace is never
/// dropped even if it is missing from the list, and the order does not depend
/// on what happens to be open — a row must not move under the pointer because
/// something else was opened.
fn plan_rows(
    linked: &[GitWorktree],
    open_roots: &[Option<PathBuf>],
    anchor: Option<&Path>,
) -> Vec<RowPlan> {
    // The repository's own checkout is usually not in `linked` at all — the
    // snapshot leaves out the worktree the project is open at — so the name
    // anchor falls back to the repository's own path, as the worktree picker's
    // does. Without it every worktree in a `<name>/<repo>` layout is named
    // after the repository instead of after itself.
    let main = linked
        .iter()
        .find(|worktree| worktree.is_main)
        .map(|worktree| worktree.path.clone())
        .or_else(|| anchor.map(Path::to_path_buf));

    let mut plans: Vec<RowPlan> = linked
        .iter()
        .map(|worktree| RowPlan {
            name: Some(worktree_display_name(worktree, main.as_deref())),
            branch: worktree_branch(worktree),
            root: Some(worktree.path.clone()),
            open: open_roots
                .iter()
                .position(|root| root.as_deref() == Some(worktree.path.as_path())),
            // The repository's own checkout is the one git will not remove.
            // It is normally absent from `linked`, but a repository opened at
            // a linked worktree does list it.
            is_main: worktree.is_main || Some(worktree.path.as_path()) == anchor,
        })
        .collect();

    // Anything open but absent from the repository's list still belongs in the
    // panel: a workspace with no git repository at all, or one whose root does
    // not match a worktree path. Hiding what the window is actually showing
    // would be worse than an unexplained row.
    for (index, root) in open_roots.iter().enumerate() {
        if plans.iter().any(|plan| plan.open == Some(index)) {
            continue;
        }
        plans.push(RowPlan {
            name: None,
            branch: None,
            // The row the snapshot leaves out is the checkout the project is
            // open at, so this is where the repository's own worktree usually
            // lands — undeletable, and first in the list.
            is_main: root.as_deref().is_some() && root.as_deref() == anchor,
            root: root.clone(),
            open: Some(index),
        });
    }

    // A project the panel knows only as a path — one git reported no worktrees
    // for, because it is not a repository or could not be asked — is still a
    // checkout the user can open.
    if let Some(anchor) = anchor
        && !plans
            .iter()
            .any(|plan| plan.root.as_deref() == Some(anchor))
    {
        plans.push(RowPlan {
            name: None,
            branch: None,
            root: Some(anchor.to_path_buf()),
            open: None,
            is_main: true,
        });
    }

    // By name, then the repository's own checkout first: opening or closing a
    // worktree never reorders the rest.
    plans.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(main_row) = plans
        .iter()
        .position(|plan| plan.root.is_some() && plan.root == main)
    {
        let main_row = plans.remove(main_row);
        plans.insert(0, main_row);
    }
    plans
}

/// Asks for the name of a new worktree, and which Linear issue it is for.
///
/// The name is the whole answer: it names the directory the worktree is created
/// in and the branch that is created with it. Where it goes is shown rather
/// than asked; see [`worktrees_directory`].
///
/// With Linear connected, an issue can be linked first, and linking one names
/// the worktree after the branch Linear suggests for it. The name stays
/// editable — the issue is linked through the branch name, so a name that no
/// longer carries the identifier is a worktree Linear will not find, but that
/// is the user's call to make.
struct NameWorktree {
    name: Entity<InputField>,
    /// The project's worktree directory, shown so that the layout Bench
    /// imposes is visible before the worktree is made.
    directory: PathBuf,
    /// Absent when Linear is not connected, and the modal is just the name.
    issue_search: Option<IssueSearch>,
    linked: Option<Arc<Issue>>,
    confirm: Box<dyn Fn(SharedString, Option<Arc<Issue>>, &mut Window, &mut App)>,
}

struct IssueSearch {
    linear: Entity<Linear>,
    field: Entity<InputField>,
    results: Vec<Arc<Issue>>,
    selected: usize,
    searching: bool,
    error: Option<SharedString>,
    _search: Task<()>,
    _subscription: gpui::Subscription,
}

/// How many issues the picker lists. It is a picker, not the panel: past a
/// handful, typing is quicker than scrolling.
const ISSUE_RESULTS: usize = 6;

/// How long typing has to pause before the picker searches.
const ISSUE_SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);

impl NameWorktree {
    fn new(
        directory: PathBuf,
        issue: Option<Arc<Issue>>,
        window: &mut Window,
        cx: &mut Context<Self>,
        confirm: impl Fn(SharedString, Option<Arc<Issue>>, &mut Window, &mut App) + 'static,
    ) -> Self {
        let name = cx.new(|cx| InputField::new(window, cx, "Worktree name"));
        if let Some(issue) = &issue {
            name.read(cx)
                .editor()
                .clone()
                .set_text(&issue.branch_name, window, cx);
        }

        let issue_search = Linear::global(cx)
            .filter(|linear| linear.read(cx).is_connected())
            .map(|linear| {
                let field = cx.new(|cx| {
                    InputField::new(window, cx, "Link a Linear issue…")
                        .start_icon(IconName::MagnifyingGlass)
                });
                let modal = cx.entity().downgrade();
                let editor = field.read(cx).editor().clone();
                let subscription = editor.subscribe(
                    Box::new(move |event, _window, cx| {
                        if event == ui_input::ErasedEditorEvent::BufferEdited {
                            modal
                                .update(cx, |modal, cx| modal.search_issues(true, cx))
                                .ok();
                        }
                    }),
                    window,
                    cx,
                );
                IssueSearch {
                    linear,
                    field,
                    results: Vec::new(),
                    selected: 0,
                    searching: false,
                    error: None,
                    _search: Task::ready(()),
                    _subscription: subscription,
                }
            });

        let mut this = Self {
            name,
            directory,
            issue_search,
            linked: issue,
            confirm: Box::new(confirm),
        };
        if this.linked.is_none() {
            this.search_issues(false, cx);
        }
        this
    }

    /// Asks Linear for issues matching the search box. Empty, that is your own
    /// unfinished issues, so the likeliest ones are there before anything is
    /// typed.
    fn search_issues(&mut self, debounce: bool, cx: &mut Context<Self>) {
        let Some(search) = &mut self.issue_search else {
            return;
        };
        let query = search.field.read(cx).text(cx);
        search.searching = true;
        search._search = cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(ISSUE_SEARCH_DEBOUNCE).await;
            }
            let Ok(found) = this.update(cx, |this, cx| {
                this.issue_search
                    .as_ref()
                    .map(|search| search.linear.read(cx).search(&query, cx))
            }) else {
                return;
            };
            let Some(found) = found else {
                return;
            };
            let found = found.await;
            this.update(cx, |this, cx| {
                let Some(search) = &mut this.issue_search else {
                    return;
                };
                search.searching = false;
                search.selected = 0;
                match found {
                    Ok(mut issues) => {
                        issues.truncate(ISSUE_RESULTS);
                        search.results = issues;
                        search.error = None;
                    }
                    Err(error) => {
                        search.results.clear();
                        search.error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    /// Links the worktree to `issue` and names it after the issue's branch,
    /// then moves on to the name, which is the one thing left to confirm.
    fn link(&mut self, issue: Arc<Issue>, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.name.read(cx).editor().clone();
        editor.set_text(&issue.branch_name, window, cx);
        self.linked = Some(issue);
        window.focus(&editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn unlink(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.linked = None;
        if let Some(search) = &self.issue_search {
            window.focus(&search.field.focus_handle(cx), cx);
        }
        cx.notify();
    }

    /// Whether the issue search is where the keyboard is, which is when Enter
    /// and the arrow keys are about its results rather than the name.
    fn searching_issues(&self, window: &Window, cx: &App) -> bool {
        self.linked.is_none()
            && self
                .issue_search
                .as_ref()
                .is_some_and(|search| search.field.focus_handle(cx).contains_focused(window, cx))
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.searching_issues(window, cx) {
            let selected = self
                .issue_search
                .as_ref()
                .and_then(|search| search.results.get(search.selected).cloned());
            if let Some(issue) = selected {
                self.link(issue, window, cx);
            }
            return;
        }

        let name = self.name.read(cx).text(cx).trim().to_owned();
        // Nothing to do with a name git would refuse but wait for a better one.
        if !is_branch_name(&name) {
            return;
        }
        (self.confirm)(name.into(), self.linked.clone(), window, cx);
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn select_next(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, window, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_selection(-1, window, cx);
    }

    fn move_selection(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.searching_issues(window, cx) {
            cx.propagate();
            return;
        }
        let Some(search) = &mut self.issue_search else {
            return;
        };
        let count = search.results.len();
        if count == 0 {
            return;
        }
        search.selected = (search.selected as isize + by).rem_euclid(count as isize) as usize;
        cx.notify();
    }

    fn render_linked(&self, issue: &Issue, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .min_w_0()
            .gap_1p5()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .child(render_issue_state(issue))
            .child(
                Label::new(issue.identifier.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(issue.title.clone())
                        .size(LabelSize::Small)
                        .single_line()
                        .truncate(),
                ),
            )
            .child(
                IconButton::new("unlink-issue", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Unlink Issue"))
                    .on_click(cx.listener(|this, _, window, cx| this.unlink(window, cx))),
            )
    }

    fn render_search(&self, search: &IssueSearch, cx: &mut Context<Self>) -> impl IntoElement {
        let status = if let Some(error) = &search.error {
            Some(Label::new(error.clone()).color(Color::Error))
        } else if search.results.is_empty() {
            Some(
                Label::new(if search.searching {
                    "Searching…"
                } else {
                    "No issues found"
                })
                .color(Color::Muted),
            )
        } else {
            None
        };

        v_flex()
            .gap_1()
            .child(search.field.clone())
            .children(status.map(|status| div().px_2().child(status.size(LabelSize::Small))))
            .children(search.results.iter().enumerate().map(|(index, issue)| {
                let chosen = issue.clone();
                ListItem::new(("issue-result", index))
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(index == search.selected)
                    .start_slot(render_issue_state(issue))
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_1p5()
                            .child(
                                Label::new(issue.identifier.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(issue.title.clone())
                                    .size(LabelSize::Small)
                                    .single_line()
                                    .truncate(),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.link(chosen.clone(), window, cx);
                    }))
            }))
    }
}

impl Focusable for NameWorktree {
    /// Where the modal opens: on the issue search when there is an issue to
    /// pick, otherwise on the name.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.issue_search {
            Some(search) if self.linked.is_none() => search.field.focus_handle(cx),
            _ => self.name.focus_handle(cx),
        }
    }
}

impl EventEmitter<DismissEvent> for NameWorktree {}

impl ModalView for NameWorktree {}

impl Render for NameWorktree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let issue_section = match (&self.linked, &self.issue_search) {
            (Some(issue), _) => Some(self.render_linked(issue, cx).into_any_element()),
            (None, Some(search)) => Some(self.render_search(search, cx).into_any_element()),
            (None, None) => None,
        };
        v_flex()
            .key_context("NameWorktree")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .elevation_3(cx)
            .w(rems(34.))
            .p_3()
            .gap_2()
            .child(Label::new("New Worktree"))
            .children(issue_section)
            .child(self.name.clone())
            .child(
                Label::new(format!("{}/<name>", self.directory.display()))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }
}

/// Asks for the title a worktree's card leads with. What it is given is stored
/// as it is, empty included: an empty title is one the issue's title is not
/// put back into, and the card shows the worktree's name.
struct EditTitle {
    root: PathBuf,
    title: Entity<InputField>,
}

impl EditTitle {
    fn new(root: PathBuf, title: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let field = cx.new(|cx| InputField::new(window, cx, "Title"));
        field.read(cx).editor().clone().set_text(title, window, cx);
        Self { root, title: field }
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let title = self.title.read(cx).text(cx).trim().to_owned();
        let root = self.root.clone();
        WorktreeMetadataStore::global(cx).update(cx, |store, cx| {
            store.update(&root, |metadata| metadata.title = Some(title), cx);
        });
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Focusable for EditTitle {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.title.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for EditTitle {}

impl ModalView for EditTitle {}

impl Render for EditTitle {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("EditWorktreeTitle")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(34.))
            .p_3()
            .gap_2()
            .child(Label::new("Worktree Title"))
            .child(self.title.clone())
            .child(
                Label::new("Leave it empty to show the worktree's name instead.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }
}

/// A project a worktree can be made in, for [`ChooseProject`].
struct ProjectTarget {
    name: SharedString,
    from: Entity<Workspace>,
    repository: Entity<Repository>,
    key: ProjectGroupKey,
}

/// Asks which project a worktree for a Linear issue goes in. An issue does
/// not say which repository its work happens in, and a window can hold
/// several.
struct ChooseProject {
    issue: Arc<Issue>,
    targets: Vec<ProjectTarget>,
    selected: usize,
    focus_handle: FocusHandle,
    choose: Box<dyn Fn(ProjectTarget, &mut Window, &mut App)>,
}

impl ChooseProject {
    fn new(
        targets: Vec<ProjectTarget>,
        issue: Arc<Issue>,
        cx: &mut Context<Self>,
        choose: impl Fn(ProjectTarget, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            issue,
            targets,
            selected: 0,
            focus_handle: cx.focus_handle(),
            choose: Box::new(choose),
        }
    }

    fn choose_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.targets.len() {
            return;
        }
        let target = self.targets.remove(index);
        cx.emit(DismissEvent);
        (self.choose)(target, window, cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_index(self.selected, window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.selected = (self.selected + 1) % self.targets.len().max(1);
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.targets.len().max(1);
        self.selected = (self.selected + count - 1) % count;
        cx.notify();
    }
}

impl Focusable for ChooseProject {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for ChooseProject {}

impl ModalView for ChooseProject {}

impl Render for ChooseProject {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ChooseProject")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .elevation_3(cx)
            .w(rems(34.))
            .p_3()
            .gap_2()
            .child(Label::new(format!(
                "Create a worktree for {} in…",
                self.issue.identifier
            )))
            .children(self.targets.iter().enumerate().map(|(index, target)| {
                ListItem::new(("project-target", index))
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(index == self.selected)
                    .start_slot(
                        Icon::new(IconName::Folder)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(target.name.clone()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.choose_index(index, window, cx);
                    }))
            }))
    }
}

/// What the window shows while a worktree opens; see
/// [`WorktreePanel::open_worktree`]. It is a modal so that it holds the
/// keyboard: what you type while a worktree opens must not go to the one you
/// are leaving.
struct OpeningWorktree {
    name: SharedString,
    focus_handle: FocusHandle,
}

impl OpeningWorktree {
    fn new(name: SharedString, cx: &mut Context<Self>) -> Self {
        Self {
            name,
            focus_handle: cx.focus_handle(),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Focusable for OpeningWorktree {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for OpeningWorktree {}

impl ModalView for OpeningWorktree {}

impl Render for OpeningWorktree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .key_context("OpeningWorktree")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(34.))
            .p_3()
            .gap_2()
            .child(
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::Small)
                    .color(Color::Muted)
                    .with_rotate_animation(2),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        Label::new(format!("Opening {}…", self.name))
                            .single_line()
                            .truncate(),
                    )
                    .child(
                        Label::new("The worktree shows here once it has loaded.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
    }
}

/// Where a new worktree goes and what it checks out: a directory named after it
/// under the project's worktree directory, on a new branch of the same name off
/// whatever the repository has checked out.
fn new_worktree(key: &ProjectGroupKey, name: &str) -> (PathBuf, CreateWorktreeTarget) {
    (
        worktrees_directory(key).join(worktree_directory_name(name)),
        CreateWorktreeTarget::NewBranch {
            branch_name: name.to_owned(),
            base_sha: None,
        },
    )
}

/// The directory a worktree of this name lives in.
///
/// A branch name carries its own hierarchy — `feature/foo` is one name with a
/// prefix, not a path — and a directory of that name would put the worktree a
/// level below every other one of the project's, in a directory named after a
/// prefix it shares with others. The branch keeps the name it was given; the
/// directory flattens it, so every worktree of a project stays one directory
/// deep and is still named after its branch.
fn worktree_directory_name(name: &str) -> String {
    name.replace('/', "-")
}

/// Whether git will take this as a branch name.
///
/// Only the parts that decide whether to ask at all: a name git is certain to
/// refuse is one the panel should keep waiting on rather than send, since the
/// answer is an error notification either way. `/` is allowed — it is what
/// makes `feature/foo` one name — but not at either end, and not doubled,
/// which are the forms git rejects. Everything subtler than that is git's to
/// judge, and its refusal is shown.
fn is_branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('\\')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("//")
}

/// Where Bench keeps one project's worktrees: `~/bench/<project>`.
///
/// One fixed place, rather than beside the repository as git defaults to and as
/// `git.worktree_directory` configures. A project's worktrees are then a
/// directory listing rather than a search, they are all in one place whatever
/// the repository's own path is, and a repository directory stays a checkout
/// instead of becoming a container of checkouts.
fn worktrees_directory(key: &ProjectGroupKey) -> PathBuf {
    let project = project_root(key)
        .as_deref()
        .and_then(directory_name)
        .unwrap_or_else(|| "project".into());
    home_dir().join(WORKTREES_DIRECTORY).join(project.as_str())
}

/// The directory under `$HOME` that every project's worktrees go in.
const WORKTREES_DIRECTORY: &str = "bench";

/// The repository a project group's new worktrees are made in: the one behind
/// an open workspace of the group, preferring the one rooted where the group
/// itself is.
fn group_repository(
    key: &ProjectGroupKey,
    open_worktrees: &[OpenWorktree],
    cx: &App,
) -> Option<Entity<Repository>> {
    let anchor = project_root(key);
    let repositories: Vec<Entity<Repository>> = open_worktrees
        .iter()
        .filter_map(|open| workspace_repository(open, cx))
        .collect();
    repositories
        .iter()
        .find(|repository| anchor.is_some() && repository_anchor(repository, cx) == anchor)
        .or_else(|| repositories.first())
        .cloned()
}

/// Drops the worktrees that live inside a worktree of another project.
///
/// A project that keeps clones of other repositories inside it — a
/// `repos/` folder listed in a manifest — gets a copy of each clone inside
/// every one of its worktrees, and each copy is a git worktree of that clone.
/// Once the clone is a project of its own, every one of those copies is in
/// its worktree list, all named after the folder they sit in. They are part
/// of the worktree that holds them, not worktrees to switch to, so they are
/// not rows.
///
/// Only when the checkout directly holding it is another *project's*: a
/// repository that keeps its worktrees inside its own checkout, such as
/// `.worktrees/`, lists them as usual. The repository's own checkout and anything the window has open stay
/// listed, as they do for the settings.
fn hide_worktrees_inside_other_checkouts(rows: &mut [RepositoryRow]) {
    let checkouts: Vec<(usize, PathBuf)> = rows
        .iter()
        .enumerate()
        .flat_map(|(project, row)| {
            row.worktrees
                .iter()
                .filter_map(move |worktree| Some((project, worktree.root.clone()?)))
        })
        .collect();
    for (project, row) in rows.iter_mut().enumerate() {
        row.worktrees.retain(|worktree| {
            let Some(root) = worktree.root.as_deref() else {
                return true;
            };
            // The checkout it is directly inside. A clone inside a project
            // can keep worktrees in its own checkout too, and those are inside
            // the outer project's checkout as well; the nearest one decides.
            let holder = checkouts
                .iter()
                .filter(|(_, checkout)| root != checkout && root.starts_with(checkout))
                .max_by_key(|(_, checkout)| checkout.components().count());
            worktree.is_main
                || worktree.workspace.is_some()
                || holder.is_none_or(|(holder, _)| *holder == project)
        });
    }
}

/// A worktree of another repository that lives inside a worktree being
/// deleted: a clone under `repos/`, checked out for this worktree.
#[derive(Clone, Debug)]
struct NestedWorktree {
    path: PathBuf,
    /// The `.git` directory of the repository it is a worktree of.
    common_dir: PathBuf,
}

impl NestedWorktree {
    /// How the prompts name it: its path inside the worktree, which is what
    /// tells two clones apart.
    fn label(&self, root: &Path) -> String {
        self.path
            .strip_prefix(root)
            .unwrap_or(&self.path)
            .to_string_lossy()
            .into_owned()
    }
}

/// How deep below a worktree nested repositories are looked for. `repos/x`
/// is two; a little more covers layouts that group them further.
const NESTED_DEPTH: usize = 3;

/// Directories never worth descending into when looking for nested
/// repositories: they are large, and what is in them is built or fetched, not
/// checked out.
const SKIPPED_DIRECTORIES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "build",
    "dist",
    ".dart_tool",
    "Pods",
    ".gradle",
];

/// The linked worktrees of other repositories inside `root`.
///
/// A linked worktree is a directory whose `.git` is a file pointing at
/// `<repository>/.git/worktrees/<name>`; a clone of its own has a `.git`
/// directory and is left alone, since deleting it is deleting the clone.
/// The search stops at each one it finds — what is inside a nested repository
/// is that repository's business — and does not go deep, because a worktree
/// can hold hundreds of thousands of files.
async fn nested_worktrees(fs: Arc<dyn Fs>, root: PathBuf) -> Vec<NestedWorktree> {
    use futures::StreamExt as _;

    let mut found = Vec::new();
    let mut level = vec![root];
    for _ in 0..NESTED_DEPTH {
        let mut next = Vec::new();
        for directory in level {
            let Ok(mut children) = fs.read_dir(&directory).await else {
                continue;
            };
            while let Some(child) = children.next().await {
                let Ok(child) = child else {
                    continue;
                };
                let skipped = child
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| SKIPPED_DIRECTORIES.contains(&name));
                if skipped || !fs.is_dir(&child).await {
                    continue;
                }
                match linked_worktree_common_dir(fs.as_ref(), &child).await {
                    Some(common_dir) => found.push(NestedWorktree {
                        path: child,
                        common_dir,
                    }),
                    None => {
                        if !fs.is_dir(&child.join(".git")).await {
                            next.push(child);
                        }
                    }
                }
            }
        }
        level = next;
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// The repository `directory` is a linked worktree of, from its `.git` file:
/// `gitdir: <repository>/.git/worktrees/<name>`, whose `commondir` names the
/// repository's `.git`.
async fn linked_worktree_common_dir(fs: &dyn Fs, directory: &Path) -> Option<PathBuf> {
    let dot_git = fs.load(&directory.join(".git")).await.ok()?;
    let gitdir = PathBuf::from(dot_git.strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_relative() {
        directory.join(gitdir)
    } else {
        gitdir
    };
    match fs.load(&gitdir.join("commondir")).await {
        Ok(common_dir) => {
            let common_dir = PathBuf::from(common_dir.trim());
            Some(if common_dir.is_relative() {
                normalize(&gitdir.join(common_dir))
            } else {
                common_dir
            })
        }
        // `<repository>/.git/worktrees/<name>` is two levels below it.
        Err(_) => Some(gitdir.parent()?.parent()?.to_path_buf()),
    }
}

/// `a/b/../c` as `a/c`, without asking the filesystem: git writes
/// `commondir` relative to the worktree's own git directory.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            component => normalized.push(component),
        }
    }
    normalized
}

/// Removes a nested worktree through the repository it belongs to, the way
/// `git worktree remove` in that repository would.
async fn remove_nested(
    fs: &Arc<dyn Fs>,
    git: Option<&Path>,
    nested: &NestedWorktree,
    force: bool,
) -> anyhow::Result<()> {
    let repository = fs.open_repo(&nested.common_dir, git)?;
    repository.remove_worktree(nested.path.clone(), force).await
}

/// Git's own reason for refusing, which is the part worth reading: the last
/// line that says something, without the "Git command failed" wrapping.
fn git_reason(error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    message
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty() && !line.starts_with("Git command failed"))
        .map(|line| line.trim_start_matches("fatal: ").to_owned())
        .unwrap_or(message)
}

/// Closes every workspace showing `root` or anything inside it. Resolves to
/// whether they all closed — one can stop to ask about unsaved changes.
///
/// A workspace whose project is the worktree itself — as a worktree is while
/// git has not yet told its project which repository it belongs to — is
/// closed along with its project. Keeping the project would reopen the very
/// worktree being deleted.
fn close_workspaces_inside(
    multi_workspace: &mut MultiWorkspace,
    root: &Path,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) -> Task<anyhow::Result<bool>> {
    let doomed = workspaces_inside(multi_workspace, root, cx);
    if doomed.is_empty() {
        return Task::ready(Ok(true));
    }
    let own_project = doomed
        .iter()
        .any(|workspace| is_inside(&workspace.read(cx).project_group_key(cx), root));
    let intent = if own_project {
        RemovalIntent::CloseProject
    } else {
        RemovalIntent::KeepProject
    };
    multi_workspace.remove(doomed, intent, window, cx)
}

/// After `root` is deleted: closes whatever still shows it and removes any
/// project made of it. See [`WorktreePanel::delete_worktree`] for why there
/// can be any.
fn clear_deleted(
    multi_workspace: &mut MultiWorkspace,
    root: &Path,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) {
    let leftovers = workspaces_inside(multi_workspace, root, cx);
    if !leftovers.is_empty() {
        log::info!(
            "closing {} workspace(s) still showing the deleted {}",
            leftovers.len(),
            root.display()
        );
        multi_workspace
            .remove(leftovers, RemovalIntent::CloseProject, window, cx)
            .detach_and_log_err(cx);
    }
    let groups: Vec<ProjectGroupKey> = multi_workspace
        .project_group_keys()
        .into_iter()
        .filter(|key| is_inside(key, root))
        .collect();
    for key in groups {
        log::info!("removing the project made of the deleted {}", root.display());
        multi_workspace
            .remove_project_group(&key, window, cx)
            .detach_and_log_err(cx);
    }
}

fn workspaces_inside(
    multi_workspace: &MultiWorkspace,
    root: &Path,
    cx: &App,
) -> Vec<Entity<Workspace>> {
    multi_workspace
        .workspaces()
        .filter(|workspace| {
            workspace_root(workspace, cx).is_some_and(|workspace_root| workspace_root.starts_with(root))
        })
        .cloned()
        .collect()
}

/// Whether a project is made of `root` or of something inside it.
fn is_inside(key: &ProjectGroupKey, root: &Path) -> bool {
    let mut paths = key.path_list().ordered_paths().peekable();
    paths.peek().is_some() && paths.all(|path| path.starts_with(root))
}

/// The directory a project group stands for: the main worktree of the
/// repository it was keyed on. A group with several roots is named by the
/// first, which is the one the panel's worktree rows hang off.
fn project_root(key: &ProjectGroupKey) -> Option<PathBuf> {
    key.path_list().ordered_paths().next().cloned()
}

/// Whether a workspace is showing nothing at all, which is the state a window
/// opened without a folder starts in.
fn is_empty_workspace(workspace: &Entity<Workspace>, cx: &App) -> bool {
    workspace
        .read(cx)
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .is_none()
}

/// The worktrees of a repository nothing is open for, asked of git.
///
/// Every failure is one row's worth of missing detail rather than an error to
/// raise: a project can be a plain directory, or one whose repository has gone
/// away since it was added, and neither is something to interrupt the user
/// over. Logged, because a git that cannot be run at all is worth finding in a
/// log.
async fn worktrees_on_disk(fs: Arc<dyn Fs>, root: PathBuf) -> Vec<GitWorktree> {
    if discover_root_repo_common_dir(&root, fs.as_ref())
        .await
        .is_none()
    {
        return Vec::new();
    }

    let git = which::which("git").ok();
    let repository = match fs.open_repo(&root.join(".git"), git.as_deref()) {
        Ok(repository) => repository,
        Err(error) => {
            log::warn!("opening the repository at {}: {error:#}", root.display());
            return Vec::new();
        }
    };
    match repository.worktrees().await {
        Ok(worktrees) => worktrees,
        Err(error) => {
            log::warn!("listing the worktrees of {}: {error:#}", root.display());
            Vec::new()
        }
    }
}

/// What git says about a worktree, from one `git status`: its changes, its
/// branch against its upstream, and when a file last changed.
///
/// The last edit is the newest of the changed files, or the last commit when
/// nothing has changed. Git's word rather than a walk of the directory, which
/// would visit every build artefact and dependency, and whose newest file is
/// as likely to be a cache written by a language server as anything anyone
/// edited. Everything is left at its default when git cannot say: not a
/// repository, no commits, no git.
/// `git fetch` in the worktree at `root`, quietly: it runs in the background,
/// with nobody to answer a credentials prompt.
async fn fetch_upstream(root: &Path) -> anyhow::Result<()> {
    let git = which::which("git").map_err(|_| anyhow::anyhow!("git is not installed"))?;
    let output = util::command::new_command(&git)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true)
        .args(["fetch", "--quiet"])
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "git fetch in {} failed:\n{}",
        root.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// `git pull --ff-only`, then `git push`, as asked, in the worktree at `root`.
async fn sync_with_upstream(root: PathBuf, push: bool, pull: bool) -> anyhow::Result<()> {
    let git = which::which("git").map_err(|_| anyhow::anyhow!("git is not installed"))?;
    let run = |args: &'static [&'static str]| {
        let mut command = util::command::new_command(&git);
        command
            .current_dir(&root)
            // Nobody is at a terminal to answer a credentials prompt, and
            // waiting for one would leave the button spinning forever.
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args);
        async move {
            let output = command.output().await?;
            anyhow::ensure!(
                output.status.success(),
                "git {} failed:\n{}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
            anyhow::Ok(())
        }
    };
    if pull {
        run(&["pull", "--ff-only"]).await?;
    }
    if push {
        run(&["push"]).await?;
    }
    Ok(())
}

async fn worktree_status(fs: Arc<dyn Fs>, root: PathBuf) -> WorktreeStatus {
    let Some(git) = which::which("git").ok() else {
        return WorktreeStatus::default();
    };
    let run = |args: &'static [&'static str]| {
        let mut command = util::command::new_command(&git);
        command.current_dir(&root).kill_on_drop(true).args(args);
        async move {
            let output = command.output().await.log_err()?;
            output.status.success().then_some(output.stdout)
        }
    };

    let committed = run(&["log", "-1", "--format=%ct"])
        .await
        .and_then(|stdout| String::from_utf8(stdout).ok()?.trim().parse::<u64>().ok())
        .map(|seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds));

    // Without optional locks: a plain `git status` refreshes the index, and
    // the lock it takes to do that can make an agent's own git command in the
    // same worktree fail.
    let output = run(&[
        "--no-optional-locks",
        "status",
        "--porcelain=v1",
        "--branch",
        "-z",
        "--untracked-files=normal",
    ])
    .await
    .unwrap_or_default();
    let parsed = parse_status(&output);

    let mut last_edit = committed;
    for path in &parsed.paths {
        // A deleted file has no time of its own, and is not what anyone
        // means by the last edit.
        let Some(metadata) = fs.metadata(&root.join(path)).await.ok().flatten() else {
            continue;
        };
        let modified = metadata.mtime.timestamp_for_user();
        if last_edit.is_none_or(|newest| modified > newest) {
            last_edit = Some(modified);
        }
    }
    WorktreeStatus {
        last_edit,
        changes: parsed.paths.len(),
        ahead_behind: parsed.ahead_behind,
    }
}

struct ParsedStatus<'a> {
    paths: Vec<&'a str>,
    ahead_behind: Option<(u32, u32)>,
}

/// `git status --porcelain=v1 --branch -z`: first a `## ` entry naming the
/// branch and its upstream, then each changed path as two status letters, a
/// space and the path. A rename or copy is followed by an entry of its own
/// holding the path it came from, which is skipped.
fn parse_status(status: &[u8]) -> ParsedStatus<'_> {
    let mut parsed = ParsedStatus {
        paths: Vec::new(),
        ahead_behind: None,
    };
    let mut entries = status.split(|byte| *byte == 0);
    while let Some(entry) = entries.next() {
        let Ok(entry_text) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some(branch) = entry_text.strip_prefix("## ") {
            parsed.ahead_behind = ahead_behind(branch);
            continue;
        }
        let Some(path) = entry_text.get(3..).filter(|path| !path.is_empty()) else {
            continue;
        };
        parsed.paths.push(path);
        if matches!(entry.first(), Some(b'R' | b'C')) {
            entries.next();
        }
    }
    parsed
}

/// The ahead and behind counts out of the branch line of `git status
/// --branch`: `main...origin/main [ahead 2, behind 5]`, with either count left
/// out when it is zero, and no brackets when both are. A branch whose line
/// names no upstream — no `...` — has none to be ahead of, and neither does
/// one whose upstream is `[gone]`.
fn ahead_behind(branch: &str) -> Option<(u32, u32)> {
    let (_, tracking) = branch.split_once("...")?;
    let counts = match tracking.split_once(" [") {
        Some((_, counts)) => counts.strip_suffix(']')?,
        None => return Some((0, 0)),
    };
    let mut ahead = 0;
    let mut behind = 0;
    for count in counts.split(", ") {
        match count.split_once(' ') {
            Some(("ahead", number)) => ahead = number.parse().ok()?,
            Some(("behind", number)) => behind = number.parse().ok()?,
            _ => return None,
        }
    }
    Some((ahead, behind))
}

/// Where the panel's one width is kept.
///
/// A dock's size is stored per workspace — the key is `<workspace>:<panel>` —
/// and Bench has one workspace per worktree, so what a worktree remembers is
/// the width it was last left at. Drag the edge in one worktree, switch to one
/// that was closed at the time, and the panel jumps. The width belongs to the
/// panel, so it is kept once, here, under no workspace at all.
const WIDTH_NAMESPACE: &str = "worktree_panel";
const WIDTH_KEY: &str = "width";

/// The width every worktree's panel is using, for as long as the app is
/// running.
///
/// In memory as well as on disk because [`WorktreePanel::match_shared_width`]
/// asks on every draw, and a panel redrawing is not a reason to read the
/// database.
#[derive(Clone, Copy, Default)]
struct SharedWidth(Option<PanelSizeState>);

impl Global for SharedWidth {}

fn shared_width(cx: &mut App) -> Option<PanelSizeState> {
    if let Some(width) = cx.try_global::<SharedWidth>() {
        return width.0;
    }
    // First ask of the session: what the last one left behind.
    let width = stored_width(cx);
    cx.set_global(SharedWidth(width));
    width
}

fn set_shared_width(width: PanelSizeState, cx: &mut App) {
    if shared_width(cx) == Some(width) {
        return;
    }
    cx.set_global(SharedWidth(Some(width)));
    store_width(width, cx);
}

fn stored_width(cx: &App) -> Option<PanelSizeState> {
    db::kvp::KeyValueStore::global(cx)
        .scoped(WIDTH_NAMESPACE)
        .read(WIDTH_KEY)
        .log_err()
        .flatten()
        .and_then(|width| serde_json::from_str::<PanelSizeState>(&width).log_err())
}

fn store_width(width: PanelSizeState, cx: &mut App) {
    let Some(width) = serde_json::to_string(&width).log_err() else {
        return;
    };
    let store = db::kvp::KeyValueStore::global(cx);
    cx.background_spawn(async move {
        store
            .scoped(WIDTH_NAMESPACE)
            .write(WIDTH_KEY.to_owned(), width)
            .await
            .log_err();
    })
    .detach();
}

/// Whether a row's name matches what was typed in the filter box.
///
/// A subsequence match, the same shape as every other fuzzy filter: `dh`
/// finds `data-hub`, `fjl` finds `fix-job-log`. Scoring and ranking are what
/// a fuzzy *picker* needs; this is a filter over a list that is already in the
/// order the user wants, so the only question is in or out.
fn matches_filter(name: &str, filter: &str) -> bool {
    let name = name.to_lowercase();
    let mut name = name.chars();
    filter
        .to_lowercase()
        .chars()
        .all(|wanted| name.any(|character| character == wanted))
}

/// The last component of a path, as a row label.
fn directory_name(path: &Path) -> Option<SharedString> {
    Some(SharedString::from(
        path.file_name()?.to_string_lossy().into_owned(),
    ))
}

/// The root directory of the worktree a workspace is showing.
fn workspace_root(workspace: &Entity<Workspace>, cx: &App) -> Option<PathBuf> {
    workspace
        .read(cx)
        .project()
        .read(cx)
        .worktree_paths(cx)
        .ordered_pairs()
        .next()
        .map(|(_, own)| own.clone())
}

/// Every worktree of the repositories behind a group's open workspaces.
///
/// Taken from the repository snapshot rather than asked of git, so the panel
/// stays a pure derivation. Worktrees of one repository are the same list
/// whichever of its checkouts is asked, so this dedupes by path: a group with
/// four worktrees open would otherwise report each of them four times.
fn linked_worktrees(
    open_worktrees: &[OpenWorktree],
    cx: &App,
) -> Vec<(GitWorktree, Entity<Repository>)> {
    let mut linked: Vec<(GitWorktree, Entity<Repository>)> = Vec::new();
    for open in open_worktrees {
        let Some(repository) = workspace_repository(open, cx) else {
            continue;
        };
        for worktree in repository.read(cx).snapshot().linked_worktrees.iter() {
            if !linked
                .iter()
                .any(|(listed, _)| listed.path == worktree.path)
            {
                linked.push((worktree.clone(), repository.clone()));
            }
        }
    }
    linked
}

/// The repository a workspace's own checkout belongs to.
///
/// A project holds a repository for every git directory it finds, and that
/// includes repositories nested inside the checkout — clones kept under a
/// `repos/` folder, or package managers' checkouts under `build/`. Their
/// worktrees are not the project's, so only the innermost repository at or
/// above the workspace root counts; anything below it is someone else's.
fn workspace_repository(open: &OpenWorktree, cx: &App) -> Option<Entity<Repository>> {
    let root = open.root.as_deref()?;
    let project = open.workspace.read(cx).project().read(cx);
    project
        .repositories(cx)
        .values()
        .filter(|repository| {
            root.starts_with(&repository.read(cx).snapshot().work_directory_abs_path)
        })
        .max_by_key(|repository| {
            repository
                .read(cx)
                .snapshot()
                .work_directory_abs_path
                .components()
                .count()
        })
        .cloned()
}

/// The panel's label for a worktree: its own directory name, or `main` for the
/// repository's original checkout.
///
/// `directory_name` says "main worktree" there, which is a sentence rather than
/// a name; the panel's rows are names.
/// What `gh` says about one project's branches, as the state of each branch's
/// pull request.
///
/// A project it cannot answer for — not a GitHub remote, no `gh`, not signed
/// in — has no pull requests as far as this panel is concerned. A worktree row
/// has nowhere to explain itself, and the git panel's Pull Requests tab is
/// where the reason is shown; here it is logged and the icons stay plain.
async fn pull_requests_by_branch(root: PathBuf) -> HashMap<SharedString, PullRequestState> {
    match github_cli::pull_requests(&root, None, github_cli::DEFAULT_LIMIT).await {
        Ok(pull_requests) => by_branch(pull_requests),
        Err(unavailable) => {
            log::debug!(
                "listing the pull requests of {}: {}",
                root.display(),
                unavailable.message()
            );
            HashMap::new()
        }
    }
}

/// One state per branch, out of a list that can hold several for the same one.
///
/// `gh` answers newest first, and a branch carries every pull request ever
/// opened from it. The newest is the one the row is about — except that an
/// open one outranks an older merged one, because a branch with something
/// still open on it is open.
fn by_branch(pull_requests: Vec<PullRequest>) -> HashMap<SharedString, PullRequestState> {
    let mut by_branch: HashMap<SharedString, PullRequestState> = HashMap::new();
    for pull_request in pull_requests {
        let first = !by_branch.contains_key(&pull_request.head_ref);
        let still_open = matches!(
            pull_request.state,
            PullRequestState::Open | PullRequestState::Draft
        );
        if first || still_open {
            by_branch.insert(pull_request.head_ref, pull_request.state);
        }
    }
    by_branch
}

/// The branch a worktree has checked out, as a pull request names it.
///
/// `refs/heads/feature/thing` is the branch `feature/thing`: the ref is what
/// git reports and the short name is what a pull request's head is. A worktree
/// with a detached head has no branch, and so can have no pull request.
fn worktree_branch(worktree: &GitWorktree) -> Option<SharedString> {
    let ref_name = worktree.ref_name.as_ref()?;
    Some(SharedString::from(
        ref_name.strip_prefix("refs/heads/").unwrap_or(ref_name).to_string(),
    ))
}

fn worktree_display_name(worktree: &GitWorktree, main: Option<&Path>) -> SharedString {
    if worktree.is_main {
        return "main".into();
    }
    SharedString::from(worktree.directory_name(main))
}

impl Render for WorktreePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.match_shared_width(window, cx);

        let closed = self.closed_project_roots(cx);
        self.discover(&closed, cx);
        let tree = self.tree(cx);
        let filter = self.filter(cx);
        let roots: Vec<PathBuf> = tree.iter().filter_map(|row| project_root(&row.key)).collect();
        self.scan_pull_requests(&roots, cx);
        let worktree_roots: Vec<(PathBuf, bool)> = tree
            .iter()
            .flat_map(|row| row.worktrees.iter())
            .filter_map(|worktree| Some((worktree.root.clone()?, worktree.is_main)))
            .collect();
        self.scan_statuses(&worktree_roots, cx);
        // A worktree whose issue Linear has now named, and which has no title
        // yet, takes the issue's: one made before titles were stored, or for
        // a branch named after an issue. After this draw, since storing it
        // tells every panel to draw again.
        let untitled: Vec<(PathBuf, SharedString)> = tree
            .iter()
            .flat_map(|row| row.worktrees.iter())
            .filter(|worktree| worktree.title.is_none())
            .filter_map(|worktree| Some((worktree.root.clone()?, worktree.issue.as_ref()?.title.clone())))
            .collect();
        if !untitled.is_empty() {
            cx.defer(move |cx| {
                WorktreeMetadataStore::global(cx).update(cx, |store, cx| {
                    for (root, title) in &untitled {
                        store.fill_title(root, title, cx);
                    }
                });
            });
        }
        if let Some(linear) = Linear::global(cx) {
            let branches: Vec<SharedString> = tree
                .iter()
                .flat_map(|row| row.worktrees.iter())
                .filter_map(|worktree| worktree.branch.clone())
                .collect();
            let ids: Vec<String> = tree
                .iter()
                .flat_map(|row| row.worktrees.iter())
                .filter_map(|worktree| worktree.linked_issue.as_ref())
                .map(|linked| linked.id.clone())
                .collect();
            linear.update(cx, |linear, cx| {
                linear.look_up_ids(ids.iter().map(String::as_str), cx);
                linear.look_up_branches(branches.iter().map(|branch| branch.as_ref()), cx);
            });
        }
        let mut worktree_index = 0;

        v_flex()
            .key_context("WorktreePanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .w_full()
                    .px_1p5()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.filter.render(window, cx)),
                    )
                    .child(
                        IconButton::new("add-project", IconName::FolderAdd)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Add Project"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_project(window, cx);
                            })),
                    ),
            )
            .when(tree.is_empty(), |this| {
                this.child(
                    v_flex().p_4().gap_1().child(
                        Label::new("No projects added")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .children(tree.iter().enumerate().map(|(index, row)| {
                let project_matches = filter
                    .as_ref()
                    .is_none_or(|filter| matches_filter(&row.name, filter));
                // While filtering, a project is shown open whatever its
                // collapsed state: hiding a match behind a chevron makes the
                // filter say a thing exists and then refuse to show it.
                let collapsed = filter.is_none() && self.is_collapsed(&row.key, cx);
                let worktrees: Vec<_> = if collapsed {
                    Vec::new()
                } else {
                    row.worktrees
                        .iter()
                        .filter(|worktree| match &filter {
                            // A project whose own name matches keeps all of
                            // its worktrees: you asked for the project.
                            Some(filter) => {
                                project_matches || matches_filter(&worktree.name, filter)
                            }
                            None => true,
                        })
                        .map(|worktree| {
                            let element = self.render_worktree(worktree, worktree_index, cx);
                            worktree_index += 1;
                            element.into_any_element()
                        })
                        .collect()
                };
                // A project with nothing left under it is only in the way,
                // unless it is itself what was asked for.
                if !project_matches && worktrees.is_empty() && filter.is_some() {
                    return div().into_any_element();
                }
                v_flex()
                    .child(self.render_repository(row, index, cx))
                    .children(worktrees)
                    .into_any_element()
            }))
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                gpui::deferred(
                    gpui::anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl Focusable for WorktreePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorktreePanel {}

impl Panel for WorktreePanel {
    fn persistent_name() -> &'static str {
        "WorktreePanel"
    }

    fn panel_key() -> &'static str {
        "WorktreePanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(240.)
    }

    /// The shared width, applied before the panel's first draw rather than
    /// after it. Correcting the width a frame later is a flicker on every
    /// switch into a worktree whose panel is new.
    fn initial_size_state(&self, _window: &Window, cx: &App) -> PanelSizeState {
        cx.try_global::<SharedWidth>()
            .and_then(|shared| shared.0)
            .or_else(|| stored_width(cx))
            .unwrap_or_default()
    }

    /// The dock calls this when the user has finished resizing. The width
    /// they chose becomes the width every worktree uses; see
    /// [`Self::match_shared_width`].
    ///
    /// Deferred, and this one is not optional: the dock calls it from inside
    /// its own update, so reading the dock here — which is the only way to
    /// learn the width it just settled on — is a double lease and a panic. It
    /// crashed on every drag.
    fn size_state_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let panel = cx.entity();
        window.defer(cx, move |_window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            let dock = workspace
                .read(cx)
                .dock_at_position(DockPosition::Left)
                .clone();
            let Some(width) = dock.read(cx).stored_panel_size_state(&panel) else {
                return;
            };
            set_shared_width(width, cx);
        });
    }

    /// Whether the dock opens on this panel as the workspace is built, which is
    /// what keeps the panel on screen across a switch of worktree.
    ///
    /// A dock's open state belongs to a workspace, and Bench has one workspace
    /// per worktree, so a worktree the window has not held before starts with
    /// every dock shut. That is the panel disappearing at the moment it is
    /// used: creating a worktree from the panel, or clicking a worktree that
    /// has never been open, ends in a window with no panel in it.
    ///
    /// So it follows the window rather than the worktree — out in one worktree
    /// is out in the next, which is the same reason a resize in one is copied
    /// to the others; see [`Self::share_width_across_worktrees`]. A panel the
    /// user has closed everywhere stays closed, and the first worktree of a
    /// fresh window has no sibling to take after, so neither case is disturbed.
    fn starts_open(&self, _window: &Window, cx: &App) -> bool {
        self.showing_in_another_worktree(cx)
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        Some(if self.is_showing(cx) {
            IconName::ThreadsSidebarLeftOpen
        } else {
            IconName::ThreadsSidebarLeftClosed
        })
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Worktree Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    /// Ahead of the project panel: which worktree you are in is the choice that
    /// decides what the file tree is even showing.
    ///
    /// `0` is also the agent panel's, and the left dock is where that one goes
    /// by default — two panels in one dock with the same priority is an
    /// assertion failure in debug builds, which is to say the app does not
    /// start. Bench does not dock the agent panel (`zed::AGENT_PANEL`), so the
    /// number is free; putting it back means giving this one another.
    fn activation_priority(&self) -> u32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;
    use project::project_settings::ProjectSettings;
    use project::{FakeFs, Project, WorktreeSettings};
    use settings::SettingsStore;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use workspace::ProjectGroup;
    use workspace::dock::PanelSizeState;

    /// Activating a workspace ends in `Workspace::fallback_focus_handle`,
    /// which reads every panel in every dock — this one included. Any caller
    /// that still holds this panel's lease at that point panics with "cannot
    /// read WorktreePanel while it is already being updated", and a lease is
    /// what every `cx.listener` click handler has.
    ///
    /// This drives `activate` from inside an update, which is the lease a
    /// click gives, so it fails exactly where a click would.
    #[gpui::test]
    async fn activating_a_row_does_not_double_lease_the_panel(cx: &mut TestAppContext) {
        let (_fs, multi_workspace, workspaces, panels, mut cx) = worktree_panels(cx, 1).await;
        let workspace = workspaces[0].clone();
        let panel = panels[0].clone();

        workspace.read_with(&mut cx, |workspace, cx| {
            let dock = workspace.dock_at_position(DockPosition::Left).read(cx);
            assert!(dock.is_open(), "the dock has to be open to be walked");
            assert!(
                dock.active_panel().is_some_and(
                    |active| active.persistent_name() == WorktreePanel::persistent_name()
                ),
                "and this panel has to be the active one"
            );
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.activate(workspace.clone(), window, cx);
        });
        cx.run_until_parked();

        multi_workspace.read_with(&mut cx, |multi_workspace, _| {
            assert_eq!(multi_workspace.workspace(), &workspace);
        });
    }

    /// A window with `count` worktrees open, each workspace carrying its own
    /// `WorktreePanel` in an open left dock — which is what the panel's docked
    /// behaviour needs in order to be exercised at all.
    async fn worktree_panels(
        cx: &mut TestAppContext,
        count: usize,
    ) -> (
        Arc<FakeFs>,
        Entity<MultiWorkspace>,
        Vec<Entity<Workspace>>,
        Vec<Entity<WorktreePanel>>,
        VisualTestContext,
    ) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            // The filter box is an `InputField`, which needs the editor the
            // application registers at startup; see `editor::init`.
            editor::init(cx);
            ProjectSettings::register(cx);
            WorktreeSettings::register(cx);
        });

        let fs = FakeFs::new(cx.executor());
        // Opening a project the window has only a row for goes through
        // `Workspace::new_local`, which reads the filesystem off the app.
        cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
        let mut projects = Vec::new();
        for index in 0..count {
            let root = format!("/repo-{index}");
            fs.create_dir(root.as_ref()).await.expect("worktree dir");
            fs.insert_file(format!("{root}/file.txt"), b"hi".to_vec())
                .await;
            projects.push(Project::test(fs.clone(), [root.as_ref()], cx).await);
        }

        // A window always holds a workspace, so with no projects asked for it
        // holds one with no folder in it.
        let first = if projects.is_empty() {
            Project::test(fs.clone(), [], cx).await
        } else {
            projects.remove(0)
        };
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(first, window, cx));
        let multi_workspace = window.root(cx).expect("the window has a multi workspace");
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        let mut workspaces = vec![multi_workspace.read_with(&mut cx, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        })];
        for project in projects {
            workspaces.push(
                multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
                    multi_workspace.test_add_workspace(project, window, cx)
                }),
            );
        }

        // In the dock *and* open: a panel merely registered is never read by
        // `fallback_focus_handle`, and never drawn.
        let panels = cx.update(|window, cx| {
            workspaces
                .iter()
                .map(|workspace| {
                    let panel = cx.new(|cx| {
                        WorktreePanel::new(
                            workspace.downgrade(),
                            multi_workspace.downgrade(),
                            window,
                            cx,
                        )
                    });
                    workspace.update(cx, |workspace, cx| {
                        workspace.add_panel(panel.clone(), window, cx);
                        workspace.open_panel::<WorktreePanel>(window, cx);
                    });
                    panel
                })
                .collect()
        });
        cx.run_until_parked();

        (fs, multi_workspace, workspaces, panels, cx)
    }

    /// A closed panel is only its button, so the button has to say the panel is
    /// closed — which is what Zed's own sidebar icon does, and why this borrows
    /// it rather than drawing a branch that never changes.
    #[gpui::test]
    async fn the_panel_button_says_whether_the_panel_is_open(cx: &mut TestAppContext) {
        let (_fs, _multi_workspace, workspaces, panels, mut cx) = worktree_panels(cx, 1).await;

        let icon = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| panels[0].read(cx).icon(window, cx))
        };
        assert_eq!(icon(&mut cx), Some(IconName::ThreadsSidebarLeftOpen));

        workspaces[0].update_in(&mut cx, |workspace, window, cx| {
            workspace.close_panel::<WorktreePanel>(window, cx);
        });
        cx.run_until_parked();

        assert_eq!(icon(&mut cx), Some(IconName::ThreadsSidebarLeftClosed));
    }

    /// Opening a worktree the window has not held before builds a workspace,
    /// and a workspace is built with its docks shut — so the panel the user
    /// opened the worktree *from* was gone the moment they used it, which is
    /// worst for the one route that always lands in a new workspace: creating
    /// a worktree.
    #[gpui::test]
    async fn a_worktree_opened_next_keeps_the_panel_out(cx: &mut TestAppContext) {
        let (fs, multi_workspace, _workspaces, _panels, mut cx) = worktree_panels(cx, 1).await;

        let opened = open_worktree_workspace(&fs, &multi_workspace, &mut cx).await;

        opened.read_with(&mut cx, |workspace, cx| {
            let dock = workspace.dock_at_position(DockPosition::Left).read(cx);
            assert!(dock.is_open(), "the new worktree opens with the panel out");
            assert!(
                dock.active_panel().is_some_and(
                    |panel| panel.persistent_name() == WorktreePanel::persistent_name()
                ),
                "and it is this panel that is out, not another of the dock's"
            );
        });
    }

    /// The other half of it: the panel follows the window, so a user who has
    /// closed it is not given it back by opening a worktree.
    #[gpui::test]
    async fn a_worktree_opened_next_keeps_the_panel_closed(cx: &mut TestAppContext) {
        let (fs, multi_workspace, workspaces, _panels, mut cx) = worktree_panels(cx, 1).await;
        workspaces[0].update_in(&mut cx, |workspace, window, cx| {
            workspace.close_panel::<WorktreePanel>(window, cx);
        });
        cx.run_until_parked();

        let opened = open_worktree_workspace(&fs, &multi_workspace, &mut cx).await;

        opened.read_with(&mut cx, |workspace, cx| {
            assert!(
                !workspace
                    .dock_at_position(DockPosition::Left)
                    .read(cx)
                    .is_open(),
                "the panel is closed in the window, so it stays closed"
            );
        });
    }

    /// A worktree opened as another workspace of the window, panel added the
    /// way `initialize_panels` adds it — added, and nothing more. Whether the
    /// dock then opens is the panel's own answer, which is what is under test.
    async fn open_worktree_workspace(
        fs: &Arc<FakeFs>,
        multi_workspace: &Entity<MultiWorkspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<Workspace> {
        fs.create_dir("/opened".as_ref()).await.expect("worktree dir");
        fs.insert_file("/opened/file.txt", b"hi".to_vec()).await;
        let project = Project::test(fs.clone(), ["/opened".as_ref()], cx).await;

        let workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(project, window, cx)
        });
        cx.update(|window, cx| {
            let panel = cx.new(|cx| {
                WorktreePanel::new(
                    workspace.downgrade(),
                    multi_workspace.downgrade(),
                    window,
                    cx,
                )
            });
            workspace.update(cx, |workspace, cx| {
                workspace.add_panel(panel, window, cx);
            });
        });
        cx.run_until_parked();

        workspace
    }

    /// Before anything is opened, the window holds a workspace with no folder
    /// in it. Nobody added it, so the panel has nothing to list until someone
    /// does — an "Empty Workspace" row with a worktree under it is the panel
    /// describing the window rather than the user's projects.
    #[gpui::test]
    async fn a_window_with_nothing_open_lists_nothing(cx: &mut TestAppContext) {
        let (_fs, _multi_workspace, _workspaces, panels, mut cx) = worktree_panels(cx, 0).await;

        let rows = panels[0].read_with(&mut cx, |panel, cx| panel.tree(cx).len());

        assert_eq!(rows, 0);
    }

    /// The panel lists projects, not open workspaces. A project the window has
    /// no workspace for — added in an earlier session and brought back as a
    /// row, or added here without being opened — is one the user added, and it
    /// stays listed until the user removes it.
    ///
    /// Its worktrees cannot come off a repository snapshot, because the window
    /// holds no repository for it, so they come off git.
    #[gpui::test]
    async fn a_project_with_nothing_open_still_lists_its_worktrees(cx: &mut TestAppContext) {
        let (fs, multi_workspace, _workspaces, panels, mut cx) = worktree_panels(cx, 1).await;
        fs.create_dir("/closed".as_ref()).await.expect("repo dir");
        fs.create_dir("/closed/.git".as_ref()).await.expect(".git");
        fs.add_linked_worktree_for_repo(
            Path::new("/closed/.git"),
            false,
            worktree("/closed-fix", "fix", false),
        )
        .await;

        let closed = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/closed")]));
        multi_workspace.update(&mut cx, |multi_workspace, _| {
            multi_workspace.test_add_project_group(ProjectGroup {
                key: closed,
                workspaces: Vec::new(),
                expanded: true,
            });
        });

        let panel = panels[0].clone();
        panel.update(&mut cx, |panel, cx| {
            let roots = panel.closed_project_roots(cx);
            assert_eq!(
                roots,
                vec![PathBuf::from("/closed")],
                "the project with no workspace open is the one to scan"
            );
            panel.discover(&roots, cx);
        });
        cx.run_until_parked();

        let rows = panel.read_with(&mut cx, |panel, cx| {
            panel
                .tree(cx)
                .into_iter()
                .map(|row| {
                    let worktrees: Vec<String> = row
                        .worktrees
                        .iter()
                        .map(|worktree| worktree.name.to_string())
                        .collect();
                    (row.name.to_string(), row.is_open, worktrees)
                })
                .collect::<Vec<_>>()
        });

        assert_eq!(
            rows,
            vec![
                (
                    "closed".to_owned(),
                    false,
                    vec!["main".to_owned(), "closed-fix".to_owned()]
                ),
                ("repo-0".to_owned(), true, vec!["main".to_owned()]),
            ]
        );
    }

    /// A Bench worktree of a project that keeps clones under `repos/`: the
    /// outer repository `/outer` with a worktree at `/wt/fix`, and inside it
    /// a worktree of the clone `/inner`, as its manifest checks one out for
    /// every worktree. `/wt/fix` is open, and the window is showing it.
    async fn worktree_with_a_nested_repository(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeFs>,
        Entity<MultiWorkspace>,
        Entity<Workspace>,
        Entity<WorktreePanel>,
        VisualTestContext,
    ) {
        let (fs, multi_workspace, _workspaces, _panels, mut cx) = worktree_panels(cx, 0).await;
        fs.insert_tree("/outer", json!({ ".git": {}, "file.txt": "hi" }))
            .await;
        fs.insert_tree("/inner", json!({ ".git": {}, "file.txt": "hi" }))
            .await;
        fs.add_linked_worktree_for_repo(
            Path::new("/outer/.git"),
            false,
            worktree("/wt/fix", "fix", false),
        )
        .await;
        fs.insert_file("/wt/fix/file.txt", b"hi".to_vec()).await;
        fs.add_linked_worktree_for_repo(
            Path::new("/inner/.git"),
            false,
            worktree("/wt/fix/repos/inner", "fix-inner", false),
        )
        .await;
        fs.insert_file("/wt/fix/repos/inner/file.txt", b"hi".to_vec())
            .await;

        let main_project = Project::test(fs.clone(), ["/outer".as_ref()], &mut cx).await;
        let fix_project = Project::test(fs.clone(), ["/wt/fix".as_ref()], &mut cx).await;
        multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(main_project, window, cx)
        });
        let fix = multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(fix_project, window, cx)
        });
        cx.run_until_parked();
        multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
            multi_workspace.activate(fix.clone(), None, window, cx);
        });
        let panel = cx.update(|window, cx| {
            let panel = cx.new(|cx| {
                WorktreePanel::new(fix.downgrade(), multi_workspace.downgrade(), window, cx)
            });
            fix.update(cx, |workspace, cx| {
                workspace.add_panel(panel.clone(), window, cx);
                workspace.open_panel::<WorktreePanel>(window, cx);
            });
            panel
        });
        cx.run_until_parked();
        (fs, multi_workspace, fix, panel, cx)
    }

    fn delete_fix(panel: &Entity<WorktreePanel>, cx: &mut VisualTestContext) {
        let row = panel.read_with(cx, |panel, cx| {
            panel
                .tree(cx)
                .into_iter()
                .flat_map(|row| row.worktrees)
                .find(|worktree| worktree.root.as_deref() == Some(Path::new("/wt/fix")))
                .expect("a row for the worktree")
        });
        panel.update_in(cx, |panel, window, cx| {
            panel.delete_worktree(
                row.repository.clone().expect("a repository"),
                row.root.clone().expect("a root"),
                "fix".into(),
                window,
                cx,
            );
        });
        cx.run_until_parked();
    }

    fn assert_nothing_left_of_fix(multi_workspace: &Entity<MultiWorkspace>, cx: &mut VisualTestContext) {
        multi_workspace.read_with(cx, |multi_workspace, cx| {
            for workspace in multi_workspace.workspaces() {
                assert!(
                    !workspace_root(workspace, cx)
                        .is_some_and(|root| root.starts_with("/wt/fix")),
                    "no workspace still shows the deleted folder"
                );
            }
            for key in multi_workspace.project_group_keys() {
                assert!(
                    !is_inside(&key, Path::new("/wt/fix")),
                    "the deleted folder is not a project: {key:?}"
                );
            }
        });
    }

    #[gpui::test]
    async fn finds_the_worktrees_of_other_repositories_inside_a_worktree(cx: &mut TestAppContext) {
        let (fs, _multi_workspace, _fix, _panel, _cx) = worktree_with_a_nested_repository(cx).await;
        // A clone of its own, and a folder that is only big: neither is a
        // worktree of another repository.
        fs.insert_tree(
            "/wt/fix/vendor/own-clone",
            json!({ ".git": {}, "file.txt": "hi" }),
        )
        .await;
        fs.insert_tree("/wt/fix/node_modules/pkg", json!({ ".git": "gitdir: /x" }))
            .await;

        let nested = nested_worktrees(fs.clone(), PathBuf::from("/wt/fix")).await;
        let found: Vec<(PathBuf, PathBuf)> = nested
            .into_iter()
            .map(|nested| (nested.path, nested.common_dir))
            .collect();
        assert_eq!(
            found,
            vec![(PathBuf::from("/wt/fix/repos/inner"), PathBuf::from("/inner/.git"))]
        );
    }

    /// Deleting a worktree whose PRs are merged: one question, and everything
    /// goes — the nested worktree through its own repository, so the clone's
    /// repository does not keep a worktree that no longer exists.
    #[gpui::test]
    async fn a_worktree_with_nested_repositories_deletes_cleanly(cx: &mut TestAppContext) {
        let (fs, multi_workspace, _fix, panel, mut cx) =
            worktree_with_a_nested_repository(cx).await;
        cx.update(|_, cx| {
            WorktreeMetadataStore::global(cx).update(cx, |store, cx| {
                store.update(Path::new("/wt/fix"), |metadata| metadata.hue = Some(4), cx)
            });
        });

        delete_fix(&panel, &mut cx);
        let (_, detail) = cx.pending_prompt().expect("the question");
        assert!(detail.contains("repos/inner"), "{detail}");
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();

        assert!(!cx.has_pending_prompt(), "nothing refused, so nothing more to ask");
        cx.update(|_, cx| {
            let store = WorktreeMetadataStore::global(cx);
            assert_eq!(
                store.read(cx).get(Path::new("/wt/fix"), cx),
                Default::default(),
                "what was stored about it goes with it"
            );
        });
        assert!(!fs.is_dir(Path::new("/wt/fix")).await);
        assert!(
            !fs.is_dir(Path::new("/inner/.git/worktrees/fix-inner")).await,
            "the clone's repository no longer holds the nested worktree"
        );
        assert_nothing_left_of_fix(&multi_workspace, &mut cx);
    }

    /// A nested repository with changes is named in the warning, with git's
    /// reason, and deleting anyway removes it too.
    #[gpui::test]
    async fn a_nested_repository_with_changes_is_named_before_deleting(cx: &mut TestAppContext) {
        let (fs, multi_workspace, _fix, panel, mut cx) =
            worktree_with_a_nested_repository(cx).await;
        fs.with_git_state(Path::new("/inner/.git"), false, |state| {
            state
                .worktrees_requiring_force_delete
                .insert(PathBuf::from("/wt/fix/repos/inner"));
        })
        .expect("the clone's repository");

        delete_fix(&panel, &mut cx);
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();

        let (title, detail) = cx.pending_prompt().expect("a warning");
        assert_eq!(title, "Could not delete “fix”.");
        assert!(detail.contains("• repos/inner: "), "{detail}");
        assert!(detail.contains("contains modified or untracked files"), "{detail}");
        cx.simulate_prompt_answer("Delete Anyway");
        cx.run_until_parked();

        assert!(!fs.is_dir(Path::new("/wt/fix")).await);
        assert!(!fs.is_dir(Path::new("/inner/.git/worktrees/fix-inner")).await);
        assert_nothing_left_of_fix(&multi_workspace, &mut cx);
    }

    /// Whatever else shows the worktree goes with it: a workspace opened at
    /// the nested repository, and a project made of the worktree itself —
    /// what a workspace of a deleted folder turns into.
    #[gpui::test]
    async fn deleting_leaves_no_workspace_or_project_behind(cx: &mut TestAppContext) {
        let (fs, multi_workspace, _fix, panel, mut cx) =
            worktree_with_a_nested_repository(cx).await;
        let nested_project =
            Project::test(fs.clone(), ["/wt/fix/repos/inner".as_ref()], &mut cx).await;
        multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(nested_project, window, cx);
            multi_workspace.test_add_project_group(ProjectGroup {
                key: ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/wt/fix")])),
                workspaces: Vec::new(),
                expanded: true,
            });
        });
        cx.run_until_parked();

        delete_fix(&panel, &mut cx);
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();

        assert!(!fs.is_dir(Path::new("/wt/fix")).await);
        assert_nothing_left_of_fix(&multi_workspace, &mut cx);
    }

    /// Clicking a worktree that is not open goes straight to it: the window
    /// switches once, to that worktree, never by way of another worktree of
    /// the same repository — the flash that left you in the wrong one — and
    /// the "Opening…" dialog is gone once it has.
    #[gpui::test]
    async fn opening_a_worktree_goes_straight_to_it(cx: &mut TestAppContext) {
        let (fs, multi_workspace, workspaces, panels, mut cx) = worktree_panels(cx, 1).await;
        fs.insert_tree("/outer", json!({ ".git": {}, "file.txt": "hi" }))
            .await;
        fs.add_linked_worktree_for_repo(
            Path::new("/outer/.git"),
            false,
            worktree("/wt/fix", "fix", false),
        )
        .await;
        fs.insert_file("/wt/fix/file.txt", b"hi".to_vec()).await;
        let outer_project = Project::test(fs.clone(), ["/outer".as_ref()], &mut cx).await;
        let outer = multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(outer_project, window, cx)
        });
        cx.run_until_parked();
        // Looking at another project entirely.
        let elsewhere = workspaces[0].clone();
        multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
            multi_workspace.activate(elsewhere.clone(), None, window, cx);
        });
        cx.run_until_parked();

        let shown = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|_, cx| {
            let shown = shown.clone();
            cx.subscribe(&multi_workspace, move |multi_workspace, event, cx| {
                if let MultiWorkspaceEvent::ActiveWorkspaceChanged { .. } = event {
                    shown
                        .borrow_mut()
                        .push(multi_workspace.read(cx).workspace().clone());
                }
            })
        });

        let key = outer.read_with(&mut cx, |workspace, cx| workspace.project_group_key(cx));
        panels[0].update_in(&mut cx, |panel, window, cx| {
            panel.open_worktree(key, PathBuf::from("/wt/fix"), "fix".into(), None, window, cx);
        });
        cx.run_until_parked();

        let shown = shown.borrow().clone();
        assert_eq!(shown.len(), 1, "one switch, not two");
        let fix = shown.first().cloned().expect("a switch");
        assert_ne!(fix, outer, "not by way of the repository's other worktree");
        fix.read_with(&mut cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .project()
                    .read(cx)
                    .worktree_paths(cx)
                    .ordered_pairs()
                    .next()
                    .map(|(_, own)| own.clone()),
                Some(PathBuf::from("/wt/fix"))
            );
        });
        elsewhere.read_with(&mut cx, |workspace, cx| {
            assert!(
                workspace.active_modal::<OpeningWorktree>(cx).is_none(),
                "the dialog is gone once the worktree is showing"
            );
        });
    }

    fn an_issue(identifier: &str) -> Arc<Issue> {
        Arc::new(
            serde_json::from_value(json!({
                "id": "3f2b1c4d-0000-4000-8000-00000000abcd",
                "identifier": identifier,
                "title": "Legal entities carry their own logo",
                "url": "https://linear.app/acme/issue/RB-116",
                "branchName": "dev/rb-116-legal-entities",
                "priority": 0,
                "priorityLabel": "No priority",
                "state": { "id": "s", "name": "Todo", "color": "#e2e2e2", "type": "unstarted" },
                "assignee": null,
                "team": { "id": "t", "key": "RB", "name": "Rentbee" },
                "project": null,
                "cycle": null,
                "labels": { "nodes": [] },
            }))
            .expect("an issue"),
        )
    }

    /// A worktree made for an issue remembers the issue by Linear's own id,
    /// which outlives the branch name's copy of its identifier, and gets the
    /// colour its project's other worktrees are not using.
    #[gpui::test]
    async fn a_new_worktree_remembers_its_issue_and_gets_its_own_colour(
        cx: &mut TestAppContext,
    ) {
        let (fs, multi_workspace, workspaces, panels, mut cx) = worktree_panels(cx, 0).await;
        fs.insert_tree("/outer", json!({ ".git": {}, "file.txt": "hi" }))
            .await;
        let project = Project::test(fs.clone(), ["/outer".as_ref()], &mut cx).await;
        let outer = multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(project.clone(), window, cx)
        });
        cx.run_until_parked();
        let key = outer.read_with(&mut cx, |workspace, cx| workspace.project_group_key(cx));
        let repository = project.read_with(&mut cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("the outer repository")
        });

        panels[0].update_in(&mut cx, |panel, window, cx| {
            panel.create_worktree(
                outer.clone(),
                repository,
                key.clone(),
                "dev/rb-116-legal-entities".into(),
                Some(an_issue("RB-116")),
                false,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let (path, _) = new_worktree(&key, "dev/rb-116-legal-entities");
        let (metadata, sibling_hue) = cx.update(|_, cx| {
            let store = WorktreeMetadataStore::global(cx);
            let store = store.read(cx);
            let outer = PathBuf::from("/outer");
            (store.get(&path, cx), store.get(&outer, cx).hue_for(&outer))
        });
        assert_eq!(
            metadata.issue,
            Some(LinkedIssue {
                id: "3f2b1c4d-0000-4000-8000-00000000abcd".into(),
                identifier: "RB-116".into(),
            })
        );
        let hue = metadata.hue.expect("a colour chosen when it was made");
        assert_ne!(hue, sibling_hue, "not the colour the project's main checkout has");

        let linked = panels[0].read_with(&mut cx, |panel, cx| {
            panel
                .tree(cx)
                .into_iter()
                .flat_map(|row| row.worktrees)
                .find(|worktree| worktree.root.as_deref() == Some(path.as_path()))
                .and_then(|worktree| worktree.linked_issue)
        });
        assert_eq!(
            linked.map(|linked| linked.identifier).as_deref(),
            Some("RB-116"),
            "the row knows its issue without reading the branch name"
        );
        drop(workspaces);
    }

    /// Right-clicking a worktree opens its menu, which draws, delete and all.
    #[gpui::test]
    async fn the_row_menu_opens(cx: &mut TestAppContext) {
        let (_fs, _multi_workspace, _fix, panel, mut cx) =
            worktree_with_a_nested_repository(cx).await;
        panel.update_in(&mut cx, |panel, window, cx| {
            let delete = panel
                .tree(cx)
                .into_iter()
                .flat_map(|row| row.worktrees)
                .find(|worktree| worktree.root.as_deref() == Some(Path::new("/wt/fix")))
                .and_then(|worktree| Some((worktree.repository?, worktree.name)));
            assert!(delete.is_some(), "a linked worktree can be deleted");
            panel.deploy_row_menu(
                PathBuf::from("/wt/fix"),
                Some("RB-116".into()),
                delete,
                gpui::point(px(10.), px(10.)),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        panel.read_with(&mut cx, |panel, _| assert!(panel.context_menu.is_some()));
    }

    #[test]
    fn git_s_reason_is_what_the_warning_says() {
        let error = anyhow::anyhow!(
            "Git command failed:\nfatal: '/wt/fix' contains modified or untracked files, use --force to delete it\n\n"
        );
        assert_eq!(
            git_reason(&error),
            "'/wt/fix' contains modified or untracked files, use --force to delete it"
        );
    }

    /// A worktree of a project with nothing open has no workspace to switch
    /// from, so clicking it opens it as a workspace of its own — under the
    /// project it was clicked in, rather than as a second project.
    #[gpui::test]
    async fn opening_a_worktree_of_a_closed_project_keeps_it_in_that_project(
        cx: &mut TestAppContext,
    ) {
        let (fs, multi_workspace, _workspaces, panels, mut cx) = worktree_panels(cx, 1).await;
        fs.create_dir("/closed".as_ref()).await.expect("repo dir");
        fs.insert_file("/closed/file.txt", b"hi".to_vec()).await;

        let closed = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/closed")]));
        multi_workspace.update(&mut cx, |multi_workspace, _| {
            multi_workspace.test_add_project_group(ProjectGroup {
                key: closed.clone(),
                workspaces: Vec::new(),
                expanded: true,
            });
        });

        panels[0].update_in(&mut cx, |panel, window, cx| {
            panel.open_worktree(
                closed.clone(),
                PathBuf::from("/closed"),
                "closed".into(),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        multi_workspace.read_with(&mut cx, |multi_workspace, cx| {
            let opened = multi_workspace
                .workspaces()
                .any(|workspace| workspace.read(cx).project_group_key(cx).matches(&closed));
            assert!(opened, "the project now has a workspace in the window");
        });
    }

    /// A branch keeps every pull request ever opened from it, so the rows have
    /// to pick one. The newest wins, unless an older one is still open.
    #[gpui::test]
    fn an_open_pull_request_outranks_an_older_merged_one() {
        // Newest first, as `gh` answers.
        let states = by_branch(vec![
            pull_request("feature/reopened", PullRequestState::Merged),
            pull_request("feature/reopened", PullRequestState::Open),
            pull_request("feature/done", PullRequestState::Merged),
            pull_request("feature/done", PullRequestState::Closed),
            pull_request("feature/draft", PullRequestState::Draft),
        ]);

        assert_eq!(
            states.get("feature/reopened"),
            Some(&PullRequestState::Open),
            "a branch with something still open on it is open"
        );
        assert_eq!(
            states.get("feature/done"),
            Some(&PullRequestState::Merged),
            "otherwise the newest is the one the row is about"
        );
        assert_eq!(states.get("feature/draft"), Some(&PullRequestState::Draft));
        assert_eq!(states.get("feature/never"), None);
    }

    /// A panel takes the shared width every time it draws, not once when it is
    /// built: a worktree comes forward long after its panel was made, and that
    /// is exactly the moment it has to agree with the one you just left.
    #[gpui::test]
    async fn a_panel_matches_the_shared_width_on_every_draw(cx: &mut TestAppContext) {
        let (_fs, _multi_workspace, workspaces, panels, mut cx) = worktree_panels(cx, 1).await;

        let width_of = |cx: &mut VisualTestContext| {
            workspaces[0].read_with(cx, |workspace, cx| {
                workspace
                    .dock_at_position(DockPosition::Left)
                    .read(cx)
                    .stored_panel_size_state(&panels[0])
                    .and_then(|state| state.size)
            })
        };

        // The panel has drawn once already, which is what seeds the shared
        // width from whatever the first worktree was wearing.
        let seeded = cx.update(|_window, cx| shared_width(cx));
        assert!(seeded.is_some(), "the first draw sets the width the rest take");

        // As if the user had dragged another worktree's panel wider.
        let wider = PanelSizeState {
            size: Some(px(420.)),
            flex: None,
        };
        cx.update(|_window, cx| set_shared_width(wider, cx));
        cx.run_until_parked();

        assert_ne!(width_of(&mut cx), Some(px(420.)), "nothing has drawn yet");

        cx.update(|window, cx| {
            panels[0].update(cx, |panel, cx| {
                panel.match_shared_width(window, cx);
            });
        });
        cx.run_until_parked();

        assert_eq!(
            width_of(&mut cx),
            Some(px(420.)),
            "drawing again is what makes this worktree agree with the others"
        );
    }

    /// The dock calls `size_state_changed` from inside its own update, so
    /// anything that reads the dock there is a double lease — which is a
    /// panic, and was a crash on every drag of the panel's edge.
    #[gpui::test]
    async fn resizing_does_not_read_the_dock_that_is_resizing(cx: &mut TestAppContext) {
        let (_fs, _multi_workspace, workspaces, _panels, mut cx) = worktree_panels(cx, 1).await;

        // The real drag: the dock resizes its active panel, which calls back
        // into the panel while the dock is still being updated.
        cx.update(|window, cx| {
            let dock = workspaces[0]
                .read(cx)
                .dock_at_position(DockPosition::Left)
                .clone();
            dock.update(cx, |dock, cx| {
                dock.resize_panel_sizes(Some(px(360.)), None, window, cx);
            });
        });
        cx.run_until_parked();

        assert_eq!(
            cx.update(|_window, cx| shared_width(cx)).and_then(|width| width.size),
            Some(px(360.)),
            "the width the drag ended at is the one every worktree takes"
        );
    }

    /// Dragging the edge publishes that width to every other worktree.
    #[gpui::test]
    async fn resizing_the_panel_sets_the_width_every_worktree_takes(cx: &mut TestAppContext) {
        let (_fs, _multi_workspace, workspaces, panels, mut cx) = worktree_panels(cx, 1).await;

        let wider = PanelSizeState {
            size: Some(px(400.)),
            flex: None,
        };
        cx.update(|window, cx| {
            let dock = workspaces[0]
                .read(cx)
                .dock_at_position(DockPosition::Left)
                .clone();
            dock.update(cx, |dock, cx| {
                dock.set_panel_size_state(&panels[0], wider, cx);
            });
            panels[0].update(cx, |panel, cx| {
                panel.size_state_changed(window, cx);
            });
        });
        cx.run_until_parked();

        assert_eq!(
            cx.update(|_window, cx| shared_width(cx)),
            Some(wider),
            "the width the user chose is the one every worktree now takes"
        );
    }

    #[gpui::test]
    fn the_filter_matches_the_way_every_other_fuzzy_filter_does() {
        assert!(matches_filter("data-hub", "dh"), "initials of the words");
        assert!(matches_filter("data-hub", "hub"), "a run of characters");
        assert!(matches_filter("data-hub", "DATA"), "case is ignored");
        assert!(
            matches_filter("fix-inefficient-job-log-insertion", "fjli"),
            "letters in order, anywhere"
        );
        assert!(matches_filter("anything", ""), "an empty filter is no filter");

        assert!(!matches_filter("data-hub", "hd"), "order matters");
        assert!(!matches_filter("data-hub", "datax"));
    }

    /// The colour comes off the worktree's *branch*, not the name in the row:
    /// a worktree's directory and its branch need not agree — Bench flattens a
    /// slash when it names a directory, and a worktree made by hand can be
    /// called anything — so matching on the row's name would colour by
    /// coincidence. Here the row says `closed-fix` and the branch is `fix`.
    #[gpui::test]
    async fn a_worktree_is_coloured_by_its_branchs_pull_request(cx: &mut TestAppContext) {
        let (fs, multi_workspace, _workspaces, panels, mut cx) = worktree_panels(cx, 1).await;
        fs.create_dir("/closed".as_ref()).await.expect("repo dir");
        fs.create_dir("/closed/.git".as_ref()).await.expect(".git");
        fs.add_linked_worktree_for_repo(
            Path::new("/closed/.git"),
            false,
            worktree("/closed-fix", "fix", false),
        )
        .await;

        let closed = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/closed")]));
        multi_workspace.update(&mut cx, |multi_workspace, _| {
            multi_workspace.test_add_project_group(ProjectGroup {
                key: closed,
                workspaces: Vec::new(),
                expanded: true,
            });
        });

        let panel = panels[0].clone();
        panel.update(&mut cx, |panel, cx| {
            let roots = panel.closed_project_roots(cx);
            panel.discover(&roots, cx);
        });
        cx.run_until_parked();

        // What a scan of the project would have left behind.
        cx.update(|_window, cx| {
            record_scan(cx, |scans| {
                scans.pull_requests.insert(
                    PathBuf::from("/closed"),
                    PullRequestScan::Found {
                        by_branch: HashMap::from_iter([(
                            SharedString::from("fix"),
                            PullRequestState::Merged,
                        )]),
                        at: Instant::now(),
                        _refresh: None,
                    },
                );
            });
        });

        let states = panel.read_with(&mut cx, |panel, cx| {
            panel
                .tree(cx)
                .into_iter()
                .flat_map(|row| row.worktrees)
                .map(|worktree| (worktree.name.to_string(), worktree.pull_request))
                .collect::<Vec<_>>()
        });

        assert_eq!(
            states,
            vec![
                ("main".to_owned(), None),
                ("closed-fix".to_owned(), Some(PullRequestState::Merged)),
                ("main".to_owned(), None),
            ],
            "only the worktree on the branch with the pull request is coloured"
        );
    }

    /// A project that keeps other repositories inside it — clones under a
    /// `repos/` folder — lists only its own worktrees. The nested repositories
    /// are still in the project for the editor and language servers, but their
    /// worktrees belong to them, not to the project.
    #[gpui::test]
    async fn repositories_nested_in_a_project_do_not_add_worktrees(cx: &mut TestAppContext) {
        let (fs, multi_workspace, _workspaces, panels, mut cx) = worktree_panels(cx, 0).await;
        fs.insert_tree(
            "/outer",
            json!({
                ".git": {},
                "file.txt": "hi",
                "repos": {
                    "inner": {
                        ".git": {},
                        "file.txt": "hi",
                    },
                },
            }),
        )
        .await;
        fs.add_linked_worktree_for_repo(
            Path::new("/outer/.git"),
            false,
            worktree("/outer-fix", "fix", false),
        )
        .await;
        fs.add_linked_worktree_for_repo(
            Path::new("/outer/repos/inner/.git"),
            false,
            worktree("/inner-fix", "inner-fix", false),
        )
        .await;

        let project = Project::test(fs.clone(), ["/outer".as_ref()], &mut cx).await;
        multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(project.clone(), window, cx)
        });
        cx.run_until_parked();

        let repositories = project.read_with(&mut cx, |project, cx| project.repositories(cx).len());
        assert_eq!(
            repositories, 2,
            "the nested repository is still part of the project"
        );

        let rows = panels[0].read_with(&mut cx, |panel, cx| {
            panel
                .tree(cx)
                .into_iter()
                .map(|row| {
                    let worktrees: Vec<String> = row
                        .worktrees
                        .iter()
                        .map(|worktree| worktree.name.to_string())
                        .collect();
                    (row.name.to_string(), worktrees)
                })
                .collect::<Vec<_>>()
        });

        assert_eq!(
            rows,
            vec![(
                "outer".to_owned(),
                vec!["main".to_owned(), "outer-fix".to_owned()]
            )]
        );
    }

    /// The title is stored as typed, trimmed, and an empty one stays empty.
    #[gpui::test]
    async fn editing_a_title_stores_it(cx: &mut TestAppContext) {
        let (_fs, multi_workspace, _fix, panel, mut cx) =
            worktree_with_a_nested_repository(cx).await;
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.edit_title(PathBuf::from("/wt/fix"), window, cx);
        });
        cx.run_until_parked();
        let modal = multi_workspace.update(&mut cx, |multi_workspace, cx| {
            multi_workspace
                .workspace()
                .read(cx)
                .active_modal::<EditTitle>(cx)
                .expect("the title modal")
        });
        modal.update_in(&mut cx, |modal, window, cx| {
            modal
                .title
                .read(cx)
                .editor()
                .clone()
                .set_text("  Fix the login  ", window, cx);
            modal.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();
        let stored = cx.update(|_, cx| {
            WorktreeMetadataStore::global(cx)
                .read(cx)
                .get(Path::new("/wt/fix"), cx)
        });
        assert_eq!(stored.title.as_deref(), Some("Fix the login"));
    }

    #[test]
    fn the_last_edit_is_said_briefly() {
        assert_eq!(edited_ago(Duration::from_secs(20)), "just now");
        assert_eq!(edited_ago(Duration::from_secs(5 * 60 + 10)), "5m ago");
        assert_eq!(edited_ago(Duration::from_secs(3 * 3600)), "3h ago");
        assert_eq!(edited_ago(Duration::from_secs(2 * 86400)), "2d ago");
        assert_eq!(edited_ago(Duration::from_secs(15 * 86400)), "2w ago");
        assert_eq!(edited_ago(Duration::from_secs(95 * 86400)), "3mo ago");
        assert_eq!(edited_ago(Duration::from_secs(800 * 86400)), "2y ago");
    }

    #[test]
    fn a_renamed_files_old_path_is_not_a_changed_file() {
        let status =
            b"## main...origin/main [ahead 2]\0 M src/main.rs\0R  new.rs\0old.rs\0?? notes/\0";
        let parsed = parse_status(status);
        assert_eq!(parsed.paths, ["src/main.rs", "new.rs", "notes/"]);
        assert_eq!(parsed.ahead_behind, Some((2, 0)));
        assert!(parse_status(b"").paths.is_empty());
    }

    #[test]
    fn a_branch_is_ahead_or_behind_only_with_an_upstream() {
        assert_eq!(
            ahead_behind("main...origin/main [ahead 2, behind 5]"),
            Some((2, 5))
        );
        assert_eq!(ahead_behind("main...origin/main [behind 1]"), Some((0, 1)));
        assert_eq!(ahead_behind("main...origin/main"), Some((0, 0)));
        assert_eq!(ahead_behind("fix-login"), None, "no upstream");
        assert_eq!(ahead_behind("fix...origin/fix [gone]"), None);
        assert_eq!(ahead_behind("HEAD (no branch)"), None);
    }

    #[test]
    fn a_linked_worktree_is_named_without_its_identifier() {
        for (name, identifier, expected) in [
            (
                "RB-146-connect-extras-and-books-by-group",
                "RB-146",
                "connect-extras-and-books-by-group",
            ),
            (
                "dev-rb-152-connect-location-details",
                "RB-152",
                "connect-location-details",
            ),
            ("rb-134-new-rentbee-website-v1", "RB-134", "new-rentbee-website-v1"),
            ("RB-146", "RB-146", "RB-146"),
            ("fix-login", "RB-146", "fix-login"),
            // Only where an identifier starts: not inside another word.
            ("arb-146-thing", "RB-146", "arb-146-thing"),
        ] {
            assert_eq!(
                name_without_identifier(name, identifier).as_ref(),
                expected,
                "{name}"
            );
        }
    }

    /// A clone kept inside a project gets a copy in each of the project's
    /// worktrees, and each copy is a worktree of the clone. Once the clone is
    /// a project of its own, those copies are part of the outer project's
    /// worktrees, not rows of their own — unless the window has one open.
    /// Worktrees a repository keeps inside its own checkout are still listed.
    #[gpui::test]
    async fn worktrees_inside_another_projects_worktree_are_not_listed(
        cx: &mut TestAppContext,
    ) {
        let (fs, multi_workspace, _workspaces, panels, mut cx) = worktree_panels(cx, 0).await;
        fs.insert_tree(
            "/outer",
            json!({
                ".git": {},
                "file.txt": "hi",
                "repos": { "inner": { ".git": {}, "file.txt": "hi" } },
            }),
        )
        .await;
        fs.insert_tree("/work/fix/repos/inner", json!({ "file.txt": "hi" }))
            .await;
        fs.insert_tree("/work/other/repos/inner", json!({ "file.txt": "hi" }))
            .await;
        for (path, branch) in [("/work/fix", "fix"), ("/work/other", "other")] {
            fs.add_linked_worktree_for_repo(
                Path::new("/outer/.git"),
                false,
                worktree(path, branch, false),
            )
            .await;
        }
        for (path, branch) in [
            ("/work/fix/repos/inner", "inner-fix"),
            ("/work/other/repos/inner", "inner-other"),
            ("/outer/repos/inner/.worktrees/own", "own"),
        ] {
            fs.add_linked_worktree_for_repo(
                Path::new("/outer/repos/inner/.git"),
                false,
                worktree(path, branch, false),
            )
            .await;
        }

        for root in ["/outer", "/work/fix/repos/inner"] {
            let project = Project::test(fs.clone(), [root.as_ref()], &mut cx).await;
            multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
                multi_workspace.test_add_workspace(project, window, cx)
            });
        }
        cx.run_until_parked();

        let rows = panels[0].read_with(&mut cx, |panel, cx| {
            panel
                .tree(cx)
                .into_iter()
                .map(|row| {
                    let roots: Vec<PathBuf> = row
                        .worktrees
                        .iter()
                        .filter_map(|worktree| worktree.root.clone())
                        .collect();
                    (row.name.to_string(), roots)
                })
                .collect::<Vec<_>>()
        });

        let inner = rows
            .iter()
            .find(|(name, _)| name == "inner")
            .map(|(_, roots)| roots.clone())
            .expect("the inner clone is a project of its own");
        assert!(inner.contains(&PathBuf::from("/outer/repos/inner")));
        assert!(
            inner.contains(&PathBuf::from("/work/fix/repos/inner")),
            "the copy the window has open stays listed"
        );
        assert!(
            inner.contains(&PathBuf::from("/outer/repos/inner/.worktrees/own")),
            "a worktree inside its own repository's checkout stays listed"
        );
        assert!(
            !inner.contains(&PathBuf::from("/work/other/repos/inner")),
            "a copy inside another project's worktree is not a row"
        );
    }

    /// Worktrees other tools made of the same repository are in its worktree
    /// list too. The settings keep them out, but never the repository's own
    /// checkout or a worktree the window has open.
    #[gpui::test]
    async fn worktrees_outside_the_allowed_directories_are_not_listed(
        cx: &mut TestAppContext,
    ) {
        let (fs, multi_workspace, _workspaces, panels, mut cx) = worktree_panels(cx, 0).await;
        fs.insert_tree("/outer", json!({ ".git": {}, "file.txt": "hi" }))
            .await;
        fs.insert_tree("/elsewhere/opened", json!({ "file.txt": "hi" }))
            .await;
        for (path, branch) in [
            ("/bench/outer-fix", "fix"),
            ("/bench/skipped/outer-skip", "skip"),
            ("/elsewhere/outer-old", "old"),
            ("/elsewhere/opened", "opened"),
        ] {
            fs.add_linked_worktree_for_repo(
                Path::new("/outer/.git"),
                false,
                worktree(path, branch, false),
            )
            .await;
        }
        cx.update(|_window, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content.worktree_panel = Some(settings::WorktreePanelSettingsContent {
                        include: Some(vec!["/bench".to_owned()]),
                        exclude: Some(vec!["/bench/skipped".to_owned()]),
                    });
                });
            });
        });

        let project = Project::test(fs.clone(), ["/outer".as_ref()], &mut cx).await;
        multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(project, window, cx)
        });
        let opened = Project::test(fs.clone(), ["/elsewhere/opened".as_ref()], &mut cx).await;
        multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(opened, window, cx)
        });
        cx.run_until_parked();

        let names = panels[0].read_with(&mut cx, |panel, cx| {
            panel
                .tree(cx)
                .into_iter()
                .flat_map(|row| row.worktrees)
                .map(|worktree| worktree.name.to_string())
                .collect::<Vec<_>>()
        });

        assert_eq!(names, vec!["main", "opened", "outer-fix"]);
    }

    fn pull_request(head_ref: &str, state: PullRequestState) -> PullRequest {
        PullRequest {
            number: 1,
            title: "Add the thing".into(),
            url: "https://github.com/o/r/pull/1".into(),
            state,
            author: "octocat".into(),
            head_ref: head_ref.to_owned().into(),
        }
    }

    fn worktree(path: &str, branch: &str, is_main: bool) -> GitWorktree {
        GitWorktree {
            path: PathBuf::from(path),
            ref_name: Some(format!("refs/heads/{branch}").into()),
            sha: "deadbeef".into(),
            is_main,
            is_bare: false,
        }
    }

    /// The point of the panel: a worktree you have not opened is still a row,
    /// so you can see it is there and click it.
    #[test]
    fn every_worktree_is_a_row_whether_open_or_not() {
        let linked = vec![
            worktree("/src/bench", "main", true),
            worktree("/src/bench-fix", "fix", false),
            worktree("/src/bench-spike", "spike", false),
        ];
        // Only the main worktree is open in the window.
        let open_roots = vec![Some(PathBuf::from("/src/bench"))];

        let plans = plan_rows(&linked, &open_roots, None);

        assert_eq!(plans.len(), 3, "every worktree of the repository is a row");
        let names: Vec<_> = plans
            .iter()
            .map(|plan| plan.name.clone().unwrap_or_default())
            .collect();
        assert_eq!(names, vec!["main", "bench-fix", "bench-spike"]);
        assert_eq!(plans[0].open, Some(0), "the open worktree knows it is open");
        assert_eq!(plans[1].open, None);
        assert_eq!(plans[2].open, None);
    }

    /// The end-to-end shape of it: two worktrees, a drag in one, a draw in the
    /// other. Together these are what stops the panel jumping on a switch.
    #[gpui::test]
    async fn a_drag_in_one_worktree_reaches_the_other(cx: &mut TestAppContext) {
        let (_fs, _multi_workspace, workspaces, panels, mut cx) = worktree_panels(cx, 2).await;

        // As if the user had dragged the first worktree's panel wider.
        let wider = PanelSizeState {
            size: Some(px(400.)),
            flex: None,
        };
        cx.update(|window, cx| {
            let dock = workspaces[0]
                .read(cx)
                .dock_at_position(DockPosition::Left)
                .clone();
            dock.update(cx, |dock, cx| {
                dock.set_panel_size_state(&panels[0], wider, cx);
            });
            panels[0].update(cx, |panel, cx| {
                panel.size_state_changed(window, cx);
            });
        });
        cx.run_until_parked();

        // Switching to the second worktree draws its panel, which is where it
        // takes the width.
        cx.update(|window, cx| {
            panels[1].update(cx, |panel, cx| {
                panel.match_shared_width(window, cx);
            });
        });
        cx.run_until_parked();

        let width_of = |index: usize, cx: &mut VisualTestContext| {
            workspaces[index].read_with(cx, |workspace, cx| {
                workspace
                    .dock_at_position(DockPosition::Left)
                    .read(cx)
                    .stored_panel_size_state(&panels[index])
                    .and_then(|state| state.size)
            })
        };
        assert_eq!(width_of(0, &mut cx), Some(px(400.)));
        assert_eq!(
            width_of(1, &mut cx),
            Some(px(400.)),
            "the other worktree's panel should have followed"
        );
    }

    /// Worktrees are commonly laid out as `<worktrees>/<name>/<repo>`, so every
    /// one of them ends in the repository's own directory name. The name that
    /// tells them apart is the parent, and finding it needs an anchor —
    /// without one every row in such a repository reads as the repository.
    ///
    /// The anchor cannot come from the worktree list: the snapshot leaves out
    /// the checkout the project is open at, so nothing in the list is marked
    /// as the main worktree.
    #[test]
    fn worktrees_nested_under_their_name_are_named_after_it() {
        // Exactly the shape `git worktree list` reports for a repository
        // opened at its own checkout.
        let linked = vec![
            worktree(
                "/Developer/worktrees/s1-commons/foo/s1-commons",
                "foo",
                false,
            ),
            worktree(
                "/Developer/worktrees/s1-commons/test/s1-commons",
                "test",
                false,
            ),
        ];
        let anchor = Path::new("/Developer/s1-commons");

        let named = |plans: &[RowPlan]| -> Vec<String> {
            plans
                .iter()
                .filter_map(|plan| Some(plan.name.clone()?.to_string()))
                .collect()
        };

        assert_eq!(
            named(&plan_rows(&linked, &[], Some(anchor))),
            vec!["foo", "test"]
        );
        // And what it looked like before the anchor was passed in.
        assert_eq!(
            named(&plan_rows(&linked, &[], None)),
            vec!["s1-commons", "s1-commons"],
            "without an anchor the rows collapse onto the repository name"
        );
    }

    /// The name the user types is the whole answer: it names the directory the
    /// worktree is created in, under one fixed place per project, and the
    /// branch that is created with it.
    #[test]
    fn a_new_worktree_is_its_name_twice_over() {
        let key = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/Developer/bench")]));

        let (path, target) = new_worktree(&key, "fix-login");

        assert_eq!(
            path,
            home_dir().join("bench").join("bench").join("fix-login"),
            "`~/bench/<project>/<worktree>`, wherever the repository itself is"
        );
        assert_eq!(
            target,
            CreateWorktreeTarget::NewBranch {
                branch_name: "fix-login".to_owned(),
                // Off whatever is checked out, rather than a branch the user
                // was never asked to pick.
                base_sha: None,
            }
        );
    }

    /// A branch name has its own hierarchy, and `feature/foo` is a name people
    /// want. The branch keeps it; the directory cannot, or the worktree would
    /// sit a level below the others in a directory named after a prefix.
    #[test]
    fn a_branch_name_with_a_slash_keeps_it_and_the_directory_does_not() {
        let key = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/Developer/bench")]));

        let (path, target) = new_worktree(&key, "feature/foo");

        assert_eq!(
            path,
            home_dir().join("bench").join("bench").join("feature-foo")
        );
        assert_eq!(
            target,
            CreateWorktreeTarget::NewBranch {
                branch_name: "feature/foo".to_owned(),
                base_sha: None,
            }
        );
    }

    /// Only the names git is certain to refuse are refused here; the rest is
    /// git's to judge, and the panel shows what it says.
    #[test]
    fn a_name_git_would_refuse_is_not_sent() {
        assert!(is_branch_name("fix-login"));
        assert!(is_branch_name("feature/foo"));
        assert!(is_branch_name("feature/foo/bar"));

        assert!(!is_branch_name(""), "nothing to name it after");
        assert!(
            !is_branch_name("/feature"),
            "a ref cannot begin with a slash"
        );
        assert!(!is_branch_name("feature/"), "nor end with one");
        assert!(!is_branch_name("feature//foo"), "nor double one");
        assert!(
            !is_branch_name("feature\\foo"),
            "a backslash is not a separator"
        );
    }

    /// A project git says nothing about — one that is not a repository, or one
    /// the scan could not reach — is still a checkout the user can open, so its
    /// own path is a row rather than an empty project.
    #[test]
    fn a_project_git_says_nothing_about_is_still_a_row() {
        let plans = plan_rows(&[], &[], Some(Path::new("/src/notes")));

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].root.as_deref(), Some(Path::new("/src/notes")));
        assert!(plans[0].is_main, "there is no repository to remove it from");
        assert!(
            plans[0].name.is_none(),
            "nothing named it, so its own directory does"
        );
    }

    /// The checkout the project is open at is missing from the worktree list,
    /// so its row comes from the open workspace instead — and it still has to
    /// be recognised as the repository's own, which is the row git will not
    /// let us delete.
    #[test]
    fn the_open_checkout_is_still_the_main_worktree() {
        let linked = vec![worktree(
            "/Developer/worktrees/s1-commons/test/s1-commons",
            "test",
            false,
        )];
        let anchor = Path::new("/Developer/s1-commons");
        let open_roots = vec![Some(PathBuf::from("/Developer/s1-commons"))];

        let plans = plan_rows(&linked, &open_roots, Some(anchor));

        assert_eq!(plans[0].root.as_deref(), Some(anchor), "and it leads");
        assert!(plans[0].is_main, "so it offers no delete");
        assert!(!plans[1].is_main);
    }

    /// Git will not remove a repository's own checkout, so the row that stands
    /// for it must not offer to.
    #[test]
    fn the_main_worktree_is_marked_undeletable() {
        let linked = vec![
            worktree("/src/bench", "main", true),
            worktree("/src/bench-fix", "fix", false),
        ];

        let plans = plan_rows(&linked, &[], None);

        assert!(plans[0].is_main, "the repository's own checkout leads");
        assert!(!plans[1].is_main);
    }

    /// The repository's own checkout reads as the root of the group, so it
    /// leads regardless of where its name would sort.
    #[test]
    fn the_main_worktree_leads_its_repository() {
        let linked = vec![
            worktree("/src/aardvark", "wip", false),
            worktree("/src/bench", "main", true),
        ];

        let plans = plan_rows(&linked, &[], None);

        assert_eq!(plans[0].root.as_deref(), Some(Path::new("/src/bench")));
        assert_eq!(plans[1].root.as_deref(), Some(Path::new("/src/aardvark")));
    }

    /// Rows are ordered by what exists, not by what is open, so opening a
    /// worktree must not shuffle the row the pointer is over.
    #[test]
    fn opening_a_worktree_does_not_reorder_the_rows() {
        let linked = vec![
            worktree("/src/bench", "main", true),
            worktree("/src/bench-fix", "fix", false),
            worktree("/src/bench-spike", "spike", false),
        ];

        let roots = |plans: &[RowPlan]| -> Vec<Option<PathBuf>> {
            plans.iter().map(|plan| plan.root.clone()).collect()
        };
        let nothing_open = plan_rows(&linked, &[], None);
        let spike_open = plan_rows(&linked, &[Some(PathBuf::from("/src/bench-spike"))], None);

        assert_eq!(roots(&nothing_open), roots(&spike_open));
    }

    /// A workspace the repository's list does not account for — no git
    /// repository, or a root that matches no worktree — is still open in the
    /// window, and a panel that hides it is lying about what is open.
    #[test]
    fn an_open_workspace_is_never_dropped_from_the_panel() {
        let linked = vec![worktree("/src/bench", "main", true)];
        let open_roots = vec![
            Some(PathBuf::from("/src/bench")),
            Some(PathBuf::from("/elsewhere/notes")),
        ];

        let plans = plan_rows(&linked, &open_roots, None);

        assert_eq!(plans.len(), 2);
        let unlisted = plans
            .iter()
            .find(|plan| plan.root.as_deref() == Some(Path::new("/elsewhere/notes")))
            .expect("the open workspace outside the repository's list still has a row");
        assert_eq!(unlisted.open, Some(1));
        assert!(
            unlisted.name.is_none(),
            "it has no worktree to name it, so the project names it instead"
        );
    }

    /// Every open workspace maps to its own index: the row that says "open"
    /// has to be the workspace that is actually showing that worktree, or
    /// clicking a row activates the wrong one.
    #[test]
    fn each_row_points_at_the_workspace_showing_it() {
        let linked = vec![
            worktree("/src/bench", "main", true),
            worktree("/src/bench-fix", "fix", false),
        ];
        // Deliberately not in row order.
        let open_roots = vec![
            Some(PathBuf::from("/src/bench-fix")),
            Some(PathBuf::from("/src/bench")),
        ];

        let plans = plan_rows(&linked, &open_roots, None);

        assert_eq!(plans[0].root.as_deref(), Some(Path::new("/src/bench")));
        assert_eq!(plans[0].open, Some(1));
        assert_eq!(plans[1].root.as_deref(), Some(Path::new("/src/bench-fix")));
        assert_eq!(plans[1].open, Some(0));
    }
}
