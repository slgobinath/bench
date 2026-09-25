use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use futures::FutureExt as _;
use gpui::{
    App, AsyncWindowContext, BorrowAppContext as _, Context, Entity, Global, WeakEntity, Window,
};
use task::RevealStrategy;
use terminal::Terminal;
use terminal_view::TerminalView;
use terminal_view::terminal_panel::TerminalPanel;
use util::ResultExt as _;
use workspace::Workspace;
use zed_actions::claude::{NewTerminal, SendCommit, SendDiagnostic, SendFile, SendSelection};

use crate::agent_panel::format_selection_for_terminal;
use crate::completion_provider::{AgentContextSelection, AgentContextSource};

/// The command Bench types to start Claude Code in a terminal it manages.
const CLAUDE_COMMAND: &str = "claude";
const CLAUDE_TERMINAL_TITLE: &str = "Claude";
/// How long to wait for the shell to finish starting before typing the launch
/// command into it.
const SHELL_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for `claude` to take over the terminal before giving up on
/// delivering a selection to it.
const CLAUDE_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CLAUDE_STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The terminals Bench started Claude in, keyed by worktree root. A terminal
/// that is still starting up has not reached `claude` yet, so looking only at
/// foreground processes would start a second Claude for the same worktree.
#[derive(Default)]
struct ManagedTerminals(HashMap<PathBuf, WeakEntity<Terminal>>);

impl Global for ManagedTerminals {}

pub fn init(cx: &mut App) {
    cx.set_global(ManagedTerminals::default());

    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace
            .register_action(|workspace, _: &NewTerminal, window, cx| {
                let Some(root) = worktree_root(workspace, cx) else {
                    return;
                };
                cx.spawn_in(window, async move |workspace, cx| {
                    let result = launch(&workspace, root, cx).await.map(|_| ());
                    report(&workspace, result, cx);
                })
                .detach();
            })
            .register_action(|workspace, _: &SendSelection, window, cx| {
                let source = AgentContextSource::from_focused(workspace, window, cx)
                    .or_else(|| AgentContextSource::from_active(workspace, cx));
                let Some(source) = source else {
                    return;
                };
                let Some(selection) = source.read_selection(workspace, true, cx) else {
                    return;
                };
                send(workspace, Payload::Selection(selection), window, cx);
            })
            .register_action(|workspace, action: &SendFile, window, cx| {
                let payload = Payload::File(PathBuf::from(action.path.clone()));
                send(workspace, payload, window, cx);
            })
            .register_action(|workspace, action: &SendCommit, window, cx| {
                send(workspace, Payload::Commit(action.sha.clone()), window, cx);
            })
            .register_action(|workspace, action: &SendDiagnostic, window, cx| {
                let payload = Payload::Diagnostic {
                    path: PathBuf::from(action.path.clone()),
                    line: action.line,
                    message: action.message.clone(),
                };
                send(workspace, payload, window, cx);
            });
    })
    .detach();
}

/// What a "send to agent" command puts in the agent's composer.
enum Payload {
    Selection(AgentContextSelection),
    File(PathBuf),
    Commit(String),
    Diagnostic {
        path: PathBuf,
        line: u32,
        message: String,
    },
}

fn send(
    workspace: &mut Workspace,
    payload: Payload,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(root) = worktree_root(workspace, cx) else {
        return;
    };
    let existing = claude_terminal(workspace, &root, cx);

    cx.spawn_in(window, async move |workspace, cx| {
        let result = deliver(&workspace, existing, payload, root, cx).await;
        report(&workspace, result, cx);
    })
    .detach();
}

async fn deliver(
    workspace: &WeakEntity<Workspace>,
    existing: Option<Entity<Terminal>>,
    payload: Payload,
    root: PathBuf,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    // A terminal Bench just launched has already been sent the command, so it
    // only has to be waited for; an existing tab may need it typed again.
    let (terminal, just_launched) = match existing {
        Some(terminal) => (terminal, false),
        None => (launch(workspace, root.clone(), cx).await?, true),
    };

    // Reveal before waiting on Claude: starting it takes seconds, and a send
    // that shows nothing until then looks like nothing happened.
    workspace.update_in(cx, |workspace, window, cx| {
        reveal(workspace, &terminal, window, cx);
    })?;

    if just_launched {
        wait_for_claude(&terminal, cx).await?;
    } else {
        ensure_claude_running(&terminal, cx).await?;
    }

    workspace.update_in(cx, |workspace, _window, cx| {
        paste(&terminal, &payload, &root, workspace, cx);
    })?;

    Ok(())
}

