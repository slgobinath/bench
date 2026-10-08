//! The status bar's list of listening ports.
//!
//! Which ports the dev servers and other programs in this worktree's terminals
//! have open, so that "what was it on again" is a glance at the corner rather
//! than a trip to a terminal. Nothing is drawn while there are none.

use std::path::PathBuf;

use gpui::{Entity, Subscription};
use project::Project;
use ui::{Tooltip, prelude::*};
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

use crate::{AgentTracker, AgentsChanged, ListeningPort};

/// As many as fit in a status bar before the rest are only a count.
const SHOWN: usize = 3;

pub struct PortsButton {
    project: Entity<Project>,
    tracker: Option<Entity<AgentTracker>>,
    _subscription: Option<Subscription>,
}

impl PortsButton {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let tracker = AgentTracker::try_global(cx);
        let subscription = tracker
            .as_ref()
            .map(|tracker| cx.subscribe(tracker, |_, _, _: &AgentsChanged, cx| cx.notify()));
        Self {
            project: workspace.project().clone(),
            tracker,
            _subscription: subscription,
        }
    }

    fn ports(&self, cx: &App) -> Vec<ListeningPort> {
        let Some(tracker) = &self.tracker else {
            return Vec::new();
        };
        let roots: Vec<PathBuf> = self
            .project
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect();
        tracker.read(cx).ports_for(&roots)
    }
}

impl Render for PortsButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ports = self.ports(cx);
        if ports.is_empty() {
            return div().into_any_element();
        }

        let tooltip = ports
            .iter()
            .map(|port| format!(":{} {}", port.port, port.command))
            .collect::<Vec<_>>()
            .join("\n");
        let hidden = ports.len().saturating_sub(SHOWN);

        h_flex()
            .id("listening-ports")
            .gap_1()
            .px_1()
            .children(ports.iter().take(SHOWN).map(|port| {
                Label::new(format!(":{}", port.port))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
            }))
            .when(hidden > 0, |this| {
                this.child(
                    Label::new(format!("+{hidden}"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .tooltip(Tooltip::text(format!("Listening on:\n{tooltip}")))
            .into_any_element()
    }
}

impl StatusItemView for PortsButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    /// Only drawn while something is listening, which is the whole point of
    /// it; there is nothing to hide the rest of the time.
    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}
