//! Focus-aware capture helpers: "the active window" and "the current output".
//!
//! Wayland deliberately gives a regular client no way to query the global pointer
//! position or which surface/output has the focus — so, like `grimshot`, we rely
//! on the compositor's own IPC. This is a small trait with per-compositor backends
//! selected from the environment: Sway, Hyprland (`hyprctl`) and niri (`niri msg`).
//!
//! Sway is spoken natively over `$SWAYSOCK` through [`swayipc`], which is both faster
//! than forking `swaymsg` (no process spawn on what is a held-modifier hot path) and
//! typed, so `scratchpad_state` and the focus arrays arrive as enums and integers
//! rather than as JSON to re-check by hand. The other two still fork their CLI.

use crate::wl::Region;
use std::collections::HashSet;
use swayipc::{Connection, Fallible, Node, NodeType, ScratchpadState};

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

/// Sway / wlroots backend, over sway's own IPC socket.
struct Sway;

impl Sway {
    /// One IPC round trip: connect to `$SWAYSOCK`, ask, hand back the typed reply.
    ///
    /// Failure is reported on stderr rather than swallowed. `swayipc` deserializes the
    /// whole reply into typed nodes and rejects a tree containing any enum variant it
    /// doesn't know, so a future sway that adds (say) a node type would fail *every*
    /// query here at once. Silently, that reads as the scratchpad filter and the focus
    /// order quietly going missing on a keybind, with nothing to point at; a line in
    /// sway's log names the cause.
    fn query<T>(what: &str, f: impl FnOnce(&mut Connection) -> Fallible<T>) -> Option<T> {
        // No `$SWAYSOCK`, no sway: quietly nothing to ask. Callers reach these helpers
        // on every compositor — the plain switcher asks for the focus order and the
        // scratchpad wherever it runs — so a missing socket is the ordinary case on
        // Hyprland or niri, not the failure the message below reports.
        std::env::var_os("SWAYSOCK")?;
        match Connection::new().and_then(|mut c| f(&mut c)) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("wlr-capture: sway IPC {what} failed: {e}");
                None
            }
        }
    }

    fn tree() -> Option<Node> {
        Self::query("get_tree", |c| c.get_tree())
    }
}

impl FocusBackend for Sway {
    fn name(&self) -> &'static str {
        "sway"
    }

    fn focused_output(&self) -> Option<String> {
        Self::query("get_outputs", |c| c.get_outputs())?
            .into_iter()
            .find(|o| o.focused)
            .map(|o| o.name)
    }

    fn active_window_rect(&self) -> Option<Region> {
        let tree = Self::tree()?;
        let node = find_focused(&tree)?;
        // Only windows have an app_id / window properties; a focused empty
        // workspace is not an "active window".
        let is_window = node.app_id.is_some()
            || node.window_properties.is_some()
            || (matches!(node.node_type, NodeType::Con | NodeType::FloatingCon)
                && node.name.is_some());
        if !is_window {
            return None;
        }
        Some(rect_of(node))
    }

    fn window_at(&self, x: i32, y: i32) -> Option<WindowRef> {
        sway_window_at(&Self::tree()?, x, y)
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
    Some(sway_scratchpad_identifiers(&Sway::tree()?))
}

/// The windows sway's scratchpad is *hiding*: the ones parked on its `__i3_scratch`
/// holding workspace, with nothing of them on screen anywhere.
///
/// Membership's other half. [`sway_scratchpad_ids`] answers "does the scratchpad own
/// this window", which stays true while the window is up on a workspace; this answers
/// "is the scratchpad holding it right now", which is what separates a window the user
/// has put away from one they are looking at. A window shown from the scratchpad has
/// been moved to a real workspace and is not in here — including when that workspace is
/// no longer the one on screen, since sway shows it again by moving it, the way it
/// treats any window on another workspace.
///
/// Sway-only (needs `SWAYSOCK`); `None` if the query fails or the reply won't parse.
pub fn sway_hidden_scratchpad_ids() -> Option<HashSet<String>> {
    Some(hidden_scratchpad_identifiers(&Sway::tree()?))
}

