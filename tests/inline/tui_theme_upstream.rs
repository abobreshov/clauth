use ratatui::style::Color;

use crate::testutil::TierSandbox;

#[test]
fn full_tier_blends_two_rgb_colors() {
    let _tier = TierSandbox::new(super::Tier::Full);
    // 0.75 · (17,17,27) + 0.25 · (40,40,56) = (23,23,34)
    assert_eq!(
        super::blend_over(Color::Rgb(40, 40, 56), Color::Rgb(17, 17, 27), 0.75),
        Color::Rgb(23, 23, 34),
    );
}

#[test]
fn full_tier_returns_over_unchanged_when_either_side_is_not_rgb() {
    let _tier = TierSandbox::new(super::Tier::Full);
    // A non-RGB under-bg does not blend on the full tier; `over` survives.
    assert_eq!(
        super::blend_over(Color::Reset, Color::Rgb(17, 17, 27), 0.75),
        Color::Rgb(17, 17, 27),
    );
    assert_eq!(
        super::blend_over(Color::Rgb(40, 40, 56), Color::Reset, 0.75),
        Color::Reset,
    );
}

#[test]
fn compatible_tier_blends_in_rgb_and_snaps_to_the_nearest_index() {
    let _tier = TierSandbox::new(super::Tier::Compatible);
    super::restore_palette(None);
    // A painted BG_HOVER cell: 0.75·(17,17,27) + 0.25·(40,40,56) = (23,23,34),
    // nearest xterm-256 index 234.
    assert_eq!(
        super::blend_over(Color::Indexed(236), Color::Indexed(233), 0.75),
        Color::Indexed(234),
    );
    // An unknown/Reset under-bg counts as BG (30,30,46): 0.75·(17,17,27) +
    // 0.25·(30,30,46) = (20,20,32), nearest index 234.
    assert_eq!(
        super::blend_over(Color::Reset, Color::Indexed(233), 0.75),
        Color::Indexed(234),
    );
    // A painted BG cell blends to the same (20,20,32) → 234.
    assert_eq!(
        super::blend_over(Color::Indexed(235), Color::Indexed(233), 0.75),
        Color::Indexed(234),
    );
    // Two beneaths whose glass snaps away from 234, so a blend that dropped the
    // beneath would red here. A focused modal button (TEXT 189): 0.75·(17,17,27)
    // + 0.25·(205,214,244) = (64,66,81), nearest index 238. A focused border
    // (LINE_STRONG 240): 0.75·(17,17,27) + 0.25·(69,71,90) = (30,31,43),
    // nearest index 235.
    assert_eq!(
        super::blend_over(Color::Indexed(189), Color::Indexed(233), 0.75),
        Color::Indexed(238),
    );
    assert_eq!(
        super::blend_over(Color::Indexed(240), Color::Indexed(233), 0.75),
        Color::Indexed(235),
    );
}

#[test]
fn nearest_index_never_targets_a_terminal_defined_ansi_index() {
    // Indices 0..15 are terminal-defined, so the snap must land on 16..=255
    // even for an RGB exactly matching one of them.
    for (i, &(r, g, b)) in super::ANSI_RGB.iter().enumerate() {
        let snap = super::nearest_xterm256((r, g, b));
        assert!(snap >= 16, "ANSI index {i} ({r},{g},{b}) snapped to {snap}");
    }
}

