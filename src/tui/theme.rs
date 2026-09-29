//! Palette and shared style helpers.
//!
//! Two palettes: Catppuccin Mocha (the fallback, table kept verbatim) and the
//! running Omarchy theme (`colors.toml`, reloaded live). `palette = "auto"`
//! (the default) picks Omarchy when its `colors.toml` exists. Two capability
//! tiers select the color depth: `full` uses 24-bit RGB; `compatible` uses the
//! xterm-256 index.
//! Every color in the TUI comes from this module — raw `Color::Rgb` or raw index
//! values anywhere else are a bug.
//!
//! # Initialization
//!
//! Call [`init`] once before the TUI starts to seed the tier from the CLI flag
//! or config file. The Config tab can later [`set_tier`] live — the holder is an
//! atomic so a re-selection re-renders in the new palette on the next frame
//! without a process restart. Renders read it via the accessor fns below.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use ratatui::style::{Color, Modifier, Style};

// ── Tier ──────────────────────────────────────────────────────────────────────

/// Color-depth capability tier. `full` = 24-bit RGB; `compatible` = xterm-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// 24-bit truecolor. Requires `$COLORTERM=truecolor|24bit` or an explicit
    /// CLI / config override.
    Full,
    /// Nearest xterm-256 palette index. Safe on any xterm-compatible terminal.
    Compatible,
}

impl Tier {
    /// Stable atomic encoding. `0` doubles as "uninitialized" so the accessor
    /// can fall back to auto-detect before [`init`] runs.
    fn as_code(self) -> u8 {
        match self {
            Tier::Full => 1,
            Tier::Compatible => 2,
        }
    }

    fn from_code(code: u8) -> Option<Tier> {
        match code {
            1 => Some(Tier::Full),
            2 => Some(Tier::Compatible),
            _ => None,
        }
    }
}

/// Process-global tier as an atomic code (`0` = unset → auto-detect). Seeded by
/// [`init`] and swappable at runtime via [`set_tier`] for the live theme picker.
static TIER: AtomicU8 = AtomicU8::new(0);

/// Detect the tier from `$COLORTERM`:
/// `truecolor` or `24bit` → [`Tier::Full`]; anything else → [`Tier::Compatible`].
pub(crate) fn detect() -> Tier {
    match std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "truecolor" | "24bit" => Tier::Full,
        _ => Tier::Compatible,
    }
}

/// Seed the process tier at startup.
/// Precedence (highest first): explicit override → auto-detect.
pub(crate) fn init(override_tier: Option<Tier>) {
    set_tier(override_tier.unwrap_or_else(detect));
}

/// Swap the active tier at runtime. The next render reads the new value, so the
/// Config tab's theme selector applies immediately.
pub(crate) fn set_tier(tier: Tier) {
    TIER.store(tier.as_code(), Ordering::Relaxed);
}

/// Return the active tier. Falls back to auto-detect if [`init`] was not called.
#[inline]
pub(crate) fn tier() -> Tier {
    Tier::from_code(TIER.load(Ordering::Relaxed)).unwrap_or_else(detect)
}

/// Serializes tests that pin the tier. Every `tests/inline/*.rs` module compiles
/// into the one bin target, so under `cargo test` they run as threads sharing
/// this `TIER`. `testutil::TierSandbox` acquires it as an RAII guard.
#[cfg(test)]
pub(crate) static TIER_TEST_LOCK: crate::lockorder::RankedMutex<
    (),
    crate::lockorder::rank::TierTest,
> = crate::lockorder::RankedMutex::new(());

/// Read the stored pin, `None` for unset. [`tier`] collapses unset into a
/// detected tier, which a restore would then write back as a real pin.
#[cfg(test)]
pub(crate) fn tier_override() -> Option<Tier> {
    Tier::from_code(TIER.load(Ordering::Relaxed))
}

/// Put back a [`tier_override`] reading.
#[cfg(test)]
pub(crate) fn restore_tier(snapshot: Option<Tier>) {
    TIER.store(snapshot.map_or(0, Tier::as_code), Ordering::Relaxed);
}

// ── Palette tables ────────────────────────────────────────────────────────────
//
// Each role holds a [`Swatch`]: the 24-bit RGB value (`full` tier) and an
// xterm-256 index (`compatible` tier). The Catppuccin Mocha indices are hand
// picked and kept verbatim; an Omarchy palette computes its indices with
// [`nearest_xterm256`]. The accessor fns below read the INSTALLED palette
// (see [`install_palette`]), so render code never changes when the palette does.

