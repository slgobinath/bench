//! Bench's MCP server: tools that let an agent see what Bench is showing,
//! create worktrees in it, open pages in it and drive its browser.
//!
//! It is served on a Unix socket at [`paths::mcp_socket_path`], which only its
//! owner can open, and agents reach it through `cli --mcp`, a stdio bridge to
//! that socket. The bridge tells the server which directory the agent was
//! started in, which is how the tools default to the worktree the agent is
//! working in.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use base64::Engine as _;
use context_server::listener::{McpServer, McpServerTool, ToolContext, ToolResponse};
use context_server::types::{
    self, Implementation, InitializeResponse, ProtocolVersion, Request, ServerCapabilities,
    ToolAnnotations, ToolResponseContent, ToolsCapabilities,
};
use editor::Editor;
use futures::FutureExt as _;
use futures::channel::oneshot;
use gpui::{
    App, AppContext as _, AsyncApp, Entity, Global, PromptLevel, Task, TaskExt as _, WeakEntity,
    WindowHandle, actions,
};
use html_preview::{
    BrowserView, ClickOutcome, ClickTarget, ConsoleEntry, HtmlPreviewView, TypeOutcome,
};
use language::{DiagnosticSeverity, OffsetRangeExt as _};
use linear::{Issue, Linear};
use project::Project;
use remote::{Interactive, RemoteClient, RemoteConnectionOptions, mcp_forward};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Toast, Workspace, notifications::NotificationId};
use worktree_panel::{ProjectListing, WorktreePanel};

/// How many characters of a selection are returned. A whole-file selection in
/// a large file would otherwise be the entire response.
const MAX_SELECTION_CHARS: usize = 20_000;

/// How many files a project-wide diagnostics call opens to read diagnostics
/// from. Each is a buffer Bench has to load.
const MAX_DIAGNOSTIC_FILES: usize = 50;

actions!(
    bench,
    [
        /// Lets agents running on this project's remote host drive Bench, such
        /// as opening a web app they serve in its browser.
        InstallMcpOnHost,
    ]
);

struct BenchMcpServer(#[allow(dead_code)] McpServer);

impl Global for BenchMcpServer {}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &InstallMcpOnHost, window, cx| {
            let Some(client) = workspace.project().read(cx).remote_client() else {
                toast(
                    "Open a project on a remote host to let its agents drive Bench.".to_string(),
                    workspace,
                    cx,
                );
                return;
            };
            install_on_host(client, window, cx);
        });
    })
    .detach();

    cx.spawn(async move |cx| {
        let mut server = McpServer::bind(paths::mcp_socket_path().clone(), cx).await?;
        server.handle_request::<InitializeRequest>(|params, cx| {
            Task::ready(Ok(initialize_response(params, cx)))
        });
        server.handle_request::<PingRequest>(|_, _| Task::ready(Ok(serde_json::Map::new())));
        server.add_tool(ListProjects);
        server.add_tool(CreateWorktree);
        server.add_tool(GetActiveEditor);
        server.add_tool(GetOpenFiles);
        server.add_tool(GetDiagnostics);
        server.add_tool(OpenHtmlFile);
        server.add_tool(BrowserOpen);
        server.add_tool(BrowserNavigate);
        server.add_tool(BrowserScreenshot);
        server.add_tool(BrowserSnapshot);
        server.add_tool(BrowserClick);
        server.add_tool(BrowserType);
        server.add_tool(BrowserPressKey);
        server.add_tool(BrowserEval);
        server.add_tool(BrowserConsoleLogs);
        cx.update(|cx| cx.set_global(BenchMcpServer(server)));
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// `initialize`, with its parameters left loose: clients send capabilities
/// newer than the types know, and only the protocol version is read.
struct InitializeRequest;

impl Request for InitializeRequest {
    type Params = serde_json::Value;
    type Response = InitializeResponse;
    const METHOD: &'static str = "initialize";
}

/// `ping`, which clients send with no parameters or with an empty object.
struct PingRequest;

impl Request for PingRequest {
    type Params = Option<serde_json::Value>;
    type Response = serde_json::Map<String, serde_json::Value>;
    const METHOD: &'static str = "ping";
}

fn initialize_response(params: serde_json::Value, cx: &App) -> InitializeResponse {
    let requested = params
        .get("protocolVersion")
        .and_then(|version| version.as_str());
    let supported = [
        types::LATEST_PROTOCOL_VERSION,
        types::VERSION_2025_06_18,
        types::VERSION_2025_03_26,
        types::VERSION_2024_11_05,
    ];
    let protocol_version = requested
        .filter(|requested| supported.contains(requested))
        .unwrap_or(types::LATEST_PROTOCOL_VERSION);
    InitializeResponse {
        protocol_version: ProtocolVersion(protocol_version.to_string()),
        capabilities: ServerCapabilities {
            tools: Some(ToolsCapabilities {
                list_changed: Some(false),
            }),
            ..ServerCapabilities::default()
        },
        server_info: Implementation {
            name: "bench".to_string(),
            title: Some("Bench".to_string()),
            version: release_channel::AppVersion::global(cx).to_string(),
            description: None,
        },
        meta: None,
    }
}

fn read_only() -> ToolAnnotations {
    ToolAnnotations {
        title: None,
        read_only_hint: Some(true),
        destructive_hint: Some(false),
        idempotent_hint: Some(true),
        open_world_hint: Some(false),
    }
}

/// Responds with the output as pretty JSON text as well as structured
/// content, since not every client reads the structured half.
fn respond<T: Serialize>(output: T) -> Result<ToolResponse<T>> {
    let text = serde_json::to_string_pretty(&output)?;
    Ok(ToolResponse {
        content: vec![ToolResponseContent::Text { text }],
        structured_content: output,
    })
}

/// The worktree a tool is about: the one given, or else the one the client is
/// working in.
fn target_directory(worktree: Option<String>, context: &ToolContext) -> Result<PathBuf> {
    worktree
        .map(PathBuf::from)
        .or_else(|| context.client_directory.clone())
        .context("Pass `worktree`: the client did not say which directory it is working in")
}

fn windows(cx: &App) -> Vec<WindowHandle<MultiWorkspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .collect()
}

fn host_name(workspace: &Entity<Workspace>, cx: &App) -> Option<String> {
    let remote_client = workspace.read(cx).project().read(cx).remote_client()?;
    Some(remote_client.read(cx).connection_options().display_name())
}

fn workspace_roots(workspace: &Entity<Workspace>, cx: &App) -> Vec<PathBuf> {
    workspace
        .read(cx)
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect()
}

/// The open workspace whose folder holds `directory`, preferring the deepest:
/// a worktree under `~/bench` and a project nested in another both sit inside
/// a folder that is open in a workspace of its own.
///
/// A client on a remote host only sees the projects on that host, since its
/// paths mean nothing in the others.
fn workspace_for_directory(
    directory: &Path,
    host: Option<&str>,
    cx: &App,
) -> Result<Entity<Workspace>> {
    let mut best: Option<(usize, Entity<Workspace>)> = None;
    for window in windows(cx) {
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        for workspace in multi_workspace.workspaces() {
            if let Some(host) = host
                && host_name(workspace, cx).as_deref() != Some(host)
            {
                continue;
            }
            for root in workspace_roots(workspace, cx) {
                let depth = root.components().count();
                if directory.starts_with(&root)
                    && best
                        .as_ref()
                        .is_none_or(|(best_depth, _)| depth > *best_depth)
                {
                    best = Some((depth, workspace.clone()));
                }
            }
        }
    }
    best.map(|(_, workspace)| workspace).with_context(|| {
        format!(
            "No worktree containing {} is open in Bench",
            directory.display()
        )
    })
}

/// Each window's projects, as its worktree panel lists them.
fn window_projects(
    cx: &App,
) -> Vec<(
    WindowHandle<MultiWorkspace>,
    Entity<WorktreePanel>,
    Vec<ProjectListing>,
)> {
    windows(cx)
        .into_iter()
        .filter_map(|window| {
            let panel = window
                .read(cx)
                .ok()?
                .workspace()
                .read(cx)
                .panel::<WorktreePanel>(cx)?;
            let projects = panel.read(cx).projects(cx);
            Some((window, panel, projects))
        })
        .collect()
}

/// The worktree root that holds `directory`, the deepest when they nest.
fn containing_root<'a>(
    roots: impl IntoIterator<Item = &'a Path>,
    directory: &Path,
) -> Option<&'a Path> {
    roots
        .into_iter()
        .filter(|root| directory.starts_with(root))
        .max_by_key(|root| root.components().count())
}

