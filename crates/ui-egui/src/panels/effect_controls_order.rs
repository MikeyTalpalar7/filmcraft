//! Effect Controls: the user's own order of an effect's parameters (say Field of View, Pan, Tilt
//! and Lens B together at the top of VR Reframe), kept per effect type in the UI state so every
//! instance shows it.
//!
//! Drag a parameter's name up or down to move it (a line shows where it lands), or right-click
//! it: Move to Top, Move Up, Move Down, Move to Bottom, Reset Order. Only the list's order
//! changes; the effect and its keyframes don't.
//!
//! Automation ids (in the menu): `effectControls.<effect>.<param>.moveTop`, `.moveUp`,
//! `.moveDown`, `.moveBottom`, `.resetOrder`.

use egui::{Pos2, Rect, Stroke, pos2};
use filmcraft_project::{EffectDef, ParamDef};

use crate::FilmcraftApp;

/// The order to list `def`'s parameters in: the user's order of the ones it names, then the rest
/// as the effect defines them.
pub(crate) fn ordered<'a>(app: &FilmcraftApp, def: &'a EffectDef) -> Vec<&'a ParamDef> {
    let ids: Vec<&str> = def.params.iter().map(|p| p.id).collect();
    let order = app.ui.param_order.get(def.id).map(|o| order_ids(o, &ids)).unwrap_or(ids);
    order.into_iter().filter_map(|id| def.params.iter().find(|p| p.id == id)).collect()
}

/// `saved` (names the effect may no longer have, repeats) made a full order of `ids`.
fn order_ids<'a>(saved: &[String], ids: &[&'a str]) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::with_capacity(ids.len());
    for s in saved {
        if let Some(id) = ids.iter().find(|id| **id == s.as_str())
            && !out.contains(id)
        {
            out.push(id);
        }
    }
    let rest: Vec<&str> = ids.iter().copied().filter(|id| !out.contains(id)).collect();
    out.extend(rest);
    out
}

/// The order with `pid` moved to position `to` (clamped; the position in the order without it).
fn moved(order: &[&str], pid: &str, to: usize) -> Vec<String> {
    let mut v: Vec<String> = order.iter().filter(|id| **id != pid).map(|s| s.to_string()).collect();
    if order.contains(&pid) {
        v.insert(to.min(v.len()), pid.to_string());
    }
    v
}

/// Move `pid` of the effect `def` to position `to`.
fn move_to(app: &mut FilmcraftApp, def: &EffectDef, pid: &str, to: usize) {
    let ids: Vec<&str> = ordered(app, def).iter().map(|p| p.id).collect();
    let next = moved(&ids, pid, to);
    app.ui.param_order.insert(def.id.to_string(), next);
}

/// Where `pid` is in the shown order.
fn position(app: &FilmcraftApp, def: &EffectDef, pid: &str) -> usize {
    ordered(app, def).iter().position(|p| p.id == pid).unwrap_or(0)
}

/// The right-click menu items of a parameter row.
pub(crate) fn menu(app: &mut FilmcraftApp, ui: &mut egui::Ui, def: &EffectDef, pid: &str) {
    let at = position(app, def, pid);
    let last = def.params.len().saturating_sub(1);
    let items: [(&str, &str, Option<usize>); 4] = [
        ("Move to Top", "moveTop", Some(0)),
        ("Move Up", "moveUp", at.checked_sub(1)),
        ("Move Down", "moveDown", (at < last).then_some(at + 1)),
        ("Move to Bottom", "moveBottom", Some(last)),
    ];
    for (label, key, to) in items {
        let r = ui.add_enabled(to.is_some_and(|t| t != at), egui::Button::new(crate::i18n::t(label)));
        app.auto.add(&format!("effectControls.{}.{pid}.{key}", def.id), r.rect, label);
        if r.clicked()
            && let Some(to) = to
        {
            move_to(app, def, pid, to);
            ui.close();
        }
    }
    ui.separator();
    let custom = app.ui.param_order.contains_key(def.id);
    let r = ui.add_enabled(custom, egui::Button::new(tl!("Reset Order")));
    app.auto.add(&format!("effectControls.{}.{pid}.resetOrder", def.id), r.rect, "Reset Order");
    if r.clicked() {
        app.ui.param_order.remove(def.id);
        ui.close();
    }
}

