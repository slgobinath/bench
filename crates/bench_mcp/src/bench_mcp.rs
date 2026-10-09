//! Bench's MCP server: tools that let an agent see what Bench is showing and
//! create worktrees in it.
//!
//! It is served on a Unix socket at [`paths::mcp_socket_path`], which only its
//! owner can open, and agents reach it through `cli --mcp`, a stdio bridge to
//! that socket. The bridge tells the server which directory the agent was
//! started in, which is how the tools default to the worktree the agent is
//! working in.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use context_server::listener::{McpServer, McpServerTool, ToolContext, ToolResponse};
use context_server::types::{
    self, Implementation, InitializeResponse, ProtocolVersion, Request, ServerCapabilities,
    ToolAnnotations, ToolResponseContent, ToolsCapabilities,
};
use editor::Editor;
use gpui::{App, AsyncApp, Entity, Global, Task, TaskExt as _, WindowHandle};
use language::{DiagnosticSeverity, OffsetRangeExt as _};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workspace::{MultiWorkspace, Workspace};
use linear::{Issue, Linear};
use worktree_panel::{ProjectListing, WorktreePanel};

/// How many characters of a selection are returned. A whole-file selection in
/// a large file would otherwise be the entire response.
const MAX_SELECTION_CHARS: usize = 20_000;

/// How many files a project-wide diagnostics call opens to read diagnostics
/// from. Each is a buffer Bench has to load.
const MAX_DIAGNOSTIC_FILES: usize = 50;

struct BenchMcpServer(#[allow(dead_code)] McpServer);

impl Global for BenchMcpServer {}

pub fn init(cx: &mut App) {
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
fn workspace_for_directory(directory: &Path, cx: &App) -> Result<Entity<Workspace>> {
    let mut best: Option<(usize, Entity<Workspace>)> = None;
    for window in windows(cx) {
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        for workspace in multi_workspace.workspaces() {
            for root in workspace_roots(workspace, cx) {
                let depth = root.components().count();
                if directory.starts_with(&root)
                    && best.as_ref().is_none_or(|(best_depth, _)| depth > *best_depth)
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
) -> Vec<(WindowHandle<MultiWorkspace>, Entity<WorktreePanel>, Vec<ProjectListing>)> {
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
) -> Result<(WindowHandle<MultiWorkspace>, Entity<WorktreePanel>, PathBuf, String)> {
    // A project open in several windows is one project.
    let mut candidates: Vec<(WindowHandle<MultiWorkspace>, Entity<WorktreePanel>, ProjectListing)> =
        Vec::new();
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
            .with_context(|| format!("No project named or rooted at “{project}” is open in Bench"))?
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
            let workspace = workspace_for_directory(&directory, cx)?;
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
            let workspace = workspace_for_directory(&directory, cx)?;
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
            workspace_for_directory(&directory, cx)
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