#[derive(Clone)]
struct ListProjects;

/// Lists the projects open in Bench and every git worktree of each, with the
/// branch it has checked out, its title, and the agents and other processes
/// running in its terminals. The worktree the caller is working in is marked
/// `is_current`.
#[derive(Deserialize, JsonSchema)]
struct ListProjectsInput {}

#[derive(Serialize, JsonSchema)]
struct ListProjectsOutput {
    projects: Vec<ProjectOutput>,
}

#[derive(Serialize, JsonSchema)]
struct ProjectOutput {
    name: String,
    /// The project's main checkout. Pass it as `project` to `create_worktree`.
    root: Option<PathBuf>,
    /// Whether Bench has any of the project's worktrees open.
    is_open: bool,
    /// The key of the Linear team whose issues are worked on here, such as
    /// `RB`.
    linear_team: Option<String>,
    worktrees: Vec<WorktreeOutput>,
}

#[derive(Serialize, JsonSchema)]
struct WorktreeOutput {
    name: String,
    path: Option<PathBuf>,
    branch: Option<String>,
    title: Option<String>,
    /// Whether Bench has the worktree open.
    is_open: bool,
    /// Whether it is the repository's main checkout rather than a linked
    /// worktree.
    is_main: bool,
    /// Whether it is the worktree the caller is working in.
    is_current: bool,
    agents_working: usize,
    agents_idle: usize,
    agents_needing_input: usize,
    /// Commands running in its terminals that are not agents, such as dev
    /// servers.
    processes: Vec<String>,
}

impl McpServerTool for ListProjects {
    type Input = ListProjectsInput;
    type Output = ListProjectsOutput;

    const NAME: &'static str = "list_projects";

    fn annotations(&self) -> ToolAnnotations {
        read_only()
    }

    async fn run(
        &self,
        _input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let listings: Vec<ProjectListing> = cx.update(|cx| {
            let mut listings: Vec<ProjectListing> = Vec::new();
            for (_, _, projects) in window_projects(cx) {
                for project in projects {
                    let already_listed = listings
                        .iter()
                        .any(|listed| listed.root.is_some() && listed.root == project.root);
                    if !already_listed {
                        listings.push(project);
                    }
                }
            }
            listings
        });

        let current = context.client_directory.as_deref().and_then(|directory| {
            let roots = listings
                .iter()
                .flat_map(|project| &project.worktrees)
                .filter_map(|worktree| worktree.root.as_deref());
            containing_root(roots, directory).map(Path::to_path_buf)
        });

        let projects = listings
            .into_iter()
            .map(|project| ProjectOutput {
                name: project.name.to_string(),
                root: project.root,
                is_open: project.is_open,
                linear_team: project.linear_team.map(|team| team.key),
                worktrees: project
                    .worktrees
                    .into_iter()
                    .map(|worktree| WorktreeOutput {
                        is_current: current.is_some() && worktree.root == current,
                        name: worktree.name.to_string(),
                        path: worktree.root,
                        branch: worktree.branch.map(|branch| branch.to_string()),
                        title: worktree.title.map(|title| title.to_string()),
                        is_open: worktree.is_open,
                        is_main: worktree.is_main,
                        agents_working: worktree.agents.working,
                        agents_idle: worktree.agents.idle,
                        agents_needing_input: worktree.agents.needs_input,
                        processes: worktree
                            .processes
                            .into_iter()
                            .map(|process| process.to_string())
                            .collect(),
                    })
                    .collect(),
            })
            .collect();
        respond(ListProjectsOutput { projects })
    }
}

#[derive(Clone)]
struct CreateWorktree;

/// Creates a git worktree in a project, on a new branch off the latest default
/// branch, under `~/bench/<project>/<name>`, and opens it in Bench. Optionally
/// starts a Claude agent in it.
///
/// For a Linear issue, pass `issue` (such as `RB-233`) and leave out `name`:
/// the branch is named the way Bench's own "new worktree" dialog names it,
/// after the branch name Linear suggests for the issue. The worktree is
/// linked to the issue and titled after it, the issue is moved to started,
/// and an agent started in it is handed the issue. Do not ask the user for a
/// branch name in that case.
#[derive(Deserialize, JsonSchema)]
struct CreateWorktreeInput {
    /// The Linear issue the worktree is for, by its identifier, such as
    /// `RB-233`.
    #[serde(default)]
    issue: Option<String>,
    /// The worktree's branch name, such as `fix-login-redirect`. Leave it out
    /// to use the Linear issue's branch name. Text that is not a valid branch
    /// name is taken as a description, and a name is made from its words.
    #[serde(default)]
    name: Option<String>,
    /// A short description of the work, shown as the worktree's title in
    /// Bench. Defaults to the Linear issue's title.
    #[serde(default)]
    description: Option<String>,
    /// The project to create the worktree in: its `root` or `name` as
    /// `list_projects` gives them. Defaults to the project linked to the
    /// issue's Linear team, and otherwise to the project the caller is working
    /// in.
    #[serde(default)]
    project: Option<String>,
    /// Start a Claude agent in a terminal in the new worktree.
    #[serde(default)]
    start_agent: bool,
    /// When a branch of the worktree's name already exists, check it out
    /// instead of failing. An existing branch is never replaced.
    #[serde(default)]
    use_existing_branch: bool,
}

#[derive(Serialize, JsonSchema)]
struct CreateWorktreeOutput {
    path: PathBuf,
    /// The project it was created in.
    project: String,
}

impl McpServerTool for CreateWorktree {
    type Input = CreateWorktreeInput;
    type Output = CreateWorktreeOutput;

    const NAME: &'static str = "create_worktree";

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            title: None,
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(false),
            open_world_hint: Some(false),
        }
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let issue = match input.issue {
            Some(identifier) => {
                let identifier = identifier.trim().to_uppercase();
                let lookup = cx.update(|cx| {
                    let linear = Linear::global(cx).context("Linear is not set up in Bench")?;
                    anyhow::Ok(linear.read(cx).issue(&identifier, cx))
                })?;
                let issue = lookup
                    .await
                    .with_context(|| format!("Could not find the Linear issue {identifier}"))?;
                Some(issue)
            }
            None => None,
        };
        let (window, panel, root, project_name) = cx.update(|cx| {
            find_project(
                input.project.as_deref(),
                issue.as_deref(),
                context.client_directory.as_deref(),
                cx,
            )
        })?;
        let created = window.update(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.create_worktree_in_project(
                    &root,
                    input.name.map(Into::into),
                    input.description.map(Into::into),
                    issue,
                    input.start_agent,
                    input.use_existing_branch,
                    window,
                    cx,
                )
            })
        })?;
        let path = created.await?;
        respond(CreateWorktreeOutput {
            path,
            project: project_name,
        })
    }
}

