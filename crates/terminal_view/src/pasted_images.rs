//! Thumbnails of the images in a Claude Code prompt, floating above it.
//!
//! The terminal cannot draw pictures, so Bench does: it reads the `[Image #n]`
//! tags off the screen and shows the files Claude Code cached them in. Going by
//! the tags rather than the paste covers every way an image gets into a prompt,
//! and drops a thumbnail when Claude Code drops its tag.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_tracker::pasted_image_path;
use gpui::{
    AnyElement, Context, Entity, Image, ImageSource, ObjectFit, RetainAllImageCache, StyledImage,
    Task, Window, img,
};
use terminal::Content;
use ui::prelude::*;
use util::ResultExt;
use workspace::{OpenOptions, OpenVisible};

use crate::TerminalView;

/// Claude Code's spinner redraws the terminal constantly while it works, so
/// scans are held to this pace rather than run on every redraw.
const SCAN_INTERVAL: Duration = Duration::from_millis(200);
/// How far up from the bottom of the screen the prompt is looked for.
const PROMPT_SEARCH_ROWS: usize = 32;
/// A tag can reach the screen just before its file reaches the disk.
const LOAD_ATTEMPTS: usize = 5;
const MAX_REMOTE_PASTES: usize = 16;

const IMAGE_HEIGHT: Pixels = px(96.);
const TILE_MAX_WIDTH: Pixels = px(192.);
const TILE_GAP: Pixels = px(6.);
const STRIP_HEIGHT: Pixels = px(128.);

pub(crate) struct PastedImages {
    tiles: Vec<Tile>,
    /// The screen row of the prompt's top edge, `None` while Claude Code shows
    /// something else in its place. The draft and its images are still there
    /// then, so the tiles are hidden rather than dropped.
    prompt_top_row: Option<usize>,
    /// Images sent to an SSH host, whose cache this machine cannot read, in
    /// the order their tags will appear.
    remote_pastes: VecDeque<Arc<Image>>,
    scan_requested: bool,
    scan: Option<Task<()>>,
    load: Task<()>,
    cache: Entity<RetainAllImageCache>,
}

struct Tile {
    number: u32,
    image: TileImage,
}

enum TileImage {
    Missing,
    File(PathBuf),
    Pasted(Arc<Image>),
}

#[derive(Debug, PartialEq, Eq)]
struct Prompt {
    top_row: usize,
    image_numbers: Vec<u32>,
}

impl PastedImages {
    pub(crate) fn new(cx: &mut App) -> Self {
        Self {
            tiles: Vec::new(),
            prompt_top_row: None,
            remote_pastes: VecDeque::new(),
            scan_requested: false,
            scan: None,
            load: Task::ready(()),
            cache: RetainAllImageCache::new(cx),
        }
    }

    pub(crate) fn request_scan(&mut self) {
        self.scan_requested = true;
    }

    pub(crate) fn remember_remote_paste(&mut self, image: Image) {
        if self.remote_pastes.len() == MAX_REMOTE_PASTES {
            self.remote_pastes.pop_front();
        }
        self.remote_pastes.push_back(Arc::new(image));
    }

    fn set_tiles(&mut self, image_numbers: &[u32], cx: &mut App) {
        let mut previous = std::mem::take(&mut self.tiles);
        for &number in image_numbers {
            let tile = match previous.iter().position(|tile| tile.number == number) {
                Some(index) => previous.swap_remove(index),
                None => Tile {
                    number,
                    image: self
                        .remote_pastes
                        .pop_front()
                        .map_or(TileImage::Missing, TileImage::Pasted),
                },
            };
            self.tiles.push(tile);
        }
        for tile in previous {
            if let TileImage::Pasted(image) = tile.image {
                ImageSource::from(image).remove_asset(cx);
            }
        }
        if self.tiles.is_empty() {
            // The old cache's images are freed when it is released.
            self.cache = RetainAllImageCache::new(cx);
        }
    }

    fn missing_numbers(&self) -> Vec<u32> {
        self.tiles
            .iter()
            .filter(|tile| matches!(tile.image, TileImage::Missing))
            .map(|tile| tile.number)
            .collect()
    }
}

