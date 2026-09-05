//! Focus-aware capture helpers: "the active window" and "the current output".
//!
//! Wayland deliberately gives a regular client no way to query the global pointer
//! position or which surface/output has the focus — so, like `grimshot`, we rely
//! on the compositor's own IPC. This is a small trait with per-compositor backends
//! selected from the environment: Sway (`swaymsg`), Hyprland (`hyprctl`) and niri
//! (`niri msg`).

use crate::wl::Region;
use std::collections::HashSet;

/// A window's identity + content geometry, for binding a region mirror to the window
/// under it (`app_id` + `title` match a `wl::Toplevel`; `rect` is its content area).
pub struct WindowRef {
    /// The window's application id (matches a [`wl::Toplevel`](crate::wl::Toplevel)).
    pub app_id: String,
    /// The window title (matches a [`wl::Toplevel`](crate::wl::Toplevel)).
    pub title: String,
    /// The window's content area in the global logical space.
    pub rect: Region,
}

/// A compositor-specific source of focus information.
pub trait FocusBackend {
    /// Name of the focused output, if any.
    fn focused_output(&self) -> Option<String>;
    /// Logical rectangle of the active (focused) window, if any.
    fn active_window_rect(&self) -> Option<Region>;
    /// The window under the given global logical point, if any. Used to make a region
    /// mirror follow the window beneath it. Default `None` (only Sway implements it).
    fn window_at(&self, _x: i32, _y: i32) -> Option<WindowRef> {
        None
    }
    /// Human-readable backend name, for error messages.
    fn name(&self) -> &'static str;
}

/// Pick a focus backend from the environment. `None` if no supported compositor
/// IPC is present (Wayland has no portable fallback — see the module docs).
pub fn detect() -> Option<Box<dyn FocusBackend>> {
    if std::env::var_os("SWAYSOCK").is_some() {
        return Some(Box::new(Sway));
    }
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return Some(Box::new(Hyprland));
    }
    if std::env::var_os("NIRI_SOCKET").is_some() {
        return Some(Box::new(Niri));
    }
    None
}

/// Sway / wlroots `swaymsg` backend.
struct Sway;

impl Sway {
    fn query(kind: &str) -> Option<serde_json::Value> {
        let out = std::process::Command::new("swaymsg")
            .args(["-t", kind, "-r"])
            .output()
            .ok()?;
        out.status.success().then_some(())?;
        serde_json::from_slice(&out.stdout).ok()
    }
}

impl FocusBackend for Sway {
    fn name(&self) -> &'static str {
        "sway"
    }

    fn focused_output(&self) -> Option<String> {
        let outputs = Self::query("get_outputs")?;
        outputs
            .as_array()?
            .iter()
            .find(|o| o["focused"].as_bool() == Some(true))?["name"]
            .as_str()
            .map(String::from)
    }

    fn active_window_rect(&self) -> Option<Region> {
        let tree = Self::query("get_tree")?;
        let node = find_focused(&tree)?;
        // Only windows have an app_id / window properties; a focused empty
        // workspace is not an "active window".
        let is_window = node.get("app_id").is_some_and(|a| !a.is_null())
            || node.get("window_properties").is_some()
            || (matches!(
                node.get("type").and_then(|t| t.as_str()),
                Some("con") | Some("floating_con")
            ) && node.get("name").is_some_and(|n| !n.is_null()));
        if !is_window {
            return None;
        }
        rect_of(node)
    }

    fn window_at(&self, x: i32, y: i32) -> Option<WindowRef> {
        sway_window_at(&Self::query("get_tree")?, x, y)
    }
}