/// The scratchpad container currently *shown* on the workspace the user is on, as the
/// sway container id that addresses it (`[con_id=...]`).
///
/// This is what the hiding half of sway's `scratchpad show` toggle acts on: a container
/// the scratchpad still owns (`scratchpad_state != none`) that is up on a workspace
/// rather than parked on `__i3_scratch`. The id names the *flagged* node, which for
/// anything but a lone window is a container — so putting it back is the whole-container
/// move sway itself makes, not a per-window one.
///
/// Scoped to the focused workspace, i.e. the screen the user is looking at: a scratchpad
/// window shown on another output is left where it is.
///
/// Sway-only (needs `SWAYSOCK`). `None` when nothing from the scratchpad is on screen
/// there — and, indistinguishably, when sway's IPC didn't answer, which suits the one
/// caller: either way there is nothing to put away.
pub fn sway_shown_scratchpad() -> Option<i64> {
    shown_scratchpad_of(&Sway::tree()?)
}

/// Put a shown scratchpad container back into the scratchpad, by the container id
/// [`sway_shown_scratchpad`] reported.
///
/// `scratchpad show` is a toggle, so the command that raises a parked container hides a
/// displayed one: aimed at a container that is currently up, it means "hide it". The
/// criteria form acts on a container that does *not* hold the focus and leaves the focus
/// where it is, so nothing else about the session moves.
///
/// `true` when sway ran it. A container that vanished between the query and the command
/// matches nothing, which sway answers with an empty outcome list — that reads as
/// `false` here rather than as a silent success.
pub fn sway_hide_scratchpad(con_id: i64) -> bool {
    let Some(outcomes) = Sway::query("run_command", |c| {
        c.run_command(format!("[con_id={con_id}] scratchpad show"))
    }) else {
        return false;
    };
    let mut ran = !outcomes.is_empty();
    for outcome in outcomes {
        if let Err(e) = outcome {
            eprintln!("wlr-capture: sway scratchpad hide failed: {e}");
            ran = false;
        }
    }
    ran
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
    Some(sway_focus_order_of(&Sway::tree()?))
}

/// The tree-walking half of [`sway_focus_order`], split out so it's unit-testable
/// without a live compositor.
fn sway_focus_order_of(root: &Node) -> FocusOrder {
    let mut order = Vec::new();
    collect_focus_order(root, &mut order);
    FocusOrder {
        order,
        focused: find_focused(root)
            .filter(|n| sway_is_window(n))
            .and_then(window_id)
            .map(String::from),
    }
}

fn collect_focus_order(node: &Node, out: &mut Vec<String>) {
    if let Some(id) = window_id(node) {
        out.push(id.to_string());
    }
    // Rank of a child in this node's `focus` array; children it doesn't name sort to
    // the back. Sway lists tiled and floating children in the one array, so both child
    // arrays are ranked together — but the sort is stable, so anything unranked keeps
    // the tree's own order.
    let mut kids: Vec<&Node> = children(node).collect();
    kids.sort_by_key(|c| {
        node.focus
            .iter()
            .position(|id| *id == c.id)
            .unwrap_or(usize::MAX)
    });
    for child in kids {
        collect_focus_order(child, out);
    }
}

/// Collect the `foreign_toplevel_identifier` of every window in a sway `get_tree` that
/// sits in the scratchpad. Free function so it's unit-testable without a live
/// compositor.
fn sway_scratchpad_identifiers(root: &Node) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_scratchpad(root, false, &mut out);
    out
}

/// Membership is inherited, hence `inherited`: sway flags only the node that was moved
/// (`scratchpad_state != "none"`), so a *container* sent to the scratchpad is the only
/// flagged node while its children — the ones that actually carry a
/// `foreign_toplevel_identifier`, since containers never do — still read `"none"`.
/// Testing each node on its own would drop every window under such a container.
fn collect_scratchpad(node: &Node, inherited: bool, out: &mut HashSet<String>) {
    let in_scratchpad = inherited || in_scratchpad(node);
    if in_scratchpad && let Some(id) = window_id(node) {
        out.insert(id.to_string());
    }
    for child in children(node) {
        collect_scratchpad(child, in_scratchpad, out);
    }
}

/// The tree-walking half of [`sway_hidden_scratchpad_ids`], split out so it's
/// unit-testable without a live compositor.
fn hidden_scratchpad_identifiers(root: &Node) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Some(ws) = scratchpad_workspace(root) {
        collect_window_ids(ws, &mut out);
    }
    out
}