/// The window, panel, root and name of the project a worktree goes in.
///
/// The one `project` names, when it names one. For an issue, the project
/// linked to the issue's Linear team, as Bench's own "worktree for issue"
/// command picks it. Otherwise, or when several projects share the team, the
/// one `client_directory` is in.
fn find_project(
    project: Option<&str>,
    issue: Option<&Issue>,
    client_directory: Option<&Path>,
    cx: &App,
) -> Result<(
    WindowHandle<MultiWorkspace>,
    Entity<WorktreePanel>,
    PathBuf,
    String,
)> {
    // A project open in several windows is one project.
    let mut candidates: Vec<(
        WindowHandle<MultiWorkspace>,
        Entity<WorktreePanel>,
        ProjectListing,
    )> = Vec::new();
    for (window, panel, projects) in window_projects(cx) {
        for project in projects {
            let already_listed = candidates
                .iter()
                .any(|(_, _, listed)| listed.root.is_some() && listed.root == project.root);
            if !already_listed {
                candidates.push((window, panel.clone(), project));
            }
        }
    }

    let named = |listing: &ProjectListing, project: &str| {
        listing.root.as_deref() == Some(Path::new(project)) || listing.name.as_ref() == project
    };
    // How deep the client's folder is inside the project, when it is: a
    // project cloned inside another project's worktree holds the folder as
    // well as the outer one does, and the deeper one is the one it is in.
    let client_depth = |listing: &ProjectListing| {
        let directory = client_directory?;
        let roots = listing
            .worktrees
            .iter()
            .filter_map(|worktree| worktree.root.as_deref())
            .chain(listing.root.as_deref());
        containing_root(roots, directory).map(|root| root.components().count())
    };
    let on_team = |listing: &ProjectListing| {
        issue.is_some_and(|issue| {
            listing
                .linear_team
                .as_ref()
                .is_some_and(|team| team.id == issue.team.id.as_ref())
        })
    };

    let index = if let Some(project) = project {
        candidates
            .iter()
            .position(|(_, _, listing)| named(listing, project))
            .with_context(|| {
                format!("No project named or rooted at “{project}” is open in Bench")
            })?
    } else {
        let on_team: Vec<(usize, &ProjectListing)> = candidates
            .iter()
            .map(|(_, _, listing)| listing)
            .enumerate()
            .filter(|(_, listing)| on_team(listing))
            .collect();
        let client_project = candidates
            .iter()
            .map(|(_, _, listing)| listing)
            .enumerate()
            .filter_map(|(index, listing)| Some((index, client_depth(listing)?)))
            .max_by_key(|(_, depth)| *depth)
            .map(|(index, _)| index);
        let client_project_on_team = on_team
            .iter()
            .filter_map(|(index, listing)| Some((*index, client_depth(listing)?)))
            .max_by_key(|(_, depth)| *depth)
            .map(|(index, _)| index);
        if let [(only, _)] = on_team.as_slice() {
            *only
        } else if let Some(index) = client_project_on_team {
            index
        } else if !on_team.is_empty() {
            let names: Vec<String> = on_team
                .iter()
                .map(|(_, listing)| format!("“{}”", listing.name))
                .collect();
            anyhow::bail!(
                "Several projects are linked to this issue's Linear team ({}); pass `project`",
                names.join(", ")
            );
        } else if let Some(index) = client_project {
            index
        } else {
            anyhow::bail!(match client_directory {
                Some(directory) => format!(
                    "{} is not in a project open in Bench; pass `project`",
                    directory.display()
                ),
                None => "Pass `project`: the client did not say which directory it is working in"
                    .to_string(),
            });
        }
    };

    let Some((window, panel, listing)) = candidates.into_iter().nth(index) else {
        anyhow::bail!("The project list changed while it was being read");
    };
    let root = listing
        .root
        .with_context(|| format!("The project “{}” has no folder", listing.name))?;
    Ok((window, panel, root, listing.name.to_string()))
}

#[derive(Clone)]
struct GetActiveEditor;

/// Returns the file open in the active editor of a worktree in Bench, with the
/// cursor positions and selected text. Lines and columns are 1-based; columns
/// count bytes.
#[derive(Deserialize, JsonSchema)]
struct GetActiveEditorInput {
    /// The worktree's directory. Defaults to the worktree the caller is
    /// working in.
    #[serde(default)]
    worktree: Option<String>,
}

#[derive(Serialize, JsonSchema)]
struct ActiveEditorOutput {
    /// The file the editor is showing, absent for an unsaved buffer or an
    /// editor showing several files.
    path: Option<PathBuf>,
    /// Whether it has changes that are not saved.
    is_dirty: bool,
    selections: Vec<SelectionOutput>,
}

#[derive(Serialize, JsonSchema)]
struct SelectionOutput {
    /// The file this selection is in, which only differs from the editor's in
    /// an editor showing several files.
    path: Option<PathBuf>,
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
    /// The selected text, absent for a cursor with nothing selected.
    text: Option<String>,
    /// Whether `text` was cut short.
    text_truncated: bool,
}

impl McpServerTool for GetActiveEditor {
    type Input = GetActiveEditorInput;
    type Output = ActiveEditorOutput;

    const NAME: &'static str = "get_active_editor";

    fn annotations(&self) -> ToolAnnotations {
        read_only()
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let directory = target_directory(input.worktree, &context)?;
        let output = cx.update(|cx| -> Result<ActiveEditorOutput> {
            let workspace =
                workspace_for_directory(&directory, context.client_host.as_deref(), cx)?;
            let editor = workspace
                .read(cx)
                .active_item_as::<Editor>(cx)
                .context("The worktree has no editor open")?;
            Ok(editor_state(&editor, cx))
        })?;
        respond(output)
    }
}

fn editor_state(editor: &Entity<Editor>, cx: &mut App) -> ActiveEditorOutput {
    editor.update(cx, |editor, cx| {
        let display_snapshot = editor.display_snapshot(cx);
        let selections = editor.selections.all_adjusted(&display_snapshot);
        let multi_buffer = editor.buffer().read(cx);
        let is_dirty = multi_buffer.is_dirty(cx);
        let snapshot = multi_buffer.snapshot(cx);

        let path = snapshot
            .as_singleton()
            .and_then(|buffer| buffer.file())
            .and_then(|file| file.as_local())
            .map(|file| file.abs_path(cx));

        let selections = selections
            .into_iter()
            .filter_map(|selection| {
                let (buffer, start) = snapshot.point_to_buffer_point(selection.start)?;
                let path = buffer
                    .file()
                    .and_then(|file| file.as_local())
                    .map(|file| file.abs_path(cx));
                let (_, end) = snapshot.point_to_buffer_point(selection.end)?;
                let (text, text_truncated) = if selection.is_empty() {
                    (None, false)
                } else {
                    let text: String = snapshot
                        .text_for_range(selection.start..selection.end)
                        .collect();
                    let truncated = text.chars().count() > MAX_SELECTION_CHARS;
                    let text = if truncated {
                        text.chars().take(MAX_SELECTION_CHARS).collect()
                    } else {
                        text
                    };
                    (Some(text), truncated)
                };
                Some(SelectionOutput {
                    path,
                    start_line: start.row + 1,
                    start_column: start.column + 1,
                    end_line: end.row + 1,
                    end_column: end.column + 1,
                    text,
                    text_truncated,
                })
            })
            .collect();

        ActiveEditorOutput {
            path,
            is_dirty,
            selections,
        }
    })
}