/// One palette colour at both depths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Swatch {
    pub(crate) rgb: (u8, u8, u8),
    /// xterm-256 index for the `compatible` tier.
    pub(crate) idx: u8,
}

impl Swatch {
    const fn new(r: u8, g: u8, b: u8, idx: u8) -> Self {
        Self {
            rgb: (r, g, b),
            idx,
        }
    }

    /// A swatch whose 256-colour index is the nearest match to `rgb`.
    pub(crate) fn nearest(rgb: (u8, u8, u8)) -> Self {
        Self {
            rgb,
            idx: nearest_xterm256(rgb),
        }
    }

    const fn pack(self) -> u32 {
        (self.rgb.0 as u32) << 24
            | (self.rgb.1 as u32) << 16
            | (self.rgb.2 as u32) << 8
            | self.idx as u32
    }

    const fn unpack(v: u32) -> Self {
        Self::new((v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8)
    }
}

/// Every palette role, in [`Palette::swatches`] order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Bg,
    BgSunken,
    BgHover,
    Line,
    LineStrong,
    Text,
    TextDim,
    TextFaint,
    Accent,
    Accent2,
    Success,
    Warning,
    Danger,
    Info,
    BgDanger,
    BgWarning,
}

/// Number of [`Role`]s.
pub(crate) const ROLE_COUNT: usize = 16;

/// Which palette family a [`Palette`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteKind {
    Catppuccin,
    Omarchy,
}

/// A full palette: one [`Swatch`] per [`Role`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Palette {
    pub(crate) kind: PaletteKind,
    pub(crate) swatches: [Swatch; ROLE_COUNT],
}

impl Palette {
    /// The swatch for `role`.
    pub(crate) fn get(&self, role: Role) -> Swatch {
        self.swatches[role as usize]
    }
}

/// Catppuccin Mocha — the fallback palette, verbatim from the pre-palette
/// accessor table (RGB and hand-picked xterm-256 index per role).
pub(crate) const CATPPUCCIN: Palette = Palette {
    kind: PaletteKind::Catppuccin,
    swatches: [
        Swatch::new(30, 30, 46, 235),    // bg
        Swatch::new(17, 17, 27, 233),    // bg_sunken
        Swatch::new(40, 40, 56, 236),    // bg_hover
        Swatch::new(49, 50, 68, 238),    // line
        Swatch::new(69, 71, 90, 240),    // line_strong
        Swatch::new(205, 214, 244, 189), // text
        Swatch::new(166, 173, 200, 145), // text_dim
        Swatch::new(127, 132, 156, 102), // text_faint
        Swatch::new(67, 171, 229, 75),   // accent (sapphire)
        Swatch::new(217, 119, 87, 173),  // accent_2 (Claude orange)
        Swatch::new(166, 227, 161, 151), // success
        Swatch::new(249, 226, 175, 223), // warning
        Swatch::new(243, 139, 168, 211), // danger
        Swatch::new(116, 199, 236, 117), // info
        Swatch::new(75, 35, 44, 52),     // bg_danger
        Swatch::new(74, 60, 33, 58),     // bg_warning
    ],
};

/// The installed palette, one packed swatch per role; read only while
/// [`PALETTE_SET`] is true, so the zero-initialised slots never render.
/// Per-role atomics rather than a lock: an accessor runs per cell per frame,
/// and a torn read across roles during a live reload lasts one frame.
static SLOTS: [AtomicU32; ROLE_COUNT] = [const { AtomicU32::new(0) }; ROLE_COUNT];
static PALETTE_SET: AtomicBool = AtomicBool::new(false);
/// `0` = Catppuccin, `1` = Omarchy.
static PALETTE_KIND: AtomicU8 = AtomicU8::new(0);

/// Make `palette` the one every accessor reads from the next frame on.
pub(crate) fn install_palette(palette: &Palette) {
    for (slot, sw) in SLOTS.iter().zip(palette.swatches.iter()) {
        slot.store(sw.pack(), Ordering::Relaxed);
    }
    PALETTE_KIND.store(
        match palette.kind {
            PaletteKind::Catppuccin => 0,
            PaletteKind::Omarchy => 1,
        },
        Ordering::Relaxed,
    );
    PALETTE_SET.store(true, Ordering::Release);
}

