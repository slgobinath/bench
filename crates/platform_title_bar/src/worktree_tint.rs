//! A colour per worktree, washed across the title bar.
//!
//! Bench's window holds a workspace per worktree and you switch between them
//! all day. The name in the title bar says which one you are in, but reading
//! is slower than seeing, and `fix-inefficient-job-log-insertion` and
//! `fix-inefficient-job-log-lookup` do not look different at a glance. A
//! colour does — this is IntelliJ's per-project tint, per worktree.
//!
//! Three things make it work rather than just be decoration:
//!
//! - **Each worktree keeps its colour.** It is chosen when Bench makes the
//!   worktree — the one its project's other worktrees use least — and stored
//!   with it; see `worktree_metadata`. A worktree Bench did not make has one
//!   worked out from its path, the same on every machine. Either can be
//!   changed from the worktree panel.
//! - **No two worktrees look *almost* alike.** The hue is one of
//!   `worktree_metadata::HUES` evenly spaced around the circle rather than any
//!   hue at all. "Almost the same colour" is the worst possible answer — it
//!   reads as *this is the other one*.
//! - **It fades out.** The gradient runs from the tint at the traffic lights
//!   to the plain title bar colour a third of the way across, so the title bar
//!   still reads as chrome rather than as a coloured band.

use gpui::{Background, Hsla, linear_color_stop, linear_gradient};
use worktree_metadata::hue_color;

/// How far across the title bar the tint has faded to nothing.
const FADE: f32 = 0.34;

/// How much of the tint is mixed into the title bar colour at its strongest.
/// Enough to tell two worktrees apart at a glance, little enough that the
/// title bar is still the title bar.
const STRENGTH: f32 = 0.30;

/// The title bar's background: the worktree's own colour fading into the
/// theme's, or just the theme's when there is no worktree to colour.
pub fn title_bar_background(hue: Option<u8>, title_bar: Hsla) -> Background {
    let Some(tint) = hue.map(hue_color) else {
        return title_bar.into();
    };
    linear_gradient(
        // Left to right, following the window rather than the text: the
        // strongest part of the tint sits where your eye already goes when a
        // window comes forward.
        90.,
        linear_color_stop(title_bar.blend(tint.opacity(STRENGTH)), 0.),
        linear_color_stop(title_bar, FADE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::hsla;
    use std::collections::HashSet;
    use std::path::Path;
    use worktree_metadata::{HUES, derived_hue};

    /// The case this exists for: several worktrees of one repository, with
    /// names that differ by a word. They may share a colour — there are
    /// sixteen — but none of them may *nearly* share one.
    #[test]
    fn no_two_worktrees_nearly_share_a_colour() {
        let worktrees = [
            "/repo/main",
            "/repo/fix-inefficient-job-log-insertion",
            "/repo/fix-inefficient-job-log-lookup",
            "/repo/feature-foo",
            "/repo/feature-bar",
            "/other/main",
        ];
        let hues: Vec<f32> = worktrees
            .iter()
            .map(|worktree| hue_color(derived_hue(Path::new(worktree))).h)
            .collect();

        for (index, hue) in hues.iter().enumerate() {
            for other in &hues[index + 1..] {
                // Around the circle, so 0.02 and 0.99 are close.
                let apart = (hue - other).abs().min(1. - (hue - other).abs());
                assert!(
                    apart == 0. || apart >= 1. / f32::from(HUES) - f32::EPSILON,
                    "{hue} and {other} are neither the same colour nor a different one"
                );
            }
        }
    }

    /// A sixteen-colour palette is only worth having if worktrees actually
    /// spread across it.
    #[test]
    fn worktrees_use_the_whole_palette() {
        let seen: HashSet<u8> = (0..200)
            .map(|index| derived_hue(Path::new(&format!("/repo/worktree-{index}"))))
            .collect();
        assert_eq!(
            seen.len(),
            usize::from(HUES),
            "two hundred worktrees should reach every colour"
        );
    }

    #[test]
    fn every_hue_is_a_hue() {
        for hue in 0..HUES {
            assert!((0. ..1.).contains(&hue_color(hue).h));
        }
    }

    /// Without a worktree — a window with nothing open — the title bar is the
    /// theme's own colour and nothing else.
    #[test]
    fn nothing_open_means_no_tint() {
        let title_bar = hsla(0.6, 0.1, 0.1, 1.);
        assert_eq!(
            title_bar_background(None, title_bar),
            Background::from(title_bar)
        );
    }
}
