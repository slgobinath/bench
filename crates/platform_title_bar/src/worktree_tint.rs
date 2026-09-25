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
//! - **It is derived, never stored.** The hue is a hash of the worktree's
//!   path, so the same worktree is the same colour in every window, on every
//!   machine, forever, with nothing to configure and nothing to migrate. The
//!   hash is written out here rather than taken from `DefaultHasher`, whose
//!   algorithm is explicitly not guaranteed between Rust releases — a
//!   toolchain bump must not repaint every worktree.
//! - **No two worktrees look *almost* alike.** The hue is one of
//!   [`HUES`] evenly spaced around the circle rather than any hue at all.
//!   Free-running hashes put two of five worktrees within a few degrees of
//!   each other about half the time, and "almost the same colour" is the worst
//!   possible answer — it reads as *this is the other one*. Two worktrees can
//!   now share a colour, which says nothing, but none can nearly share one.
//! - **It fades out.** The gradient runs from the tint at the traffic lights
//!   to the plain title bar colour a third of the way across, so the title bar
//!   still reads as chrome rather than as a coloured band.

use gpui::{Background, Hsla, hsla, linear_color_stop, linear_gradient};
use std::path::Path;

/// How far across the title bar the tint has faded to nothing.
const FADE: f32 = 0.34;

/// How much of the tint is mixed into the title bar colour at its strongest.
/// Enough to tell two worktrees apart at a glance, little enough that the
/// title bar is still the title bar.
const STRENGTH: f32 = 0.30;

/// How many colours there are.
///
/// Sixteen is far enough apart to tell any two apart at a glance — 22.5° of
/// hue, at this saturation — and enough of them that a handful of worktrees
/// usually get a colour each. More would be harder to tell apart; fewer would
/// collide too often to mean anything.
const HUES: u64 = 16;

/// The saturation and lightness every worktree's tint shares, so that hue is
/// the only thing that varies and no worktree gets a muddier colour than
/// another. Mid-lightness, because this is blended into a title bar that is
/// nearly black in one theme and nearly white in the other.
const SATURATION: f32 = 0.72;
const LIGHTNESS: f32 = 0.58;

/// The title bar's background: the worktree's own colour fading into the
/// theme's, or just the theme's when there is no worktree to colour.
pub fn title_bar_background(worktree: Option<&Path>, title_bar: Hsla) -> Background {
    let Some(tint) = worktree.map(tint_for) else {
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

/// The colour of one worktree, from its path.
pub fn tint_for(worktree: &Path) -> Hsla {
    hsla(hue_of(worktree), SATURATION, LIGHTNESS, 1.)
}

/// One of [`HUES`] hues for a path.
///
/// FNV-1a over the path's bytes, finished with splitmix64's avalanche. FNV on
/// its own barely moves when the last few bytes change — which is exactly the
/// case here, `fix-login` against `fix-logout` — and the avalanche is what
/// turns a one-character difference into an unrelated number. Both are fixed
/// arithmetic, so the colour is the same next year as it is today.
fn hue_of(worktree: &Path) -> f32 {
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

    (hash % HUES) as f32 / HUES as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_worktree_keeps_its_colour() {
        assert_eq!(
            tint_for(Path::new("/repo/fix-login")),
            tint_for(Path::new("/repo/fix-login")),
            "the colour is derived, so it survives restarts and machines"
        );
        assert_ne!(
            tint_for(Path::new("/repo/fix-login")),
            tint_for(Path::new("/repo/fix-logout"))
        );
    }

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
            .map(|worktree| hue_of(Path::new(worktree)))
            .collect();

        for (index, hue) in hues.iter().enumerate() {
            for other in &hues[index + 1..] {
                // Around the circle, so 0.02 and 0.99 are close.
                let apart = (hue - other).abs().min(1. - (hue - other).abs());
                assert!(
                    apart == 0. || apart >= 1. / HUES as f32 - f32::EPSILON,
                    "{hue} and {other} are neither the same colour nor a different one"
                );
            }
        }
    }

    /// A sixteen-colour palette is only worth having if worktrees actually
    /// spread across it.
    #[test]
    fn worktrees_use_the_whole_palette() {
        let mut seen = HashSet::new();
        for index in 0..200 {
            let hue = hue_of(Path::new(&format!("/repo/worktree-{index}")));
            seen.insert((hue * HUES as f32).round() as u64);
        }
        assert_eq!(
            seen.len(),
            HUES as usize,
            "two hundred worktrees should reach every colour"
        );
    }

    #[test]
    fn every_hue_is_a_hue() {
        for index in 0..500 {
            let hue = hue_of(Path::new(&format!("/repo/worktree-{index}")));
            assert!((0. ..1.).contains(&hue), "{hue} is not a hue");
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