#[derive(Clone)]
struct GetOpenFiles;

/// Lists the files open in tabs in a worktree in Bench.
#[derive(Deserialize, JsonSchema)]
struct GetOpenFilesInput {
    /// The worktree's directory. Defaults to the worktree the caller is
    /// working in.
    #[serde(default)]
    worktree: Option<String>,
}

#[derive(Serialize, JsonSchema)]
struct OpenFilesOutput {
    files: Vec<OpenFileOutput>,
}

#[derive(Serialize, JsonSchema)]
struct OpenFileOutput {
    path: PathBuf,
    /// Whether it is the tab in front in the active pane.
    is_active: bool,
    /// Whether it has changes that are not saved.
    is_dirty: bool,
}

impl McpServerTool for GetOpenFiles {
    type Input = GetOpenFilesInput;
    type Output = OpenFilesOutput;

    const NAME: &'static str = "get_open_files";

    fn annotations(&self) -> ToolAnnotations {
        read_only()
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let directory = target_directory(input.worktree, &context)?;
        let files = cx.update(|cx| -> Result<Vec<OpenFileOutput>> {
            let workspace =
                workspace_for_directory(&directory, context.client_host.as_deref(), cx)?;
            let workspace = workspace.read(cx);
            let project = workspace.project().read(cx);
            let active_item = workspace.active_item(cx).map(|item| item.item_id());
            let mut files: Vec<OpenFileOutput> = Vec::new();
            for item in workspace.items(cx) {
                let Some(path) = item
                    .project_path(cx)
                    .and_then(|project_path| project.absolute_path(&project_path, cx))
                else {
                    continue;
                };
                let is_active = active_item == Some(item.item_id());
                let is_dirty = item.is_dirty(cx);
                // The same file in two panes is one open file.
                match files.iter_mut().find(|file| file.path == path) {
                    Some(file) => {
                        file.is_active |= is_active;
                        file.is_dirty |= is_dirty;
                    }
                    None => files.push(OpenFileOutput {
                        path,
                        is_active,
                        is_dirty,
                    }),
                }
            }
            Ok(files)
        })?;
        respond(OpenFilesOutput { files })
    }
}

#[derive(Clone)]
struct GetDiagnostics;

/// Returns the errors and warnings Bench's language servers report, for one
/// file or for a whole worktree. Lines and columns are 1-based; columns count
/// bytes.
#[derive(Deserialize, JsonSchema)]
struct GetDiagnosticsInput {
    /// The file to report on, absolute or relative to the worktree. Omit it to
    /// report on every file in the worktree that has diagnostics.
    #[serde(default)]
    path: Option<String>,
    /// The worktree's directory. Defaults to the worktree the caller is
    /// working in.
    #[serde(default)]
    worktree: Option<String>,
    /// Include warnings as well as errors. Defaults to true.
    #[serde(default)]
    include_warnings: Option<bool>,
}

#[derive(Serialize, JsonSchema)]
struct DiagnosticsOutput {
    diagnostics: Vec<DiagnosticOutput>,
    /// Whether some files with diagnostics were left out, because there were
    /// too many to read.
    files_truncated: bool,
}

#[derive(Serialize, JsonSchema)]
struct DiagnosticOutput {
    path: PathBuf,
    severity: String,
    message: String,
    /// The language server or tool that reported it, such as `rustc`.
    source: Option<String>,
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
}

impl McpServerTool for GetDiagnostics {
    type Input = GetDiagnosticsInput;
    type Output = DiagnosticsOutput;

    const NAME: &'static str = "get_diagnostics";

    fn annotations(&self) -> ToolAnnotations {
        read_only()
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let include_warnings = input.include_warnings.unwrap_or(true);
        let directory = match (&input.worktree, &input.path) {
            // An absolute path is enough to find its worktree by.
            (None, Some(path)) if Path::new(path).is_absolute() => PathBuf::from(path),
            _ => target_directory(input.worktree.clone(), &context)?,
        };
        let project = cx.update(|cx| {
            workspace_for_directory(&directory, context.client_host.as_deref(), cx)
                .map(|workspace| workspace.read(cx).project().clone())
        })?;

        let (paths, files_truncated) = match input.path {
            Some(path) => {
                let root = cx.update(|cx| {
                    let roots = workspace_root_paths(&project, cx);
                    containing_root(roots.iter().map(PathBuf::as_path), &directory)
                        .map(Path::to_path_buf)
                });
                let path = PathBuf::from(path);
                let path = match root {
                    Some(root) if path.is_relative() => root.join(path),
                    _ => path,
                };
                (vec![path], false)
            }
            None => cx.update(|cx| files_with_diagnostics(&project, include_warnings, cx)),
        };

        let mut diagnostics = Vec::new();
        for path in paths {
            diagnostics.extend(file_diagnostics(&project, &path, include_warnings, cx).await?);
        }
        respond(DiagnosticsOutput {
            diagnostics,
            files_truncated,
        })
    }
}

#[derive(Clone)]
struct OpenHtmlFile;

/// Opens an HTML file from a worktree in a browser tab in Bench, so the person
/// can look at the page and select parts of it to send back to you. The tab
/// reloads when the file changes on disk. Pages are served from the worktree,
/// so relative links to scripts, styles and images work.
#[derive(Deserialize, JsonSchema)]
struct OpenHtmlFileInput {
    /// The HTML file: an absolute path, or one relative to the worktree's root.
    path: String,
    /// The worktree's directory. Defaults to the worktree the caller is
    /// working in.
    #[serde(default)]
    worktree: Option<String>,
    /// Open the tab in a split beside the active pane rather than in it.
    #[serde(default)]
    to_the_side: bool,
}

#[derive(Serialize, JsonSchema)]
struct OpenHtmlFileOutput {
    path: PathBuf,
}

impl McpServerTool for OpenHtmlFile {
    type Input = OpenHtmlFileInput;
    type Output = OpenHtmlFileOutput;