/// The installed palette (Catppuccin until something is installed).
#[cfg(test)]
pub(crate) fn current_palette() -> Palette {
    if !PALETTE_SET.load(Ordering::Acquire) {
        return CATPPUCCIN;
    }
    let mut swatches = CATPPUCCIN.swatches;
    for (sw, slot) in swatches.iter_mut().zip(SLOTS.iter()) {
        *sw = Swatch::unpack(slot.load(Ordering::Relaxed));
    }
    Palette {
        kind: if PALETTE_KIND.load(Ordering::Relaxed) == 1 {
            PaletteKind::Omarchy
        } else {
            PaletteKind::Catppuccin
        },
        swatches,
    }
}

/// Test-only: the raw install state, so a test that installs a palette can put
/// the process back the way it found it.
#[cfg(test)]
pub(crate) fn palette_snapshot() -> Option<Palette> {
    PALETTE_SET.load(Ordering::Acquire).then(current_palette)
}

/// Test-only: restore a [`palette_snapshot`].
#[cfg(test)]
pub(crate) fn restore_palette(snapshot: Option<Palette>) {
    match snapshot {
        Some(p) => install_palette(&p),
        None => PALETTE_SET.store(false, Ordering::Release),
    }
}

/// The installed swatch for `role`.
#[inline]
pub(crate) fn swatch(role: Role) -> Swatch {
    if !PALETTE_SET.load(Ordering::Acquire) {
        return CATPPUCCIN.get(role);
    }
    Swatch::unpack(SLOTS[role as usize].load(Ordering::Relaxed))
}

/// `role`'s colour at the active tier.
#[inline]
fn pick(role: Role) -> Color {
    let sw = swatch(role);
    match tier() {
        Tier::Full => Color::Rgb(sw.rgb.0, sw.rgb.1, sw.rgb.2),
        Tier::Compatible => Color::Indexed(sw.idx),
    }
}

// ── Surfaces ──────────────────────────────────────────────────────────────────
#[inline]
pub(crate) fn bg() -> Color {
    pick(Role::Bg)
}
#[inline]
pub(crate) fn bg_sunken() -> Color {
    pick(Role::BgSunken)
}
#[inline]
pub(crate) fn bg_hover() -> Color {
    pick(Role::BgHover)
}

// ── Lines ─────────────────────────────────────────────────────────────────────
#[inline]
pub(crate) fn line_color() -> Color {
    pick(Role::Line)
}
#[inline]
pub(crate) fn line_strong_color() -> Color {
    pick(Role::LineStrong)
}

// ── Text ──────────────────────────────────────────────────────────────────────
#[inline]
pub(crate) fn text_color() -> Color {
    pick(Role::Text)
}
#[inline]
pub(crate) fn text_dim_color() -> Color {
    pick(Role::TextDim)
}
#[inline]
pub(crate) fn text_faint_color() -> Color {
    pick(Role::TextFaint)
}

// ── Accents ───────────────────────────────────────────────────────────────────
/// Primary accent — sapphire on Catppuccin, the theme's `accent` on Omarchy.
#[inline]
pub(crate) fn accent_color() -> Color {
    pick(Role::Accent)
}
/// Warm secondary — Claude orange on Catppuccin, the theme's `orange` on
/// Omarchy.
#[inline]
pub(crate) fn accent_2_color() -> Color {
    pick(Role::Accent2)
}

// ── Semantic ──────────────────────────────────────────────────────────────────
#[inline]
pub(crate) fn success_color() -> Color {
    pick(Role::Success)
}
#[inline]
pub(crate) fn warning_color() -> Color {
    pick(Role::Warning)
}
#[inline]
pub(crate) fn danger_color() -> Color {
    pick(Role::Danger)
}
#[inline]
pub(crate) fn info_color() -> Color {
    pick(Role::Info)
}

// ── Banner background tints ───────────────────────────────────────────────────
/// DANGER wash blended into BG — banner background for critical conditions.
#[inline]
pub(crate) fn bg_danger_color() -> Color {
    pick(Role::BgDanger)
}
/// WARNING wash blended into BG — muted warm-amber background for warning rows.
#[inline]
pub(crate) fn bg_warning_color() -> Color {
    pick(Role::BgWarning)
}

