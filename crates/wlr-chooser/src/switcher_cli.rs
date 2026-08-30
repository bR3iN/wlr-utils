//! wlr-switcher — window switcher / Alt-Tab / exposé for wlroots compositors.
//!
//! Picks a window from a live overlay and **focuses** it (via
//! `zwlr-foreign-toplevel-management-v1`). Bind it to a held modifier for a true
//! Alt-Tab: hold the modifier, `Tab`/`Shift+Tab` cycle, release to switch. Three
//! presentations via `--layout`; live previews are the differentiator.
//!
//! For the xdg-desktop-portal-wlr picker (prints to stdout), see `wlr-chooser`.

use crate::keys::{self, CycleKeys};
use crate::ui::{self, Live, Mode, Options, View};
use crate::{acquire_switch_lock, run_overlay};
use crate::{i18n, tr};
use clap::{Parser, ValueEnum};
use std::time::Instant;
use wlr_capture::wl;

/// Presentation of the switcher (CLI mirror of [`View`]).
#[derive(Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
enum LayoutArg {
    /// macOS-style single row of tiles (default).
    #[default]
    Strip,
    /// Full-screen mission-control exposé grid.
    Grid,
    /// Centred rofi-like card with tabs + search.
    Card,
}

/// Which tiles show a live preview (CLI mirror of [`Live`]).
#[derive(Clone, Copy, ValueEnum)]
enum LiveArg {
    None,
    Current,
    All,
}

impl From<LiveArg> for Live {
    fn from(v: LiveArg) -> Self {
        match v {
            LiveArg::None => Live::None,
            LiveArg::Current => Live::Current,
            LiveArg::All => Live::All,
        }
    }
}

/// Window switcher / Alt-Tab / exposé for wlroots: focuses the picked window.
#[derive(Parser)]
#[command(
    name = "wlr-switcher",
    version,
    about = "Window switcher / Alt-Tab / exposé for wlroots (focuses the picked window)"
)]
struct Cli {
    /// Capture through shared memory instead of the zero-copy dma-buf path.
    /// Use it if previews or captures come out broken on your driver; also
    /// settable with WLR_NO_GPU=1.
    #[arg(long)]
    no_gpu: bool,
    /// Presentation: `strip` (macOS-style row, default), `grid` (full-screen
    /// exposé) or `card` (centred rofi-like card).
    #[arg(long, value_enum, default_value_t = LayoutArg::Strip)]
    layout: LayoutArg,
    /// Live previews: `none` (icons only), `current` (only the highlighted window)
    /// or `all` (default). Live capture is the differentiator.
    #[arg(long, value_enum, default_value_t = LiveArg::All)]
    live: LiveArg,
    /// Hold-to-switch: confirm and close the moment the held launch modifier
    /// (Alt/Super) is released. Default: on for `strip`, off for `grid`/`card`.
    /// Bind it to a held modifier — e.g. `Mod1+Tab exec wlr-switcher` — for a
    /// true Alt-Tab. Use this to force it on for `grid`/`card`.
    #[arg(long)]
    hold: bool,
    /// Disable hold-to-switch: the overlay stays open after releasing the
    /// modifier — confirm with Enter or a click. Overrides the per-layout default.
    #[arg(long, conflicts_with = "hold")]
    no_hold: bool,
    /// Key that moves the highlight to the next window. Named the way egui names
    /// keys: `Tab`, `Down`, `j`, `F5`, `1`, `-`.
    #[arg(long, value_name = "KEY", value_parser = keys::parse, default_value = "Tab")]
    cycle_next: egui::Key,
    /// Key that moves it back. Left on the same key as `--cycle-next` — as in the
    /// `Tab` / `Shift+Tab` default — Shift picks the direction; bind the two to
    /// different keys and each means what it says.
    #[arg(long, value_name = "KEY", value_parser = keys::parse, default_value = "Tab")]
    cycle_prev: egui::Key,
    /// Include windows with no app-id (system surfaces)
    #[arg(long)]
    include_system: bool,
    /// Switch only between the windows in sway's scratchpad (sway-only; needs
    /// SWAYSOCK). Includes scratchpad windows that are currently shown. Opens on
    /// the first window rather than the second (no Alt-Tab-style initial advance).
    #[arg(long)]
    scratchpad: bool,
    /// Report which capture protocols the current compositor supports, then exit.
    #[arg(long)]
    doctor: bool,
}

/// What the switcher has to offer, resolved before the overlay is raised.
enum Candidates {
    /// `--scratchpad` was asked for but sway's IPC didn't answer (no `SWAYSOCK`, no
    /// `swaymsg`, not sway), so the filter can't be evaluated at all.
    Unavailable,
    /// No windows to switch between: do nothing rather than raise an overlay with no
    /// tiles in it.
    Empty,
    /// Exactly one window — nothing to choose *between*, so focus it directly.
    Sole(Box<ui::Selection>),
    /// Several windows: raise the overlay as usual.
    Choose,
}