    const NAME: &'static str = "open_html_file";

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            title: None,
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: Some(false),
        }
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let path = PathBuf::from(&input.path);
        let directory = match &input.worktree {
            None if path.is_absolute() => path.clone(),
            _ => target_directory(input.worktree.clone(), &context)?,
        };
        let (window, workspace, path) = cx.update(|cx| -> Result<_> {
            let workspace =
                workspace_for_directory(&directory, context.client_host.as_deref(), cx)?;
            let path = if path.is_relative() {
                let roots = workspace_roots(&workspace, cx);
                let root = containing_root(roots.iter().map(PathBuf::as_path), &directory)
                    .context("The worktree is not open in Bench")?;
                root.join(&path)
            } else {
                path
            };
            let window = window_for_workspace(&workspace, cx)?;
            Ok((window, workspace, path))
        })?;
        anyhow::ensure!(
            html_preview::is_html_file(&path),
            "{} is not an HTML file",
            path.display()
        );
        let fs = cx.update(|cx| workspace.read(cx).project().read(cx).fs().clone());
        anyhow::ensure!(fs.is_file(&path).await, "{} does not exist", path.display());

        window.update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let project_path = workspace
                    .project()
                    .read(cx)
                    .find_project_path(&path, cx)
                    .with_context(|| format!("{} is not in an open worktree", path.display()))?;
                let pane = workspace.active_pane().clone();
                let pane = if input.to_the_side {
                    workspace.adjacent_pane_of(&pane, window, cx)
                } else {
                    pane
                };
                HtmlPreviewView::open(workspace, project_path, pane, false, window, cx)?;
                anyhow::Ok(())
            })
        })??;
        respond(OpenHtmlFileOutput { path })
    }
}

/// The window a workspace is open in.
fn window_for_workspace(
    workspace: &Entity<Workspace>,
    cx: &App,
) -> Result<WindowHandle<MultiWorkspace>> {
    windows(cx)
        .into_iter()
        .find(|window| {
            window.read(cx).is_ok_and(|multi_workspace| {
                multi_workspace
                    .workspaces()
                    .any(|candidate| candidate == workspace)
            })
        })
        .context("The worktree's window is no longer open")
}

fn workspace_root_paths(project: &Entity<Project>, cx: &App) -> Vec<PathBuf> {
    project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect()
}

/// The files the language servers have reported problems in, and whether
/// there were more than [`MAX_DIAGNOSTIC_FILES`].
fn files_with_diagnostics(
    project: &Entity<Project>,
    include_warnings: bool,
    cx: &App,
) -> (Vec<PathBuf>, bool) {
    let project = project.read(cx);
    let mut paths: Vec<PathBuf> = Vec::new();
    for (project_path, _, summary) in project.diagnostic_summaries(false, cx) {
        let relevant = summary.error_count > 0 || (include_warnings && summary.warning_count > 0);
        if !relevant {
            continue;
        }
        let Some(path) = project.absolute_path(&project_path, cx) else {
            continue;
        };
        // A file several language servers report on is listed once per server.
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    let truncated = paths.len() > MAX_DIAGNOSTIC_FILES;
    paths.truncate(MAX_DIAGNOSTIC_FILES);
    (paths, truncated)
}

async fn file_diagnostics(
    project: &Entity<Project>,
    path: &Path,
    include_warnings: bool,
    cx: &mut AsyncApp,
) -> Result<Vec<DiagnosticOutput>> {
    let open_buffer = project.update(cx, |project, cx| {
        let project_path = project
            .find_project_path(path, cx)
            .with_context(|| format!("{} is not in the worktree", path.display()))?;
        anyhow::Ok(project.open_buffer(project_path, cx))
    })?;
    let buffer = open_buffer
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());

    let mut diagnostics = Vec::new();
    for (_, group) in snapshot.diagnostic_groups(None) {
        let Some(entry) = group.entries.get(group.primary_ix) else {
            continue;
        };
        let severity = match entry.diagnostic.severity {
            DiagnosticSeverity::ERROR => "error",
            DiagnosticSeverity::WARNING if include_warnings => "warning",
            _ => continue,
        };
        let range = entry.range.to_point(&snapshot);
        diagnostics.push(DiagnosticOutput {
            path: path.to_path_buf(),
            severity: severity.to_string(),
            message: entry.diagnostic.message.as_str().to_string(),
            source: entry.diagnostic.source.clone(),
            start_line: range.start.row + 1,
            start_column: range.start.column + 1,
            end_line: range.end.row + 1,
            end_column: range.end.column + 1,
        });
    }
    Ok(diagnostics)
}

/// How long `browser_open` and `browser_navigate` wait for the page to load
/// before answering without it.
const PAGE_LOAD_TIMEOUT: Duration = Duration::from_secs(15);

/// The browser tab an agent last opened, which browser tools act on when they
/// aren't given one.
#[derive(Default)]
struct AgentBrowserTab(Option<WeakEntity<BrowserView>>);

impl Global for AgentBrowserTab {}

fn browser_tabs(workspace: &Entity<Workspace>, cx: &App) -> Vec<Entity<BrowserView>> {
    workspace
        .read(cx)
        .panes()
        .iter()
        .flat_map(|pane| {
            pane.read(cx)
                .items_of_type::<BrowserView>()
                .collect::<Vec<_>>()
        })
        .collect()
}