/// Per-channel RGB blend of `over` onto `beneath`, weighted by `alpha`
/// (the weight of `over`, clamped to `0.0..=1.0`).
/// Blends only on the full truecolor tier with both colors RGB-resolvable;
/// otherwise returns `over` unchanged.
pub(crate) fn blend_over(beneath: Color, over: Color, alpha: f64) -> Color {
    let (Color::Rgb(br, bg, bb), Color::Rgb(or, og, ob)) = (beneath, over) else {
        return over;
    };
    if tier() != Tier::Full {
        return over;
    }
    let a = alpha.clamp(0.0, 1.0);
    let mix = |o: u8, b: u8| -> u8 { (a * f64::from(o) + (1.0 - a) * f64::from(b)).round() as u8 };
    Color::Rgb(mix(or, br), mix(og, bg), mix(ob, bb))
}

// ── Toggle glyphs (tier-sensitive) ────────────────────────────────────────────

/// Toggle switch in the **on** state.
/// `full`: `─●`  `compatible`: `[on]`
pub(crate) fn toggle_on() -> &'static str {
    match tier() {
        Tier::Full => "─●",
        Tier::Compatible => "[on]",
    }
}

/// Toggle switch in the **off** state.
/// `full`: `○─`  `compatible`: `[off]`
pub(crate) fn toggle_off() -> &'static str {
    match tier() {
        Tier::Full => "○─",
        Tier::Compatible => "[off]",
    }
}

/// Gutter glyph for a row in edit mode — replaces the `❯` selection caret while
/// a text/stepper field is being typed into. Same on both tiers.
pub(crate) fn edit_glyph() -> &'static str {
    "✎"
}

/// Compact blocked-reason marker for a dead login credential — `AuthBroken`
/// and `KeyRejected` deliberately share it (the detail pill and the help-modal
/// legend carry which credential class failed). Same on both tiers.
pub(crate) fn dead_credential_glyph() -> &'static str {
    "×"
}

// ── Style helpers ─────────────────────────────────────────────────────────────

pub(crate) fn base() -> Style {
    Style::default().fg(text_color()).bg(bg())
}

/// Plain body text — foreground only.
pub(crate) fn body() -> Style {
    Style::default().fg(text_color())
}

/// Hairline chrome — tooltip `└ ` leaders and borders at `line_color()`.
pub(crate) fn line() -> Style {
    Style::default().fg(line_color())
}

/// Stronger line color — empty-gauge track and structural fills above `line_color()`.
pub(crate) fn line_strong() -> Style {
    Style::default().fg(line_strong_color())
}

pub(crate) fn dim() -> Style {
    Style::default().fg(text_dim_color())
}

pub(crate) fn faint() -> Style {
    Style::default().fg(text_faint_color())
}

/// Eyebrow label — bold + dim.
pub(crate) fn label() -> Style {
    Style::default()
        .fg(text_dim_color())
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn accent() -> Style {
    Style::default().fg(accent_color())
}

pub(crate) fn warning() -> Style {
    Style::default().fg(warning_color())
}

pub(crate) fn danger() -> Style {
    Style::default().fg(danger_color())
}

/// Background for the selected list row.
pub(crate) fn selected_row() -> Style {
    Style::default().bg(bg_hover())
}

/// Utilization color: dim <60%, warning 60–80%, danger >80%.
pub(crate) fn util_color(pct: f64) -> Color {
    let pct = pct.clamp(0.0, 100.0);
    if pct >= 80.0 {
        danger_color()
    } else if pct >= 60.0 {
        warning_color()
    } else {
        text_dim_color()
    }
}

/// `util_color` as a ready-to-use foreground style.
pub(crate) fn util(pct: f64) -> Style {
    Style::default().fg(util_color(pct))
}

/// Sapphire info accent; spinner color for refresh ops.
pub(crate) fn info() -> Style {
    Style::default().fg(info_color())
}

/// Catppuccin green — success tint; spinner color for auto-start.
pub(crate) fn success() -> Style {
    Style::default().fg(success_color())
}

// ── xterm-256 nearest match ───────────────────────────────────────────────────

/// Channel levels of the xterm 6×6×6 colour cube (indices 16–231).
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// RGB of xterm-256 index `idx` for the fixed part of the table (16–255).
/// The 16 system colours (0–15) are terminal-themed, so they never match.
pub(crate) fn xterm256_rgb(idx: u8) -> Option<(u8, u8, u8)> {
    match idx {
        0..=15 => None,
        16..=231 => {
            let i = idx - 16;
            Some((
                CUBE_LEVELS[usize::from(i / 36)],
                CUBE_LEVELS[usize::from((i / 6) % 6)],
                CUBE_LEVELS[usize::from(i % 6)],
            ))
        }
        232..=255 => {
            let v = 8 + (idx - 232) * 10;
            Some((v, v, v))
        }
    }
}

/// The xterm-256 index (16–255) nearest to `rgb` by squared RGB distance;
/// ties keep the lower index.
pub(crate) fn nearest_xterm256(rgb: (u8, u8, u8)) -> u8 {
    let dist = |(r, g, b): (u8, u8, u8)| -> u32 {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).unsigned_abs().pow(2);
        d(r, rgb.0) + d(g, rgb.1) + d(b, rgb.2)
    };
    (16..=255u8)
        .filter_map(|i| xterm256_rgb(i).map(|c| (i, dist(c))))
        .min_by_key(|&(i, d)| (d, i))
        .map_or(16, |(i, _)| i)
}

