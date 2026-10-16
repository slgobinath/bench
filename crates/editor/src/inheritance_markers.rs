use std::ops::Range;

use collections::HashMap;
use gpui::{ClickEvent, Context, Task, Window};
use language::{BufferSnapshot, SymbolKind};
use multi_buffer::MultiBufferRow;
use project::{DocumentSymbol, InheritanceDirection, InheritanceFlags, InheritanceQuery};
use settings::Settings as _;
use text::{Bias, BufferId, PointUtf16, Unclipped};
use ui::{ContextMenu, IconButton, IconButtonShape, IconName, IconSize, Tooltip, prelude::*};
use util::ResultExt as _;

use crate::display_map::ToDisplayPoint as _;
use crate::hover_links::HoverLink;
use crate::mouse_context_menu::MouseContextMenu;
use crate::{
    DisplayRow, Editor, EditorSettings, EditorSnapshot, GotoDefinitionKind,
    LSP_REQUEST_DEBOUNCE_TIMEOUT,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct InheritanceMarker {
    pub(crate) query: InheritanceQuery,
    pub(crate) flags: InheritanceFlags,
    /// Shown as "I" when the symbol implements, or is implemented by, an
    /// interface or trait, and as "O" when it overrides or extends.
    implements: bool,
}

/// Identifies a symbol across edits: the names and kinds of it and its
/// ancestors, plus its index among symbols sharing that path.
type SymbolKey = (Vec<(String, i32)>, usize);

#[derive(Clone)]
struct Candidate {
    key: SymbolKey,
    query: InheritanceQuery,
    /// The symbol is an interface, or is declared in one.
    in_interface: bool,
    /// The type a method or function is declared in.
    container: Option<SymbolKey>,
}

pub(crate) struct InheritanceMarkers {
    /// The setting's value at the last refresh.
    enabled: bool,
    buffer_id: Option<BufferId>,
    /// Sorted by position.
    markers: Vec<InheritanceMarker>,
    /// Flags of symbols seen since the last full refresh, so that an edit only
    /// queries the symbols it added or renamed.
    known_flags: HashMap<SymbolKey, InheritanceFlags>,
    refresh_task: Task<()>,
}

impl Default for InheritanceMarkers {
    fn default() -> Self {
        Self {
            enabled: false,
            buffer_id: None,
            markers: Vec::new(),
            known_flags: HashMap::default(),
            refresh_task: Task::ready(()),
        }
    }
}

impl InheritanceMarkers {
    pub(crate) fn is_empty(&self) -> bool {
        self.markers.is_empty()
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }
}

impl Editor {
    /// Re-fetches the inheritance markers of the editor's buffer. With
    /// `invalidate`, every symbol is queried again, since implementations in
    /// other files may have changed.
    pub(crate) fn refresh_inheritance_markers(&mut self, invalidate: bool, cx: &mut Context<Self>) {
        if invalidate {
            self.inheritance_markers.known_flags.clear();
        }
        self.inheritance_markers.enabled = EditorSettings::get_global(cx).gutter.inheritance;
        let buffer = self.buffer.read(cx).as_singleton();
        let buffer_id = buffer.as_ref().map(|buffer| buffer.read(cx).remote_id());
        if self.inheritance_markers.buffer_id != buffer_id {
            self.clear_inheritance_markers(cx);
            self.inheritance_markers.buffer_id = buffer_id;
        }

        let (Some(buffer), Some(buffer_id), Some(project)) = (buffer, buffer_id, self.project.clone())
        else {
            return;
        };
        if !self.lsp_data_enabled()
            || !self.inheritance_markers.enabled
            || !self.registered_buffers.contains_key(&buffer_id)
        {
            self.clear_inheritance_markers(cx);
            return;
        }

        self.inheritance_markers.refresh_task = cx.spawn(async move |editor, cx| {
            cx.background_executor()
                .timer(LSP_REQUEST_DEBOUNCE_TIMEOUT)
                .await;

            let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
            let symbols = project
                .update(cx, |project, cx| project.document_symbols(&buffer, cx))
                .await;
            let Some(symbols) = symbols
                .map_err(|error| error.context("fetching symbols for inheritance markers"))
                .log_err()
            else {
                return;
            };

            let mut candidates = Vec::new();
            collect_candidates(&symbols, None, None, &mut Vec::new(), &snapshot, &mut candidates);
            let Ok(unknown) = editor.read_with(cx, |editor, _| {
                candidates
                    .iter()
                    .filter(|candidate| {
                        !editor
                            .inheritance_markers
                            .known_flags
                            .contains_key(&candidate.key)
                    })
                    .map(|candidate| (candidate.key.clone(), candidate.query))
                    .collect::<Vec<_>>()
            }) else {
                return;
            };

            let flags = project
                .update(cx, |project, cx| {
                    project.inheritance_flags(
                        &buffer,
                        unknown.iter().map(|(_, query)| *query).collect(),
                        cx,
                    )
                })
                .await;
            let Some(flags) = flags
                .map_err(|error| error.context("fetching inheritance markers"))
                .log_err()
            else {
                return;
            };

            editor
                .update(cx, |editor, cx| {
                    let state = &mut editor.inheritance_markers;
                    state
                        .known_flags
                        .extend(unknown.into_iter().map(|(key, _)| key).zip(flags));
                    state.markers = candidates
                        .into_iter()
                        .filter_map(|candidate| {
                            let flags = *state.known_flags.get(&candidate.key)?;
                            if !flags.has_children && !flags.has_parents {
                                return None;
                            }
                            let implements = if flags.has_parents {
                                // Some servers cannot tell whether a super method is
                                // declared in an interface. If every parent of its
                                // type is one, so is the method's.
                                flags.parents_are_interfaces
                                    || candidate
                                        .container
                                        .and_then(|container| state.known_flags.get(&container))
                                        .is_some_and(|container| container.parents_are_interfaces)
                            } else {
                                candidate.in_interface
                            };
                            Some(InheritanceMarker {
                                query: candidate.query,
                                flags,
                                implements,
                            })
                        })
                        .collect();
                    cx.notify();
                })
                .ok();
        });
    }

    fn clear_inheritance_markers(&mut self, cx: &mut Context<Self>) {
        let state = &mut self.inheritance_markers;
        state.refresh_task = Task::ready(());
        state.known_flags.clear();
        if !state.markers.is_empty() {
            state.markers.clear();
            cx.notify();
        }
    }

    /// Markers on the rows in `range`, at most one per row.
    pub(crate) fn active_inheritance_markers(
        &self,
        range: Range<DisplayRow>,
        snapshot: &EditorSnapshot,
    ) -> Vec<(DisplayRow, InheritanceMarker)> {
        let multi_buffer_snapshot = snapshot.buffer_snapshot();
        let mut markers = self
            .inheritance_markers
            .markers
            .iter()
            .filter_map(|marker| {
                let anchor = multi_buffer_snapshot.anchor_in_buffer(marker.query.position)?;
                let point = multi_buffer_snapshot.summary_for_anchor::<language::Point>(&anchor);
                let row = MultiBufferRow(point.row);
                // Rows hidden in a fold map onto the fold's first row, which
                // shows only its own marker.
                if snapshot.is_line_folded(row)
                    && row
                        .0
                        .checked_sub(1)
                        .is_some_and(|previous| snapshot.is_line_folded(MultiBufferRow(previous)))
                {
                    return None;
                }
                let display_row = point.to_display_point(snapshot).row();
                range.contains(&display_row).then_some((display_row, *marker))
            })
            .collect::<Vec<_>>();
        markers.dedup_by_key(|(row, _)| *row);
        markers
    }

    pub(crate) fn render_inheritance_marker(
        &self,
        row: DisplayRow,
        marker: InheritanceMarker,
        cx: &mut Context<Self>,
    ) -> IconButton {
        let flags = marker.flags;
        let is_type = !matches!(marker.query.kind, SymbolKind::Method | SymbolKind::Function);
        let (icon, tooltip) = match (flags.has_children, flags.has_parents, marker.implements) {
            (true, true, true) => (IconName::InheritanceIUpDown, "Show Parents or Implementations"),
            (true, true, false) => (IconName::InheritanceOUpDown, "Show Parents or Overrides"),
            (false, true, true) if is_type => {
                (IconName::InheritanceIUp, "Go to Implemented Interfaces")
            }
            (false, true, true) => (IconName::InheritanceIUp, "Go to Implemented Method"),
            (false, true, false) if is_type => (IconName::InheritanceOUp, "Go to Supertypes"),
            (false, true, false) => (IconName::InheritanceOUp, "Go to Overridden Method"),
            (_, _, true) => (IconName::InheritanceIDown, "Go to Implementations"),
            (_, _, false) if is_type => (IconName::InheritanceODown, "Go to Subclasses"),
            (_, _, false) => (IconName::InheritanceODown, "Go to Overriding Methods"),
        };
        // The colors IntelliJ uses: green for implementing, blue for overriding.
        let color = if marker.implements {
            Color::Success
        } else {
            Color::Accent
        };
        IconButton::new(("inheritance_marker", row.0 as usize), icon)
            .shape(IconButtonShape::Square)
            .icon_size(IconSize::XSmall)
            .icon_color(color)
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(move |editor, event: &ClickEvent, window, cx| {
                if flags.has_children && flags.has_parents {
                    editor.deploy_inheritance_menu(marker.query, event.position(), window, cx);
                } else if flags.has_parents {
                    editor.open_inheritance_targets(
                        marker.query,
                        InheritanceDirection::Parents,
                        window,
                        cx,
                    );
                } else {
                    editor.open_inheritance_targets(
                        marker.query,
                        InheritanceDirection::Children,
                        window,
                        cx,
                    );
                }
            }))
    }

    fn deploy_inheritance_menu(
        &mut self,
        query: InheritanceQuery,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self
            .buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_buffer(query.position)
        else {
            return;
        };
        let editor = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            menu.entry("Go to Parents", None, {
                let editor = editor.clone();
                move |window, cx| {
                    editor
                        .update(cx, |editor, cx| {
                            editor.open_inheritance_targets(
                                query,
                                InheritanceDirection::Parents,
                                window,
                                cx,
                            );
                        })
                        .ok();
                }
            })
            .entry("Go to Implementations", None, move |window, cx| {
                editor
                    .update(cx, |editor, cx| {
                        editor.open_inheritance_targets(
                            query,
                            InheritanceDirection::Children,
                            window,
                            cx,
                        );
                    })
                    .ok();
            })
        });
        self.mouse_context_menu =
            MouseContextMenu::pinned_to_editor(self, source, position, menu, window, cx);
        cx.notify();
    }

    /// Looks up the marker's parents or children, then jumps to the only one
    /// or lists them in the locations picker.
    fn open_inheritance_targets(
        &mut self,
        query: InheritanceQuery,
        direction: InheritanceDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project.clone() else {
            return;
        };
        let Some(buffer) = self.buffer.read(cx).buffer(query.position.buffer_id) else {
            return;
        };
        let Some(position) = self
            .buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_buffer(query.position)
        else {
            return;
        };
        let kind = match direction {
            InheritanceDirection::Children => GotoDefinitionKind::Implementation,
            InheritanceDirection::Parents => GotoDefinitionKind::Super,
        };
        let origin = self.navigation_entry(self.selections.newest_anchor().head(), cx);
        let targets = project.update(cx, |project, cx| {
            project.inheritance_targets(&buffer, query, direction, cx)
        });
        cx.spawn_in(window, async move |editor, cx| {
            let links = targets.await?;
            editor.update_in(cx, |editor, window, cx| {
                editor.reveal_clicked_links(
                    kind,
                    links.into_iter().map(HoverLink::Text).collect(),
                    position,
                    origin,
                    false,
                    window,
                    cx,
                );
            })
        })
        .detach_and_log_err(cx);
    }
}