fn all_browser_tabs(cx: &App) -> Vec<Entity<BrowserView>> {
    windows(cx)
        .into_iter()
        .filter_map(|window| window.read(cx).ok())
        .flat_map(|multi_workspace| {
            multi_workspace
                .workspaces()
                .flat_map(|workspace| browser_tabs(workspace, cx))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The browser tab a tool acts on: the one given; else the one the agent last
/// opened; else, in the worktree the agent is working in, the one showing in
/// the active pane or the only one; else the only one open in Bench.
fn find_browser_tab(
    tab: Option<u64>,
    context: &ToolContext,
    cx: &App,
) -> Result<Entity<BrowserView>> {
    let tabs = all_browser_tabs(cx);
    if let Some(tab) = tab {
        return tabs
            .into_iter()
            .find(|browser| browser.entity_id().as_u64() == tab)
            .with_context(|| format!("No browser tab {tab} is open"));
    }
    if let Some(browser) = cx
        .try_global::<AgentBrowserTab>()
        .and_then(|last| last.0.as_ref()?.upgrade())
        .filter(|browser| tabs.contains(browser))
    {
        return Ok(browser);
    }
    if let Some(directory) = &context.client_directory
        && let Ok(workspace) =
            workspace_for_directory(directory, context.client_host.as_deref(), cx)
    {
        let active = workspace
            .read(cx)
            .active_pane()
            .read(cx)
            .active_item()
            .and_then(|item| item.downcast::<BrowserView>());
        if let Some(browser) = active {
            return Ok(browser);
        }
        if let [browser] = browser_tabs(&workspace, cx).as_slice() {
            return Ok(browser.clone());
        }
    }
    match tabs.as_slice() {
        [browser] => Ok(browser.clone()),
        [] => anyhow::bail!("No browser tab is open in Bench. Open one with `browser_open`."),
        _ => anyhow::bail!(
            "Several browser tabs are open; pass `tab`, one of {}",
            tabs.iter()
                .map(|browser| format!(
                    "{} ({})",
                    browser.entity_id().as_u64(),
                    browser.read(cx).url()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[derive(Serialize, JsonSchema)]
struct BrowserPageOutput {
    /// The tab's id, which other browser tools take as `tab`.
    tab: u64,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// Whether the page finished loading before this answered.
    loaded: bool,
}

fn browser_page_output(browser: &Entity<BrowserView>, loaded: bool, cx: &App) -> BrowserPageOutput {
    let view = browser.read(cx);
    BrowserPageOutput {
        tab: browser.entity_id().as_u64(),
        url: view.url().to_string(),
        title: view.title().map(|title| title.to_string()),
        loaded,
    }
}

/// Waits for `load` to resolve, up to [`PAGE_LOAD_TIMEOUT`], returning
/// whether it did.
async fn wait_for_page_load(load: oneshot::Receiver<()>, cx: &mut AsyncApp) -> bool {
    let timeout = cx.background_executor().timer(PAGE_LOAD_TIMEOUT);
    futures::select_biased! {
        loaded = load.fuse() => loaded.is_ok(),
        _ = timeout.fuse() => false,
    }
}

#[derive(Clone)]
struct BrowserOpen;

/// Opens a web page in a browser tab in Bench, such as the app the worktree
/// serves in development (`localhost:3000`). It reuses the tab you last
/// opened unless `new_tab` is set. In a remote project, `localhost` is the
/// remote host: its port is forwarded over the project's SSH connection. It
/// waits up to 15 seconds for the page to load. The tab's engine is WebKit
/// (Safari's).
#[derive(Deserialize, JsonSchema)]
struct BrowserOpenInput {
    /// The page, such as `http://localhost:5173/login`. A bare host and port
    /// such as `localhost:3000`, or just a port, is served over HTTP from
    /// this machine (or the remote host, in a remote project).
    url: String,
    /// The worktree's directory, which picks the window the tab opens in.
    /// Defaults to the worktree the caller is working in.
    #[serde(default)]
    worktree: Option<String>,
    /// A tab to load the page in, rather than the one you last opened.
    #[serde(default)]
    tab: Option<u64>,
    /// Open a new tab even if there is one to reuse.
    #[serde(default)]
    new_tab: bool,
    /// Open a new tab in a split beside the active pane rather than in it.
    #[serde(default)]
    to_the_side: bool,
}

impl McpServerTool for BrowserOpen {
    type Input = BrowserOpenInput;
    type Output = BrowserPageOutput;

    const NAME: &'static str = "browser_open";

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            title: None,
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: Some(true),
        }
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let existing = cx.update(|cx| {
            if input.tab.is_none() && input.new_tab {
                return Ok(None);
            }
            match find_browser_tab(input.tab, &context, cx) {
                Ok(browser) => Ok(Some(browser)),
                Err(error) if input.tab.is_some() => Err(error),
                Err(_) => Ok(None),
            }
        })?;
        let browser = match existing {
            Some(browser) => browser,
            None => {
                let directory = input
                    .worktree
                    .clone()
                    .map(PathBuf::from)
                    .or_else(|| context.client_directory.clone());
                let (window, workspace) = cx.update(|cx| -> Result<_> {
                    let workspace = match &directory {
                        Some(directory) => {
                            workspace_for_directory(directory, context.client_host.as_deref(), cx)?
                        }
                        None => windows(cx)
                            .first()
                            .and_then(|window| window.read(cx).ok())
                            .map(|multi_workspace| multi_workspace.workspace().clone())
                            .context("No Bench window is open")?,
                    };
                    let window = window_for_workspace(&workspace, cx)?;
                    Ok((window, workspace))
                })?;
                window.update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        let pane = workspace.active_pane().clone();
                        let pane = if input.to_the_side {
                            workspace.adjacent_pane_of(&pane, window, cx)
                        } else {
                            pane
                        };
                        BrowserView::open(workspace, pane, false, window, cx)
                    })
                })?
            }
        };
        cx.update(|cx| {
            cx.set_global(AgentBrowserTab(Some(browser.downgrade())));
        });

        let (navigation, load) = browser.update(cx, |browser, cx| {
            (browser.navigate(&input.url, cx), browser.wait_for_load())
        });
        navigation.await?;
        html_preview::reveal(&browser, cx).await.log_err();
        let loaded = wait_for_page_load(load, cx).await;
        respond(cx.update(|cx| browser_page_output(&browser, loaded, cx)))
    }
}

#[derive(Clone)]
struct BrowserNavigate;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum BrowserNavigation {
    Back,
    Forward,
    Reload,
}

/// Goes back or forward in a browser tab's history, or reloads its page, and
/// waits up to 15 seconds for the page to load. Reloading picks up changes
/// to the app when its dev server doesn't reload it itself.
#[derive(Deserialize, JsonSchema)]
struct BrowserNavigateInput {
    action: BrowserNavigation,
    /// The tab. Defaults to the one you last opened.
    #[serde(default)]
    tab: Option<u64>,
}

impl McpServerTool for BrowserNavigate {
    type Input = BrowserNavigateInput;
    type Output = BrowserPageOutput;

    const NAME: &'static str = "browser_navigate";

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            title: None,
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(false),
            open_world_hint: Some(true),
        }
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let browser = cx.update(|cx| find_browser_tab(input.tab, &context, cx))?;
        let (navigation, load) = browser.update(cx, |browser, cx| {
            let load = browser.wait_for_load();
            let navigation = match input.action {
                BrowserNavigation::Back => Task::ready(browser.go_back(cx).map(|()| String::new())),
                BrowserNavigation::Forward => {
                    Task::ready(browser.go_forward(cx).map(|()| String::new()))
                }
                BrowserNavigation::Reload => browser.reload(cx),
            };
            (navigation, load)
        });
        navigation.await?;
        let loaded = wait_for_page_load(load, cx).await;
        respond(cx.update(|cx| browser_page_output(&browser, loaded, cx)))
    }
}

#[derive(Clone)]
struct BrowserScreenshot;

#[derive(Serialize, JsonSchema)]
struct BrowserScreenshotOutput {
    tab: u64,
    url: String,
    /// The image's size, which is the page's size in CSS pixels: a point in
    /// the image is a point `browser_click` takes as `x` and `y`.
    width: u32,
    height: u32,
}

/// Captures what a browser tab shows, as a PNG the size of the page in CSS
/// pixels. Brings the tab to the front of its pane if it isn't. Use it to
/// check how the app looks, and with `browser_click` at `x`/`y` for apps
/// such as Flutter's that draw to a canvas.
#[derive(Deserialize, JsonSchema)]
struct BrowserScreenshotInput {
    /// The tab. Defaults to the one you last opened.
    #[serde(default)]
    tab: Option<u64>,
}

impl McpServerTool for BrowserScreenshot {
    type Input = BrowserScreenshotInput;
    type Output = BrowserScreenshotOutput;

    const NAME: &'static str = "browser_screenshot";

    fn annotations(&self) -> ToolAnnotations {
        read_only()
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let browser = cx.update(|cx| find_browser_tab(input.tab, &context, cx))?;
        let screenshot = html_preview::screenshot(&browser, cx).await?;
        let output = BrowserScreenshotOutput {
            tab: browser.entity_id().as_u64(),
            url: cx.update(|cx| browser.read(cx).url().to_string()),
            width: screenshot.width,
            height: screenshot.height,
        };
        Ok(ToolResponse {
            content: vec![
                ToolResponseContent::Image {
                    data: base64::engine::general_purpose::STANDARD.encode(&screenshot.png),
                    mime_type: "image/png".to_string(),
                },
                ToolResponseContent::Text {
                    text: serde_json::to_string_pretty(&output)?,
                },
            ],
            structured_content: output,
        })
    }
}

#[derive(Clone)]
struct BrowserSnapshot;

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all(deserialize = "camelCase"))]
struct BrowserSnapshotOutput {
    #[serde(default)]
    tab: u64,
    url: String,
    title: String,
    /// Whether the page is a Flutter web app.
    flutter: bool,
    /// Whether this call turned on Flutter's semantics tree, which Flutter
    /// only builds elements for once accessibility is on.
    flutter_semantics_enabled: bool,
    /// The page's elements, one per line, indented by nesting. Elements to
    /// click or type into have a ref, such as `[ref=e3]`.
    snapshot: String,
}