/// `over` blended onto `beneath` with `over` weighted `alpha` — the
/// tier-independent RGB mix the Omarchy mapping derives roles with.
pub(crate) fn mix_rgb(over: (u8, u8, u8), beneath: (u8, u8, u8), alpha: f64) -> (u8, u8, u8) {
    let a = alpha.clamp(0.0, 1.0);
    let m = |o: u8, b: u8| (a * f64::from(o) + (1.0 - a) * f64::from(b)).round() as u8;
    (
        m(over.0, beneath.0),
        m(over.1, beneath.1),
        m(over.2, beneath.2),
    )
}

// ── Omarchy palette ───────────────────────────────────────────────────────────

/// `palette` config key: which palette the TUI and the CLI render with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PaletteSetting {
    /// Omarchy when its `colors.toml` exists, else Catppuccin.
    #[default]
    Auto,
    /// The Omarchy theme; Catppuccin while no `colors.toml` can be read.
    Omarchy,
    /// Catppuccin Mocha, always.
    Catppuccin,
}

/// Where Omarchy keeps the running theme's colours, in lookup order, under
/// `home`.
pub(crate) fn omarchy_colors_candidates(home: &Path) -> [PathBuf; 2] {
    [
        home.join(".local/state/omarchy/current/theme/colors.toml"),
        home.join(".config/omarchy/current/theme/colors.toml"),
    ]
}

/// The first Omarchy `colors.toml` that exists under `home`.
pub(crate) fn omarchy_colors_path(home: &Path) -> Option<PathBuf> {
    omarchy_colors_candidates(home)
        .into_iter()
        .find(|p| p.is_file())
}

/// `#rrggbb` (the `#` optional) → RGB.
pub(crate) fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim();
    let h = h.strip_prefix('#').unwrap_or(h);
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let ch = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((ch(0)?, ch(2)?, ch(4)?))
}