/// Dragging a parameter row: remembers the row (for the drop position), draws where it lands
/// while dragged, and moves it on release. `rows` are this effect's rows (parameter, rect) as
/// laid out last frame.
pub(crate) fn drag(app: &mut FilmcraftApp, ui: &egui::Ui, def: &EffectDef, pid: &str, row: &egui::Response, rows: &[(String, Rect)]) {
    if !(row.dragged() || row.drag_stopped()) {
        if row.hovered() && !rows.is_empty() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Default);
        }
        return;
    }
    let Some(p) = ui.ctx().pointer_latest_pos() else { return };
    let Some(to) = drop_index(rows, p) else { return };
    if row.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        let y = rows.get(to).map_or_else(|| rows.last().map_or(p.y, |(_, r)| r.max.y), |(_, r)| r.min.y);
        let x = rows.first().map_or((row.rect.min.x, row.rect.max.x), |(_, r)| (r.min.x, r.max.x));
        ui.painter().line_segment([pos2(x.0, y), pos2(x.1, y)], Stroke::new(2.0, app.tokens.accent));
    } else {
        // the slot is counted with the dragged row still in the list
        let from = rows.iter().position(|(id, _)| id == pid).unwrap_or(0);
        move_to(app, def, pid, if to > from { to - 1 } else { to });
    }
}

/// Remembers parameter row `pid` of effect `idx` on `clip` at `r` this frame, and returns the
/// effect's rows as laid out last frame (for [`drag`]).
pub(crate) fn rows(ui: &egui::Ui, clip: u64, idx: usize, pid: &str, r: Rect) -> Vec<(String, Rect)> {
    type Rows = (u64, Vec<(String, Rect)>, Vec<(String, Rect)>);
    let key = egui::Id::unique(("ec-param-rows", clip, idx));
    let frame = ui.ctx().cumulative_frame_nr();
    let (seen, mut prev, mut cur): Rows = ui.data(|d| d.get_temp(key)).unwrap_or_default();
    if seen != frame {
        prev = std::mem::take(&mut cur);
    }
    cur.retain(|(id, _)| id != pid);
    cur.push((pid.to_string(), r));
    ui.data_mut(|d| d.insert_temp(key, (frame, prev.clone(), cur)));
    prev
}

/// The slot (0 = before the first row, `rows.len()` = after the last) nearest the pointer.
fn drop_index(rows: &[(String, Rect)], p: Pos2) -> Option<usize> {
    if rows.is_empty() || !p.y.is_finite() {
        return None;
    }
    Some(rows.iter().position(|(_, r)| p.y < r.center().y).unwrap_or(rows.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_orders_survive_changed_effects() {
        let ids = ["view", "lens", "fov", "pan", "tilt", "lens2"];
        // the user's order first, then the rest; unknown names and repeats are dropped
        let saved: Vec<String> = ["fov", "pan", "gone", "tilt", "fov", "lens2"].iter().map(|s| s.to_string()).collect();
        assert_eq!(order_ids(&saved, &ids), ["fov", "pan", "tilt", "lens2", "view", "lens"]);
        assert_eq!(order_ids(&[], &ids), ids);
    }

    #[test]
    fn moving_keeps_every_parameter_once() {
        let order = ["a", "b", "c", "d"];
        assert_eq!(moved(&order, "d", 0), ["d", "a", "b", "c"]);
        assert_eq!(moved(&order, "a", 2), ["b", "c", "a", "d"]);
        assert_eq!(moved(&order, "b", 99), ["a", "c", "d", "b"]);
        assert_eq!(moved(&order, "zz", 0), ["a", "b", "c", "d"]);
    }

    #[test]
    fn drops_land_between_rows() {
        let rows: Vec<(String, Rect)> = (0..3).map(|i| (format!("p{i}"), Rect::from_min_size(pos2(0.0, i as f32 * 20.0), egui::vec2(100.0, 20.0)))).collect();
        assert_eq!(drop_index(&rows, pos2(5.0, -50.0)), Some(0));
        assert_eq!(drop_index(&rows, pos2(5.0, 25.0)), Some(1));
        assert_eq!(drop_index(&rows, pos2(5.0, 500.0)), Some(3));
        assert_eq!(drop_index(&rows, pos2(5.0, f32::NAN)), None);
        assert_eq!(drop_index(&[], pos2(0.0, 0.0)), None);
    }
}