/// Lists the elements on a browser tab's page as an outline: links, buttons,
/// fields, headings and text, with their state (`[checked]`, `[disabled]`,
/// `value="…"`). Elements you can act on have a `ref` to pass to
/// `browser_click` and `browser_type`. Refs stay the same for an element
/// while it's on the page. For a Flutter app, this turns on Flutter's
/// semantics tree so its widgets are listed; if they still aren't, use
/// `browser_screenshot` and click by `x`/`y`.
#[derive(Deserialize, JsonSchema)]
struct BrowserSnapshotInput {
    /// The tab. Defaults to the one you last opened.
    #[serde(default)]
    tab: Option<u64>,
}

impl McpServerTool for BrowserSnapshot {
    type Input = BrowserSnapshotInput;
    type Output = BrowserSnapshotOutput;

    const NAME: &'static str = "browser_snapshot";

    fn annotations(&self) -> ToolAnnotations {
        read_only()
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let browser = cx.update(|cx| find_browser_tab(input.tab, &context, cx))?;
        let value =
            html_preview::run_script(&browser, "return await window.__bench.snapshot();", cx)
                .await?;
        let mut output: BrowserSnapshotOutput =
            serde_json::from_value(value).context("reading the page's snapshot")?;
        output.tab = browser.entity_id().as_u64();
        let mut text = format!(
            "Tab {}: {}\nURL: {}\n",
            output.tab, output.title, output.url
        );
        if output.flutter {
            text.push_str(if output.flutter_semantics_enabled {
                "Flutter app: turned on its semantics tree for this snapshot.\n"
            } else {
                "Flutter app.\n"
            });
        }
        text.push('\n');
        text.push_str(&output.snapshot);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text }],
            structured_content: output,
        })
    }
}

#[derive(Clone)]
struct BrowserClick;

/// Clicks in a browser tab's page with real mouse events, as a person would:
/// an element by its `ref` from `browser_snapshot` or a CSS `selector`
/// (scrolled into view, then clicked at its center), or a point `x`, `y` in
/// CSS pixels from the page's top left, as measured on `browser_screenshot`.
/// Brings the tab to the front of its pane if it isn't. Clicking a text field
/// gives the page the keyboard, as a person's click would; other clicks
/// leave the keyboard where it was.
#[derive(Deserialize, JsonSchema)]
struct BrowserClickInput {
    /// The tab. Defaults to the one you last opened.
    #[serde(default)]
    tab: Option<u64>,
    /// An element's ref from `browser_snapshot`, such as `e3`.
    #[serde(default, rename = "ref")]
    element_ref: Option<String>,
    /// A CSS selector for the element, such as `button[type=submit]`. It
    /// looks inside open shadow roots too.
    #[serde(default)]
    selector: Option<String>,
    /// A point to click, from the page's left edge, in CSS pixels.
    #[serde(default)]
    x: Option<f32>,
    /// A point to click, from the page's top edge, in CSS pixels.
    #[serde(default)]
    y: Option<f32>,
    /// Double-click instead.
    #[serde(default)]
    double_click: bool,
}

#[derive(Serialize, JsonSchema)]
struct BrowserClickOutput {
    tab: u64,
    #[serde(flatten)]
    outcome: ClickOutcome,
}

/// The element a tool acts on, as the page script takes it.
fn element_target(
    element_ref: Option<String>,
    selector: Option<String>,
) -> Option<serde_json::Value> {
    match (element_ref, selector) {
        (Some(element_ref), _) => Some(serde_json::json!({ "ref": element_ref })),
        (None, Some(selector)) => Some(serde_json::json!({ "selector": selector })),
        (None, None) => None,
    }
}

impl McpServerTool for BrowserClick {
    type Input = BrowserClickInput;
    type Output = BrowserClickOutput;

    const NAME: &'static str = "browser_click";

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            title: None,
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(false),
            open_world_hint: Some(true),
        }
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let target = match (
            element_target(input.element_ref, input.selector),
            input.x,
            input.y,
        ) {
            (Some(element), _, _) => ClickTarget::Element(element),
            (None, Some(x), Some(y)) => ClickTarget::Point(gpui::point(gpui::px(x), gpui::px(y))),
            _ => anyhow::bail!("Pass `ref`, `selector`, or both `x` and `y`"),
        };
        let browser = cx.update(|cx| find_browser_tab(input.tab, &context, cx))?;
        let click_count = if input.double_click { 2 } else { 1 };
        let outcome = html_preview::click(&browser, target, click_count, cx).await?;
        respond(BrowserClickOutput {
            tab: browser.entity_id().as_u64(),
            outcome,
        })
    }
}

#[derive(Clone)]
struct BrowserType;

/// Types text into a text field in a browser tab's page, the way pasting it
/// would: the page sees the input events a person's typing makes. Pass the
/// field's `ref` or `selector` to click it first, or leave both out to type
/// into the field that has focus.
#[derive(Deserialize, JsonSchema)]
struct BrowserTypeInput {
    text: String,
    /// The tab. Defaults to the one you last opened.
    #[serde(default)]
    tab: Option<u64>,
    /// The field's ref from `browser_snapshot`.
    #[serde(default, rename = "ref")]
    element_ref: Option<String>,
    /// A CSS selector for the field.
    #[serde(default)]
    selector: Option<String>,
    /// Replace the field's text instead of adding to it.
    #[serde(default)]
    clear: bool,
    /// Press Enter afterwards, as to submit a form.
    #[serde(default)]
    submit: bool,
}

#[derive(Serialize, JsonSchema)]
struct BrowserTypeOutput {
    tab: u64,
    #[serde(flatten)]
    outcome: TypeOutcome,
}

impl McpServerTool for BrowserType {
    type Input = BrowserTypeInput;
    type Output = BrowserTypeOutput;

    const NAME: &'static str = "browser_type";

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            title: None,
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(false),
            open_world_hint: Some(true),
        }
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let browser = cx.update(|cx| find_browser_tab(input.tab, &context, cx))?;
        let target = element_target(input.element_ref, input.selector);
        let outcome =
            html_preview::type_text(&browser, target, &input.text, input.clear, input.submit, cx)
                .await?;
        respond(BrowserTypeOutput {
            tab: browser.entity_id().as_u64(),
            outcome,
        })
    }
}

#[derive(Clone)]
struct BrowserPressKey;

/// Presses a key in a browser tab's page with a real key event, such as
/// `Enter`, `Tab`, `Escape`, `Backspace`, `ArrowDown` or `a`, optionally
/// with modifiers: `Shift+Tab`, `Meta+a`, `Control+Enter`, `Alt+ArrowLeft`.
#[derive(Deserialize, JsonSchema)]
struct BrowserPressKeyInput {
    key: String,
    /// The tab. Defaults to the one you last opened.
    #[serde(default)]
    tab: Option<u64>,
}

#[derive(Serialize, JsonSchema)]
struct BrowserTabOutput {
    tab: u64,
}

impl McpServerTool for BrowserPressKey {
    type Input = BrowserPressKeyInput;
    type Output = BrowserTabOutput;

    const NAME: &'static str = "browser_press_key";

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            title: None,
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(false),
            open_world_hint: Some(true),
        }
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let browser = cx.update(|cx| find_browser_tab(input.tab, &context, cx))?;
        html_preview::press_key(&browser, &input.key, cx).await?;
        respond(BrowserTabOutput {
            tab: browser.entity_id().as_u64(),
        })
    }
}

