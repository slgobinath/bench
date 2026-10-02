//! What Bench knows about a worktree that git does not: the colour it is
//! tinted in, the Linear issue it was made for, and the title it is shown by.
//!
//! # Where it is kept
//!
//! In a `bench.json` in the worktree's own git directory: for a linked
//! worktree `<repository>/.git/worktrees/<name>/bench.json`, the directory
//! its `.git` file points at, and for a repository's own checkout
//! `.git/bench.json`. There, rather than in Bench's database or beside the
//! files:
//!
//! - It goes where the worktree goes. `git worktree move` carries it, and
//!   `git worktree remove` deletes it, whoever runs them.
//! - Git never looks at it. It cannot show up as a change, be committed by an
//!   agent's `git add -A`, or make git refuse to remove the worktree for
//!   holding untracked files.
//! - It is a file you can read and edit, and other tools can find it. An edit
//!   made outside Bench is picked up while Bench is running.
//!
//! Read in the background the first time a worktree is asked about, and
//! cached: the title bar asks on every draw. Until it has been read — and for
//! a worktree Bench did not make, which has none — every reader has an answer:
//! the colour comes from the path, the issue from the branch name.
//!
//! # Why store at all
//!
//! A branch name says which issue a worktree is for only while the issue
//! keeps its identifier: moving it to another team, or archiving it, and the
//! name points nowhere. And a colour worked out from the path can land on the
//! same colour as another worktree of the project, where one chosen when the
//! worktree is made can be the one nobody else has.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fs::{Fs, RemoveOptions};
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, Hsla, Task, WeakEntity, hsla,
};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

/// The file's name in the worktree's git directory.
const FILE_NAME: &str = "bench.json";

/// How long after a change to a worktree's git directory its file is read
/// again. Git writes there in bursts — the index, `HEAD`, a lock and its
/// removal — and one read at the end of a burst is all it needs.
const WATCH_LATENCY: Duration = Duration::from_millis(200);

/// How many colours a worktree can have: evenly spaced hues, far enough apart
/// to tell any two apart at a glance.
pub const HUES: u8 = 16;

/// The colours by name, in hue order, for a menu to offer.
pub const HUE_NAMES: [&str; HUES as usize] = [
    "Red", "Orange", "Amber", "Yellow", "Lime", "Green", "Emerald", "Teal", "Cyan", "Sky",
    "Blue", "Indigo", "Violet", "Purple", "Fuchsia", "Pink",
];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeMetadata {
    /// The worktree's colour, as one of [`HUES`]. `None` is "worked out from
    /// the path"; see [`derived_hue`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hue: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<LinkedIssue>,
    /// What the worktree panel shows the worktree as, above its name. Taken
    /// from its issue's title when Bench first learns it, and then left alone,
    /// so that it can be edited here. An empty title is "none, and do not fill
    /// one in".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The icon the worktree panel draws its project with, by icon name. Only
    /// read off a repository's own checkout. A name rather than the icon
    /// itself so that this crate need not know the icon set, and a name the
    /// set no longer has falls back to the folder instead of failing the read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// The Linear issue a worktree was made for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedIssue {
    /// Linear's own id, which stays with the issue when its identifier
    /// changes.
    pub id: String,
    /// The identifier when it was linked, such as `ENG-123`, for showing
    /// before Linear has answered.
    pub identifier: String,
}

impl WorktreeMetadata {
    /// The worktree's colour, stored or worked out from its path.
    pub fn hue_for(&self, worktree: &Path) -> u8 {
        self.hue.unwrap_or_else(|| derived_hue(worktree))
    }

    /// The title to show, if there is one to show.
    pub fn shown_title(&self) -> Option<&str> {
        self.title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
    }
}

pub enum MetadataChanged {
    Changed(PathBuf),
}

enum Cached {
    /// Being read; readers get the defaults meanwhile.
    Loading,
    Loaded(WorktreeMetadata),
}

pub struct WorktreeMetadataStore {
    /// Behind a `RefCell` so a read — which starts loading the first time —
    /// needs only `&self`: the panels that read it draw from `&App`.
    cache: RefCell<HashMap<PathBuf, Cached>>,
    /// A watch on the git directory of every worktree whose file has been
    /// read, so that an edit made to it by hand shows up. Behind a `RefCell`
    /// for the same reason as the cache.
    watches: RefCell<HashMap<PathBuf, Task<()>>>,
    /// `None` only where no filesystem has been set up, which is some tests;
    /// the store then holds what it is told and keeps it nowhere.
    fs: Option<Arc<dyn Fs>>,
    this: WeakEntity<Self>,
}

