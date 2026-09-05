//! Shared internals for the two front-ends built on the same capture-fed overlay:
//! `wlr-chooser` (the xdg-desktop-portal-wlr picker) and `wlr-switcher` (the
//! window switcher / Alt-Tab / exposé). Both bind the [`ui`] egui app to the
//! [`shell`] layer-shell host; the binaries differ only in their CLI and in what
//! they do with the picked source (print a token vs. focus the window).

pub mod chooser_cli;
mod i18n;
pub mod keys;
pub mod shell;
pub mod switcher_cli;
pub mod ui;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;
use wlr_capture::theme;

/// Parse a `COLSxROWS` grid spec (e.g. `4x3`).
pub fn parse_grid(s: &str) -> Result<(u32, u32), String> {
    let (c, r) = s
        .split_once(['x', 'X', '×'])
        .ok_or("expected COLSxROWS, e.g. 4x3")?;
    let n = |v: &str, what: &str| {
        v.trim()
            .parse::<u32>()
            .ok()
            .filter(|&n| n >= 1)
            .ok_or(format!("{what} must be a positive integer"))
    };
    Ok((n(c, "columns")?, n(r, "rows")?))
}

/// Parse `--scale`: a linear size multiplier for the overlay, `1.0` being the built-in
/// size. Bounded because the value ends up dividing the screen into points — small
/// enough and the UI is unreadable, large enough and a single tile no longer fits.
pub fn parse_scale(s: &str) -> Result<f32, String> {
    s.trim()
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite() && (0.25..=4.0).contains(v))
        .ok_or_else(|| "expected a number between 0.25 and 4.0, e.g. 1.5".to_string())
}

/// Acquire the single-instance advisory lock for the interactive switcher.
/// Returns the held lock file (keep it alive), or `None` if another instance owns
/// it — sway processes its own keybinding even over our exclusive keyboard grab,
/// so re-pressing the bind would otherwise stack overlays.
pub fn acquire_switch_lock() -> Option<std::fs::File> {
    use rustix::fs::{FlockOperation, flock};
    let dir = wlr_capture::paths::runtime_dir();
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join("wlr-switcher.lock"))
        .ok()?;
    flock(&f, FlockOperation::NonBlockingLockExclusive).ok()?;
    Some(f)
}

/// Spawn the capture thread, build the overlay for `opts`, run it to completion,
/// and return the picked source (if any). `t0` is the process start, for
/// cold-start timing (see [`shell::tlog`]).
pub fn run_overlay(opts: ui::Options, t0: Instant) -> anyhow::Result<Option<ui::Selection>> {
    // Start capturing first thing: the thread owns the non-Send Wayland client and
    // must connect, enumerate, and open sessions before any thumbnail appears.
    let (tx, rx) = mpsc::channel();
    // Raised by the UI when a dma-buf import fails; the capture thread then
    // reallocates in shm.
    let gpu_failed = Arc::new(AtomicBool::new(false));
    let flag = gpu_failed.clone();
    let scratchpad = opts.scratchpad;
    let mru = opts.focus.order.clone();
    let scale = opts.scale;
    std::thread::spawn(move || ui::capture_thread(tx, flag, scratchpad, mru, scale));
    shell::tlog(t0, "capture-thread spawned");

    let out: ui::Outcome = Arc::new(Mutex::new(None));
    let theme = theme::Theme::load();
    let app = ui::App::new(rx, out.clone(), opts, theme, gpu_failed);
    shell::tlog(t0, "ui ready, entering overlay");
    shell::run(app, t0)?;

    let sel = out.lock().unwrap().take();
    Ok(sel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_grid_reads_cols_and_rows() {
        assert_eq!(parse_grid("4x3"), Ok((4, 3)));
        assert_eq!(parse_grid("10X2"), Ok((10, 2)));
        assert_eq!(parse_grid("2×5"), Ok((2, 5)));
        // Surrounding whitespace on either factor is tolerated.
        assert_eq!(parse_grid(" 4 x 3 "), Ok((4, 3)));
    }

    #[test]
    fn parse_scale_takes_positive_factors_in_range() {
        assert_eq!(parse_scale("1"), Ok(1.0));
        assert_eq!(parse_scale(" 1.5 "), Ok(1.5));
        assert_eq!(parse_scale("0.25"), Ok(0.25));
        assert_eq!(parse_scale("4"), Ok(4.0));
    }

    #[test]
    fn parse_scale_rejects_out_of_range_and_nonsense() {
        assert!(parse_scale("0").is_err());
        assert!(parse_scale("-2").is_err());
        assert!(parse_scale("4.5").is_err());
        assert!(parse_scale("nan").is_err());
        assert!(parse_scale("big").is_err());
        assert!(parse_scale("").is_err());
    }

    #[test]
    fn parse_grid_rejects_bad_specs() {
        assert!(parse_grid("4").unwrap_err().contains("COLSxROWS"));
        assert!(parse_grid("0x3").unwrap_err().contains("columns"));
        assert!(parse_grid("4x0").unwrap_err().contains("rows"));
        assert!(parse_grid("axb").unwrap_err().contains("columns"));
        assert!(parse_grid("").is_err());
    }
}