#[derive(Clone)]
struct BrowserEval;

/// Runs JavaScript in a browser tab's page and returns its result as JSON.
/// The script is the body of an async function: `return` a value to get it
/// back, and `await` freely, as in `return document.title` or
/// `await fetch('/api/me').then(r => r.json())`. It runs alongside the
/// page's own scripts, with its globals. Gives up after 30 seconds.
#[derive(Deserialize, JsonSchema)]
struct BrowserEvalInput {
    script: String,
    /// The tab. Defaults to the one you last opened.
    #[serde(default)]
    tab: Option<u64>,
}

#[derive(Serialize, JsonSchema)]
struct BrowserEvalOutput {
    tab: u64,
    /// What the script returned, or `null` when it returned nothing or
    /// something JSON can't express.
    result: serde_json::Value,
}

impl McpServerTool for BrowserEval {
    type Input = BrowserEvalInput;
    type Output = BrowserEvalOutput;

    const NAME: &'static str = "browser_eval";

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            title: None,
            read_only_hint: Some(false),
            destructive_hint: Some(true),
            idempotent_hint: Some(false),
            open_world_hint: Some(true),
        }
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let browser = cx.update(|cx| find_browser_tab(input.tab, &context, cx))?;
        let result = html_preview::run_script(&browser, &input.script, cx).await?;
        respond(BrowserEvalOutput {
            tab: browser.entity_id().as_u64(),
            result,
        })
    }
}

#[derive(Clone)]
struct BrowserConsoleLogs;

/// Reads what a browser tab's page logged: console messages, uncaught errors
/// and unhandled promise rejections, failed requests (`network`: HTTP errors
/// and requests that didn't complete) and page loads (`navigation`). The tab
/// keeps its last 1000 entries, across reloads. Pass the `last_id` from one
/// call as `after` in the next to get only what's new.
#[derive(Deserialize, JsonSchema)]
struct BrowserConsoleLogsInput {
    /// The tab. Defaults to the one you last opened.
    #[serde(default)]
    tab: Option<u64>,
    /// Only entries after this id.
    #[serde(default)]
    after: Option<u64>,
    /// Only entries at these levels: `log`, `info`, `warn`, `error`,
    /// `debug`, `network` or `navigation`.
    #[serde(default)]
    levels: Option<Vec<String>>,
    /// Clear the tab's entries after reading them.
    #[serde(default)]
    clear: bool,
}

#[derive(Serialize, JsonSchema)]
struct BrowserConsoleLogsOutput {
    tab: u64,
    entries: Vec<ConsoleEntry>,
    /// The id of the last entry the tab has, to pass as `after` next time.
    last_id: u64,
}

impl McpServerTool for BrowserConsoleLogs {
    type Input = BrowserConsoleLogsInput;
    type Output = BrowserConsoleLogsOutput;

    const NAME: &'static str = "browser_console_logs";

    fn annotations(&self) -> ToolAnnotations {
        read_only()
    }

    async fn run(
        &self,
        input: Self::Input,
        context: ToolContext,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let browser = cx.update(|cx| find_browser_tab(input.tab, &context, cx))?;
        let (all_entries, last_id) = browser.update(cx, |browser, _| {
            let entries = browser.console_entries(input.after.unwrap_or(0));
            if input.clear {
                browser.clear_console();
            }
            (entries, browser.last_console_id())
        });
        let entries = all_entries
            .into_iter()
            .filter(|entry| {
                input
                    .levels
                    .as_ref()
                    .is_none_or(|levels| levels.contains(&entry.level))
            })
            .collect();
        respond(BrowserConsoleLogsOutput {
            tab: browser.entity_id().as_u64(),
            entries,
            last_id,
        })
    }
}

fn toast(message: String, workspace: &mut Workspace, cx: &mut gpui::Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<InstallMcpOnHost>(), message),
        cx,
    );
}

/// Forwards Bench's MCP socket to the host and registers the host's
/// `remote_server` as an MCP server with the Claude Code there, after asking.
fn install_on_host(
    client: Entity<RemoteClient>,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let options = client.read(cx).connection_options();
    let name = options.display_name();
    let RemoteConnectionOptions::Ssh(ssh_options) = options else {
        let message = format!("Bench's MCP server can only be installed on SSH hosts, not {name}.");
        cx.defer_in(window, move |workspace, _, cx| {
            toast(message, workspace, cx)
        });
        return;
    };

    let answer = window.prompt(
        PromptLevel::Info,
        &format!("Let agents on {name} drive Bench?"),
        Some(
            "Bench's MCP server is added to Claude Code on the host, and Bench's MCP socket is \
             forwarded there whenever the host is connected. Any process of your user on the \
             host can then open pages in Bench's browser and run scripts in them.",
        ),
        &["Install", "Cancel"],
        cx,
    );
    cx.spawn_in(window, async move |workspace, cx| {
        if answer.await? != 0 {
            return anyhow::Ok(());
        }
        let outcome = register_on_host(&client, &ssh_options, &name, cx).await;
        let message = match outcome {
            Ok(()) => format!(
                "Claude Code on {name} can now drive Bench. Start a new Claude session there \
                 to pick up the `bench` MCP server."
            ),
            Err(error) => {
                log::error!("installing Bench's MCP server on {name}: {error:#}");
                format!("Could not install Bench's MCP server on {name}: {error:#}")
            }
        };
        workspace.update(cx, |workspace, cx| toast(message, workspace, cx))
    })
    .detach_and_log_err(cx);
}

async fn register_on_host(
    client: &Entity<RemoteClient>,
    ssh_options: &remote::SshConnectionOptions,
    name: &str,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<()> {
    let server_path = client
        .read_with(cx, |client, _| client.remote_server_path())
        .context("could not tell where Bench's server is on the host")?;
    let remote_socket = mcp_forward::remote_socket_path();

    mcp_forward::enable(ssh_options)?;
    client
        .read_with(cx, |client, cx| {
            client.forward_socket_to_host(
                remote_socket.clone(),
                paths::mcp_socket_path().clone(),
                cx,
            )
        })
        .await?;

    let arguments = vec![
        "-lc".to_owned(),
        // `claude` is rarely on the PATH of a non-interactive shell: its own
        // installer puts it in `~/.local/bin`.
        r#"PATH="$HOME/.local/bin:$HOME/.claude/local:$PATH"
claude mcp remove --scope user bench >/dev/null 2>&1
exec claude mcp add --scope user bench -- "$@""#
            .to_owned(),
        "sh".to_owned(),
        server_path.clone(),
        "--mcp-bridge".to_owned(),
        remote_socket.clone(),
        "--mcp-host".to_owned(),
        name.to_owned(),
    ];
    let template = client.read_with(cx, |client, _| {
        client.build_command(
            Some("sh".to_owned()),
            &arguments,
            &std::collections::HashMap::default(),
            None,
            None,
            Interactive::No,
        )
    })?;
    let mut command = util::command::new_command(template.program);
    command
        .args(template.args)
        .envs(template.env)
        .stdin(util::command::Stdio::null())
        .kill_on_drop(true);
    let output = cx
        .background_spawn(async move { command.output().await })
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "{}. To add it yourself, run on the host: claude mcp add --scope user bench -- \
         {server_path} --mcp-bridge {remote_socket} --mcp-host '{name}'",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}
