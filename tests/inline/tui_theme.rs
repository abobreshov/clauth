#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The palette engine: the Catppuccin table kept verbatim, the Omarchy
//! `colors.toml` mapping and its fallbacks, nearest-256 matching, `auto`
//! resolution with and without the file, and the live reload (mtime and
//! symlink changes, a malformed file keeping the last palette, reported once).

use super::*;

/// The Tokyo Night `colors.toml` Omarchy ships, verbatim in shape.
const TOKYO_NIGHT: &str = r##"mode = "dark"

accent = "#7aa2f7"
selection = "#292e42"
muted = "#414868"

background = "#1a1b26"
dark_background = "#13141c"
darker_background = "#0e0e14"
lighter_background = "#24283b"

foreground = "#a9b1d6"
dark_foreground = "#565f89"
light_foreground = "#b4bee6"
bright_foreground = "#c0caf5"

red = "#f7768e"
yellow = "#e0af68"
orange = "#eb927b"
green = "#9ece6a"
cyan = "#449dab"
blue = "#7aa2f7"
magenta = "#ad8ee6"
"##;

/// Only the required keys.
const MINIMAL: &str = r##"
accent = "#112233"
foreground = "#f0f0f0"
background = "#101010"
red = "#ff0000"
yellow = "#ffff00"
green = "#00ff00"
cyan = "#00ffff"
"##;

/// Holds the tier lock (every palette-pinning test shares it with
/// `TierSandbox`) and puts the process palette back on drop.
struct PaletteGuard {
    _tier: crate::testutil::TierSandbox,
    prev: Option<Palette>,
}

impl PaletteGuard {
    fn new(tier: Tier) -> Self {
        let tier = crate::testutil::TierSandbox::new(tier);
        Self {
            _tier: tier,
            prev: palette_snapshot(),
        }
    }
}

impl Drop for PaletteGuard {
    fn drop(&mut self) {
        restore_palette(self.prev);
    }
}