struct GlobalWorktreeMetadataStore(Entity<WorktreeMetadataStore>);

impl Global for GlobalWorktreeMetadataStore {}

impl EventEmitter<MetadataChanged> for WorktreeMetadataStore {}

impl WorktreeMetadataStore {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalWorktreeMetadataStore>() {
            return global.0.clone();
        }
        let fs = <dyn Fs>::try_global(cx);
        let store = cx.new(|cx| Self {
            cache: RefCell::new(HashMap::new()),
            watches: RefCell::new(HashMap::new()),
            fs,
            this: cx.weak_entity(),
        });
        cx.set_global(GlobalWorktreeMetadataStore(store.clone()));
        store
    }

    /// The store, if anything has asked for it yet; for readers that have
    /// only `&App`.
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalWorktreeMetadataStore>()
            .map(|global| global.0.clone())
    }

    /// What is stored for `worktree`: the defaults until its file has been
    /// read, which this starts the first time it is asked about. Everyone
    /// drawing it is told once it has.
    pub fn get(&self, worktree: &Path, cx: &App) -> WorktreeMetadata {
        if let Some(cached) = self.cache.borrow().get(worktree) {
            return match cached {
                Cached::Loaded(metadata) => metadata.clone(),
                Cached::Loading => WorktreeMetadata::default(),
            };
        }
        let Some(fs) = self.fs.clone() else {
            return WorktreeMetadata::default();
        };
        self.cache
            .borrow_mut()
            .insert(worktree.to_path_buf(), Cached::Loading);
        let this = self.this.clone();
        let worktree = worktree.to_path_buf();
        cx.spawn(async move |cx| {
            let metadata = read(fs.as_ref(), &worktree).await;
            this.update(cx, |this, cx| {
                let cached = this.cache.borrow().get(&worktree).map(|cached| {
                    matches!(cached, Cached::Loading)
                });
                match cached {
                    // Something stored while this was reading is newer.
                    Some(false) => {}
                    Some(true) => {
                        this.cache
                            .borrow_mut()
                            .insert(worktree.clone(), Cached::Loaded(metadata.clone()));
                        if metadata != WorktreeMetadata::default() {
                            cx.emit(MetadataChanged::Changed(worktree.clone()));
                            cx.refresh_windows();
                        }
                    }
                    // Forgotten while it was being read: the worktree is gone.
                    None => return,
                }
                this.watch(worktree, cx);
            })
            .log_err();
        })
        .detach();
        WorktreeMetadata::default()
    }

    /// Reads `worktree`'s file again whenever it changes, and takes what it
    /// says. Bench's own writes come back through here too, and change
    /// nothing, since the cache already holds what they wrote.
    ///
    /// The directory is watched rather than the file: a write replaces the
    /// file, whoever makes it, and a watch on the file would end with the
    /// first one.
    fn watch(&self, worktree: PathBuf, cx: &mut Context<Self>) {
        let Some(fs) = self.fs.clone() else {
            return;
        };
        if self.watches.borrow().contains_key(&worktree) {
            return;
        }
        let task = cx.spawn({
            let worktree = worktree.clone();
            async move |this, cx| {
                let Some(file) = file_for(fs.as_ref(), &worktree).await else {
                    return;
                };
                let Some(directory) = file.parent() else {
                    return;
                };
                let (mut events, _watcher) = fs.watch(directory, WATCH_LATENCY).await;
                while let Some(events) = events.next().await {
                    // By name: the watch may report the directory by another
                    // spelling of its path, through a symlink.
                    if !events
                        .iter()
                        .any(|event| event.path.file_name() == file.file_name())
                    {
                        continue;
                    }
                    let metadata = read(fs.as_ref(), &worktree).await;
                    let updated = this.update(cx, |this, cx| {
                        let mut cache = this.cache.borrow_mut();
                        let changed = match cache.get(&worktree) {
                            Some(Cached::Loaded(cached)) => *cached != metadata,
                            // Forgotten, or not read yet: whatever is under
                            // way will have the last word.
                            Some(Cached::Loading) | None => false,
                        };
                        if changed {
                            cache.insert(worktree.clone(), Cached::Loaded(metadata));
                            drop(cache);
                            cx.emit(MetadataChanged::Changed(worktree.clone()));
                            cx.refresh_windows();
                        }
                    });
                    if updated.is_err() {
                        return;
                    }
                }
            }
        });
        self.watches.borrow_mut().insert(worktree, task);
    }

    /// Changes what is stored for `worktree`, and tells everyone drawing it.
    /// The new value is what readers get straight away; the file follows.
    pub fn update(
        &mut self,
        worktree: &Path,
        change: impl FnOnce(&mut WorktreeMetadata),
        cx: &mut Context<Self>,
    ) {
        let mut metadata = match self.cache.borrow().get(worktree) {
            Some(Cached::Loaded(metadata)) => metadata.clone(),
            Some(Cached::Loading) | None => WorktreeMetadata::default(),
        };
        change(&mut metadata);
        self.cache
            .borrow_mut()
            .insert(worktree.to_path_buf(), Cached::Loaded(metadata.clone()));
        if let Some(fs) = self.fs.clone() {
            let worktree = worktree.to_path_buf();
            cx.background_spawn(async move {
                if let Err(error) = write(fs.as_ref(), &worktree, &metadata).await {
                    log::error!(
                        "storing Bench's metadata for {}: {error:#}",
                        worktree.display()
                    );
                }
            })
            .detach();
        }
        cx.emit(MetadataChanged::Changed(worktree.to_path_buf()));
        // The title bar is drawn by every window, and none of them subscribe.
        cx.refresh_windows();
    }

    /// Gives `worktree` the title of its issue, unless it already has one or
    /// its file has not been read yet: a title that is there was either put
    /// there by hand or is the one it was given when it was made.
    pub fn fill_title(&mut self, worktree: &Path, title: &str, cx: &mut Context<Self>) {
        let missing = matches!(
            self.cache.borrow().get(worktree),
            Some(Cached::Loaded(metadata)) if metadata.title.is_none()
        );
        if missing {
            self.update(worktree, |metadata| metadata.title = Some(title.to_string()), cx);
        }
    }

    /// Forgets `worktree`, when it is deleted. Its file normally went with
    /// it — `git worktree remove` deletes the worktree's git directory — and
    /// is removed here in case it did not.
    pub fn remove(&mut self, worktree: &Path, cx: &mut Context<Self>) {
        self.cache.borrow_mut().remove(worktree);
        self.watches.borrow_mut().remove(worktree);
        if let Some(fs) = self.fs.clone() {
            let worktree = worktree.to_path_buf();
            cx.background_spawn(async move {
                let Some(file) = file_for(fs.as_ref(), &worktree).await else {
                    return;
                };
                fs.remove_file(
                    &file,
                    RemoveOptions {
                        recursive: false,
                        ignore_if_not_exists: true,
                    },
                )
                .await
                .log_err();
            })
            .detach();
        }
        cx.emit(MetadataChanged::Changed(worktree.to_path_buf()));
    }

    /// The colour for a new worktree: the one least used by `siblings`, the
    /// project's other worktrees, so that each worktree looks like no other
    /// for as long as there are colours to go round. Ties go to the lowest
    /// hue, so the choice does not depend on the order siblings come in.
    pub fn least_used_hue(&self, siblings: &[PathBuf], cx: &App) -> u8 {
        let mut uses = [0usize; HUES as usize];
        for sibling in siblings {
            let hue = self.get(sibling, cx).hue_for(sibling);
            if let Some(count) = uses.get_mut(hue as usize) {
                *count += 1;
            }
        }
        least_used(&uses)
    }
}