/// The windows in sway's scratchpad, as `ext-foreign-toplevel-list-v1` identifiers.
///
/// Sway's `get_tree` reports `foreign_toplevel_identifier` per window, which is the
/// very string [`wl::Toplevel::identifier`](crate::wl::Toplevel::identifier) carries —
/// so callers filter a toplevel list by set membership, with none of the app-id/title
/// guesswork the rest of this module has to do.
///
/// Scratchpad membership, not visibility: a window *shown* from the scratchpad keeps a
/// non-`none` `scratchpad_state` until it is moved back to a workspace, so it is
/// included here too.
///
/// Membership is inherited: sending a whole container to the scratchpad puts every
/// window under it in here, even though sway flags only the container itself.
///
/// Sway-only (needs `SWAYSOCK`); `None` if the query fails or the reply won't parse.
pub fn sway_scratchpad_ids() -> Option<HashSet<String>> {
    Some(sway_scratchpad_identifiers(&Sway::query("get_tree")?))
}

/// Sway's window focus order: which windows exist, most-recently-focused first, and
/// which one holds the focus right now.
///
/// Both fields name windows by `foreign_toplevel_identifier` — the string
/// [`wl::Toplevel::identifier`](crate::wl::Toplevel::identifier) carries — so callers
/// order a toplevel list by lookup, with none of the app-id/title guesswork the rest of
/// this module has to do.
#[derive(Debug, Clone, Default)]
pub struct FocusOrder {
    /// Every window in the tree, most-recently-focused first. Empty if sway's IPC
    /// couldn't be reached.
    pub order: Vec<String>,
    /// The window that currently has the focus, if a *window* does: focus sitting on a
    /// layer surface or an empty workspace leaves this `None` while `order` still
    /// records which window was focused last.
    pub focused: Option<String>,
}

/// Read sway's focus order out of `get_tree`.
///
/// Sway hangs a `focus` array on every container — its children's ids, most recently
/// focused first — so descending the tree along those arrays enumerates the windows in
/// focus order. `focused` comes from the `focused: true` flag, which sits on a node only
/// when a window really has the focus; when it does, it is necessarily `order`'s first
/// entry, since the same focus arrays lead to it.
///
/// A one-shot snapshot, not a subscription: callers take it once at startup and keep it,
/// so the list can't reshuffle under the user mid-switch.
///
/// Sway-only (needs `SWAYSOCK`); `None` if the query fails or the reply won't parse.
pub fn sway_focus_order() -> Option<FocusOrder> {
    Some(sway_focus_order_of(&Sway::query("get_tree")?))
}

/// The tree-walking half of [`sway_focus_order`], split out so it's unit-testable
/// without a live compositor.
fn sway_focus_order_of(root: &serde_json::Value) -> FocusOrder {
    let mut order = Vec::new();
    collect_focus_order(root, &mut order);
    FocusOrder {
        order,
        focused: find_focused(root)
            .filter(|n| sway_is_window(n))
            .and_then(|n| n.get("foreign_toplevel_identifier"))
            .and_then(|i| i.as_str())
            .filter(|i| !i.is_empty())
            .map(String::from),
    }
}

fn collect_focus_order(node: &serde_json::Value, out: &mut Vec<String>) {
    if let Some(id) = node
        .get("foreign_toplevel_identifier")
        .and_then(|i| i.as_str())
        && !id.is_empty()
    {
        out.push(id.to_string());
    }
    // Rank of a child in this node's `focus` array; children it doesn't name sort to
    // the back. Sway lists tiled and floating children in the one array, so both child
    // arrays are ranked together — but the sort is stable, so anything unranked keeps
    // the tree's own order.
    let focus: Vec<i64> = node
        .get("focus")
        .and_then(|f| f.as_array())
        .map(|f| f.iter().filter_map(|i| i.as_i64()).collect())
        .unwrap_or_default();
    let mut children: Vec<&serde_json::Value> = ["nodes", "floating_nodes"]
        .iter()
        .filter_map(|k| node.get(*k))
        .filter_map(|c| c.as_array())
        .flatten()
        .collect();
    children.sort_by_key(|c| {
        c.get("id")
            .and_then(|i| i.as_i64())
            .and_then(|id| focus.iter().position(|f| *f == id))
            .unwrap_or(usize::MAX)
    });
    for child in children {
        collect_focus_order(child, out);
    }
}