/// Map an Omarchy `colors.toml` onto the palette roles:
///
/// | Omarchy | role |
/// |---|---|
/// | `accent` / `orange` | accent / accent_2 |
/// | `foreground` / `dark_foreground` / blend(fg, bg, .72) | text / text_faint / text_dim |
/// | `background` / `darker_background` / `lighter_background` | bg / bg_sunken / bg_hover |
/// | `muted` / `selection` | line / line_strong |
/// | `red` / `yellow` / `green` / `cyan` | danger / warning / success / info |
/// | red, yellow at 20 % over bg | bg_danger / bg_warning |
///
/// `accent`, `foreground`, `background`, `red`, `yellow`, `green` and `cyan`
/// are required; the rest fall back to blends of those (a theme without
/// `orange` uses `yellow`). Every 256-colour index is the nearest match.
pub(crate) fn parse_omarchy_colors(text: &str) -> Result<Palette, String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| {
        let msg = e.to_string();
        msg.lines().next().unwrap_or("invalid TOML").to_string()
    })?;
    let color = |key: &str| -> Result<Option<(u8, u8, u8)>, String> {
        match table.get(key) {
            None => Ok(None),
            Some(toml::Value::String(s)) => parse_hex(s)
                .map(Some)
                .ok_or_else(|| format!("`{key}` is not a #rrggbb colour")),
            Some(_) => Err(format!("`{key}` is not a string")),
        }
    };
    let required = |key: &str| -> Result<(u8, u8, u8), String> {
        color(key)?.ok_or_else(|| format!("missing `{key}`"))
    };
    let accent = required("accent")?;
    let fg = required("foreground")?;
    let bg = required("background")?;
    let red = required("red")?;
    let yellow = required("yellow")?;
    let green = required("green")?;
    let cyan = required("cyan")?;
    let orange = color("orange")?.unwrap_or(yellow);
    let dark_fg = color("dark_foreground")?.unwrap_or_else(|| mix_rgb(fg, bg, 0.5));
    let darker_bg = color("darker_background")?.unwrap_or_else(|| mix_rgb((0, 0, 0), bg, 0.4));
    let lighter_bg = color("lighter_background")?.unwrap_or_else(|| mix_rgb(fg, bg, 0.08));
    let muted = color("muted")?.unwrap_or_else(|| mix_rgb(fg, bg, 0.25));
    let selection = color("selection")?.unwrap_or(lighter_bg);
    let s = Swatch::nearest;
    Ok(Palette {
        kind: PaletteKind::Omarchy,
        swatches: [
            s(bg),
            s(darker_bg),
            s(lighter_bg),
            s(muted),
            s(selection),
            s(fg),
            s(mix_rgb(fg, bg, 0.72)),
            s(dark_fg),
            s(accent),
            s(orange),
            s(green),
            s(yellow),
            s(red),
            s(cyan),
            s(mix_rgb(red, bg, 0.2)),
            s(mix_rgb(yellow, bg, 0.2)),
        ],
    })
}

/// What [`resolve_palette`] settled on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedPalette {
    pub(crate) palette: Palette,
    /// The `colors.toml` read, when one was.
    pub(crate) source: Option<PathBuf>,
    /// Why the Omarchy file could not be used (the palette is then Catppuccin).
    pub(crate) error: Option<String>,
}

/// Resolve `setting` against the Omarchy files under `home`. Never fails: a
/// missing or malformed file yields Catppuccin and, when malformed, the reason.
pub(crate) fn resolve_palette(setting: PaletteSetting, home: &Path) -> ResolvedPalette {
    let catppuccin = |source, error| ResolvedPalette {
        palette: CATPPUCCIN,
        source,
        error,
    };
    if setting == PaletteSetting::Catppuccin {
        return catppuccin(None, None);
    }
    let Some(path) = omarchy_colors_path(home) else {
        return catppuccin(None, None);
    };
    match read_omarchy(&path) {
        Ok(palette) => ResolvedPalette {
            palette,
            source: Some(path),
            error: None,
        },
        Err(e) => catppuccin(Some(path), Some(e)),
    }
}

fn read_omarchy(path: &Path) -> Result<Palette, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    parse_omarchy_colors(&text)
}

/// Resolve and install `setting` for this process (`home` = the user's home).
/// Returns the resolution so a caller can report a malformed file.
pub(crate) fn init_palette(setting: PaletteSetting, home: &Path) -> ResolvedPalette {
    let resolved = resolve_palette(setting, home);
    install_palette(&resolved.palette);
    resolved
}

/// Identity of the Omarchy colour source as seen on disk: which candidate
/// exists, where its symlinks resolve, and its size and mtime. Any change
/// means the theme may have switched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaletteFingerprint {
    path: Option<PathBuf>,
    /// Canonical path — changes when `current/theme` is repointed.
    target: Option<PathBuf>,
    mtime: Option<std::time::SystemTime>,
    len: u64,
}

/// Fingerprint the Omarchy colour source under `home`.
pub(crate) fn palette_fingerprint(home: &Path) -> PaletteFingerprint {
    let path = omarchy_colors_path(home);
    let meta = path.as_deref().and_then(|p| std::fs::metadata(p).ok());
    PaletteFingerprint {
        target: path.as_deref().and_then(|p| std::fs::canonicalize(p).ok()),
        path,
        mtime: meta.as_ref().and_then(|m| m.modified().ok()),
        len: meta.map_or(0, |m| m.len()),
    }
}

/// Live-reload interval for the TUI (plan §4.5: 2 s).
pub(crate) const PALETTE_RELOAD_MS: u64 = 2_000;

