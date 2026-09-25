//! Project roots for languages Zed does not build in.
//!
//! A language server is rooted at the manifest of the project it is opening —
//! `Cargo.toml` for rust-analyzer, `pyproject.toml` for the Python servers —
//! and a language that comes from an extension has no way to say what its
//! manifest is: the extension API has no field for it, so `extension_host`
//! loads every one of them with `manifest_name: None` and their servers are
//! rooted at the worktree root instead.
//!
//! For a worktree that is itself the project, the worktree root is the right
//! answer and nothing here matters. It is nested projects that break: a Maven
//! module, or a Flutter package, one directory inside the worktree gets a
//! server rooted above it. Servers that import and build what they are given —
//! jdtls, the Dart analysis server — then find no project at all, which is
//! silent: the server runs, and nothing resolves.
//!
//! [`language::default_manifest_name`] says which manifest such a
//! language uses; this is the half that knows how to find it.

use std::sync::Arc;

use language::{
    DART_MANIFEST, DART_MANIFESTS, JAVA_MANIFEST, JAVA_MANIFESTS, ManifestDelegate, ManifestName,
    ManifestProvider, ManifestQuery,
};
use util::rel_path::RelPath;

/// Finds a project root by looking for files of known names.
///
/// One provider answers to one [`ManifestName`] but may look for several
/// files, because one language can have several kinds of project: a Java
/// project is a `pom.xml` or any of Gradle's build files, and a server wants
/// whichever of them is there.
struct FileNameManifestProvider {
    name: ManifestName,
    file_names: &'static [&'static str],
}

impl ManifestProvider for FileNameManifestProvider {
    fn name(&self) -> ManifestName {
        self.name.clone()
    }

    /// The outermost directory holding one of the names, as Cargo's provider
    /// does: for a multi-module build the root is the build that owns the
    /// modules, not the module the file happens to be in. The search is
    /// bounded by `depth`, which is how far towards the worktree root the tree
    /// has not been explored yet.
    fn search(
        &self,
        ManifestQuery {
            path,
            depth,
            delegate,
        }: ManifestQuery,
    ) -> Option<Arc<RelPath>> {
        let mut outermost = None;
        for directory in path.ancestors().take(depth) {
            if self.holds_a_manifest(directory, delegate.as_ref()) {
                outermost = Some(Arc::from(directory));
            }
        }
        outermost
    }
}

impl FileNameManifestProvider {
    fn holds_a_manifest(&self, directory: &RelPath, delegate: &dyn ManifestDelegate) -> bool {
        self.file_names.iter().any(|file_name| {
            RelPath::from_unix_str(file_name)
                .ok()
                .is_some_and(|file_name| delegate.exists(&directory.join(&file_name), Some(false)))
        })
    }
}

/// Every provider for a language Zed does not build in.
///
/// A language is looked up by one name, so that is the name its provider
/// carries. The rest are registered as well, pointing at the same search:
/// `LspStore` watches the registered names to know when a manifest appears or
/// goes, and a Gradle build that only ever registered as `pom.xml` would never
/// be noticed.
pub(crate) fn providers() -> Vec<Arc<dyn ManifestProvider>> {
    let mut providers: Vec<Arc<dyn ManifestProvider>> = Vec::new();
    for (names, manifests) in [
        (JAVA_MANIFESTS, JAVA_MANIFESTS),
        (DART_MANIFESTS, DART_MANIFESTS),
    ] {
        for name in names {
            providers.push(Arc::new(FileNameManifestProvider {
                name: ManifestName::from(gpui::SharedString::new_static(name)),
                file_names: manifests,
            }));
        }
    }
    debug_assert!(
        providers
            .iter()
            .any(|provider| provider.name().as_ref().as_ref() == JAVA_MANIFEST)
            && providers
                .iter()
                .any(|provider| provider.name().as_ref().as_ref() == DART_MANIFEST),
        "the name a language is looked up by must have a provider registered for it"
    );
    providers
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::WorktreeId;
    use std::collections::HashSet;

    struct FakeWorktree(HashSet<String>);

    impl ManifestDelegate for FakeWorktree {
        fn worktree_id(&self) -> WorktreeId {
            WorktreeId::from_usize(0)
        }

        fn exists(&self, path: &RelPath, _is_dir: Option<bool>) -> bool {
            self.0.contains(path.as_unix_str())
        }
    }

    fn root_of(files: &[&str], file: &str, manifests: &'static [&'static str]) -> Option<String> {
        let provider = FileNameManifestProvider {
            name: ManifestName::from(gpui::SharedString::new_static("test")),
            file_names: manifests,
        };
        let path: Arc<RelPath> = RelPath::from_unix_str(file)
            .expect("a relative path")
            .into();
        provider
            .search(ManifestQuery {
                depth: path.components().count() + 1,
                path,
                delegate: Arc::new(FakeWorktree(
                    files.iter().map(|file| file.to_string()).collect(),
                )),
            })
            .map(|root| root.as_unix_str().to_owned())
    }

    /// The point of the whole thing: a project one directory inside the
    /// worktree is rooted at itself, not at the worktree.
    #[test]
    fn a_nested_project_is_rooted_at_itself() {
        let root = root_of(
            &["repos/orders/pom.xml", "repos/orders/src/Main.java"],
            "repos/orders/src/Main.java",
            JAVA_MANIFESTS,
        );

        assert_eq!(root.as_deref(), Some("repos/orders"));
    }

    /// Gradle names a project as surely as Maven does, and a language has only
    /// one manifest name to be looked up by — so the search cannot be the one
    /// file that name happens to be.
    #[test]
    fn a_gradle_build_is_a_java_project_too() {
        let root = root_of(
            &[
                "repos/orders/build.gradle.kts",
                "repos/orders/src/Main.java",
            ],
            "repos/orders/src/Main.java",
            JAVA_MANIFESTS,
        );

        assert_eq!(root.as_deref(), Some("repos/orders"));
    }

    /// A module of a multi-module build belongs to the build: rooting a server
    /// at the module would hide every other module from it.
    #[test]
    fn a_module_is_rooted_at_the_build_that_owns_it() {
        let root = root_of(
            &[
                "orders/pom.xml",
                "orders/api/pom.xml",
                "orders/api/src/Main.java",
            ],
            "orders/api/src/Main.java",
            JAVA_MANIFESTS,
        );

        assert_eq!(root.as_deref(), Some("orders"));
    }

    /// A Flutter app is a Dart package, and its package is where the analysis
    /// server has to start.
    #[test]
    fn a_flutter_package_is_rooted_at_its_pubspec() {
        let root = root_of(
            &["repos/app/pubspec.yaml", "repos/app/lib/main.dart"],
            "repos/app/lib/main.dart",
            DART_MANIFESTS,
        );

        assert_eq!(root.as_deref(), Some("repos/app"));
    }

    /// Nothing to root at is not the worktree root by default — saying so is
    /// what lets the caller fall back deliberately.
    #[test]
    fn a_directory_with_no_manifest_has_no_root() {
        let root = root_of(
            &["repos/notes/README.md"],
            "repos/notes/README.md",
            JAVA_MANIFESTS,
        );

        assert_eq!(root, None);
    }

    /// Every language's lookup name has to be a name something is registered
    /// under, or the search never runs.
    #[test]
    fn the_names_languages_are_looked_up_by_are_registered() {
        let names: Vec<String> = providers()
            .iter()
            .map(|provider| provider.name().as_ref().to_string())
            .collect();

        assert!(names.iter().any(|name| name == JAVA_MANIFEST));
        assert!(names.iter().any(|name| name == DART_MANIFEST));
    }
}
