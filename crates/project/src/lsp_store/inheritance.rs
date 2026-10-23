use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures::{StreamExt as _, stream};
use gpui::{AppContext as _, AsyncApp, Context, Entity, Task};
use itertools::Itertools as _;
use language::proto::{
    deserialize_anchor, deserialize_version, serialize_anchor, serialize_version,
};
use language::{Buffer, File as _, SymbolKind, point_to_lsp};
use lsp::{LanguageServer, LanguageServerName};
use rpc::{TypedEnvelope, proto};
use serde::{Deserialize, Serialize};
use settings::Settings as _;
use text::{Anchor, BufferId, ToPointUtf16 as _};
use util::ResultExt as _;

use crate::lsp_command::{
    definition_locations_from_lsp, file_path_to_lsp_url, location_links_from_lsp,
    location_links_from_proto, location_links_to_proto,
};
use crate::lsp_store::LspStore;
use crate::lsp_store::rust_analyzer_ext::RUST_ANALYZER_NAME;
use crate::project_settings::ProjectSettings;
use crate::{File, LocationLink};

const JDTLS_NAME: LanguageServerName = LanguageServerName::new_static("jdtls");

/// Every symbol of a file is checked when it opens, so the server gets a
/// bounded number of these requests at a time instead of all of them at once.
const MAX_CONCURRENT_QUERIES: usize = 8;

/// A symbol whose implementations and parents are looked up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InheritanceQuery {
    /// The start of the symbol's name.
    pub position: Anchor,
    pub kind: SymbolKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InheritanceFlags {
    /// Subtypes, implementations or overriding members exist.
    pub has_children: bool,
    /// Supertypes, or a member this one implements or overrides, exist.
    pub has_parents: bool,
    /// Every parent is known to be an interface or trait, so the symbol
    /// implements rather than overrides or extends. False when unknown.
    pub parents_are_interfaces: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InheritanceDirection {
    Children,
    Parents,
}

impl InheritanceQuery {
    fn to_proto(self) -> proto::InheritanceQuery {
        proto::InheritanceQuery {
            position: Some(serialize_anchor(&self.position)),
            symbol_kind: self.kind.to_proto(),
        }
    }

    fn from_proto(query: proto::InheritanceQuery) -> Result<Self> {
        Ok(Self {
            position: query
                .position
                .and_then(deserialize_anchor)
                .context("invalid inheritance query position")?,
            kind: SymbolKind::from_proto(query.symbol_kind),
        })
    }
}

fn can_have_children(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Interface | SymbolKind::Method | SymbolKind::Function
    )
}

fn is_type(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Interface | SymbolKind::Enum | SymbolKind::Struct
    )
}

fn is_member(kind: SymbolKind) -> bool {
    matches!(kind, SymbolKind::Method | SymbolKind::Function)
}

/// What a local query needs from the buffer, captured up front so the
/// requests can run off the main thread.
struct LocalTarget {
    servers: Vec<Arc<LanguageServer>>,
    uri: lsp::Uri,
    request_timeout: Duration,
}