impl TerminalView {
    /// Called from `render` because a terminal's screen contents only update
    /// when it paints, which also leaves hidden terminals out of the scanning.
    pub(crate) fn schedule_pasted_images_scan(&mut self, cx: &mut Context<Self>) {
        if !self.pasted_images.scan_requested || self.pasted_images.scan.is_some() {
            return;
        }
        self.pasted_images.scan_requested = false;
        if self.pasted_images.tiles.is_empty() && !self.terminal.read(cx).is_running_claude() {
            return;
        }
        self.pasted_images.scan = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SCAN_INTERVAL).await;
            this.update(cx, |this, cx| {
                this.pasted_images.scan = None;
                this.scan_pasted_images(cx);
                this.schedule_pasted_images_scan(cx);
            })
            .log_err();
        }));
    }

    fn scan_pasted_images(&mut self, cx: &mut Context<Self>) {
        let terminal = self.terminal.read(cx);
        if !terminal.is_running_claude() {
            self.pasted_images.prompt_top_row = None;
            self.pasted_images.remote_pastes.clear();
            if !self.pasted_images.tiles.is_empty() {
                self.pasted_images.set_tiles(&[], cx);
                cx.notify();
            }
            return;
        }
        let content = terminal.last_content();
        if content.display_offset != 0 {
            return;
        }
        let prompt = find_prompt(&screen_rows(content));
        let process_id = terminal.foreground_process_id();

        let top_row = prompt.as_ref().map(|prompt| prompt.top_row);
        let mut changed = self.pasted_images.prompt_top_row != top_row;
        self.pasted_images.prompt_top_row = top_row;
        if let Some(prompt) = prompt {
            let shown = self.pasted_images.tiles.iter().map(|tile| tile.number);
            if !shown.eq(prompt.image_numbers.iter().copied()) {
                self.pasted_images.set_tiles(&prompt.image_numbers, cx);
                if let Some(process_id) = process_id {
                    self.load_pasted_images(process_id, cx);
                }
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn load_pasted_images(&mut self, process_id: u32, cx: &mut Context<Self>) {
        let mut numbers = self.pasted_images.missing_numbers();
        if numbers.is_empty() {
            return;
        }
        self.pasted_images.load = cx.spawn(async move |this, cx| {
            for _ in 0..LOAD_ATTEMPTS {
                let found = cx
                    .background_spawn(async move {
                        numbers
                            .into_iter()
                            .filter_map(|number| {
                                Some((number, pasted_image_path(process_id, number)?))
                            })
                            .collect::<Vec<_>>()
                    })
                    .await;
                let Ok(missing) = this.update(cx, |this, cx| {
                    for (number, path) in found {
                        if let Some(tile) = this
                            .pasted_images
                            .tiles
                            .iter_mut()
                            .find(|tile| tile.number == number)
                        {
                            tile.image = TileImage::File(path);
                        }
                    }
                    cx.notify();
                    this.pasted_images.missing_numbers()
                }) else {
                    return;
                };
                if missing.is_empty() {
                    return;
                }
                numbers = missing;
                cx.background_executor().timer(SCAN_INTERVAL).await;
            }
        });
    }

    pub(crate) fn render_pasted_images(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let images = &self.pasted_images;
        let prompt_top_row = images.prompt_top_row?;
        if images.tiles.is_empty() {
            return None;
        }
        let content = self.terminal.read(cx).last_content();
        if content.display_offset != 0 {
            return None;
        }
        let bounds = content.terminal_bounds;
        let prompt_top = self.content_top_padding.to_pixels(window.rem_size())
            + bounds.line_height() * prompt_top_row as f32;
        let fitting =
            ((bounds.bounds.size.width + TILE_GAP) / (TILE_MAX_WIDTH + TILE_GAP)) as usize;
        let shown = if images.tiles.len() <= fitting {
            images.tiles.len()
        } else {
            // Leaves a tile's width for the `+N` label.
            fitting.saturating_sub(1).max(1)
        };
        let hidden = images.tiles.len() - shown;

        let tiles = images
            .tiles
            .iter()
            .take(shown)
            .map(|tile| self.render_tile(tile, cx))
            .collect::<Vec<_>>();
        Some(
            h_flex()
                .absolute()
                .left_2()
                .top((prompt_top - STRIP_HEIGHT).max(px(0.)))
                .h(STRIP_HEIGHT)
                .items_end()
                .gap(TILE_GAP)
                .children(tiles)
                .when(hidden > 0, |strip| {
                    strip.child(Label::new(format!("+{hidden}")).color(Color::Muted))
                })
                .into_any_element(),
        )
    }

    fn render_tile(&self, tile: &Tile, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let number = tile.number;
        let no_preview = || {
            h_flex()
                .h(IMAGE_HEIGHT)
                .px_2()
                .child(
                    Label::new("no preview")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        };
        let source = match &tile.image {
            TileImage::Missing => None,
            TileImage::File(path) => Some(ImageSource::from(path.clone())),
            TileImage::Pasted(image) => Some(ImageSource::from(image.clone())),
        };
        let image = match source {
            None => no_preview(),
            Some(source) => img(source)
                .h(IMAGE_HEIGHT)
                .max_w(TILE_MAX_WIDTH)
                .object_fit(ObjectFit::Contain)
                .image_cache(&self.pasted_images.cache)
                .with_fallback(no_preview)
                .into_any_element(),
        };
        let path = match &tile.image {
            TileImage::File(path) => Some(path.clone()),
            TileImage::Missing | TileImage::Pasted(_) => None,
        };

        v_flex()
            .id(("pasted-image", number as usize))
            .occlude()
            .items_center()
            .gap_0p5()
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background)
            .child(image)
            .child(
                Label::new(format!("#{number}"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .when_some(path, |tile, path| {
                tile.cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_pasted_image(path.clone(), window, cx);
                    }))
            })
            .into_any_element()
    }

    fn open_pasted_image(&self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let options = OpenOptions {
            visible: Some(OpenVisible::None),
            ..Default::default()
        };
        workspace
            .update(cx, |workspace, cx| {
                workspace.open_abs_path(path, options, window, cx)
            })
            .detach_and_log_err(cx);
    }
}

fn screen_rows(content: &Content) -> Vec<(usize, String)> {
    let first_cell = content
        .cells
        .len()
        .saturating_sub(content.columns * PROMPT_SEARCH_ROWS);
    content.cells[first_cell..]
        .chunk_by(|left, right| left.point.line == right.point.line)
        .filter_map(|row| {
            let line = row.first()?.point.line;
            Some((
                usize::try_from(line).ok()?,
                row.iter().map(|cell| cell.character()).collect(),
            ))
        })
        .collect()
}

fn is_prompt_edge(row: &str) -> bool {
    row.trim_start().starts_with("──")
}

/// Finds the live prompt: the last `❯` directly under a rule. Past messages
/// carry the same marker and tags, and a menu's selection the same marker, but
/// neither sits under a rule.
fn find_prompt(rows: &[(usize, String)]) -> Option<Prompt> {
    let prompt_index = (1..rows.len()).rev().find(|&index| {
        rows[index].1.trim_start().starts_with('❯') && is_prompt_edge(&rows[index - 1].1)
    })?;
    let text = rows[prompt_index..]
        .iter()
        .map(|(_, row)| row.as_str())
        .take_while(|row| !is_prompt_edge(row))
        .map(str::trim)
        .collect::<Vec<_>>()
        // A tag wrapped across rows breaks at its space.
        .join(" ");
    Some(Prompt {
        top_row: rows[prompt_index - 1].0,
        image_numbers: image_numbers(&text),
    })
}

fn image_numbers(text: &str) -> Vec<u32> {
    let mut numbers = Vec::new();
    for rest in text.split("[Image #").skip(1) {
        if let Some((number, _)) = rest.split_once(']')
            && let Ok(number) = number.parse()
            && !numbers.contains(&number)
        {
            numbers.push(number);
        }
    }
    numbers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &[&str]) -> Vec<(usize, String)> {
        lines
            .iter()
            .enumerate()
            .map(|(index, line)| (index + 10, line.to_string()))
            .collect()
    }

    #[test]
    fn reads_the_tags_in_the_live_prompt() {
        let screen = rows(&[
            "⏺ Done.",
            "──────────────",
            "❯\u{a0}compare [Image #2] with [Image #1] and [Image #2]",
            "──────────────",
            "  ? for shortcuts",
        ]);
        assert_eq!(
            find_prompt(&screen),
            Some(Prompt {
                top_row: 11,
                image_numbers: vec![2, 1],
            })
        );
    }

    #[test]
    fn ignores_tags_in_sent_messages() {
        let screen = rows(&[
            "❯ what is this [Image #1]",
            "",
            "⏺ A screenshot.",
            "──────────────",
            "❯ ",
            "──────────────",
        ]);
        assert_eq!(
            find_prompt(&screen),
            Some(Prompt {
                top_row: 13,
                image_numbers: Vec::new(),
            })
        );
    }

    #[test]
    fn reads_a_prompt_wrapped_over_several_rows() {
        let screen = rows(&[
            "──────────────",
            "❯ look at [Image #1] and [Image",
            "  #2] and [Image #3]",
            "──────────────",
        ]);
        assert_eq!(
            find_prompt(&screen).map(|prompt| prompt.image_numbers),
            Some(vec![1, 2, 3])
        );
    }

    #[test]
    fn finds_no_prompt_while_a_menu_replaces_it() {
        let screen = rows(&[
            "❯ what is this [Image #1]",
            " Do you want to proceed?",
            " ❯ 1. Yes",
            "   2. No",
        ]);
        assert_eq!(find_prompt(&screen), None);
    }

    #[test]
    fn skips_text_that_only_resembles_a_tag() {
        assert_eq!(image_numbers("[Image #x] [Image #4"), Vec::<u32>::new());
    }
}