/// Sway's holding workspace for the scratchpad — `__i3_scratch`, the name i3 gave it.
/// Everything the scratchpad is hiding hangs off it, and nothing else does: showing a
/// window moves it out to a real workspace, putting it away moves it back.
fn scratchpad_workspace(node: &Node) -> Option<&Node> {
    if matches!(node.node_type, NodeType::Workspace) {
        return (node.name.as_deref() == Some("__i3_scratch")).then_some(node);
    }
    children(node).find_map(scratchpad_workspace)
}

/// Every window identifier in a subtree, whatever the containers in between.
fn collect_window_ids(node: &Node, out: &mut HashSet<String>) {
    if let Some(id) = window_id(node) {
        out.insert(id.to_string());
    }
    for child in children(node) {
        collect_window_ids(child, out);
    }
}

/// The tree-walking half of [`sway_shown_scratchpad`], split out so it's unit-testable
/// without a live compositor.
fn shown_scratchpad_of(root: &Node) -> Option<i64> {
    let ws = focused_workspace(root)?;
    // Never the scratchpad's own holding workspace: everything parked there is flagged
    // *and* hidden, so toggling one would raise a window rather than put one away. Sway
    // doesn't focus `__i3_scratch`, so this is insurance, not an expected case.
    if ws.name.as_deref() == Some("__i3_scratch") {
        return None;
    }
    let mut shown = Vec::new();
    collect_topmost_scratchpad(ws, &mut shown);
    shown.retain(|c| is_on_screen(c));
    // Most recently focused first, off the workspace's own focus array: a focused
    // scratchpad window is the one that goes back — as it would with sway's own binding
    // — and with several of them up, the pick is the one last used rather than whichever
    // the tree happens to list first.
    shown.sort_by_key(|c| {
        ws.focus
            .iter()
            .position(|id| *id == c.id)
            .unwrap_or(usize::MAX)
    });
    shown.first().map(|c| c.id)
}

/// Every node in `node`'s subtree the scratchpad owns, *outermost* first and never both
/// a node and its children: sending back the container is what sway does, and its
/// children are in the scratchpad only by inheritance.
///
/// Shown scratchpad containers are floating, so in practice they sit one level down —
/// but a window tiled back into the layout while the scratchpad still owns it does not,
/// hence the walk rather than a look at the workspace's own children.
fn collect_topmost_scratchpad<'a>(node: &'a Node, out: &mut Vec<&'a Node>) {
    for child in children(node) {
        if in_scratchpad(child) {
            out.push(child);
        } else {
            collect_topmost_scratchpad(child, out);
        }
    }
}

/// The workspace the focus is on: the workspace node whose subtree holds the focused
/// node. `None` when nothing in the tree has the focus.
fn focused_workspace(node: &Node) -> Option<&Node> {
    if matches!(node.node_type, NodeType::Workspace) {
        return find_focused(node).map(|_| node);
    }
    children(node).find_map(focused_workspace)
}

/// Whether the scratchpad owns this node. [`ScratchpadState`] is `#[non_exhaustive]`, so
/// this asks "anything but `None`" rather than matching the members — a variant a future
/// sway adds still counts as in.
fn in_scratchpad(node: &Node) -> bool {
    !matches!(node.scratchpad_state, None | Some(ScratchpadState::None))
}

/// Whether any window in this subtree is on screen right now. Sway sets `visible` on
/// views only, and only for those on a displayed workspace — a tabbed or stacked sibling
/// reads `false` — so "some descendant is visible" is what separates a scratchpad
/// container that is up from one parked on `__i3_scratch`.
fn is_on_screen(node: &Node) -> bool {
    node.visible == Some(true) || children(node).any(is_on_screen)
}

/// A node's `ext-foreign-toplevel-list-v1` identifier, if it is a window that has one.
/// Only views carry one — containers, workspaces and outputs never do.
fn window_id(node: &Node) -> Option<&str> {
    node.foreign_toplevel_identifier
        .as_deref()
        .filter(|id| !id.is_empty())
}

/// A node's children, tiled then floating.
fn children(node: &Node) -> impl Iterator<Item = &Node> {
    node.nodes.iter().chain(node.floating_nodes.iter())
}

/// Whether a sway node is a window (vs a container/workspace/output).
fn sway_is_window(node: &Node) -> bool {
    node.app_id.is_some() || node.window_properties.is_some()
}

