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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use git::repository::{CreateWorktreeTarget, Worktree as GitWorktree};
use agent_tracker::{AgentSummary, AgentTracker, AgentsChanged, agent_state_color};
use git_ui_core::pull_request_color::pull_request_color;
use github_cli::{self, PullRequest, PullRequestState};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AsyncWindowContext, Context, DismissEvent,
    Entity,
    EventEmitter, FocusHandle, Focusable, Global, Task, Transformation, WeakEntity, Window,
    actions, percentage, prelude::*, svg,
};
use project::{
    Fs, ProjectGroupKey, discover_root_repo_common_dir, git_store::Repository,
    git_store::linked_worktree_short_name, repo_identity_path_if_local,
};
use ui::{Indicator, Label, ListItem, ListItemSpacing, Tooltip, prelude::*};
use ui_input::{ErasedEditor, InputField};
use util::path_list::PathList;
use util::paths::home_dir;
use util::ResultExt as _;
use workspace::{
    ModalView, MultiWorkspace, MultiWorkspaceEvent, OpenMode, RemovalIntent, Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelSizeState},
};
use zed_actions::SwitchWorktree;

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
}

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
        let switch_from = open_worktrees.first().map(|open| open.workspace.clone());
        let pull_requests = self.scanned_pull_requests(key, cx);
        let tracker = AgentTracker::try_global(cx);

        plan_rows(&worktrees, &open_roots, anchor.as_deref())
            .into_iter()
            .filter_map(|plan| {
                let open = match plan.open {
                    Some(index) => Some(open_worktrees.get(index)?),
                    None => None,
                };
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
                    root: plan.root,
                })
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

    /// Opens a worktree the window does not have open yet.
    ///
    /// With a workspace of the worktree's own project to switch from, this goes
    /// through [`SwitchWorktree`], whose handler `git_ui` registers on the
    /// workspace: it opens the worktree as another workspace in this window,
    /// and is the same call the worktree picker makes, so both routes land in
    /// the same place. It activates that workspace first, so that the action
    /// dispatches against it — the panel lists every project, so the workspace
    /// the user is looking at is not necessarily the one this worktree belongs
    /// to.
    ///
    /// A project with nothing open has no workspace to switch from, so its
    /// worktree is opened as a workspace of its own. Naming the project group
    /// keeps it under the row it was clicked in rather than starting a second
    /// one.
    ///
    /// Like [`Self::activate`], this runs on `window.defer` and takes no
    /// `&mut Self` — see there for why a lease on this panel across an
    /// activation is a panic.
    fn open_worktree(
        &mut self,
        from: Option<Entity<Workspace>>,
        key: ProjectGroupKey,
        root: PathBuf,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multi_workspace = self.multi_workspace.clone();
        window.defer(cx, move |window, cx| {
            let Some(from) = from else {
                multi_workspace
                    .update(cx, |multi_workspace, cx| {
                        multi_workspace
                            .find_or_create_local_workspace(
                                PathList::new(&[root]),
                                Some(key),
                                None,
                                OpenMode::Activate,
                                None,
                                window,
                                cx,
                            )
                            .detach_and_log_err(cx);
                    })
                    .ok();
                return;
            };
            multi_workspace
                .update(cx, |multi_workspace, cx| {
                    multi_workspace.activate(from, None, window, cx);
                })
                .ok();
            window.dispatch_action(
                Box::new(SwitchWorktree {
                    path: root,
                    display_name: name.to_string(),
                }),
                cx,
            );
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
    fn add_worktree(
        &mut self,
        from: Entity<Workspace>,
        repository: Entity<Repository>,
        key: ProjectGroupKey,
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
                    NameWorktree::new(directory, window, cx, move |name, window, cx| {
                        let from = from.clone();
                        let repository = repository.clone();
                        let key = key.clone();
                        panel
                            .update(cx, |panel, cx| {
                                panel.create_worktree(from, repository, key, name, window, cx);
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
    fn create_worktree(
        &mut self,
        from: Entity<Workspace>,
        repository: Entity<Repository>,
        key: ProjectGroupKey,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (path, target) = new_worktree(&key, &name);
        cx.spawn_in(window, async move |this, cx| {
            let created = repository
                .update(cx, |repository, _| {
                    repository.create_worktree(target, path.clone())
                })
                .await?;

            match created {
                Ok(()) => this.update_in(cx, |this, window, cx| {
                    this.rediscover(project_root(&key), cx);
                    this.open_worktree(Some(from), key, path, name, window, cx);
                })?,
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

    /// Deletes a worktree: closes its workspace if the window has it open,
    /// then removes it with `git worktree remove`.
    ///
    /// It asks first, because this deletes a directory. It asks a second time
    /// when git refuses — which is what git does when the worktree has changes
    /// that removing it would lose — rather than forcing straight away.
    ///
    /// The workspace goes before the directory does: a workspace whose folder
    /// has just stopped existing shows an empty tree and errors on every file
    /// watch. [`RemovalIntent::KeepProject`] is the distinction that matters
    /// there — deleting one worktree must not close the repository it belongs
    /// to, even when it is the last one open.
    fn delete_worktree(
        &mut self,
        repository: Entity<Repository>,
        workspace: Option<Entity<Workspace>>,
        root: PathBuf,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multi_workspace = self.multi_workspace.clone();
        // The project git will be asked about again once this worktree is
        // gone; see `rediscover`.
        let project = repository_anchor(&repository, cx);
        let confirmed = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Delete the worktree “{name}”?"),
            Some(&format!("{} will be removed from disk.", root.display())),
            &["Delete", "Cancel"],
            cx,
        );

        cx.spawn_in(window, async move |this, cx| {
            if confirmed.await? != 0 {
                return anyhow::Ok(());
            }

            if let Some(workspace) = workspace {
                let closed = multi_workspace
                    .update_in(cx, |multi_workspace, window, cx| {
                        multi_workspace.remove([workspace], RemovalIntent::KeepProject, window, cx)
                    })?
                    .await?;
                // Closing can stop to ask about unsaved changes, and answering
                // no means the worktree stays. Deleting the directory anyway
                // would throw away the very work the prompt was protecting.
                if !closed {
                    return anyhow::Ok(());
                }
            }

            let removed = repository
                .update(cx, |repository, _| {
                    repository.remove_worktree(root.clone(), false)
                })
                .await?;

            let Err(refused) = removed else {
                return anyhow::Ok(());
            };
            log::warn!("git refused to remove the worktree {name}: {refused:#}");

            let forced = cx.update(|window, cx| {
                window.prompt(
                    gpui::PromptLevel::Warning,
                    &format!("Could not delete “{name}”."),
                    Some(&format!(
                        "{refused}\n\nDeleting it anyway discards whatever is in it."
                    )),
                    &["Delete Anyway", "Cancel"],
                    cx,
                )
            })?;
            if forced.await? != 0 {
                return anyhow::Ok(());
            }
            repository
                .update(cx, |repository, _| repository.remove_worktree(root, true))
                .await??;
            this.update(cx, |this, cx| this.rediscover(project.clone(), cx))
                .ok();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
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
            .then(|| row.repository.clone().zip(row.root.clone()))
            .flatten()
            .map(|(repository, root)| (repository, root, row.workspace.clone()));
        let delete_name = row.name.clone();
        // What a click does: go to the worktree if the window has it open,
        // otherwise open it.
        let activate = row.workspace.clone();
        let open = row
            .root
            .clone()
            .filter(|_| !is_open)
            .map(|root| (row.switch_from.clone(), row.key.clone(), root));
        let open_name = row.name.clone();
        ListItem::new(("worktree", index))
            .spacing(ListItemSpacing::Sparse)
            .height(ROW_HEIGHT)
            .indent_level(1)
            .indent_step_size(px(12.))
            .selectable(true)
            .toggle_state(row.is_active)
            .start_slot(
                Icon::new(IconName::GitBranch)
                    .size(IconSize::Small)
                    // What became of the branch outranks which worktree you
                    // are in: the row you are in is already the tinted one,
                    // and a pull request is the thing you cannot see from
                    // here. Green while it is open, purple once it is merged.
                    .color(match row.pull_request {
                        Some(state) => pull_request_color(state),
                        // The worktree the window is showing. One accented
                        // icon says which of them you are in from across the
                        // tree.
                        None if row.is_active => Color::Accent,
                        None if is_open => Color::Muted,
                        // A worktree that exists but is not open in this
                        // window: the row is there to be clicked, and should
                        // not read as one of the window's own.
                        None => Color::Ignored,
                    }),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_1p5()
                    .justify_between()
                    .child(
                        // Both of these truncate: a worktree named after a long
                        // branch would otherwise widen the row past the panel
                        // and carry the delete button off the edge with it.
                        Label::new(row.name.clone())
                            .single_line()
                            .truncate()
                            .color(if is_open {
                                Color::Default
                            } else {
                                Color::Muted
                            }),
                    )
                    .children(render_agents(index, &row.agents)),
            )
            .end_slot_on_hover(
                h_flex().when_some(delete, |this, (repository, root, workspace)| {
                    this.child(
                        IconButton::new(("delete-worktree", index), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Delete Worktree"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.delete_worktree(
                                    repository.clone(),
                                    workspace.clone(),
                                    root.clone(),
                                    delete_name.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    )
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                match (activate.clone(), open.clone()) {
                    (Some(workspace), _) => this.activate(workspace, window, cx),
                    (None, Some((from, key, root))) => {
                        this.open_worktree(from, key, root, open_name.clone(), window, cx)
                    }
                    (None, None) => {}
                }
            }))
    }
}

/// The height of a row, project and worktree alike.
///
/// Taller than what the list item's own padding gives it, which is sized for a
/// long list read at a glance. This one is short — a handful of projects and
/// their worktrees — and its rows are click targets you aim at, so they are
/// given the room. In rems, so that it follows the UI font size.
const ROW_HEIGHT: Rems = Rems(1.875);

/// How long the chevron takes to turn. Short enough that expanding still feels
/// like a direct response to the click, long enough to be seen.
const CHEVRON_TURN: Duration = Duration::from_millis(120);

/// The agents running in a worktree: a dot for what they are doing, and a
/// count once there is more than one of them.
///
/// It sits at the end of the row rather than beside the branch icon, because
/// the branch icon already says something about the branch itself. This says
/// what is happening in it right now.
fn render_agents(index: usize, agents: &AgentSummary) -> Option<impl IntoElement> {
    let state = agents.state()?;
    let total = agents.total();
    let summary = if agents.needs_input > 0 {
        format!(
            "{} of {total} waiting for you",
            agents.needs_input
        )
    } else if agents.working > 0 {
        format!("{} of {total} working", agents.working)
    } else {
        format!("{total} idle")
    };

    Some(
        h_flex()
            .id(("agents", index))
            .gap_0p5()
            .child(Indicator::dot().color(agent_state_color(state)))
            .when(total > 1, |this| {
                this.child(
                    Label::new(total.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .tooltip(Tooltip::text(format!(
                "{total} agent{}: {summary}",
                if total == 1 { "" } else { "s" }
            ))),
    )
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

/// Asks for the name of a new worktree.
///
/// One field, because the name is the whole answer: it names the directory the
/// worktree is created in and the branch that is created with it. Where it goes
/// is shown rather than asked; see [`worktrees_directory`].
struct NameWorktree {
    name: Entity<InputField>,
    /// The project's worktree directory, shown so that the layout Bench
    /// imposes is visible before the worktree is made.
    directory: PathBuf,
    confirm: Box<dyn Fn(SharedString, &mut Window, &mut App)>,
}

impl NameWorktree {
    fn new(
        directory: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        confirm: impl Fn(SharedString, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            name: cx.new(|cx| InputField::new(window, cx, "Worktree name")),
            directory,
            confirm: Box::new(confirm),
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text(cx).trim().to_owned();
        // Nothing to do with a name git would refuse but wait for a better one.
        if !is_branch_name(&name) {
            return;
        }
        (self.confirm)(name.into(), window, cx);
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Focusable for NameWorktree {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for NameWorktree {}

impl ModalView for NameWorktree {}

impl Render for NameWorktree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("NameWorktree")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(34.))
            .p_3()
            .gap_2()
            .child(Label::new("New Worktree"))
            .child(self.name.clone())
            .child(
                Label::new(format!("{}/<name>", self.directory.display()))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
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
    let mut repositories: Vec<Entity<Repository>> = Vec::new();
    for open in open_worktrees {
        let project = open.workspace.read(cx).project().read(cx);
        repositories.extend(project.repositories(cx).values().cloned());
    }
    repositories
        .iter()
        .find(|repository| anchor.is_some() && repository_anchor(repository, cx) == anchor)
        .or_else(|| repositories.first())
        .cloned()
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
        let project = open.workspace.read(cx).project().read(cx);
        for repository in project.repositories(cx).values() {
            for worktree in repository.read(cx).snapshot().linked_worktrees.iter() {
                if !linked
                    .iter()
                    .any(|(listed, _)| listed.path == worktree.path)
                {
                    linked.push((worktree.clone(), repository.clone()));
                }
            }
        }
    }
    linked
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
    use project::project_settings::ProjectSettings;
    use project::{FakeFs, Project, WorktreeSettings};
    use settings::{Settings as _, SettingsStore};
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
                None,
                closed.clone(),
                PathBuf::from("/closed"),
                "closed".into(),
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