/// The TUI's live palette reloader: re-reads the Omarchy colours when the
/// file's mtime, size or symlink target changes, keeps the last good palette
/// on a malformed file and reports that file once.
#[derive(Debug)]
pub(crate) struct PaletteWatch {
    setting: PaletteSetting,
    home: PathBuf,
    seen: PaletteFingerprint,
    /// The fingerprint whose malformed file was already reported.
    reported: Option<PaletteFingerprint>,
    last_check: Option<std::time::Instant>,
}

/// What one [`PaletteWatch::poll`] did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PaletteChange {
    /// Nothing changed on disk (or the setting ignores Omarchy).
    Unchanged,
    /// A new palette is installed.
    Reloaded(PaletteKind),
    /// The file changed but cannot be read; the last palette stays. Returned
    /// once per broken file state — the caller toasts it.
    Malformed(String),
}

impl PaletteWatch {
    /// Start watching after an [`init_palette`] of the same `setting`.
    pub(crate) fn new(setting: PaletteSetting, home: PathBuf) -> Self {
        let seen = palette_fingerprint(&home);
        Self {
            setting,
            home,
            seen,
            reported: None,
            last_check: None,
        }
    }

    /// [`Self::poll`], at most every [`PALETTE_RELOAD_MS`].
    pub(crate) fn tick(&mut self) -> PaletteChange {
        let now = std::time::Instant::now();
        if self.last_check.is_some_and(|t| {
            now.duration_since(t) < std::time::Duration::from_millis(PALETTE_RELOAD_MS)
        }) {
            return PaletteChange::Unchanged;
        }
        self.last_check = Some(now);
        self.poll()
    }

    /// Check now: reload when the fingerprint moved.
    pub(crate) fn poll(&mut self) -> PaletteChange {
        if self.setting == PaletteSetting::Catppuccin {
            return PaletteChange::Unchanged;
        }
        let fp = palette_fingerprint(&self.home);
        if fp == self.seen {
            return PaletteChange::Unchanged;
        }
        self.seen = fp.clone();
        let Some(path) = fp.path.clone() else {
            // The theme file went away: back to the fallback.
            install_palette(&CATPPUCCIN);
            self.reported = None;
            return PaletteChange::Reloaded(PaletteKind::Catppuccin);
        };
        match read_omarchy(&path) {
            Ok(p) => {
                install_palette(&p);
                self.reported = None;
                PaletteChange::Reloaded(PaletteKind::Omarchy)
            }
            Err(e) if self.reported.as_ref() != Some(&fp) => {
                self.reported = Some(fp);
                PaletteChange::Malformed(e)
            }
            Err(_) => PaletteChange::Unchanged,
        }
    }
}

// ── ANSI (CLI) ────────────────────────────────────────────────────────────────

/// The SGR sequence for a `color` foreground (`\x1b[38;2;r;g;bm` for RGB,
/// `\x1b[38;5;nm` for an index), bold when asked. Empty when there is
/// nothing to set.
pub(crate) fn ansi_style(color: Color, bold: bool) -> String {
    let mut codes: Vec<String> = Vec::new();
    if bold {
        codes.push("1".to_string());
    }
    match color {
        Color::Rgb(r, g, b) => codes.push(format!("38;2;{r};{g};{b}")),
        Color::Indexed(i) => codes.push(format!("38;5;{i}")),
        _ => {}
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

/// The SGR reset.
pub(crate) const ANSI_RESET: &str = "\x1b[0m";

// ── Card ink (usage cards) ────────────────────────────────────────────────────

/// The colour of a usage-card [`Ink`](crate::usage::cards::Ink) at the active
/// tier and palette. Severity: ok → success, mid → warning, high → accent_2,
/// critical → danger.
pub(crate) fn ink_color(ink: crate::usage::cards::Ink) -> Color {
    use crate::usage::cards::Ink;
    use crate::usage::derive::Severity;
    match ink {
        Ink::Text => text_color(),
        Ink::Dim => text_dim_color(),
        Ink::Faint => text_faint_color(),
        Ink::Accent => accent_color(),
        Ink::Active => accent_2_color(),
        Ink::Track => line_strong_color(),
        Ink::Warning => warning_color(),
        Ink::Danger => danger_color(),
        Ink::Sev(Severity::Ok) => success_color(),
        Ink::Sev(Severity::Mid) => warning_color(),
        Ink::Sev(Severity::High) => accent_2_color(),
        Ink::Sev(Severity::Critical) => danger_color(),
    }
}

#[cfg(test)]
#[path = "../../tests/inline/tui_theme.rs"]
mod tests;