fn write_theme(home: &std::path::Path, rel: &str, text: &str) -> std::path::PathBuf {
    let path = home.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

const STATE_REL: &str = ".local/state/omarchy/current/theme/colors.toml";
const CONFIG_REL: &str = ".config/omarchy/current/theme/colors.toml";

#[test]
fn the_catppuccin_table_is_verbatim_at_both_tiers() {
    let _g = PaletteGuard::new(Tier::Full);
    restore_palette(None);
    assert_eq!(bg(), Color::Rgb(30, 30, 46));
    assert_eq!(bg_sunken(), Color::Rgb(17, 17, 27));
    assert_eq!(accent_color(), Color::Rgb(67, 171, 229));
    assert_eq!(accent_2_color(), Color::Rgb(217, 119, 87));
    assert_eq!(danger_color(), Color::Rgb(243, 139, 168));
    assert_eq!(bg_warning_color(), Color::Rgb(74, 60, 33));
    set_tier(Tier::Compatible);
    // The hand-picked indices, not recomputed ones.
    let idx: Vec<Color> = vec![
        bg(),
        bg_sunken(),
        bg_hover(),
        line_color(),
        line_strong_color(),
        text_color(),
        text_dim_color(),
        text_faint_color(),
        accent_color(),
        accent_2_color(),
        success_color(),
        warning_color(),
        danger_color(),
        info_color(),
        bg_danger_color(),
        bg_warning_color(),
    ];
    let want: Vec<Color> = [
        235, 233, 236, 238, 240, 189, 145, 102, 75, 173, 151, 223, 211, 117, 52, 58,
    ]
    .into_iter()
    .map(Color::Indexed)
    .collect();
    assert_eq!(idx, want);
    // Installing Catppuccin explicitly reads the same.
    install_palette(&CATPPUCCIN);
    assert_eq!(accent_color(), Color::Indexed(75));
}

#[test]
fn nearest_256_matches_the_cube_and_the_grey_ramp() {
    assert_eq!(xterm256_rgb(15), None, "system colours are terminal-themed");
    assert_eq!(xterm256_rgb(16), Some((0, 0, 0)));
    assert_eq!(xterm256_rgb(67), Some((95, 135, 175)));
    assert_eq!(xterm256_rgb(231), Some((255, 255, 255)));
    assert_eq!(xterm256_rgb(232), Some((8, 8, 8)));
    assert_eq!(xterm256_rgb(255), Some((238, 238, 238)));
    assert_eq!(nearest_xterm256((0, 0, 0)), 16);
    assert_eq!(nearest_xterm256((255, 255, 255)), 231);
    assert_eq!(nearest_xterm256((95, 135, 175)), 67);
    assert_eq!(nearest_xterm256((128, 128, 128)), 244);
    assert_eq!(nearest_xterm256((9, 9, 9)), 232);
    // Tokyo Night's accent lands on the nearest cube blue.
    assert_eq!(nearest_xterm256((0x7a, 0xa2, 0xf7)), 111);
    // Every answer is a real index whose colour is no farther than any other.
    for rgb in [(30, 30, 46), (217, 119, 87), (1, 200, 3)] {
        let got = nearest_xterm256(rgb);
        let d = |c: (u8, u8, u8)| {
            let s = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
            s(c.0, rgb.0) + s(c.1, rgb.1) + s(c.2, rgb.2)
        };
        let best = d(xterm256_rgb(got).unwrap());
        assert!((16..=255u8).all(|i| d(xterm256_rgb(i).unwrap()) >= best));
    }
}

#[test]
fn an_omarchy_theme_maps_onto_every_role() {
    let p = parse_omarchy_colors(TOKYO_NIGHT).unwrap();
    assert_eq!(p.kind, PaletteKind::Omarchy);
    let rgb = |r: Role| p.get(r).rgb;
    assert_eq!(rgb(Role::Accent), (0x7a, 0xa2, 0xf7));
    assert_eq!(rgb(Role::Accent2), (0xeb, 0x92, 0x7b), "orange → accent_2");
    assert_eq!(rgb(Role::Text), (0xa9, 0xb1, 0xd6));
    assert_eq!(rgb(Role::TextFaint), (0x56, 0x5f, 0x89), "dark_foreground");
    assert_eq!(
        rgb(Role::TextDim),
        mix_rgb((0xa9, 0xb1, 0xd6), (0x1a, 0x1b, 0x26), 0.72),
        "blend(fg, bg, .72)"
    );
    assert_eq!(rgb(Role::Bg), (0x1a, 0x1b, 0x26));
    assert_eq!(rgb(Role::BgSunken), (0x0e, 0x0e, 0x14), "darker_background");
    assert_eq!(rgb(Role::BgHover), (0x24, 0x28, 0x3b), "lighter_background");
    assert_eq!(rgb(Role::Line), (0x41, 0x48, 0x68), "muted");
    assert_eq!(rgb(Role::LineStrong), (0x29, 0x2e, 0x42), "selection");
    assert_eq!(rgb(Role::Danger), (0xf7, 0x76, 0x8e));
    assert_eq!(rgb(Role::Warning), (0xe0, 0xaf, 0x68));
    assert_eq!(rgb(Role::Success), (0x9e, 0xce, 0x6a));
    assert_eq!(rgb(Role::Info), (0x44, 0x9d, 0xab));
    assert_eq!(
        rgb(Role::BgDanger),
        mix_rgb((0xf7, 0x76, 0x8e), (0x1a, 0x1b, 0x26), 0.2)
    );
    assert_eq!(
        rgb(Role::BgWarning),
        mix_rgb((0xe0, 0xaf, 0x68), (0x1a, 0x1b, 0x26), 0.2)
    );
    // Every index is the computed nearest one.
    for sw in p.swatches {
        assert_eq!(sw.idx, nearest_xterm256(sw.rgb));
    }
}

#[test]
fn a_minimal_theme_falls_back_to_blends() {
    let p = parse_omarchy_colors(MINIMAL).unwrap();
    let (fg, bg) = ((0xf0, 0xf0, 0xf0), (0x10, 0x10, 0x10));
    assert_eq!(
        p.get(Role::Accent2).rgb,
        (0xff, 0xff, 0x00),
        "no orange → yellow"
    );
    assert_eq!(p.get(Role::TextFaint).rgb, mix_rgb(fg, bg, 0.5));
    assert_eq!(p.get(Role::BgHover).rgb, mix_rgb(fg, bg, 0.08));
    assert_eq!(p.get(Role::LineStrong).rgb, p.get(Role::BgHover).rgb);
    assert_eq!(p.get(Role::Line).rgb, mix_rgb(fg, bg, 0.25));
}

#[test]
fn a_malformed_theme_is_an_error_naming_the_problem() {
    assert!(parse_omarchy_colors("accent = ").is_err(), "invalid TOML");
    let missing = parse_omarchy_colors(&MINIMAL.replace("accent = \"#112233\"", "")).unwrap_err();
    assert_eq!(missing, "missing `accent`");
    let bad = parse_omarchy_colors(&MINIMAL.replace("#112233", "#12345")).unwrap_err();
    assert_eq!(bad, "`accent` is not a #rrggbb colour");
    let not_str = parse_omarchy_colors(&MINIMAL.replace("\"#ff0000\"", "3")).unwrap_err();
    assert_eq!(not_str, "`red` is not a string");
    assert_eq!(parse_hex("7aa2f7"), Some((0x7a, 0xa2, 0xf7)));
    assert_eq!(parse_hex("#GGGGGG"), None);
}

#[test]
fn auto_picks_omarchy_only_when_its_colours_exist() {
    let home = tempfile::tempdir().unwrap();
    let none = resolve_palette(PaletteSetting::Auto, home.path());
    assert_eq!(none.palette, CATPPUCCIN);
    assert_eq!((none.source, none.error), (None, None));
    // An explicit omarchy with no file is Catppuccin too, not an error.
    assert_eq!(
        resolve_palette(PaletteSetting::Omarchy, home.path()).palette,
        CATPPUCCIN
    );

    // The ~/.config location alone is found.
    let cfg = write_theme(home.path(), CONFIG_REL, MINIMAL);
    let r = resolve_palette(PaletteSetting::Auto, home.path());
    assert_eq!(r.palette.kind, PaletteKind::Omarchy);
    assert_eq!(r.source.as_deref(), Some(cfg.as_path()));

    // ~/.local/state wins over ~/.config.
    let state = write_theme(home.path(), STATE_REL, TOKYO_NIGHT);
    let r = resolve_palette(PaletteSetting::Auto, home.path());
    assert_eq!(r.source.as_deref(), Some(state.as_path()));
    assert_eq!(r.palette.get(Role::Accent).rgb, (0x7a, 0xa2, 0xf7));

    // `catppuccin` ignores the file.
    assert_eq!(
        resolve_palette(PaletteSetting::Catppuccin, home.path()).palette,
        CATPPUCCIN
    );

    // A malformed file: Catppuccin plus the reason.
    std::fs::write(&state, "accent = \"#zz\"").unwrap();
    let bad = resolve_palette(PaletteSetting::Auto, home.path());
    assert_eq!(bad.palette, CATPPUCCIN);
    assert!(bad.error.is_some());
}

#[test]
fn the_watch_reloads_on_change_keeps_the_last_palette_and_reports_once() {
    let _g = PaletteGuard::new(Tier::Full);
    let home = tempfile::tempdir().unwrap();
    let path = write_theme(home.path(), STATE_REL, TOKYO_NIGHT);
    init_palette(PaletteSetting::Auto, home.path());
    assert_eq!(accent_color(), Color::Rgb(0x7a, 0xa2, 0xf7));
    let mut watch = PaletteWatch::new(PaletteSetting::Auto, home.path().to_path_buf());
    assert_eq!(watch.poll(), PaletteChange::Unchanged);

    // A new theme (different size, so the change is seen whatever the mtime
    // granularity) repaints.
    std::fs::write(&path, MINIMAL).unwrap();
    assert_eq!(watch.poll(), PaletteChange::Reloaded(PaletteKind::Omarchy));
    assert_eq!(accent_color(), Color::Rgb(0x11, 0x22, 0x33));

    // Malformed: the last palette stays, reported once, then quiet.
    std::fs::write(&path, "accent = [").unwrap();
    assert!(matches!(watch.poll(), PaletteChange::Malformed(_)));
    assert_eq!(accent_color(), Color::Rgb(0x11, 0x22, 0x33));
    assert_eq!(watch.poll(), PaletteChange::Unchanged);

    // Fixed again: reloads.
    std::fs::write(&path, TOKYO_NIGHT).unwrap();
    assert_eq!(watch.poll(), PaletteChange::Reloaded(PaletteKind::Omarchy));
    assert_eq!(accent_color(), Color::Rgb(0x7a, 0xa2, 0xf7));

    // Gone: back to Catppuccin.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        watch.poll(),
        PaletteChange::Reloaded(PaletteKind::Catppuccin)
    );
    assert_eq!(accent_color(), Color::Rgb(67, 171, 229));
}

