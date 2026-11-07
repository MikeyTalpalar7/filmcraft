//! Aiming VR Reframe on the Program monitor: drag the picture to pan and tilt the view (with
//! ⌥/Alt, to roll it), scroll to change its field of view (the Planet Field of View in the Little
//! Planet and Rabbit Hole views).
//!
//! It acts on the selected effect when that is VR Reframe, and with no effect selected on the
//! topmost clip under the playhead that has one. The values go through `effects.setParam`, so a
//! parameter with keyframes gets a keyframe at the playhead and one that has none a new value; a
//! drag (and a run of wheel steps) is one undo step.
//!
//! Automation id: `program.vrReframe`.

use egui::{Color32, Pos2, Rect, Sense, pos2, vec2};
use filmcraft_project::{ClipId, EffectInstance};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::FilmcraftApp;
use crate::state::Tool;
use crate::theme::Tokens;

const EFFECT: &str = "vr_reframe";
/// `effects.setParam` merge keys: a drag, and a run of wheel steps, are one undo step each.
const AIM_MERGE: &str = "programVrReframeAim";
const ZOOM_MERGE: &str = "programVrReframeZoom";

/// The VR Reframe instance the monitor aims: its clip, index and media time at the playhead.
fn target(app: &FilmcraftApp) -> Option<(ClipId, usize, Tick)> {
    let seq = app.session.active_sequence()?;
    let t = app.session.playhead();
    if let Some(sel) = app.session.state.selected_effect.as_ref() {
        // another selected effect (Motion, Corner Pin…) has the picture
        let (_, it) = seq.find_item(sel.clip)?;
        let index = sel.index(&it.effects)?;
        it.effects.get(index).filter(|e| e.enabled && e.effect == EFFECT)?;
        return it.range().contains(t).then(|| (sel.clip, index, it.effect_time_at(t)));
    }
    seq.video_tracks.iter().rev().filter(|tr| tr.enabled).filter_map(|tr| tr.item_at(t)).filter(|it| it.enabled).find_map(|it| {
        let index = it.effects.iter().position(|e| e.enabled && e.effect == EFFECT)?;
        Some((it.id, index, it.effect_time_at(t)))
    })
}

/// Where a drag started and the view's angles then.
#[derive(Clone, Copy, Debug)]
struct Aim {
    clip: ClipId,
    index: usize,
    from: Pos2,
    pan: f64,
    tilt: f64,
    roll: f64,
    fov: f64,
    /// The drag has sent its first change (the one that starts its undo step).
    begun: bool,
}

/// The parameter the wheel zooms in the instance's View, and its range.
fn zoom_param(e: &EffectInstance, mt: Tick) -> (&'static str, f64, f64) {
    // mid-morph between views, the one it is heading to
    let (from, to, f) = filmcraft_render::vfx::reframe_choice(e, "view", mt, 0);
    if (if f > 0.0 { to } else { from }) == 0 { ("fov", 10.0, 360.0) } else { ("planet_fov", 90.0, 360.0) }
}

/// The new angles after a drag of `moved` points across a picture `width` points wide: the
/// picture follows the pointer (dragging left looks right), by the field of view per width.
fn dragged(a: &Aim, moved: egui::Vec2, width: f32, roll: bool) -> (f64, f64, f64) {
    let per = if width > 0.0 { a.fov / f64::from(width) } else { 0.0 };
    let (dx, dy) = (f64::from(moved.x) * per, f64::from(moved.y) * per);
    let wrap = |v: f64| if v.is_finite() { (v + 180.0).rem_euclid(360.0) - 180.0 } else { 0.0 };
    if roll { (a.pan, a.tilt, wrap(a.roll + dx)) } else { (wrap(a.pan - dx), (a.tilt + dy).clamp(-180.0, 180.0), a.roll) }
}

/// The new field of view after a wheel step of `scroll` points, within `lo`…`hi`.
fn zoomed(fov: f64, scroll: f32, lo: f64, hi: f64) -> f64 {
    let f = if fov.is_finite() { fov } else { lo };
    (f * (-f64::from(scroll) * 0.002).exp()).clamp(lo, hi)
}

/// The wheel over the Program monitor picture `pic` zooms VR Reframe this frame (not the
/// monitor's Magnification).
pub fn takes_wheel(app: &FilmcraftApp, ui: &egui::Ui, pic: Rect) -> bool {
    app.ui.tool == Tool::Selection
        && ui.input(|i| i.smooth_scroll_delta.y != 0.0 && i.pointer.hover_pos().is_some_and(|p| pic.contains(p)))
        && target(app).is_some()
}