/// Resolve what the switcher would actually put on screen. Independent of *why* the
/// list came out the size it did: `--scratchpad` decides what goes in, this decides
/// whether an overlay is worth raising over what came out.
///
/// Applies the same filters [`ui::App::visible`] would, so the count here matches the
/// tiles the user would have seen — `--scratchpad`, plus the app-id-less "system"
/// windows that stay hidden without `--include-system`. (Mode and the search box need
/// no handling: the switcher is always [`Mode::Windows`], and the filter starts empty.)
fn resolve_candidates(
    toplevels: &[wlr_capture::wl::Toplevel],
    scratchpad: bool,
    show_system: bool,
) -> Candidates {
    let windows = if scratchpad {
        match ui::scratchpad_windows(toplevels) {
            Some(w) => w,
            None => return Candidates::Unavailable,
        }
    } else {
        ui::ordered_windows(toplevels)
    };
    let windows: Vec<_> = windows
        .into_iter()
        .filter(|(w, _)| show_system || !w.app_id.is_empty())
        .collect();

    match windows.as_slice() {
        [] => Candidates::Empty,
        // The selection the overlay would have returned had it been shown, so the
        // caller runs its ordinary "picked something" path.
        [(w, dup_index)] => Candidates::Sole(Box::new(ui::Selection {
            token: format!("Window: {}", w.identifier),
            is_window: true,
            identifier: w.identifier.clone(),
            app_id: w.app_id.clone(),
            title: w.title.clone(),
            dup_index: *dup_index,
        })),
        _ => Candidates::Choose,
    }
}

pub fn main() {
    let t0 = Instant::now();
    let cli = Cli::parse();
    if cli.no_gpu {
        wlr_capture::wl::disable_gpu_globally();
    }
    i18n::init();

    if cli.doctor {
        if let Err(e) = wlr_capture::doctor::report("wlr-switcher", env!("CARGO_PKG_VERSION")) {
            eprintln!("wlr-switcher: {e}");
            std::process::exit(1);
        }
        return;
    }

    // Single-instance guard: re-pressing the keybind while we're up is a no-op
    // rather than a stacked overlay (sway runs its bindings over our grab).
    let _lock = match acquire_switch_lock() {
        Some(lock) => lock,
        None => return,
    };

    let view = match cli.layout {
        LayoutArg::Strip => View::Strip,
        LayoutArg::Grid => View::Grid,
        LayoutArg::Card => View::Card,
    };
    // Hold-to-switch defaults on for the strip (a true Alt-Tab) and off for the
    // exposé/card; --hold / --no-hold force either.
    let hold = if cli.hold {
        true
    } else if cli.no_hold {
        false
    } else {
        cli.layout == LayoutArg::Strip
    };
    let opts = Options {
        mode: Mode::Windows,
        show_system: cli.include_system,
        grid: None,
        view,
        hold,
        live: cli.live.into(),
        scratchpad: cli.scratchpad,
        cycle: CycleKeys {
            next: cli.cycle_next,
            prev: cli.cycle_prev,
        },
    };

    // Pre-flight: wlr-switcher switches *windows*, which need the foreign-toplevel
    // capture source (wlroots >= 0.20 / Sway >= 1.12). On older compositors connect()
    // now succeeds for screen-only capture, but there are no windows to offer — so say
    // so clearly and exit, instead of showing an empty dimmed overlay (issue #1).
    // The same client also settles whether an overlay is worth raising at all, off the
    // toplevels it has already enumerated — cheaper than a second connection on what is
    // a held-modifier hot path.
    let candidates = match wl::Client::connect() {
        Ok(client) if !client.can_capture_windows() => {
            eprintln!("{}", tr!("capture-no-window"));
            std::process::exit(2);
        }
        Ok(client) => resolve_candidates(client.toplevels(), cli.scratchpad, cli.include_system),
        Err(e) => {
            eprintln!("{}", tr!("error", error = format!("{e:#}")));
            std::process::exit(2);
        }
    };

    match candidates {
        Candidates::Unavailable => {
            eprintln!(
                "{}",
                tr!(
                    "error",
                    error = "--scratchpad needs sway's IPC (is SWAYSOCK set?)"
                )
            );
            std::process::exit(2);
        }
        // Nothing to switch to: a no-op, like losing the single-instance race.
        Candidates::Empty => return,
        // One window, so nothing to choose between: focus it, no overlay. Worth doing
        // even unfiltered — the sole window is not necessarily the focused one (it may
        // sit on another workspace), and a one-tile overlay tells the user nothing.
        Candidates::Sole(sel) => {
            if let Err(e) = wl::activate_window(&sel.app_id, &sel.title, sel.dup_index) {
                eprintln!("{}", tr!("error", error = format!("{e:#}")));
                std::process::exit(2);
            }
            return;
        }
        Candidates::Choose => {}
    }

    match run_overlay(opts, t0) {
        Ok(Some(sel)) => {
            // Focus the picked window (outputs aren't focusable, so ignore them).
            if sel.is_window
                && let Err(e) = wl::activate_window(&sel.app_id, &sel.title, sel.dup_index)
            {
                eprintln!("{}", tr!("error", error = format!("{e:#}")));
                std::process::exit(2);
            }
        }
        Ok(None) => std::process::exit(1), // cancelled
        Err(e) => {
            eprintln!("{}", tr!("error", error = format!("{e:#}")));
            std::process::exit(2);
        }
    }
}