/// Omarchy switches themes by repointing `current/theme`; two themes can have
/// identical sizes and mtimes, so the resolved target is part of the check.
#[cfg(unix)]
#[test]
fn the_watch_reloads_when_the_theme_symlink_is_repointed() {
    let _g = PaletteGuard::new(Tier::Full);
    let home = tempfile::tempdir().unwrap();
    let themes = home.path().join("themes");
    let a = themes.join("a");
    let b = themes.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    // Same byte length, different accent.
    std::fs::write(a.join("colors.toml"), MINIMAL).unwrap();
    std::fs::write(b.join("colors.toml"), MINIMAL.replace("#112233", "#445566")).unwrap();
    let mtime = std::fs::metadata(a.join("colors.toml"))
        .unwrap()
        .modified()
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(b.join("colors.toml"))
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let current = home.path().join(".local/state/omarchy/current");
    std::fs::create_dir_all(&current).unwrap();
    let link = current.join("theme");
    std::os::unix::fs::symlink(&a, &link).unwrap();

    init_palette(PaletteSetting::Omarchy, home.path());
    assert_eq!(accent_color(), Color::Rgb(0x11, 0x22, 0x33));
    let mut watch = PaletteWatch::new(PaletteSetting::Omarchy, home.path().to_path_buf());

    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&b, &link).unwrap();
    assert_eq!(watch.poll(), PaletteChange::Reloaded(PaletteKind::Omarchy));
    assert_eq!(accent_color(), Color::Rgb(0x44, 0x55, 0x66));
}