#[test]
fn nearest_index_matches_an_independent_bruteforce_over_a_sampled_set() {
    // The exhaustive 16.7M-point equality against an independent search is too
    // slow in a debug test; it was checked out-of-band (0 diffs). This sampled
    // pin walks every 997th RGB against a reference that rebuilds the palette
    // from the xterm-256 formula instead of the module's tables.
    fn reference_palette(i: u8) -> (u8, u8, u8) {
        let cube = [0u8, 1, 2, 3, 4, 5].map(|n| if n == 0 { 0 } else { 55 + 40 * n });
        match i {
            16..=231 => {
                let n = i - 16;
                (
                    cube[usize::from(n / 36)],
                    cube[usize::from((n / 6) % 6)],
                    cube[usize::from(n % 6)],
                )
            }
            _ => {
                let v = 8 + (i - 232) * 10;
                (v, v, v)
            }
        }
    }
    let reference = |(r, g, b): (u8, u8, u8)| -> u8 {
        let mut best = 16u8;
        let mut best_d = u32::MAX;
        for i in 16..=u8::MAX {
            let (pr, pg, pb) = reference_palette(i);
            let dr = i32::from(r) - i32::from(pr);
            let dg = i32::from(g) - i32::from(pg);
            let db = i32::from(b) - i32::from(pb);
            let d = (dr * dr + dg * dg + db * db) as u32;
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best
    };
    let mut checked = 0usize;
    for v in (0..16_777_216usize).step_by(997) {
        let rgb = ((v >> 16) as u8, (v >> 8) as u8, v as u8);
        assert_eq!(super::nearest_xterm256(rgb), reference(rgb), "rgb {rgb:?}");
        checked += 1;
    }
    assert!(checked > 16_000, "sampled {checked} points");
}

#[test]
fn indexed_rgb_resolves_the_theme_tokens_to_their_rgb_column() {
    super::restore_palette(None);
    // The surface tokens the toast blend reads: an index stands in for the RGB
    // its accessor pairs with it, never the terminal's generic value for it.
    assert_eq!(super::indexed_rgb(233), (17, 17, 27));
    assert_eq!(super::indexed_rgb(235), (30, 30, 46));
    assert_eq!(super::indexed_rgb(236), (40, 40, 56));
}

#[test]
fn nearest_index_matches_the_grayscale_surfaces() {
    // Toast blends stay near the grayscale ramp, where squared distance and the
    // theme's nearest-index column agree.
    assert_eq!(super::nearest_xterm256((17, 17, 27)), 233);
    assert_eq!(super::nearest_xterm256((30, 30, 46)), 235);
    assert_eq!(super::nearest_xterm256((40, 40, 56)), 236);
    assert_eq!(super::nearest_xterm256((20, 20, 32)), 234);
    assert_eq!(super::nearest_xterm256((23, 23, 34)), 234);
}

/// Claude Code's working-line mark: six frames, eased by
/// `round((1 - cos(2πT/2000)) / 2 · 5)` over a 2 s period, T snapped down to
/// 64 ms. Expected frames worked by hand from that formula.
#[test]
fn the_active_claude_frame_follows_the_eased_two_second_cycle() {
    let frame = |ms| super::active_claude_frame(ms, false);
    // T=0: (1-1)/2*5 = 0.
    assert_eq!(frame(0), "·");
    // T=63 snaps to 0.
    assert_eq!(frame(63), "·");
    // T=250 snaps DOWN to 192: cos(2π·0.096) = 0.823 -> 0.44 -> 0. An unsnapped
    // or nearest-snapped T lands on index 1 instead, so this pins the direction.
    assert_eq!(frame(250), "·");
    // T=640: cos(2π·0.32) = -0.426 -> 3.56 -> 4.
    assert_eq!(frame(640), "✻");
    // T=1000 snaps to 960: cos(2π·0.48) = -0.992 -> 4.98 -> 5.
    assert_eq!(frame(1000), "✽");
    // T=1216: cos(2π·0.608) = -0.780 -> 4.45 -> 4, on the way back.
    assert_eq!(frame(1216), "✻");
    // T=448: cos(2π·0.224) = 0.163 -> 2.09 -> 2, the default terminals' ASCII frame.
    assert_eq!(frame(448), "*");
    // T=2000 wraps to the cycle start.
    assert_eq!(frame(2000), "·");
    // xterm-ghostty draws its own array: `✳` in place of `*`, `✻` twice at the end.
    assert_eq!(super::active_claude_frame(448, true), "✳");
    assert_eq!(super::active_claude_frame(1000, true), "✻");
}