/// Surfaces a failure where the user is looking, rather than only in the log:
/// the common one is `claude` not being installed, which otherwise looks like
/// the editor ignoring the selection.
fn report(workspace: &WeakEntity<Workspace>, result: Result<()>, cx: &mut AsyncWindowContext) {
    let Err(error) = result else {
        return;
    };
    log::error!("Claude terminal: {error:#}");
    workspace
        .update(cx, |workspace, cx| workspace.show_error(error, cx))
        .log_err();
}

fn paste(
    terminal: &Entity<Terminal>,
    payload: &Payload,
    root: &Path,
    workspace: &Workspace,
    cx: &mut App,
) {
    // Mentions are resolved against the terminal's own cwd, which is where
    // Claude reads relative paths from.
    let working_directory = terminal
        .read(cx)
        .working_directory()
        .unwrap_or_else(|| root.to_path_buf());
    let text = match payload {
        Payload::Selection(selection) => format_selection_for_terminal(
            selection,
            workspace.project(),
            Some(working_directory.as_path()),
            cx,
        ),
        // Trailing space so the mention doesn't fuse with the next input.
        Payload::File(path) => {
            format!("{} ", mention_path(path, &working_directory, workspace, cx))
        }
        Payload::Commit(sha) => format!("{sha} "),
        Payload::Diagnostic {
            path,
            line,
            message,
        } => {
            let path = mention_path(path, &working_directory, workspace, cx);
            format!("{path}:{line} {message} ")
        }
    };
    if text.is_empty() {
        return;
    }

    // No trailing carriage return: the text lands in Claude's composer so the
    // prompt can be typed around it, rather than being submitted on its own.
    terminal.update(cx, |terminal, _| terminal.paste(&text));
}