#[test]
fn the_catppuccin_setting_never_watches() {
    let home = tempfile::tempdir().unwrap();
    let mut watch = PaletteWatch::new(PaletteSetting::Catppuccin, home.path().to_path_buf());
    write_theme(home.path(), STATE_REL, TOKYO_NIGHT);
    assert_eq!(watch.poll(), PaletteChange::Unchanged);
}

#[test]
fn ansi_sequences_follow_the_tier_encoding() {
    assert_eq!(ansi_style(Color::Rgb(1, 2, 3), true), "\x1b[1;38;2;1;2;3m");
    assert_eq!(ansi_style(Color::Indexed(75), false), "\x1b[38;5;75m");
    assert_eq!(ansi_style(Color::Reset, false), "");
}

#[test]
fn the_palette_key_round_trips_and_defaults_to_auto() {
    let default = crate::profile::AppState::default();
    let base = toml::to_string(&default).unwrap();
    let with = |value: &str| {
        toml::from_str::<crate::profile::AppState>(&format!("palette = \"{value}\"\n{base}"))
    };
    assert_eq!(
        with("omarchy").unwrap().palette_setting(),
        PaletteSetting::Omarchy
    );
    assert_eq!(
        with("catppuccin").unwrap().palette_setting(),
        PaletteSetting::Catppuccin
    );
    assert_eq!(
        with("auto").unwrap().palette_setting(),
        PaletteSetting::Auto
    );
    assert!(with("neon").is_err());
    let back = toml::to_string(&with("omarchy").unwrap()).unwrap();
    assert!(back.contains("palette = \"omarchy\""), "{back}");
    assert_eq!(default.palette_setting(), PaletteSetting::Auto);
    // Unset stays unwritten, so an untouched profiles.toml is unchanged.
    assert!(!toml::to_string(&default).unwrap().contains("palette"));
}
