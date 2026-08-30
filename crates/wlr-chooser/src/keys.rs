//! Which keys cycle the highlighted window.
//!
//! The default is the Alt-Tab one everybody expects: `Tab` forward, `Shift+Tab`
//! back. Both directions then live on the *same* key and Shift picks between
//! them, so [`CycleKeys`] carries a key per direction and lets them coincide.
//!
//! `wlr-switcher --cycle-next/--cycle-prev` move either direction elsewhere; the
//! values are key names, parsed by egui itself (see [`parse`]).

use egui::Key;

/// Which way a keystroke moves the highlight.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    /// To the next window in the list.
    Next,
    /// To the previous one.
    Prev,
}

/// The keys bound to the two cycle directions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CycleKeys {
    /// Advances the highlight.
    pub next: Key,
    /// Retreats it — the same key as `next` in the default `Tab`/`Shift+Tab` setup.
    pub prev: Key,
}

impl Default for CycleKeys {
    fn default() -> Self {
        Self {
            next: Key::Tab,
            prev: Key::Tab,
        }
    }
}

impl CycleKeys {
    /// Which direction a keystroke cycles, if any. `shift` is the Shift state at
    /// the time of the press.
    ///
    /// With the default binding both directions sit on `Tab`, so Shift is what
    /// separates them. Once the two are bound to different keys, each key means
    /// what it says and Shift stops mattering — `Shift+<next>` still goes forward.
    pub fn dir(&self, key: Key, shift: bool) -> Option<Dir> {
        if key == self.prev && (shift || self.prev != self.next) {
            return Some(Dir::Prev);
        }
        (key == self.next).then_some(Dir::Next)
    }
}

/// Parse a key name, for the `--cycle-next` / `--cycle-prev` value parser.
///
/// egui's own parser, so it takes the variant names (`Tab`, `ArrowDown`, `Num1`)
/// as well as their aliases and bare characters (`Down`, `j`, `1`, `-`).
pub fn parse(name: &str) -> Result<Key, String> {
    // clap prints the offending value itself, so the message only has to say what
    // a good one looks like.
    Key::from_name(name.trim()).ok_or_else(|| "unknown key (try Tab, Down, j, F5)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_separates_the_directions_when_both_sit_on_tab() {
        let k = CycleKeys::default();
        assert_eq!(k.dir(Key::Tab, false), Some(Dir::Next));
        assert_eq!(k.dir(Key::Tab, true), Some(Dir::Prev));
        assert_eq!(k.dir(Key::J, false), None);
    }

    #[test]
    fn distinct_keys_mean_what_they_say() {
        let k = CycleKeys {
            next: Key::J,
            prev: Key::K,
        };
        assert_eq!(k.dir(Key::J, false), Some(Dir::Next));
        assert_eq!(k.dir(Key::J, true), Some(Dir::Next));
        assert_eq!(k.dir(Key::K, false), Some(Dir::Prev));
        assert_eq!(k.dir(Key::Tab, false), None);
    }

    #[test]
    fn key_names_include_egui_aliases_and_bare_characters() {
        assert_eq!(parse("Tab"), Ok(Key::Tab));
        assert_eq!(parse("ArrowDown"), Ok(Key::ArrowDown));
        assert_eq!(parse("Down"), Ok(Key::ArrowDown));
        assert_eq!(parse(" j "), Ok(Key::J));
        assert_eq!(parse("-"), Ok(Key::Minus));
        assert!(parse("nonsense").is_err());
    }
}