/// The worktree's git directory: where its `.git` file points, for a linked
/// worktree, or its `.git` directory, for a repository's own checkout. `None`
/// for a folder that is not a git worktree at all.
async fn git_directory(fs: &dyn Fs, worktree: &Path) -> Option<PathBuf> {
    let dot_git = worktree.join(".git");
    if fs.is_dir(&dot_git).await {
        return Some(dot_git);
    }
    let pointer = fs.load(&dot_git).await.ok()?;
    let gitdir = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
    Some(if gitdir.is_relative() {
        worktree.join(gitdir)
    } else {
        gitdir
    })
}

async fn file_for(fs: &dyn Fs, worktree: &Path) -> Option<PathBuf> {
    Some(git_directory(fs, worktree).await?.join(FILE_NAME))
}

async fn read(fs: &dyn Fs, worktree: &Path) -> WorktreeMetadata {
    let Some(file) = file_for(fs, worktree).await else {
        return WorktreeMetadata::default();
    };
    match fs.load(&file).await {
        Ok(text) => serde_json::from_str(&text).log_err().unwrap_or_default(),
        // No file is the ordinary case: a worktree Bench did not make.
        Err(_) => WorktreeMetadata::default(),
    }
}

async fn write(fs: &dyn Fs, worktree: &Path, metadata: &WorktreeMetadata) -> anyhow::Result<()> {
    let Some(file) = file_for(fs, worktree).await else {
        anyhow::bail!("{} is not a git worktree", worktree.display());
    };
    let mut text = serde_json::to_string_pretty(metadata)?;
    text.push('\n');
    fs.atomic_write(file, text).await
}

