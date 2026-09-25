//! The status bar's stay-awake toggle.
//!
//! It shows what is true rather than what was clicked: lit while the machine
//! is actually being held awake, dim while the behaviour is on but no agent is
//! working, struck through when it is off. That way the answer to "is my Mac
//! going to sleep during this run" is one glance rather than a settings file.

use gpui::{Entity, Subscription, actions};
use ui::{Tooltip, prelude::*};
use workspace::{
    HideStatusItem, StatusItemView, Toast, Workspace,
    item::ItemHandle,
    notifications::NotificationId,
};

use crate::{AgentState, AgentSummary, AgentTracker, AgentsChanged};

actions!(
    agents,
    [
        /// Turns the stay-awake behaviour on or off.
        ToggleKeepAwake,
        /// Adds Bench's hooks to Claude Code's settings, so that agent status
        /// is reported exactly rather than guessed from terminal output.
        InstallClaudeHooks,
        /// Takes Bench's hooks back out of Claude Code's settings.
        UninstallClaudeHooks,
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|_, _: &ToggleKeepAwake, _, cx| {
            let Some(tracker) = AgentTracker::try_global(cx) else {
                return;
            };
            tracker.update(cx, |tracker, cx| {
                let keep_awake = tracker.keep_awake();
                tracker.set_keep_awake(!keep_awake, cx);
            });
        });
        workspace.register_action(|workspace, _: &InstallClaudeHooks, _, cx| {
            report(
                crate::hooks::install().map(|path| {
                    format!("Claude Code will now report agent status to Bench ({}). Sessions already running pick it up on their next turn.", path.display())
                }),
                workspace,
                cx,
            );
        });
        workspace.register_action(|workspace, _: &UninstallClaudeHooks, _, cx| {
            report(
                crate::hooks::uninstall()
                    .map(|()| "Bench's hooks are out of Claude Code's settings.".to_owned()),
                workspace,
                cx,
            );
        });
    })
    .detach();
}

/// Says what happened, either way. Writing to someone's Claude Code settings
/// is not something to do silently, and failing to is not something to swallow.
fn report(
    outcome: anyhow::Result<String>,
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
) {
    let message = match outcome {
        Ok(message) => message,
        Err(error) => {
            log::error!("changing Claude Code's hooks: {error:#}");
            format!("Could not change Claude Code's settings: {error}")
        }
    };
    workspace.show_toast(Toast::new(NotificationId::unique::<ToggleKeepAwake>(), message), cx);
}

pub struct KeepAwakeButton {
    tracker: Option<Entity<AgentTracker>>,
    _subscription: Option<Subscription>,
}

impl KeepAwakeButton {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let tracker = AgentTracker::try_global(cx);
        let subscription = tracker
            .as_ref()
            .map(|tracker| cx.subscribe(tracker, |_, _, _: &AgentsChanged, cx| cx.notify()));
        Self {
            tracker,
            _subscription: subscription,
        }
    }

    fn state(&self, cx: &App) -> Option<(bool, bool, AgentSummary)> {
        let tracker = self.tracker.as_ref()?.read(cx);
        Some((
            tracker.keep_awake(),
            tracker.is_holding_awake(),
            tracker.summary(),
        ))
    }
}

impl Render for KeepAwakeButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((keep_awake, holding, summary)) = self.state(cx) else {
            return div().into_any_element();
        };

        let (icon, color) = match (keep_awake, holding) {
            (false, _) => (IconName::Power, Color::Disabled),
            (true, false) => (IconName::Power, Color::Muted),
            (true, true) => (IconName::Power, Color::Accent),
        };

        let tooltip = match (keep_awake, holding, summary.working) {
            (false, _, _) => "Sleep prevention off — the Mac may sleep mid-run".to_owned(),
            (true, true, working) => format!(
                "Keeping the Mac awake: {working} agent{} working",
                if working == 1 { "" } else { "s" }
            ),
            (true, false, _) => "Sleep prevention on — nothing is working right now".to_owned(),
        };

        IconButton::new("keep-awake", icon)
            .icon_size(IconSize::Small)
            .icon_color(color)
            .toggle_state(holding)
            .tooltip(move |_window, cx| {
                Tooltip::for_action(tooltip.clone(), &ToggleKeepAwake, cx)
            })
            .on_click(cx.listener(|this, _, _, cx| {
                let Some(tracker) = this.tracker.clone() else {
                    return;
                };
                tracker.update(cx, |tracker, cx| {
                    let keep_awake = tracker.keep_awake();
                    tracker.set_keep_awake(!keep_awake, cx);
                });
            }))
            .into_any_element()
    }
}

impl StatusItemView for KeepAwakeButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    /// Not hideable from settings: it is only ever drawn when Bench is
    /// tracking agents at all, and the thing it reports — whether your Mac is
    /// about to sleep under a running agent — is not something to hide behind
    /// a settings key.
    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

/// The colour an agent's state is drawn in, wherever it is drawn.
///
/// Not green: green is a worktree with an open pull request, and two meanings
/// for one colour in the same panel would make both unreadable.
pub fn agent_state_color(state: AgentState) -> Color {
    match state {
        AgentState::NeedsInput => Color::Warning,
        AgentState::Working => Color::Accent,
        AgentState::Idle => Color::Muted,
    }
}