/// Drag and wheel input on the Program monitor picture `pic` for VR Reframe.
pub fn monitor_overlay(app: &mut FilmcraftApp, ui: &mut egui::Ui, pic: Rect) {
    if app.ui.tool != Tool::Selection || app.ui.mask_pen.is_some() || app.ui.gfx_edit.is_some() {
        return;
    }
    let Some((clip, index, mt)) = target(app) else { return };
    let Some(e) = app.session.active_sequence().and_then(|s| s.find_item(clip)).and_then(|(_, it)| it.effects.get(index)).cloned() else { return };
    let id = egui::Id::unique("program-vr-reframe");
    let resp = ui.interact(pic, id, Sense::drag());
    app.auto.add("program.vrReframe", pic, "VR Reframe: drag to aim");
    let (zoom_id, lo, hi) = zoom_param(&e, mt);
    // what the picture shows (Smooth Motion's values between keyframes), so a drag starts there
    let at = |id: &str| filmcraft_render::vfx::reframe_value(&e, id, mt);
    let fov = at(zoom_id);
    let mut actions: Vec<(String, Value)> = Vec::new();
    let left = egui::PointerButton::Primary;
    if resp.drag_started_by(left) {
        let from = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()).unwrap_or(pic.center());
        let aim = Aim { clip, index, from, pan: at("pan"), tilt: at("tilt"), roll: at("roll"), fov, begun: false };
        ui.data_mut(|d| d.insert_temp(id, aim));
    }
    let aim: Option<Aim> = ui.data(|d| d.get_temp(id)).filter(|a: &Aim| a.clip == clip && a.index == index);
    if let Some(a) = aim
        && resp.dragged_by(left)
        && resp.drag_delta() != egui::Vec2::ZERO
    {
        let cur = resp.interact_pointer_pos().unwrap_or(a.from);
        let (pan, tilt, roll) = dragged(&a, cur - a.from, pic.width(), ui.input(|i| i.modifiers.alt));
        let begin = !a.begun;
        ui.data_mut(|d| d.insert_temp(id, Aim { begun: true, ..a }));
        for (param, value) in [("pan", pan), ("tilt", tilt), ("roll", roll)] {
            actions.push((
                "effects.setParam".into(),
                json!({"clip": clip.0, "effect": index, "param": param, "value": value, "merge": AIM_MERGE, "begin": begin && param == "pan"}),
            ));
        }
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    } else if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }
    if resp.drag_stopped() {
        ui.data_mut(|d| d.remove::<Aim>(id));
    }
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 && scroll.is_finite() {
            let v = zoomed(fov, scroll, lo, hi);
            actions.push(("effects.setParam".into(), json!({"clip": clip.0, "effect": index, "param": zoom_id, "value": v, "merge": ZOOM_MERGE})));
        }
    }
    // what the picture does, on a backing (the picture under it can be any colour)
    let t = app.tokens;
    let hint = crate::i18n::t("VR Reframe: drag to aim, ⌥ drag to roll, scroll to zoom");
    let galley = ui.painter().layout_no_wrap(hint.to_string(), Tokens::ui(10.0), t.text);
    let at = pos2(pic.min.x + 8.0, pic.max.y - 6.0 - galley.size().y);
    ui.painter().rect_filled(Rect::from_min_size(at, galley.size()).expand2(vec2(5.0, 2.0)), 3.0, Color32::from_black_alpha(160));
    ui.painter().galley(at, galley, t.text);
    crate::panels::effect_controls::run(app, ui.ctx(), actions);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aim() -> Aim {
        Aim { clip: ClipId(1), index: 0, from: pos2(0.0, 0.0), pan: 170.0, tilt: 10.0, roll: 0.0, fov: 100.0, begun: false }
    }

    #[test]
    fn dragging_follows_the_pointer_and_wraps() {
        // a whole picture width to the left turns the view right by its field of view (and wraps)
        let (pan, tilt, roll) = dragged(&aim(), vec2(-200.0, 0.0), 200.0, false);
        assert!((pan - -90.0).abs() < 1e-9 && tilt == 10.0 && roll == 0.0, "{pan}");
        // down looks up; tilt stops at ±180°
        let (_, tilt, _) = dragged(&aim(), vec2(0.0, 1e6), 200.0, false);
        assert_eq!(tilt, 180.0);
        // ⌥ rolls instead
        let (pan, _, roll) = dragged(&aim(), vec2(50.0, 0.0), 200.0, true);
        assert!(pan == 170.0 && (roll - 25.0).abs() < 1e-9);
        // hostile sizes and values
        let (pan, tilt, roll) = dragged(&Aim { pan: f64::NAN, fov: f64::INFINITY, ..aim() }, vec2(f32::NAN, 3.0), 0.0, false);
        assert!(pan.is_finite() && tilt.is_finite() && roll.is_finite());
    }

    #[test]
    fn zooming_stays_in_range() {
        assert!(zoomed(100.0, 100.0, 10.0, 360.0) < 100.0);
        assert!(zoomed(100.0, -100.0, 10.0, 360.0) > 100.0);
        assert_eq!(zoomed(350.0, -1e6, 10.0, 360.0), 360.0);
        assert_eq!(zoomed(f64::NAN, 0.0, 90.0, 360.0), 90.0);
    }
}
