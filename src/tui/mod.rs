//! TUI runtime. `run` is the only public surface; everything below is glue
//! between ratatui, the `App` state machine, and shutdown housekeeping.

mod app;
mod render;
pub(crate) mod theme;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::profile::AppConfig;

/// 80ms tick: spinner advances every frame per contract; responsive without burning CPU.
const TICK: Duration = Duration::from_millis(80);

/// Launch the full-screen TUI. Returns on quit (q/⎋/Ctrl+C) or fatal error.
/// `herdr_mode` is decided once by `cmd_tui` from `HERDR_ENV` and applied to
/// the constructed [`app::App`] via [`app::App::with_herdr_mode`]; nothing
/// else here reads the environment.
pub(crate) fn run(
    config: AppConfig,
    herdr_mode: bool,
    open_tab: Option<crate::profile::HomeTab>,
) -> Result<()> {
    // `try_init` owns raw mode + alt screen and installs a restore panic hook,
    // so a panic mid-draw no longer leaves the terminal corrupted.
    let mut terminal = ratatui::try_init().context("Failed to initialize the terminal")?;
    let outcome = run_loop(&mut terminal, config, herdr_mode, open_tab);
    ratatui::restore();
    outcome
}

fn run_loop(
    terminal: &mut DefaultTerminal,
    config: AppConfig,
    herdr_mode: bool,
    open_tab: Option<crate::profile::HomeTab>,
) -> Result<()> {
    // Palette (plan §4.5): resolve `palette` against the Omarchy theme files
    // before the first paint, then let the tick reload it live.
    let palette_setting = config.state.palette_setting();
    let palette = crate::profile::home_dir()
        .ok()
        .map(|home| (theme::init_palette(palette_setting, &home), home));
    let mut application = app::App::new(config)
        .with_herdr_mode(herdr_mode)
        .with_open_tab(open_tab)
        .with_guest_mode(crate::identity::upstream_active());
    if let Some((resolved, home)) = palette {
        if let Some(why) = resolved.error {
            application.toast(
                app::ToastKind::Warning,
                format!("omarchy theme colours unreadable, using catppuccin\n{why}"),
            );
        }
        application =
            application.with_palette_watch(theme::PaletteWatch::new(palette_setting, home));
    }
    // Non-blocking reconcile: fast path runs inline; verdict sequenced via
    // `StartupSignal`. Bootstrap is spawned from `on_tick` once reconcile
    // settles — neither blocks the first paint.
    app::reconcile_startup(&mut application);

    let mut last_tick = Instant::now();

    while !application.quit {
        if application.shutting_down.load(Ordering::SeqCst) {
            application.quit = true;
        }
        terminal.draw(|frame| render::draw(frame, &application))?;
        // Update compact state each frame so the transition toast fires as soon
        // as the terminal shrinks below 14 rows (or re-arms when it grows back).
        application.update_compact(terminal.size()?.height);

        let timeout = TICK.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app::handle_key(&mut application, key);
                }
                Event::Resize(_, _) => { /* redraw next iteration */ }
                _ => {}
            }
        }

        if last_tick.elapsed() >= TICK {
            app::on_tick(&mut application);
            last_tick = Instant::now();
        }
    }

    app::shutdown(&mut application)
}

// Fake-data TUI for README screenshots (test-only).
// Run: `cargo test showcase -- --ignored --nocapture`
#[cfg(test)]
#[path = "../../tests/inline/showcase.rs"]
mod showcase;

// Test-only: `HomeSandbox::drop` joins detached `spawn_worker` threads before it
// clears `HOME_OVERRIDE`, so a worker can never resolve the operator's real
// `$HOME` and lock under their `~/.tollgate`.
#[cfg(test)]
pub(crate) use app::join_test_workers;