impl LspStore {
    /// Reports, for each query, whether the symbol has children or parents.
    /// The result has one entry per query, in order.
    pub fn inheritance_flags(
        &mut self,
        buffer: &Entity<Buffer>,
        queries: Vec<InheritanceQuery>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<InheritanceFlags>>> {
        if queries.is_empty() {
            return Task::ready(Ok(Vec::new()));
        }

        if let Some((client, project_id)) = self.upstream_client() {
            let query_count = queries.len();
            let request = proto::GetInheritanceFlags {
                project_id,
                buffer_id: buffer.read(cx).remote_id().into(),
                queries: queries
                    .into_iter()
                    .map(InheritanceQuery::to_proto)
                    .collect(),
                version: serialize_version(&buffer.read(cx).version()),
            };
            return cx.background_spawn(async move {
                let response = client.request(request).await?;
                anyhow::ensure!(
                    response.flags.len() == query_count,
                    "expected {query_count} inheritance flags, got {}",
                    response.flags.len()
                );
                Ok(response
                    .flags
                    .into_iter()
                    .map(|flags| InheritanceFlags {
                        has_children: flags.has_children,
                        has_parents: flags.has_parents,
                        parents_are_interfaces: flags.parents_are_interfaces,
                    })
                    .collect())
            });
        }

        let Some(target) = self.local_inheritance_target(buffer, cx) else {
            return Task::ready(Ok(vec![InheritanceFlags::default(); queries.len()]));
        };
        let snapshot = buffer.read(cx).snapshot();
        let positions = queries
            .into_iter()
            .map(|query| {
                (
                    point_to_lsp(query.position.to_point_utf16(&snapshot)),
                    query.kind,
                )
            })
            .collect::<Vec<_>>();
        cx.background_spawn(async move {
            let flags = stream::iter(positions)
                .map(|(position, kind)| query_flags(&target, position, kind))
                .buffered(MAX_CONCURRENT_QUERIES)
                .collect::<Vec<_>>()
                .await;
            Ok(flags)
        })
    }

    /// Looks up the locations of a symbol's children or parents.
    pub fn inheritance_targets(
        &mut self,
        buffer: &Entity<Buffer>,
        query: InheritanceQuery,
        direction: InheritanceDirection,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<LocationLink>>> {
        if let Some((client, project_id)) = self.upstream_client() {
            let request = proto::GetInheritanceTargets {
                project_id,
                buffer_id: buffer.read(cx).remote_id().into(),
                query: Some(query.to_proto()),
                parents: direction == InheritanceDirection::Parents,
                version: serialize_version(&buffer.read(cx).version()),
            };
            return cx.spawn(async move |lsp_store, cx| {
                let response = client.request(request).await?;
                let lsp_store = lsp_store.upgrade().context("project dropped")?;
                location_links_from_proto(response.links, lsp_store, cx.clone()).await
            });
        }

        let Some(target) = self.local_inheritance_target(buffer, cx) else {
            return Task::ready(Ok(Vec::new()));
        };
        let position = point_to_lsp(query.position.to_point_utf16(&buffer.read(cx).snapshot()));
        let buffer = buffer.clone();
        cx.spawn(async move |lsp_store, cx| {
            let lsp_store = lsp_store.upgrade().context("project dropped")?;
            let mut links = Vec::new();
            for server in &target.servers {
                let locations = match direction {
                    InheritanceDirection::Children => {
                        if !can_have_children(query.kind) {
                            continue;
                        }
                        children_locations(server, &target, position).await
                    }
                    InheritanceDirection::Parents => {
                        parent_locations(server, &target, position, query.kind)
                            .await
                            .map(|parents| parents.locations)
                    }
                };
                let locations = locations.with_context(|| {
                    format!("finding inheritance targets via {}", server.name())
                })?;
                if locations.is_empty() {
                    continue;
                }
                links.extend(
                    location_links_from_lsp(
                        Some(lsp::GotoDefinitionResponse::Array(locations)),
                        lsp_store.clone(),
                        buffer.clone(),
                        server.server_id(),
                        cx.clone(),
                    )
                    .await?,
                );
            }
            Ok(links.into_iter().dedup().collect())
        })
    }

    fn local_inheritance_target(
        &self,
        buffer: &Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Option<LocalTarget> {
        let local = self.as_local()?;
        let abs_path = File::from_dyn(buffer.read(cx).file())?
            .as_local()?
            .abs_path(cx);
        let uri = file_path_to_lsp_url(&abs_path).log_err()?;
        let servers = buffer.update(cx, |buffer, cx| {
            local
                .language_servers_for_buffer(buffer, cx)
                .map(|(_, server)| server.clone())
                .collect::<Vec<_>>()
        });
        Some(LocalTarget {
            servers,
            uri,
            request_timeout: ProjectSettings::get_global(cx)
                .global_lsp_settings
                .get_request_timeout(),
        })
    }

    pub(super) async fn handle_get_inheritance_flags(
        lsp_store: Entity<Self>,
        envelope: TypedEnvelope<proto::GetInheritanceFlags>,
        mut cx: AsyncApp,
    ) -> Result<proto::GetInheritanceFlagsResponse> {
        let buffer_id = BufferId::new(envelope.payload.buffer_id)?;
        let buffer = lsp_store.update(&mut cx, |lsp_store, cx| {
            lsp_store.buffer_store.read(cx).get_existing(buffer_id)
        })?;
        buffer
            .update(&mut cx, |buffer, _| {
                buffer.wait_for_version(deserialize_version(&envelope.payload.version))
            })
            .await?;
        let queries = envelope
            .payload
            .queries
            .into_iter()
            .map(InheritanceQuery::from_proto)
            .collect::<Result<Vec<_>>>()?;
        let flags = lsp_store
            .update(&mut cx, |lsp_store, cx| {
                lsp_store.inheritance_flags(&buffer, queries, cx)
            })
            .await?;
        Ok(proto::GetInheritanceFlagsResponse {
            flags: flags
                .into_iter()
                .map(|flags| proto::InheritanceFlags {
                    has_children: flags.has_children,
                    has_parents: flags.has_parents,
                    parents_are_interfaces: flags.parents_are_interfaces,
                })
                .collect(),
        })
    }

    pub(super) async fn handle_get_inheritance_targets(
        lsp_store: Entity<Self>,
        envelope: TypedEnvelope<proto::GetInheritanceTargets>,
        mut cx: AsyncApp,
    ) -> Result<proto::GetInheritanceTargetsResponse> {
        let sender_id = envelope.original_sender_id().unwrap_or_default();
        let buffer_id = BufferId::new(envelope.payload.buffer_id)?;
        let buffer = lsp_store.update(&mut cx, |lsp_store, cx| {
            lsp_store.buffer_store.read(cx).get_existing(buffer_id)
        })?;
        buffer
            .update(&mut cx, |buffer, _| {
                buffer.wait_for_version(deserialize_version(&envelope.payload.version))
            })
            .await?;
        let query = InheritanceQuery::from_proto(
            envelope
                .payload
                .query
                .context("missing inheritance query")?,
        )?;
        let direction = if envelope.payload.parents {
            InheritanceDirection::Parents
        } else {
            InheritanceDirection::Children
        };
        let links = lsp_store
            .update(&mut cx, |lsp_store, cx| {
                lsp_store.inheritance_targets(&buffer, query, direction, cx)
            })
            .await?;
        Ok(lsp_store.update(&mut cx, |lsp_store, cx| {
            proto::GetInheritanceTargetsResponse {
                links: location_links_to_proto(links, lsp_store, sender_id, cx),
            }
        }))
    }
}

async fn query_flags(
    target: &LocalTarget,
    position: lsp::Position,
    kind: SymbolKind,
) -> InheritanceFlags {
    let mut flags = InheritanceFlags::default();
    for server in &target.servers {
        if !flags.has_children && can_have_children(kind) {
            match children_locations(server, target, position).await {
                Ok(locations) => flags.has_children = !locations.is_empty(),
                Err(error) => {
                    log::debug!("finding implementations via {}: {error:#}", server.name())
                }
            }
        }
        if !flags.has_parents {
            match parent_locations(server, target, position, kind).await {
                Ok(parents) => {
                    flags.has_parents = !parents.locations.is_empty();
                    flags.parents_are_interfaces = flags.has_parents && parents.all_interfaces;
                }
                // Most servers lack type hierarchy support and answer with an error.
                Err(error) => log::debug!("finding parents via {}: {error:#}", server.name()),
            }
        }
    }
    flags
}

fn text_document_position(
    target: &LocalTarget,
    position: lsp::Position,
) -> lsp::TextDocumentPositionParams {
    lsp::TextDocumentPositionParams {
        text_document: lsp::TextDocumentIdentifier {
            uri: target.uri.clone(),
        },
        position,
    }
}

async fn children_locations(
    server: &LanguageServer,
    target: &LocalTarget,
    position: lsp::Position,
) -> Result<Vec<lsp::Location>> {
    let supports_implementations =
        server
            .capabilities()
            .implementation_provider
            .is_some_and(|capability| match capability {
                lsp::ImplementationProviderCapability::Simple(supported) => supported,
                lsp::ImplementationProviderCapability::Options(_) => true,
            });
    if !supports_implementations {
        return Ok(Vec::new());
    }
    let response = server
        .request::<lsp::request::GotoImplementation>(
            lsp::GotoDefinitionParams {
                text_document_position_params: text_document_position(target, position),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
            target.request_timeout,
        )
        .await
        .into_response()?;
    Ok(other_locations(response, target, position))
}

struct Parents {
    locations: Vec<lsp::Location>,
    all_interfaces: bool,
}

async fn parent_locations(
    server: &LanguageServer,
    target: &LocalTarget,
    position: lsp::Position,
    kind: SymbolKind,
) -> Result<Parents> {
    if is_type(kind) {
        supertype_locations(server, target, position).await
    } else if is_member(kind) && server.name() == JDTLS_NAME {
        let locations = server
            .request::<JavaFindLinks>(
                JavaFindLinksParams {
                    kind: "superImplementation".to_string(),
                    position: text_document_position(target, position),
                },
                target.request_timeout,
            )
            .await
            .into_response()?;
        Ok(Parents {
            locations: other_locations(
                locations.map(lsp::GotoDefinitionResponse::Array),
                target,
                position,
            ),
            // jdtls does not say whether the super method is declared in an
            // interface or a class.
            all_interfaces: false,
        })
    } else if is_member(kind) && server.name() == RUST_ANALYZER_NAME {
        // rust-analyzer resolves the declaration of a method in a trait impl
        // to the trait's method, and of any other method to itself.
        let response = server
            .request::<lsp::request::GotoDeclaration>(
                lsp::GotoDefinitionParams {
                    text_document_position_params: text_document_position(target, position),
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                },
                target.request_timeout,
            )
            .await
            .into_response()?;
        Ok(Parents {
            locations: other_locations(response, target, position),
            all_interfaces: true,
        })
    } else {
        Ok(Parents {
            locations: Vec::new(),
            all_interfaces: false,
        })
    }
}

async fn supertype_locations(
    server: &LanguageServer,
    target: &LocalTarget,
    position: lsp::Position,
) -> Result<Parents> {
    let items = server
        .request::<TypeHierarchyPrepare>(
            lsp::TypeHierarchyPrepareParams {
                text_document_position_params: text_document_position(target, position),
                work_done_progress_params: Default::default(),
            },
            target.request_timeout,
        )
        .await
        .into_response()?
        .unwrap_or_default();
    let mut locations = Vec::new();
    let mut all_interfaces = true;
    for item in items {
        let supertypes = server
            .request::<TypeHierarchySupertypes>(
                TypeHierarchySupertypesParams { item },
                target.request_timeout,
            )
            .await
            .into_response()?
            .unwrap_or_default();
        for supertype in supertypes {
            let supertype = serde_json::from_value::<TypeHierarchyItem>(supertype)
                .context("parsing a supertype")?;
            if !is_implicit_supertype(&supertype) {
                all_interfaces &= supertype.kind == lsp::SymbolKind::INTERFACE;
                locations.push(lsp::Location {
                    uri: supertype.uri,
                    range: supertype.selection_range,
                });
            }
        }
    }
    Ok(Parents {
        locations,
        all_interfaces,
    })
}

/// Java types extend these without saying so, which makes them noise.
fn is_implicit_supertype(item: &TypeHierarchyItem) -> bool {
    item.detail.as_deref() == Some("java.lang")
        && ["Object", "Enum", "Record"]
            .iter()
            .any(|name| item.name == *name || item.name.starts_with(&format!("{name}<")))
}

// `lsp::TypeHierarchyItem` declares `tags` as a single tag rather than a list,
// so it fails to parse items from servers that send any tags, jdtls included.
// Items are passed back to the server untouched, and only the fields read
// here are parsed.
enum TypeHierarchyPrepare {}

impl lsp::request::Request for TypeHierarchyPrepare {
    type Params = lsp::TypeHierarchyPrepareParams;
    type Result = Option<Vec<serde_json::Value>>;
    const METHOD: &'static str = "textDocument/prepareTypeHierarchy";
}

enum TypeHierarchySupertypes {}

#[derive(Debug, Serialize, Deserialize)]
struct TypeHierarchySupertypesParams {
    item: serde_json::Value,
}

impl lsp::request::Request for TypeHierarchySupertypes {
    type Params = TypeHierarchySupertypesParams;
    type Result = Option<Vec<serde_json::Value>>;
    const METHOD: &'static str = "typeHierarchy/supertypes";
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TypeHierarchyItem {
    name: String,
    kind: lsp::SymbolKind,
    detail: Option<String>,
    uri: lsp::Uri,
    selection_range: lsp::Range,
}

/// Servers commonly list the queried symbol among its own implementations or
/// declarations, which must not count as a child or parent.
fn other_locations(
    response: Option<lsp::GotoDefinitionResponse>,
    target: &LocalTarget,
    position: lsp::Position,
) -> Vec<lsp::Location> {
    definition_locations_from_lsp(response)
        .into_iter()
        .filter(|(_, uri, range)| {
            !(uri == &target.uri && range.start <= position && position <= range.end)
        })
        .map(|(_, uri, range)| lsp::Location { uri, range })
        .collect()
}

/// jdtls' request behind "Go to Super Implementation".
enum JavaFindLinks {}

#[derive(Debug, Serialize, Deserialize)]
struct JavaFindLinksParams {
    #[serde(rename = "type")]
    kind: String,
    position: lsp::TextDocumentPositionParams,
}

impl lsp::request::Request for JavaFindLinks {
    type Params = JavaFindLinksParams;
    type Result = Option<Vec<lsp::Location>>;
    const METHOD: &'static str = "java/findLinks";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jdtls_supertypes_with_tag_lists() {
        let supertype = serde_json::json!({
            "name": "JdbcClient",
            "detail": "ai.signal1.forge.connect.workflow.operator",
            "kind": 11,
            "tags": [],
            "uri": "file:///project/src/JdbcClient.java",
            "range": {"start": {"line": 5, "character": 0}, "end": {"line": 21, "character": 1}},
            "selectionRange": {"start": {"line": 12, "character": 17}, "end": {"line": 12, "character": 27}},
            "data": {"element": "=project/src<operator{JdbcClient.java[JdbcClient"}
        });
        let item = serde_json::from_value::<TypeHierarchyItem>(supertype)
            .expect("jdtls supertype should parse");
        assert_eq!(item.name, "JdbcClient");
        assert_eq!(item.kind, lsp::SymbolKind::INTERFACE);
        assert_eq!(item.selection_range.start, lsp::Position::new(12, 17));
        assert!(!is_implicit_supertype(&item));
    }

    #[test]
    fn hides_implicit_java_supertypes() {
        let item = |name: &str, detail: &str| TypeHierarchyItem {
            name: name.to_string(),
            kind: lsp::SymbolKind::CLASS,
            detail: Some(detail.to_string()),
            uri: "file:///Object.java".parse().expect("valid uri"),
            selection_range: lsp::Range::default(),
        };
        assert!(is_implicit_supertype(&item("Object", "java.lang")));
        assert!(is_implicit_supertype(&item("Enum<Color>", "java.lang")));
        assert!(!is_implicit_supertype(&item("Runnable", "java.lang")));
        assert!(!is_implicit_supertype(&item("Object", "com.example")));
    }
}