/// A node's content rectangle in global logical coordinates: its `rect` shifted by the
/// `window_rect` (content offset within the node), so the crop lines up with what the
/// foreign-toplevel capture actually contains (no server-side borders).
fn sway_content_rect(node: &Node) -> Region {
    let rect = rect_of(node);
    let wr = &node.window_rect;
    // Sway sends a zeroed `window_rect` for nodes with no content offset (and typed
    // parsing turns an absent one into the same zeros), so a positive size is what
    // separates "there is a content rect" from "use the node's own".
    if wr.width > 0 && wr.height > 0 {
        return Region {
            x: rect.x + wr.x,
            y: rect.y + wr.y,
            w: wr.width as u32,
            h: wr.height as u32,
        };
    }
    rect
}

/// The deepest window node whose `rect` contains the global logical point `(x, y)`.
fn sway_window_at(node: &Node, x: i32, y: i32) -> Option<WindowRef> {
    // Skip anything not actually on screen — sway keeps the geometry of windows on
    // hidden workspaces (and tabbed/stacked-behind windows) in the tree, so without
    // this we'd match a window the point only "contains" on a workspace you can't see.
    if node.visible == Some(false) {
        return None;
    }
    // Descend into children first so the innermost (leaf) window wins. Floating before
    // tiled: a floating window sits above the tiling it overlaps.
    for child in node.floating_nodes.iter().chain(node.nodes.iter()) {
        if contains(&rect_of(child), x, y)
            && let Some(found) = sway_window_at(child, x, y)
        {
            return Some(found);
        }
    }
    if sway_is_window(node) && contains(&rect_of(node), x, y) {
        let app_id = node
            .app_id
            .as_deref()
            .or_else(|| {
                node.window_properties
                    .as_ref()
                    .and_then(|w| w.class.as_deref())
            })
            .unwrap_or_default()
            .to_string();
        return Some(WindowRef {
            app_id,
            title: node.name.clone().unwrap_or_default(),
            rect: sway_content_rect(node),
        });
    }
    None
}

/// Whether `(x, y)` falls inside a logical region.
fn contains(r: &Region, x: i32, y: i32) -> bool {
    x >= r.x && x < r.x + r.w as i32 && y >= r.y && y < r.y + r.h as i32
}

/// The single node with `focused: true` in a sway tree (the active container).
fn find_focused(node: &Node) -> Option<&Node> {
    if node.focused {
        return Some(node);
    }
    children(node).find_map(find_focused)
}