fn is_container(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::Struct
            // rust-analyzer reports `impl` blocks as objects.
            | SymbolKind::Object
    )
}

/// Collects the types, and the methods and functions declared in types, which
/// can have implementations or parents.
fn collect_candidates(
    symbols: &[DocumentSymbol],
    parent_kind: Option<SymbolKind>,
    parent_key: Option<&SymbolKey>,
    path: &mut Vec<(String, i32)>,
    snapshot: &BufferSnapshot,
    candidates: &mut Vec<Candidate>,
) {
    let mut occurrences = HashMap::<(String, i32), usize>::default();
    for symbol in symbols {
        let identity = (symbol.name.clone(), symbol.kind.to_proto());
        let occurrence = occurrences.entry(identity.clone()).or_default();
        let index = *occurrence;
        *occurrence += 1;

        path.push(identity);
        let is_member = matches!(symbol.kind, SymbolKind::Method | SymbolKind::Function)
            && parent_kind.is_some_and(is_container);
        let is_type = matches!(
            symbol.kind,
            SymbolKind::Class | SymbolKind::Interface | SymbolKind::Enum | SymbolKind::Struct
        );
        let key = (path.clone(), index);
        if is_member || is_type {
            candidates.push(Candidate {
                key: key.clone(),
                query: InheritanceQuery {
                    position: anchor_for(symbol.selection_range.start, snapshot),
                    kind: symbol.kind,
                },
                in_interface: symbol.kind == SymbolKind::Interface
                    || (is_member && parent_kind == Some(SymbolKind::Interface)),
                container: is_member.then(|| parent_key.cloned()).flatten(),
            });
        }
        collect_candidates(
            &symbol.children,
            Some(symbol.kind),
            Some(&key),
            path,
            snapshot,
            candidates,
        );
        path.pop();
    }
}

fn anchor_for(point: Unclipped<PointUtf16>, snapshot: &BufferSnapshot) -> text::Anchor {
    snapshot.anchor_after(snapshot.clip_point_utf16(point, Bias::Left))
}