/// Collect the `foreign_toplevel_identifier` of every window in a sway `get_tree` that
/// sits in the scratchpad. Free function so it's unit-testable without a live
/// compositor.
fn sway_scratchpad_identifiers(root: &serde_json::Value) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_scratchpad(root, false, &mut out);
    out
}

/// Membership is inherited, hence `in_scratchpad`: sway flags only the node that was
/// moved (`scratchpad_state != "none"`), so a *container* sent to the scratchpad is the
/// only flagged node while its children — the ones that actually carry a
/// `foreign_toplevel_identifier`, since containers never do — still read `"none"`.
/// Testing each node on its own would drop every window under such a container.
fn collect_scratchpad(node: &serde_json::Value, in_scratchpad: bool, out: &mut HashSet<String>) {
    let in_scratchpad = in_scratchpad
        || node
            .get("scratchpad_state")
            .and_then(|s| s.as_str())
            .is_some_and(|s| s != "none");
    if in_scratchpad
        && let Some(id) = node
            .get("foreign_toplevel_identifier")
            .and_then(|i| i.as_str())
        && !id.is_empty()
    {
        out.insert(id.to_string());
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node.get(key).and_then(|c| c.as_array()) {
            for child in children {
                collect_scratchpad(child, in_scratchpad, out);
            }
        }
    }
}

/// Whether a sway node is a window (vs a container/workspace/output).
fn sway_is_window(node: &serde_json::Value) -> bool {
    node.get("app_id").is_some_and(|a| !a.is_null()) || node.get("window_properties").is_some()
}

/// A node's content rectangle in global logical coordinates: its `rect` shifted by the
/// `window_rect` (content offset within the node), so the crop lines up with what the
/// foreign-toplevel capture actually contains (no server-side borders).
fn sway_content_rect(node: &serde_json::Value) -> Option<Region> {
    let rect = rect_of(node)?;
    if let Some(wr) = node.get("window_rect")
        && let (Some(w), Some(h)) = (wr["width"].as_u64(), wr["height"].as_u64())
        && w > 0
        && h > 0
    {
        return Some(Region {
            x: rect.x + wr["x"].as_i64().unwrap_or(0) as i32,
            y: rect.y + wr["y"].as_i64().unwrap_or(0) as i32,
            w: w as u32,
            h: h as u32,
        });
    }
    Some(rect)
}

/// The deepest window node whose `rect` contains the global logical point `(x, y)`.
fn sway_window_at(node: &serde_json::Value, x: i32, y: i32) -> Option<WindowRef> {
    // Skip anything not actually on screen — sway keeps the geometry of windows on
    // hidden workspaces (and tabbed/stacked-behind windows) in the tree, so without
    // this we'd match a window the point only "contains" on a workspace you can't see.
    if node.get("visible").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    // Descend into children first so the innermost (leaf) window wins.
    for key in ["floating_nodes", "nodes"] {
        if let Some(children) = node.get(key).and_then(|c| c.as_array()) {
            for child in children {
                if rect_of(child).is_some_and(|r| contains(&r, x, y))
                    && let Some(found) = sway_window_at(child, x, y)
                {
                    return Some(found);
                }
            }
        }
    }
    if sway_is_window(node) && rect_of(node).is_some_and(|r| contains(&r, x, y)) {
        let app_id = node["app_id"]
            .as_str()
            .or_else(|| node["window_properties"]["class"].as_str())
            .unwrap_or_default()
            .to_string();
        return Some(WindowRef {
            app_id,
            title: node["name"].as_str().unwrap_or_default().to_string(),
            rect: sway_content_rect(node)?,
        });
    }
    None
}

/// Whether `(x, y)` falls inside a logical region.
fn contains(r: &Region, x: i32, y: i32) -> bool {
    x >= r.x && x < r.x + r.w as i32 && y >= r.y && y < r.y + r.h as i32
}