/// A path relative to the terminal's cwd when it sits under it, so the agent
/// reads it the way it reads its own output, and absolute otherwise.
fn mention_path(path: &Path, working_directory: &Path, workspace: &Workspace, cx: &App) -> String {
    let path_style = workspace.project().read(cx).path_style(cx);
    path_style
        .strip_prefix(path, working_directory)
        .map(|relative| relative.display(path_style).into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Opens a terminal in the worktree root, names it, and types the launch
/// command once the shell is ready.
async fn launch(
    workspace: &WeakEntity<Workspace>,
    root: PathBuf,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Terminal>> {
    let panel = workspace
        .read_with(cx, |workspace, cx| workspace.panel::<TerminalPanel>(cx))?
        .context("terminal panel is unavailable")?;

    let terminal = panel
        .update_in(cx, |panel, window, cx| {
            panel.add_terminal_shell(
                false,
                Some(root.clone()),
                RevealStrategy::Always,
                window,
                cx,
            )
        })?
        .await?
        .upgrade()
        .context("the Claude terminal closed while it was starting")?;

    cx.update(|_window, cx| {
        cx.update_global::<ManagedTerminals, _>(|terminals, _| {
            terminals.0.insert(root, terminal.downgrade());
        });
    })?;

    workspace.update_in(cx, |workspace, _window, cx| {
        if let Some((_, _, view)) = terminal_view(workspace, &terminal, cx) {
            view.update(cx, |view, cx| {
                view.set_custom_title(Some(CLAUDE_TERMINAL_TITLE.to_string()), cx)
            });
        }
    })?;

    // Typing before the shell has drawn its prompt loses the command, so wait
    // for the shell's own startup marker (or the timeout) the way the agent
    // panel's terminal threads do.
    let startup =
        terminal.update(cx, |terminal, _| {
            terminal.start_init_command_startup_handshake()
        });
    let timeout = cx.background_executor().timer(SHELL_STARTUP_TIMEOUT);
    futures::select_biased! {
        _ = startup.fuse() => {}
        _ = timeout.fuse() => {}
    }

    let wrote = terminal.update(cx, |terminal, cx| {
        terminal.write_init_command_after_startup(launch_command_input(), cx)
    });
    anyhow::ensure!(
        wrote,
        "the Claude terminal stopped accepting input before Claude could be started"
    );

    Ok(terminal)
}

/// Waits for Claude to hold the terminal, typing the launch command first if
/// the tab is sitting at a shell prompt because Claude was exited.
async fn ensure_claude_running(
    terminal: &Entity<Terminal>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let running = terminal.read_with(cx, |terminal, _| is_claude(terminal));
    if !running {
        terminal.update(cx, |terminal, _| {
            terminal.write_init_command(launch_command_input())
        });
    }
    wait_for_claude(terminal, cx).await
}

async fn wait_for_claude(
    terminal: &Entity<Terminal>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    // The terminal refreshes its foreground process on every wakeup, so this
    // observes `claude` taking over as soon as it draws anything.
    let attempts = CLAUDE_STARTUP_TIMEOUT.as_millis() / CLAUDE_STARTUP_POLL_INTERVAL.as_millis();
    for _ in 0..attempts {
        if terminal.read_with(cx, |terminal, _| is_claude(terminal)) {
            return Ok(());
        }
        cx.background_executor()
            .timer(CLAUDE_STARTUP_POLL_INTERVAL)
            .await;
    }
    Err(anyhow!("timed out waiting for Claude Code to start"))
}

/// The Claude terminal for `root`: the one Bench started there if it is still
/// open, otherwise any terminal in the worktree that Claude is running in.
fn claude_terminal(workspace: &Workspace, root: &Path, cx: &App) -> Option<Entity<Terminal>> {
    let views = terminal_views(workspace, cx);

    let managed = cx
        .try_global::<ManagedTerminals>()
        .and_then(|terminals| terminals.0.get(root))
        .and_then(|terminal| terminal.upgrade());
    if let Some(managed) = managed
        && views
            .iter()
            .any(|(_, _, view)| view.read(cx).terminal().entity_id() == managed.entity_id())
    {
        return Some(managed);
    }

    views.into_iter().find_map(|(_, _, view)| {
        let terminal = view.read(cx).terminal();
        let in_worktree = terminal
            .read(cx)
            .working_directory()
            .is_some_and(|directory| directory.starts_with(root));
        (in_worktree && is_claude(terminal.read(cx))).then(|| terminal.clone())
    })
}

/// Terminals in the terminal panel and in the center, in that order, so a
/// panel tab wins when the same Claude is somehow reachable twice.
///
/// The panes are read here rather than through the panel, which cannot look at
/// the center without reading the workspace that is often mid-update.
fn terminal_views(
    workspace: &Workspace,
    cx: &App,
) -> Vec<(usize, Entity<workspace::Pane>, Entity<TerminalView>)> {
    let mut views = workspace
        .panel::<TerminalPanel>(cx)
        .map(|panel| panel.read(cx).terminal_views(cx))
        .unwrap_or_default();

    views.extend(workspace.panes().iter().flat_map(|pane| {
        pane.read(cx)
            .items()
            .enumerate()
            .filter_map(|(index, item)| {
                Some((index, (*pane).clone(), item.act_as::<TerminalView>(cx)?))
            })
            .collect::<Vec<_>>()
    }));

    views
}

fn terminal_view(
    workspace: &Workspace,
    terminal: &Entity<Terminal>,
    cx: &App,
) -> Option<(usize, Entity<workspace::Pane>, Entity<TerminalView>)> {
    terminal_views(workspace, cx)
        .into_iter()
        .find(|(_, _, view)| view.read(cx).terminal().entity_id() == terminal.entity_id())
}

fn reveal(
    workspace: &mut Workspace,
    terminal: &Entity<Terminal>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some((index, pane, _)) = terminal_view(workspace, terminal, cx) else {
        return;
    };
    let Some(panel) = workspace.panel::<TerminalPanel>(cx) else {
        return;
    };

    // A terminal opened in the center is activated in place; only a tab in the
    // panel's own panes needs the dock brought forward.
    let in_dock = panel
        .read(cx)
        .panes()
        .iter()
        .any(|panel_pane| panel_pane.entity_id() == pane.entity_id());
    if in_dock {
        workspace.focus_panel::<TerminalPanel>(window, cx);
    }
    panel.update(cx, |panel, cx| {
        panel.activate_terminal_view(&pane, index, true, window, cx)
    });
}

fn worktree_root(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    workspace
        .project()
        .read(cx)
        .active_project_directory(cx)
        .map(|root| root.to_path_buf())
}

fn is_claude(terminal: &Terminal) -> bool {
    terminal.foreground_process_command_name().as_deref() == Some(CLAUDE_COMMAND)
}

fn launch_command_input() -> Vec<u8> {
    let mut input = CLAUDE_COMMAND.as_bytes().to_vec();
    // CR, not "\r\n", which puts PowerShell into continuation mode.
    input.push(b'\x0d');
    input
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::RealFs;
    use gpui::{AppContext as _, TestAppContext, UpdateGlobal as _};
    use project::Project;
    use settings::SettingsStore;
    use workspace::MultiWorkspace;

    #[gpui::test]
    async fn test_launch_opens_a_claude_terminal_in_the_worktree_root(cx: &mut TestAppContext) {
        let (_worktree, root, window, workspace) = init_workspace(cx).await;
        let terminal = launch_in(&window, &workspace, &root, cx).await;

        let (working_directory, writes) = terminal.update(cx, |terminal, _| {
            (terminal.working_directory(), terminal.take_pty_write_log())
        });
        assert_eq!(
            working_directory.as_deref(),
            Some(root.as_path()),
            "the Claude terminal should start in the worktree root"
        );
        assert!(
            writes.contains(&launch_command_input()),
            "Bench should type the launch command into the terminal, wrote: {writes:?}"
        );

        let title = window
            .update(cx, |_, _, cx| {
                workspace.read_with(cx, |workspace, cx| {
                    terminal_view(workspace, &terminal, cx)
                        .map(|(_, _, view)| view.read(cx).custom_title().map(str::to_owned))
                })
            })
            .expect("failed to read the terminal's tab");
        assert_eq!(
            title,
            Some(Some(CLAUDE_TERMINAL_TITLE.to_string())),
            "the tab should be named after the agent running in it"
        );

        let managed = cx.update(|cx| {
            cx.global::<ManagedTerminals>()
                .0
                .get(&root)
                .and_then(|terminal| terminal.upgrade())
                .map(|terminal| terminal.entity_id())
        });
        assert_eq!(
            managed,
            Some(terminal.entity_id()),
            "the launched terminal should be tracked as the worktree's Claude terminal"
        );
    }

    #[gpui::test]
    async fn test_a_starting_terminal_is_reused_for_the_worktree(cx: &mut TestAppContext) {
        let (_worktree, root, window, workspace) = init_workspace(cx).await;
        let terminal = launch_in(&window, &workspace, &root, cx).await;

        // Claude has not taken over the shell yet, which is exactly when
        // looking only at foreground processes would start a second one.
        assert!(
            !terminal.read_with(cx, |terminal, _| is_claude(terminal)),
            "this test covers the window before Claude has started"
        );

        let found = window
            .update(cx, |_, _, cx| {
                workspace.read_with(cx, |workspace, cx| {
                    claude_terminal(workspace, &root, cx).map(|terminal| terminal.entity_id())
                })
            })
            .expect("failed to look up the worktree's Claude terminal");
        assert_eq!(
            found,
            Some(terminal.entity_id()),
            "the terminal Bench started should be reused while Claude is still starting"
        );
    }

    /// A workspace over a real directory, since the terminal spawns a real
    /// shell that has to have somewhere to run.
    async fn init_workspace(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        gpui::WindowHandle<MultiWorkspace>,
        Entity<Workspace>,
    ) {
        // The shell and its startup handshake are real processes, so the test
        // has to be allowed to block on them.
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            terminal_view::init(cx);
            super::init(cx);
        });

        let worktree = tempfile::tempdir().expect("failed to create a worktree directory");
        let root = worktree
            .path()
            .canonicalize()
            .expect("failed to resolve the worktree path");

        // The test types the real launch command into a real shell, so run a
        // bare `sh` with an empty `PATH`: Claude must not actually start on a
        // machine that has it installed, and a login shell would put it back
        // on `PATH` from the developer's own shell configuration.
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    let terminal = &mut settings.terminal.get_or_insert_default().project;
                    terminal.shell = Some(settings::Shell::Program("/bin/sh".to_owned()));
                    terminal.env = Some(collections::HashMap::from_iter([(
                        "PATH".to_owned(),
                        String::new(),
                    )]));
                });
            });
        });
        let fs = RealFs::new(None, cx.executor());
        let project = Project::test(fs, [root.as_path()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                workspace.update(cx, |workspace, cx| {
                    let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
                    workspace.add_panel(panel, window, cx);
                });
                workspace
            })
            .expect("failed to set up the workspace");

        (worktree, root, window, workspace)
    }

    async fn launch_in(
        window: &gpui::WindowHandle<MultiWorkspace>,
        workspace: &Entity<Workspace>,
        root: &Path,
        cx: &mut TestAppContext,
    ) -> Entity<Terminal> {
        let terminal = window
            .update(cx, |_, window, cx| {
                let workspace = workspace.downgrade();
                let root = root.to_path_buf();
                cx.spawn_in(window, async move |_, cx| launch(&workspace, root, cx).await)
            })
            .expect("failed to spawn the launch task")
            .await
            .expect("failed to launch the Claude terminal");
        cx.run_until_parked();
        terminal
    }
}
