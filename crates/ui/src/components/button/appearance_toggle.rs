use gpui::{App, Window};
use theme::Appearance;

use crate::{IconButton, IconName, IconSize, Tooltip, prelude::*};

/// A button that switches a view's content between light and dark. It shows
/// the appearance it switches to, and passes it to `on_toggle`.
pub fn appearance_toggle(
    id: impl Into<ElementId>,
    current: Appearance,
    on_toggle: impl Fn(&Appearance, &mut Window, &mut App) + 'static,
) -> IconButton {
    let (next, icon, tooltip) = match current {
        Appearance::Dark => (Appearance::Light, IconName::Sun, "Switch to Light"),
        Appearance::Light => (Appearance::Dark, IconName::Moon, "Switch to Dark"),
    };
    IconButton::new(id, icon)
        .icon_size(IconSize::Small)
        .tooltip(Tooltip::text(tooltip))
        .on_click(move |_, window, cx| on_toggle(&next, window, cx))
}