/// The single node with `"focused": true` in a sway tree (the active container).
fn find_focused(node: &serde_json::Value) -> Option<&serde_json::Value> {
    if node.get("focused").and_then(|f| f.as_bool()) == Some(true) {
        return Some(node);
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node.get(key).and_then(|c| c.as_array()) {
            for child in children {
                if let Some(found) = find_focused(child) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// Read a sway `rect` object into a logical [`Region`].
fn rect_of(node: &serde_json::Value) -> Option<Region> {
    let r = node.get("rect")?;
    Some(Region {
        x: r["x"].as_i64()? as i32,
        y: r["y"].as_i64()? as i32,
        w: r["width"].as_u64()? as u32,
        h: r["height"].as_u64()? as u32,
    })
}

/// Hyprland `hyprctl -j` backend.
struct Hyprland;

impl Hyprland {
    fn query(cmd: &str) -> Option<serde_json::Value> {
        let out = std::process::Command::new("hyprctl")
            .args(["-j", cmd])
            .output()
            .ok()?;
        out.status.success().then_some(())?;
        serde_json::from_slice(&out.stdout).ok()
    }
}

impl FocusBackend for Hyprland {
    fn name(&self) -> &'static str {
        "Hyprland"
    }

    fn focused_output(&self) -> Option<String> {
        hypr_focused_output(&Self::query("monitors")?)
    }

    fn active_window_rect(&self) -> Option<Region> {
        hypr_active_window_rect(&Self::query("activewindow")?)
    }
}

/// Pick the focused monitor's name from `hyprctl -j monitors` (an array of monitors,
/// one with `"focused": true`).
fn hypr_focused_output(monitors: &serde_json::Value) -> Option<String> {
    monitors
        .as_array()?
        .iter()
        .find(|m| m["focused"].as_bool() == Some(true))?
        .get("name")?
        .as_str()
        .map(String::from)
}

/// Read the active window's rectangle from `hyprctl -j activewindow`: `at: [x, y]`
/// and `size: [w, h]` in global logical coordinates. An empty object (`{}`) — nothing
/// focused — yields `None`.
fn hypr_active_window_rect(w: &serde_json::Value) -> Option<Region> {
    let at = w.get("at")?.as_array()?;
    let size = w.get("size")?.as_array()?;
    Some(Region {
        x: at.first()?.as_i64()? as i32,
        y: at.get(1)?.as_i64()? as i32,
        w: size.first()?.as_i64()? as u32,
        h: size.get(1)?.as_i64()? as u32,
    })
}

/// niri `niri msg --json` backend.
struct Niri;

impl Niri {
    fn query(action: &str) -> Option<serde_json::Value> {
        let out = std::process::Command::new("niri")
            .args(["msg", "--json", action])
            .output()
            .ok()?;
        out.status.success().then_some(())?;
        serde_json::from_slice(&out.stdout).ok()
    }
}

impl FocusBackend for Niri {
    fn name(&self) -> &'static str {
        "niri"
    }

    fn focused_output(&self) -> Option<String> {
        niri_focused_output(&Self::query("focused-output")?)
    }

    fn active_window_rect(&self) -> Option<Region> {
        // niri's IPC does not expose a window's rectangle in global logical
        // coordinates (scrollable tiling lets windows extend off-screen), so the
        // active-window source is unavailable — callers get a clear error and can
        // use `--current-output` or `-g` instead.
        None
    }
}

/// Pick the focused output's name from `niri msg --json focused-output` (the Output
/// object, or `null` when none).
fn niri_focused_output(o: &serde_json::Value) -> Option<String> {
    o.get("name")?.as_str().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trimmed but faithful `hyprctl -j monitors` sample (two monitors, the second
    // focused) — locks the field names (`focused`, `name`) the parser relies on.
    const HYPR_MONITORS: &str = r#"[
        {"id":0,"name":"DP-1","make":"Dell","model":"X","width":2560,"height":1440,
         "x":0,"y":0,"refreshRate":59.95,"scale":1.0,"focused":false},
        {"id":1,"name":"HDMI-A-1","make":"LG","model":"Y","width":1920,"height":1080,
         "x":2560,"y":0,"refreshRate":60.0,"scale":1.0,"focused":true}
    ]"#;

    // `hyprctl -j activewindow` gives `at`/`size` pairs in global logical coords.
    const HYPR_ACTIVEWINDOW: &str =
        r#"{"address":"0x55","class":"foot","title":"foot","at":[120,340],"size":[800,600]}"#;

    // A trimmed sway `get_tree`: an output with a visible workspace (firefox, with a
    // 20px title-bar `window_rect`) and a *hidden* workspace whose window (vim) covers
    // the same coordinates — sway keeps its geometry even though it's off screen.
    const SWAY_TREE: &str = r#"{
      "type":"root","rect":{"x":0,"y":0,"width":3840,"height":1440},
      "nodes":[{
        "type":"output","name":"DP-4","rect":{"x":0,"y":0,"width":3840,"height":1440},
        "nodes":[
          {
            "type":"workspace","name":"1","visible":true,
            "rect":{"x":0,"y":0,"width":3840,"height":1440},
            "nodes":[{
              "type":"con","app_id":"firefox","name":"Page Title","visible":true,
              "rect":{"x":100,"y":100,"width":800,"height":600},
              "window_rect":{"x":0,"y":20,"width":800,"height":580}
            }]
          },
          {
            "type":"workspace","name":"2","visible":false,
            "rect":{"x":0,"y":0,"width":3840,"height":1440},
            "nodes":[{
              "type":"con","app_id":"vim","name":"editor","visible":false,
              "rect":{"x":100,"y":100,"width":800,"height":600}
            }]
          }
        ]
      }]
    }"#;

    // A trimmed sway `get_tree` covering the cases the scratchpad filter has to
    // separate: an ordinary tiled window, a *hidden* scratchpad window parked on the
    // `__i3_scratch` workspace, a scratchpad window currently *shown* on a real
    // workspace (still `scratchpad_state != "none"`, so still in the scratchpad), and a
    // whole *container* sent to the scratchpad — flagged on the container, which has no
    // `foreign_toplevel_identifier`, while its windows carry one but read `"none"`.
    const SWAY_SCRATCHPAD_TREE: &str = r#"{
      "type":"root",
      "nodes":[
        {
          "type":"output","name":"__i3",
          "nodes":[{
            "type":"workspace","name":"__i3_scratch","visible":false,
            "floating_nodes":[{
              "type":"floating_con","app_id":"foot","name":"term",
              "scratchpad_state":"fresh","visible":false,
              "foreign_toplevel_identifier":"ext-toplevel-0x1a"
            },{
              "type":"floating_con","layout":"tabbed",
              "scratchpad_state":"fresh","visible":false,
              "nodes":[{
                "type":"con","app_id":"kitty","name":"~",
                "scratchpad_state":"none","visible":false,
                "foreign_toplevel_identifier":"ext-toplevel-0x4d"
              },{
                "type":"con","app_id":"kitty","name":"~",
                "scratchpad_state":"none","visible":false,
                "foreign_toplevel_identifier":"ext-toplevel-0x5e"
              }]
            }]
          }]
        },
        {
          "type":"output","name":"DP-5",
          "nodes":[{
            "type":"workspace","name":"1","visible":true,
            "nodes":[{
              "type":"con","app_id":"firefox","name":"Page",
              "scratchpad_state":"none","visible":true,
              "foreign_toplevel_identifier":"ext-toplevel-0x2b"
            }],
            "floating_nodes":[{
              "type":"floating_con","app_id":"pavucontrol","name":"Volume",
              "scratchpad_state":"changed","visible":true,
              "foreign_toplevel_identifier":"ext-toplevel-0x3c"
            }]
          }]
        }
      ]
    }"#;

    #[test]
    fn sway_scratchpad_identifiers_collects_hidden_and_shown_scratchpad_windows() {
        let v: serde_json::Value = serde_json::from_str(SWAY_SCRATCHPAD_TREE).unwrap();
        let ids = sway_scratchpad_identifiers(&v);
        // The hidden one on __i3_scratch, and the one currently shown from the
        // scratchpad — both are "in the scratchpad".
        assert!(ids.contains("ext-toplevel-0x1a"));
        assert!(ids.contains("ext-toplevel-0x3c"));
        // The ordinary tiled window is not.
        assert!(!ids.contains("ext-toplevel-0x2b"));
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn sway_scratchpad_identifiers_collects_windows_under_a_scratchpad_container() {
        let v: serde_json::Value = serde_json::from_str(SWAY_SCRATCHPAD_TREE).unwrap();
        let ids = sway_scratchpad_identifiers(&v);
        // Both windows of the tabbed container that was sent to the scratchpad, even
        // though only the container carries a non-`none` `scratchpad_state`.
        assert!(ids.contains("ext-toplevel-0x4d"));
        assert!(ids.contains("ext-toplevel-0x5e"));
    }

    // A trimmed sway `get_tree` for the focus order: two workspaces on one output, with
    // sway's `focus` arrays disagreeing with the tree's own child order at every level,
    // so a walk that ignored them would come out differently. Workspace 2 is the most
    // recent, and within it the *second* child holds the focus.
    const SWAY_FOCUS_TREE: &str = r#"{
      "type":"root","id":1,"focus":[3],
      "nodes":[{
        "type":"output","id":3,"name":"DP-5","focus":[20,10],
        "nodes":[
          {
            "type":"workspace","id":10,"name":"1","focus":[12,11],
            "nodes":[
              {"type":"con","id":11,"app_id":"firefox","name":"Page","focus":[],
               "focused":false,"foreign_toplevel_identifier":"ext-3"},
              {"type":"con","id":12,"app_id":"foot","name":"term","focus":[],
               "focused":false,"foreign_toplevel_identifier":"ext-2"}
            ]
          },
          {
            "type":"workspace","id":20,"name":"2","focus":[22,21],
            "nodes":[
              {"type":"con","id":21,"app_id":"alacritty","name":"shell","focus":[],
               "focused":false,"foreign_toplevel_identifier":"ext-4"},
              {"type":"con","id":22,"app_id":"emacs","name":"main.rs","focus":[],
               "focused":true,"foreign_toplevel_identifier":"ext-1"}
            ]
          }
        ]
      }]
    }"#;

    #[test]
    fn sway_focus_order_lists_windows_most_recently_focused_first() {
        let v: serde_json::Value = serde_json::from_str(SWAY_FOCUS_TREE).unwrap();
        let f = sway_focus_order_of(&v);
        // Down the `focus` arrays: workspace 2 before workspace 1, and within each the
        // focus array's order, not the tree's.
        assert_eq!(f.order, ["ext-1", "ext-4", "ext-2", "ext-3"]);
        // A window holds the focus, and it necessarily leads the order.
        assert_eq!(f.focused.as_deref(), Some("ext-1"));
        assert_eq!(f.order.first().map(String::as_str), f.focused.as_deref());
    }

    #[test]
    fn sway_focus_order_reports_no_focused_window_when_none_is_focused() {
        // Focus on a layer surface or an empty workspace: no node carries
        // `focused: true`, but the focus arrays still rank the windows.
        let v: serde_json::Value =
            serde_json::from_str(&SWAY_FOCUS_TREE.replace("\"focused\":true", "\"focused\":false"))
                .unwrap();
        let f = sway_focus_order_of(&v);
        assert_eq!(f.focused, None);
        assert_eq!(f.order, ["ext-1", "ext-4", "ext-2", "ext-3"]);
    }

    #[test]
    fn sway_focus_order_collects_windows_under_a_scratchpad_container() {
        // Containers carry no `foreign_toplevel_identifier`, so the walk has to descend
        // through them — the same shape that broke the scratchpad filter.
        let v: serde_json::Value = serde_json::from_str(SWAY_SCRATCHPAD_TREE).unwrap();
        let f = sway_focus_order_of(&v);
        assert!(f.order.contains(&"ext-toplevel-0x4d".to_string()));
        assert!(f.order.contains(&"ext-toplevel-0x5e".to_string()));
        // Every window in the tree is listed, ranked or not.
        assert_eq!(f.order.len(), 5);
    }

    #[test]
    fn sway_focus_order_without_focus_arrays_keeps_tree_order() {
        // The focus tests' tree carries no `focus` arrays at all: nothing to rank by,
        // so the walk falls back to the order the tree lists windows in.
        let v: serde_json::Value = serde_json::from_str(SWAY_TREE).unwrap();
        let f = sway_focus_order_of(&v);
        assert_eq!(f.focused, None);
    }

    #[test]
    fn sway_scratchpad_state_null_is_not_scratchpad_membership() {
        // Sway emits `"scratchpad_state": null` on outputs and workspaces — the key is
        // present, so a `!= "none"` test that doesn't check for a *string* would read
        // the root as in the scratchpad and, membership being inherited, drag every
        // window in with it.
        let v: serde_json::Value = serde_json::from_str(
            r#"{"type":"root","scratchpad_state":null,
                "nodes":[{"type":"con","app_id":"foot","name":"term",
                          "scratchpad_state":"none",
                          "foreign_toplevel_identifier":"ext-1"}]}"#,
        )
        .unwrap();
        assert!(sway_scratchpad_identifiers(&v).is_empty());
    }

    #[test]
    fn sway_scratchpad_identifiers_on_empty_scratchpad_is_empty() {
        // The tree used by the focus tests carries no scratchpad_state at all.
        let v: serde_json::Value = serde_json::from_str(SWAY_TREE).unwrap();
        assert!(sway_scratchpad_identifiers(&v).is_empty());
    }

    #[test]
    fn sway_window_at_finds_visible_window_and_content_rect() {
        let v: serde_json::Value = serde_json::from_str(SWAY_TREE).unwrap();
        let w = sway_window_at(&v, 200, 200).expect("window under the point");
        // The visible window wins, never the one on the hidden workspace.
        assert_eq!(w.app_id, "firefox");
        assert_eq!(w.title, "Page Title");
        // Content rect = node rect shifted by the 20px title bar.
        assert_eq!(
            w.rect,
            Region {
                x: 100,
                y: 120,
                w: 800,
                h: 580
            }
        );
        // A point on the empty desktop hits no window.
        assert!(sway_window_at(&v, 2000, 1300).is_none());
    }

    #[test]
    fn hypr_focused_output_picks_focused_monitor() {
        let v: serde_json::Value = serde_json::from_str(HYPR_MONITORS).unwrap();
        assert_eq!(hypr_focused_output(&v).as_deref(), Some("HDMI-A-1"));
    }

    #[test]
    fn hypr_active_window_rect_reads_at_and_size() {
        let v: serde_json::Value = serde_json::from_str(HYPR_ACTIVEWINDOW).unwrap();
        assert_eq!(
            hypr_active_window_rect(&v),
            Some(Region {
                x: 120,
                y: 340,
                w: 800,
                h: 600
            })
        );
    }

    #[test]
    fn hypr_no_active_window_is_none() {
        // Hyprland returns `{}` when nothing is focused.
        let v: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert!(hypr_active_window_rect(&v).is_none());
    }

    #[test]
    fn niri_focused_output_reads_name() {
        // Shape per niri's `focused-output` (the Output object). Unverified live.
        let v: serde_json::Value = serde_json::from_str(
            r#"{"name":"eDP-1","make":"BOE","model":"Z",
                "logical":{"x":0,"y":0,"width":1920,"height":1080,"scale":1.0}}"#,
        )
        .unwrap();
        assert_eq!(niri_focused_output(&v).as_deref(), Some("eDP-1"));
        // `null` (no focused output) → None.
        assert!(niri_focused_output(&serde_json::Value::Null).is_none());
    }
}