/// Read a sway `rect` into a logical [`Region`]. Sway never sends a negative size, so
/// the clamp only guards against a nonsense reply rather than an expected case.
fn rect_of(node: &Node) -> Region {
    Region {
        x: node.rect.x,
        y: node.rect.y,
        w: node.rect.width.max(0) as u32,
        h: node.rect.height.max(0) as u32,
    }
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
    use serde_json::{Value, json};

    /// Parse a trimmed `get_tree` fixture into a [`Node`].
    ///
    /// `Node` mirrors sway's reply in full, so deserializing one straight demands every
    /// field sway always sends — four rects, a border style, a layout — none of which
    /// any test here is about. This fills those in, recursively, so the fixtures below
    /// stay readable and carry only what they are testing.
    fn tree(mut v: Value) -> Node {
        fill(&mut v);
        serde_json::from_value(v).expect("fixture should be a valid sway node")
    }

    fn fill(v: &mut Value) {
        let zero = json!({"x": 0, "y": 0, "width": 0, "height": 0});
        let obj = v.as_object_mut().expect("a node is a JSON object");
        for (key, default) in [
            ("id", json!(0)),
            ("type", json!("con")),
            ("border", json!("none")),
            ("current_border_width", json!(0)),
            ("layout", json!("none")),
            ("orientation", json!("none")),
            ("percent", json!(null)),
            ("rect", zero.clone()),
            // Zero-sized, so `sway_content_rect` falls back to `rect` unless a fixture
            // asks otherwise — matching a sway node with no content offset.
            ("window_rect", zero.clone()),
            ("deco_rect", zero.clone()),
            ("geometry", zero),
            ("urgent", json!(false)),
            ("focused", json!(false)),
            ("focus", json!([])),
            ("sticky", json!(false)),
            ("nodes", json!([])),
            ("floating_nodes", json!([])),
        ] {
            obj.entry(key).or_insert(default);
        }
        for key in ["nodes", "floating_nodes"] {
            if let Some(kids) = obj.get_mut(key).and_then(|c| c.as_array_mut()) {
                kids.iter_mut().for_each(fill);
            }
        }
    }

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
      "type":"root","id":1,"focus":[3,2147483647],
      "nodes":[
        {
          "type":"output","id":2147483647,"name":"__i3","focus":[2147483646],
          "nodes":[{
            "type":"workspace","id":2147483646,"name":"__i3_scratch","visible":false,
            "focus":[10,11],
            "floating_nodes":[{
              "type":"floating_con","id":10,"app_id":"foot","name":"term",
              "scratchpad_state":"fresh","visible":false,
              "foreign_toplevel_identifier":"ext-toplevel-0x1a"
            },{
              "type":"floating_con","id":11,"layout":"tabbed",
              "scratchpad_state":"fresh","visible":false,
              "nodes":[{
                "type":"con","id":12,"app_id":"kitty","name":"~",
                "scratchpad_state":"none","visible":false,
                "foreign_toplevel_identifier":"ext-toplevel-0x4d"
              },{
                "type":"con","id":13,"app_id":"kitty","name":"~",
                "scratchpad_state":"none","visible":false,
                "foreign_toplevel_identifier":"ext-toplevel-0x5e"
              }]
            }]
          }]
        },
        {
          "type":"output","id":3,"name":"DP-5","focus":[20],
          "nodes":[{
            "type":"workspace","id":20,"name":"1","visible":true,"focus":[21,22],
            "nodes":[{
              "type":"con","id":21,"app_id":"firefox","name":"Page","focused":true,
              "scratchpad_state":"none","visible":true,
              "foreign_toplevel_identifier":"ext-toplevel-0x2b"
            }],
            "floating_nodes":[{
              "type":"floating_con","id":22,"app_id":"pavucontrol","name":"Volume",
              "scratchpad_state":"changed","visible":true,
              "foreign_toplevel_identifier":"ext-toplevel-0x3c"
            }]
          }]
        }
      ]
    }"#;

    #[test]
    fn sway_scratchpad_identifiers_collects_hidden_and_shown_scratchpad_windows() {
        let v = tree(serde_json::from_str(SWAY_SCRATCHPAD_TREE).unwrap());
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
        let v = tree(serde_json::from_str(SWAY_SCRATCHPAD_TREE).unwrap());
        let ids = sway_scratchpad_identifiers(&v);
        // Both windows of the tabbed container that was sent to the scratchpad, even
        // though only the container carries a non-`none` `scratchpad_state`.
        assert!(ids.contains("ext-toplevel-0x4d"));
        assert!(ids.contains("ext-toplevel-0x5e"));
    }

    // A scratchpad *container* — two windows, tabbed — currently shown on the focused
    // workspace: only the container carries the flag, and only the active tab reads
    // `visible`. This is the shape `[con_id=...] scratchpad show` has to be aimed at.
    const SWAY_SHOWN_CONTAINER_TREE: &str = r#"{
      "type":"root","id":1,"focus":[3],
      "nodes":[{
        "type":"output","id":3,"name":"DP-5","focus":[20],
        "nodes":[{
          "type":"workspace","id":20,"name":"1","focus":[30,21],
          "nodes":[{
            "type":"con","id":21,"app_id":"firefox","name":"Page",
            "scratchpad_state":"none","visible":true,
            "foreign_toplevel_identifier":"ext-toplevel-0x2b"
          }],
          "floating_nodes":[{
            "type":"floating_con","id":30,"layout":"tabbed",
            "scratchpad_state":"fresh",
            "nodes":[{
              "type":"con","id":31,"app_id":"kitty","name":"~","focused":true,
              "scratchpad_state":"none","visible":true,
              "foreign_toplevel_identifier":"ext-toplevel-0x4d"
            },{
              "type":"con","id":32,"app_id":"kitty","name":"~",
              "scratchpad_state":"none","visible":false,
              "foreign_toplevel_identifier":"ext-toplevel-0x5e"
            }]
          }]
        }]
      }]
    }"#;

    #[test]
    fn hidden_scratchpad_identifiers_leave_out_the_window_that_is_up() {
        let v = tree(serde_json::from_str(SWAY_SCRATCHPAD_TREE).unwrap());
        let ids = hidden_scratchpad_identifiers(&v);
        // Parked on `__i3_scratch`: the lone window and both windows of the container.
        assert!(ids.contains("ext-toplevel-0x1a"));
        assert!(ids.contains("ext-toplevel-0x4d"));
        assert!(ids.contains("ext-toplevel-0x5e"));
        // Shown from the scratchpad, so not hidden — even though the scratchpad still
        // owns it, which is what `sway_scratchpad_ids` reports.
        assert!(!ids.contains("ext-toplevel-0x3c"));
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn nothing_is_hidden_when_the_session_has_no_scratchpad_workspace() {
        let v = tree(serde_json::from_str(SWAY_FOCUS_TREE).unwrap());
        assert!(hidden_scratchpad_identifiers(&v).is_empty());
    }

    #[test]
    fn shown_scratchpad_is_the_one_up_on_the_focused_workspace() {
        let v = tree(serde_json::from_str(SWAY_SCRATCHPAD_TREE).unwrap());
        // The pavucontrol window shown from the scratchpad — not the tiled firefox that
        // holds the focus, and not the two containers parked on `__i3_scratch`.
        assert_eq!(shown_scratchpad_of(&v), Some(22));
    }

    #[test]
    fn shown_scratchpad_names_the_container_not_the_window_inside_it() {
        let v = tree(serde_json::from_str(SWAY_SHOWN_CONTAINER_TREE).unwrap());
        // The flagged container, which is what sway would put back; its focused child
        // (31) would move only that one window.
        assert_eq!(shown_scratchpad_of(&v), Some(30));
    }

    #[test]
    fn nothing_is_shown_when_the_scratchpad_holds_only_parked_windows() {
        let mut v: Value = serde_json::from_str(SWAY_SCRATCHPAD_TREE).unwrap();
        // Send the one shown window back: the focused workspace keeps just the tiled
        // firefox, and the scratchpad holds nothing that is on screen. The parked
        // containers on `__i3_scratch` must not be offered — toggling one shows it.
        v["nodes"][1]["nodes"][0]["floating_nodes"] = json!([]);
        assert_eq!(shown_scratchpad_of(&tree(v)), None);
    }

    #[test]
    fn nothing_is_shown_when_the_session_has_no_scratchpad_at_all() {
        let v = tree(serde_json::from_str(SWAY_FOCUS_TREE).unwrap());
        assert_eq!(shown_scratchpad_of(&v), None);
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
        let v = tree(serde_json::from_str(SWAY_FOCUS_TREE).unwrap());
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
        let v = tree(
            serde_json::from_str(&SWAY_FOCUS_TREE.replace("\"focused\":true", "\"focused\":false"))
                .unwrap(),
        );
        let f = sway_focus_order_of(&v);
        assert_eq!(f.focused, None);
        assert_eq!(f.order, ["ext-1", "ext-4", "ext-2", "ext-3"]);
    }

    #[test]
    fn sway_focus_order_collects_windows_under_a_scratchpad_container() {
        // Containers carry no `foreign_toplevel_identifier`, so the walk has to descend
        // through them — the same shape that broke the scratchpad filter.
        let v = tree(serde_json::from_str(SWAY_SCRATCHPAD_TREE).unwrap());
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
        let v = tree(serde_json::from_str(SWAY_TREE).unwrap());
        let f = sway_focus_order_of(&v);
        assert_eq!(f.focused, None);
    }

    #[test]
    fn sway_scratchpad_state_null_is_not_scratchpad_membership() {
        // Sway emits `"scratchpad_state": null` on outputs and workspaces — the key is
        // present, so a `!= "none"` test that doesn't check for a *string* would read
        // the root as in the scratchpad and, membership being inherited, drag every
        // window in with it.
        let v = tree(
            serde_json::from_str(
                r#"{"type":"root","scratchpad_state":null,
                    "nodes":[{"type":"con","app_id":"foot","name":"term",
                              "scratchpad_state":"none",
                              "foreign_toplevel_identifier":"ext-1"}]}"#,
            )
            .unwrap(),
        );
        assert!(sway_scratchpad_identifiers(&v).is_empty());
    }

    #[test]
    fn sway_scratchpad_identifiers_on_empty_scratchpad_is_empty() {
        // The tree used by the focus tests carries no scratchpad_state at all.
        let v = tree(serde_json::from_str(SWAY_TREE).unwrap());
        assert!(sway_scratchpad_identifiers(&v).is_empty());
    }

    #[test]
    fn sway_window_at_finds_visible_window_and_content_rect() {
        let v = tree(serde_json::from_str(SWAY_TREE).unwrap());
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
