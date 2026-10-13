use std::sync::Arc;

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window,
};

use crate::{GlobalTheme, Theme};

/// Draws its child in `theme` rather than the app's theme, so that a view can
/// show its content light or dark whatever Bench's theme is. Popovers and
/// tooltips the child opens are deferred, and so drawn in the app's theme.
pub struct ThemeScope {
    theme: Option<Arc<Theme>>,
    child: AnyElement,
}

impl ThemeScope {
    /// `None` draws the child in the app's theme.
    pub fn new(theme: Option<Arc<Theme>>, child: impl IntoElement) -> Self {
        Self {
            theme,
            child: child.into_any_element(),
        }
    }

    fn scoped<R>(theme: &Option<Arc<Theme>>, cx: &mut App, f: impl FnOnce(&mut App) -> R) -> R {
        match theme {
            Some(theme) => GlobalTheme::with_scoped_theme(cx, theme.clone(), f),
            None => f(cx),
        }
    }
}

impl IntoElement for ThemeScope {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ThemeScope {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let child = &mut self.child;
        let layout_id = Self::scoped(&self.theme, cx, |cx| child.request_layout(window, cx));
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let child = &mut self.child;
        Self::scoped(&self.theme, cx, |cx| child.prepaint(window, cx));
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let child = &mut self.child;
        Self::scoped(&self.theme, cx, |cx| child.paint(window, cx));
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use crate::{ActiveTheme as _, LoadThemes};

    use super::*;

    #[gpui::test]
    fn a_scoped_theme_is_active_only_inside_its_scope(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::init(LoadThemes::JustBase, cx);
            let app_theme = cx.theme().clone();
            let mut scoped_theme = (*app_theme).clone();
            scoped_theme.name = "Scoped".into();
            let scoped_theme = Arc::new(scoped_theme);

            let name_inside = GlobalTheme::with_scoped_theme(cx, scoped_theme, |cx| {
                cx.theme().name.clone()
            });

            assert_eq!(name_inside, "Scoped");
            assert_eq!(cx.theme().name, app_theme.name);
        });
    }
}