/// The saturation and lightness every worktree colour shares, so that hue is
/// the only thing that varies. Mid-lightness, because it is drawn on a title
/// bar that is nearly black in one theme and nearly white in the other.
const SATURATION: f32 = 0.72;
const LIGHTNESS: f32 = 0.58;

/// One of the [`HUES`] as a colour, for the title bar and the colour menu.
pub fn hue_color(hue: u8) -> Hsla {
    hsla(
        f32::from(hue % HUES) / f32::from(HUES),
        SATURATION,
        LIGHTNESS,
        1.,
    )
}

fn least_used(uses: &[usize]) -> u8 {
    uses.iter()
        .enumerate()
        .min_by_key(|(hue, count)| (**count, *hue))
        .map(|(hue, _)| hue as u8)
        .unwrap_or(0)
}

/// The colour a worktree has when none is stored: one of [`HUES`], from its
/// path.
///
/// FNV-1a over the path's bytes, finished with splitmix64's avalanche. FNV on
/// its own barely moves when the last few bytes change — which is exactly the
/// case here, `fix-login` against `fix-logout` — and the avalanche is what
/// turns a one-character difference into an unrelated number. Both are fixed
/// arithmetic, so the colour is the same next year as it is today: a
/// toolchain bump must not repaint every worktree.
pub fn derived_hue(worktree: &Path) -> u8 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for byte in worktree.as_os_str().as_encoded_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;

    (hash % HUES as u64) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;

    #[test]
    fn a_new_worktree_gets_the_colour_nobody_has() {
        let mut uses = [1usize; HUES as usize];
        uses[5] = 0;
        assert_eq!(least_used(&uses), 5);
        assert_eq!(least_used(&[0; HUES as usize]), 0, "ties go to the lowest");
    }

    #[test]
    fn a_path_always_gets_the_same_colour() {
        let path = Path::new("/Users/me/bench/project/fix-login");
        assert_eq!(derived_hue(path), derived_hue(path));
        assert!(derived_hue(path) < HUES);
    }

    /// A repository's own checkout and a linked worktree of it, as git lays
    /// them out.
    async fn repository(cx: &mut TestAppContext) -> (Arc<FakeFs>, Entity<WorktreeMetadataStore>) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({
                ".git": { "worktrees": { "fix": {} } },
                "file.txt": "hi",
            }),
        )
        .await;
        fs.insert_tree(
            "/wt/fix",
            json!({ ".git": "gitdir: /repo/.git/worktrees/fix", "file.txt": "hi" }),
        )
        .await;
        cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
        let store = cx.update(WorktreeMetadataStore::global);
        (fs, store)
    }

    /// Kept in the worktree's git directory, where git never looks and
    /// `git worktree remove` deletes it — not in the worktree itself.
    #[gpui::test]
    async fn it_is_kept_in_the_worktrees_git_directory(cx: &mut TestAppContext) {
        let (fs, store) = repository(cx).await;
        store.update(cx, |store, cx| {
            store.update(
                Path::new("/wt/fix"),
                |metadata| {
                    metadata.hue = Some(3);
                    metadata.issue = Some(LinkedIssue {
                        id: "uuid".into(),
                        identifier: "ENG-1".into(),
                    });
                },
                cx,
            );
            store.update(Path::new("/repo"), |metadata| metadata.hue = Some(9), cx);
        });
        cx.run_until_parked();

        let linked: serde_json::Value = serde_json::from_str(
            &fs.load(Path::new("/repo/.git/worktrees/fix/bench.json"))
                .await
                .expect("the linked worktree's file"),
        )
        .expect("json");
        assert_eq!(
            linked,
            json!({ "hue": 3, "issue": { "id": "uuid", "identifier": "ENG-1" } })
        );
        assert!(fs.is_file(Path::new("/repo/.git/bench.json")).await);
        assert!(
            !fs.is_file(Path::new("/wt/fix/bench.json")).await,
            "nothing in the worktree itself"
        );
    }

    /// Read back when first asked about, as after a restart: the defaults
    /// until the file has been read, then what it says.
    #[gpui::test]
    async fn it_is_read_back_after_a_restart(cx: &mut TestAppContext) {
        let (fs, store) = repository(cx).await;
        fs.insert_file(
            "/repo/.git/worktrees/fix/bench.json",
            br#"{ "hue": 12 }"#.to_vec(),
        )
        .await;

        let first = store.read_with(cx, |store, cx| store.get(Path::new("/wt/fix"), cx));
        assert_eq!(first, WorktreeMetadata::default(), "not read yet");
        cx.run_until_parked();
        let read = store.read_with(cx, |store, cx| store.get(Path::new("/wt/fix"), cx));
        assert_eq!(read.hue, Some(12));
    }

    /// An issue's title is filled in once the file has been read, and never
    /// over one already there: that is either the one it was made with or
    /// one written by hand, empty included.
    #[gpui::test]
    async fn a_title_is_filled_in_only_where_there_is_none(cx: &mut TestAppContext) {
        let (fs, store) = repository(cx).await;
        fs.insert_file(
            "/repo/.git/worktrees/fix/bench.json",
            br#"{ "hue": 2 }"#.to_vec(),
        )
        .await;
        fs.insert_file("/repo/.git/bench.json", br#"{ "title": "" }"#.to_vec())
            .await;

        store.update(cx, |store, cx| {
            store.fill_title(Path::new("/wt/fix"), "Not read yet", cx)
        });
        store.read_with(cx, |store, cx| {
            store.get(Path::new("/wt/fix"), cx);
            store.get(Path::new("/repo"), cx);
        });
        cx.run_until_parked();
        store.update(cx, |store, cx| {
            store.fill_title(Path::new("/wt/fix"), "Fix login", cx);
            store.fill_title(Path::new("/wt/fix"), "Something else", cx);
            store.fill_title(Path::new("/repo"), "Fix login", cx);
        });
        cx.run_until_parked();

        let fix = store.read_with(cx, |store, cx| store.get(Path::new("/wt/fix"), cx));
        assert_eq!(fix.hue, Some(2), "the rest of the file is kept");
        assert_eq!(fix.shown_title(), Some("Fix login"));
        let main = store.read_with(cx, |store, cx| store.get(Path::new("/repo"), cx));
        assert_eq!(main.shown_title(), None, "emptied by hand, and left so");
    }

    #[gpui::test]
    async fn an_edit_by_hand_is_picked_up(cx: &mut TestAppContext) {
        let (fs, store) = repository(cx).await;
        store.read_with(cx, |store, cx| store.get(Path::new("/wt/fix"), cx));
        cx.run_until_parked();

        fs.insert_file(
            "/repo/.git/worktrees/fix/bench.json",
            br#"{ "title": "Renamed by hand" }"#.to_vec(),
        )
        .await;
        cx.run_until_parked();

        let read = store.read_with(cx, |store, cx| store.get(Path::new("/wt/fix"), cx));
        assert_eq!(read.shown_title(), Some("Renamed by hand"));
    }

    #[gpui::test]
    async fn forgetting_removes_the_file(cx: &mut TestAppContext) {
        let (fs, store) = repository(cx).await;
        store.update(cx, |store, cx| {
            store.update(Path::new("/wt/fix"), |metadata| metadata.hue = Some(1), cx)
        });
        cx.run_until_parked();
        store.update(cx, |store, cx| store.remove(Path::new("/wt/fix"), cx));
        cx.run_until_parked();
        assert!(!fs.is_file(Path::new("/repo/.git/worktrees/fix/bench.json")).await);
    }
}
