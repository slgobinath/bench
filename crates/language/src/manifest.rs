use std::sync::Arc;

use gpui::SharedString;
use settings::WorktreeId;
use util::rel_path::RelPath;

// Re-export ManifestName from language_core.
pub use language_core::ManifestName;

/// Represents a manifest query; given a path to a file, the manifest provider is tasked with finding a path to the directory containing the manifest for that file.
///
/// Since parts of the path might have already been explored, there's an additional `depth` parameter that indicates to what ancestry level a given path should be explored.
/// For example, given a path like `foo/bar/baz`, a depth of 2 would explore `foo/bar/baz` and `foo/bar`, but not `foo`.
pub struct ManifestQuery {
    /// Path to the file, relative to worktree root.
    pub path: Arc<RelPath>,
    pub depth: usize,
    pub delegate: Arc<dyn ManifestDelegate>,
}

pub trait ManifestProvider {
    fn name(&self) -> ManifestName;
    fn search(&self, query: ManifestQuery) -> Option<Arc<RelPath>>;
}

pub trait ManifestDelegate: Send + Sync {
    fn worktree_id(&self) -> WorktreeId;
    fn exists(&self, path: &RelPath, is_dir: Option<bool>) -> bool;
}

/// The manifests that mark the root of a project in a language Zed does not
/// build in.
///
/// A language that comes from an extension carries no manifest name — the
/// extension API has nowhere to declare one, and `extension_host` loads every
/// language with `manifest_name: None` — so its language servers are rooted at
/// the worktree root. That is right for a worktree that *is* the project and
/// wrong for anything nested: a Maven module or a Flutter package one directory
/// down gets a server rooted above it, which for servers that import and build
/// a project (jdtls, the Dart analysis server) means no import, and so no
/// go-to-definition.
///
/// These fill that gap from Bench's side, so the extensions need no change.
/// Each name here has to have a provider registered for it in `languages`;
/// [`JAVA_MANIFESTS`] and [`DART_MANIFESTS`] are what those providers search.
pub fn default_manifest_name(language: &str) -> Option<ManifestName> {
    let manifest = match language {
        "Java" => JAVA_MANIFEST,
        "Dart" => DART_MANIFEST,
        _ => return None,
    };
    Some(ManifestName::from(SharedString::new_static(manifest)))
}

/// The manifest name Java projects are looked up under. A language has one
/// name, and a Java project has several possible manifests, so this is the
/// name the search is keyed by rather than the only file it looks for; see
/// [`JAVA_MANIFESTS`].
pub const JAVA_MANIFEST: &str = "pom.xml";

/// Every file that marks the root of a Java project: Maven's, then Gradle's.
/// A build file names a module, and a settings file names the build that owns
/// it, so both are roots to look for — the outermost match wins, which is the
/// aggregator rather than one module of it.
pub const JAVA_MANIFESTS: &[&str] = &[
    "pom.xml",
    "settings.gradle",
    "settings.gradle.kts",
    "build.gradle",
    "build.gradle.kts",
];

/// The manifest name Dart and Flutter projects are looked up under.
pub const DART_MANIFEST: &str = "pubspec.yaml";

/// Every file that marks the root of a Dart or Flutter project. A Flutter app
/// is a Dart package, and both are a `pubspec.yaml`.
pub const DART_MANIFESTS: &[&str] = &["pubspec.yaml"];
