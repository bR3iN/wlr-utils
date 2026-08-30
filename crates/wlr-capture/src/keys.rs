//! Keyboard conventions shared by every wlr-utils overlay.
//!
//! Each tool runs its own windowing host and owns its own key handling, but the
//! *cancel* gesture is one convention across all of them, so it is defined here
//! once instead of being re-spelled per overlay.

use smithay_client_toolkit::seat::keyboard::Keysym;

/// True for the keystrokes that mean "cancel / back out one layer".
///
/// That is `Esc`, or the terminal-style `Ctrl+[` — the chord that has sent the
/// same byte (`0x1b`) since the ASCII days, and that touch typists reach without
/// leaving the home row. `ctrl` is the physical Control state at the time of the
/// press (xkb leaves the keysym as `bracketleft`; only the modifier tells the two
/// apart).
pub fn is_cancel(keysym: Keysym, ctrl: bool) -> bool {
    keysym == Keysym::Escape || (ctrl && keysym == Keysym::bracketleft)
}
