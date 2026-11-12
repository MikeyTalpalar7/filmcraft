//! Aiming VR Reframe on the Program monitor: drag the picture to pan and tilt the view (with
//! ⌥/Alt, to roll it), scroll to change its field of view (the Planet Field of View in the Little
//! Planet and Rabbit Hole views).
//!
//! With Split Screen on, the drag and the wheel aim the view under the pointer (A or B).
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
use crate::state::{Tool, VrPending};
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

/// The parameters of one of VR Reframe's views: View, Pan, Tilt, Roll, Field of View and the
/// Planet Field of View.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Ids {
    view: &'static str,
    pan: &'static str,
    tilt: &'static str,
    roll: &'static str,
    fov: &'static str,
    planet_fov: &'static str,
}
const VIEW_A: Ids = Ids { view: "view", pan: "pan", tilt: "tilt", roll: "roll", fov: "fov", planet_fov: "planet_fov" };
const VIEW_B: Ids = Ids { view: "view2", pan: "pan2", tilt: "tilt2", roll: "roll2", fov: "fov2", planet_fov: "fov2" };

/// The view under `pos` on the picture `pic` and the width (points) it fills: A, or with Split
/// Screen on, the one whose part of the frame it is.
fn view_at(e: &EffectInstance, mt: Tick, pic: Rect, pos: Pos2) -> (Ids, f32) {
    use filmcraft_render::vfx::{reframe_choice, reframe_value};
    let axis = match e.param("split").map(|p| &p.value) {
        Some(filmcraft_project::ParamValue::Choice(a @ (1 | 2))) => *a,
        _ => return (VIEW_A, pic.width()),
    };
    let at = (reframe_value(e, "split_position", mt) / 100.0).clamp(0.05, 0.95) as f32;
    let along = if axis == 1 { (pos.y - pic.min.y) / pic.height().max(1.0) } else { (pos.x - pic.min.x) / pic.width().max(1.0) };
    let first = along < at;
    // mid-swap, the order it is heading to
    let (from, to, f) = reframe_choice(e, "split_order", mt, 0);
    let b_first = (if f >= 0.5 { to } else { from }) == 1;
    let ids = if first != b_first { VIEW_A } else { VIEW_B };
    let width = if axis == 1 { pic.width() } else { pic.width() * if first { at } else { 1.0 - at } };
    (ids, width)
}

/// Where a drag started and the view's angles then.
#[derive(Clone, Copy, Debug)]
struct Aim {
    clip: ClipId,
    index: usize,
    ids: Ids,
    width: f32,
    from: Pos2,
    pan: f64,
    tilt: f64,
    roll: f64,
    fov: f64,
    /// The drag has sent its first change (the one that starts its undo step).
    begun: bool,
    /// Auto keyframing while playing: the playhead (ticks) of the last recorded keyframe.
    recorded: i64,
}

/// The parameter the wheel zooms in the view's View, and its range.
fn zoom_param(e: &EffectInstance, mt: Tick, ids: Ids) -> (&'static str, f64, f64) {
    // mid-morph between views, the one it is heading to
    let (from, to, f) = filmcraft_render::vfx::reframe_choice(e, ids.view, mt, 0);
    if (if f > 0.0 { to } else { from }) == 0 || ids.fov == ids.planet_fov { (ids.fov, 10.0, 360.0) } else { (ids.planet_fov, 90.0, 360.0) }
}

/// The new angles after a drag of `moved` points across a picture `width` points wide: the
/// picture follows the pointer (dragging left looks right), by the field of view per width.
fn dragged(a: &Aim, moved: egui::Vec2, roll: bool) -> (f64, f64, f64) {
    let width = a.width;
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

/// Auto keyframing records a drag made while playing at most this often (ticks; a tenth of a
/// second), and Smooth Motion runs a curve through the keyframes.
const RECORD_EVERY: i64 = filmcraft_time::TICKS_PER_SECOND / 10;
/// Manual keyframing: the merge key of the undo step that previews an aim not yet keyframed.
const PREVIEW_MERGE: &str = "programVrReframePreview";

/// The aim parameters (Pan, Tilt, Roll, Field of View, Planet Field of View) of view A, and of B
/// with Split Screen on.
fn aim_params(e: &EffectInstance) -> Vec<&'static str> {
    let mut ids = vec![VIEW_A.pan, VIEW_A.tilt, VIEW_A.roll, VIEW_A.fov, VIEW_A.planet_fov];
    if matches!(e.param("split").map(|p| &p.value), Some(filmcraft_project::ParamValue::Choice(1 | 2))) {
        ids.extend([VIEW_B.pan, VIEW_B.tilt, VIEW_B.roll, VIEW_B.fov]);
    }
    ids
}

fn animated(e: &EffectInstance, id: &str) -> bool {
    e.param(id).is_some_and(filmcraft_project::Param::is_animated)
}

/// Manual keyframing: an aim that wasn't set as a keyframe goes away once the playhead leaves
/// it or playback starts (its undo step is undone, and doesn't linger as a redo). If something
/// else was edited since, the aim stays as it is.
fn drop_stale_preview(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let Some(p) = app.ui.vr_pending.clone() else { return };
    if app.session.history.undo.len() != p.depth {
        app.ui.vr_pending = None;
    } else if app.session.playhead() != p.time || app.playback.playing || !app.ui.vr_keyframing_manual {
        app.ui.vr_pending = None;
        crate::panels::effect_controls::run(app, ctx, vec![("edit.undo".into(), json!({}))]);
        app.session.history.redo.pop();
    }
}

/// Sequence ▸ Set VR Keyframe (⌥⇧K): keyframes the VR Reframe view under the playhead as it is
/// shown, at the playhead (its Pan, Tilt, Roll and Fields of View; B's too with Split Screen),
/// animating the ones that aren't yet. Keeps a Manual-keyframing aim.
pub fn set_keyframe(app: &mut FilmcraftApp, ctx: &egui::Context) -> Result<Value, String> {
    let (clip, index, mt) = target(app).ok_or("no VR Reframe under the playhead")?;
    let e = app.session.active_sequence().and_then(|s| s.find_item(clip)).and_then(|(_, it)| it.effects.get(index)).cloned().ok_or("no VR Reframe")?;
    app.ui.vr_pending = None;
    let mut actions: Vec<(String, Value)> = Vec::new();
    let ids = aim_params(&e);
    for id in &ids {
        if animated(&e, id) {
            let v = filmcraft_render::vfx::reframe_value(&e, id, mt);
            actions.push(("effects.setParam".into(), json!({"clip": clip.0, "effect": index, "param": id, "value": v})));
        } else {
            actions.push(("effects.toggleAnimation".into(), json!({"clip": clip.0, "effect": index, "param": id})));
        }
    }
    crate::panels::effect_controls::run(app, ctx, actions);
    Ok(json!({"keyframed": ids}))
}

/// The angles to send for a drag this frame, or None to skip it: Auto keyframing records a drag
/// made while playing every [`RECORD_EVERY`] (animating the view's angles when it starts).
fn record_step(app: &FilmcraftApp, e: &EffectInstance, a: &mut Aim, actions: &mut Vec<(String, Value)>) -> bool {
    if app.ui.vr_keyframing_manual || !app.playback.playing {
        return true;
    }
    let ph = app.session.playhead().0;
    if !a.begun {
        for id in [a.ids.pan, a.ids.tilt, a.ids.roll] {
            if !animated(e, id) {
                actions.push(("effects.toggleAnimation".into(), json!({"clip": a.clip.0, "effect": a.index, "param": id})));
            }
        }
    } else if (0..RECORD_EVERY).contains(&(ph - a.recorded)) {
        return false;
    }
    a.recorded = ph;
    true
}

/// Drag and wheel input on the Program monitor picture `pic` for VR Reframe.
pub fn monitor_overlay(app: &mut FilmcraftApp, ui: &mut egui::Ui, pic: Rect) {
    if app.ui.tool != Tool::Selection || app.ui.mask_pen.is_some() || app.ui.gfx_edit.is_some() {
        return;
    }
    drop_stale_preview(app, ui.ctx());
    let Some((clip, index, mt)) = target(app) else { return };
    let Some(e) = app.session.active_sequence().and_then(|s| s.find_item(clip)).and_then(|(_, it)| it.effects.get(index)).cloned() else { return };
    let id = egui::Id::unique("program-vr-reframe");
    let resp = ui.interact(pic, id, Sense::drag());
    app.auto.add("program.vrReframe", pic, "VR Reframe: drag to aim");
    // the view under the pointer (a drag keeps the one it started on)
    let pointer = ui.input(|i| i.pointer.press_origin().filter(|_| resp.drag_started()).or(i.pointer.hover_pos())).unwrap_or(pic.center());
    let (ids, width) = view_at(&e, mt, pic, pointer);
    let (zoom_id, lo, hi) = zoom_param(&e, mt, ids);
    // what the picture shows (Smooth Motion's values between keyframes), so a drag starts there
    let at = |id: &str| filmcraft_render::vfx::reframe_value(&e, id, mt);
    let fov = at(zoom_id);
    // Manual keyframing previews an aim of an animated view in one undo step until it is set
    let manual = app.ui.vr_keyframing_manual && aim_params(&e).iter().any(|id| animated(&e, id));
    let previewing = app.ui.vr_pending.as_ref().is_some_and(|p| (p.clip, p.index) == (clip.0, index));
    let (aim_merge, zoom_merge) = if manual { (PREVIEW_MERGE, PREVIEW_MERGE) } else { (AIM_MERGE, ZOOM_MERGE) };
    let mut actions: Vec<(String, Value)> = Vec::new();
    let left = egui::PointerButton::Primary;
    if resp.drag_started_by(left) {
        let from = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()).unwrap_or(pic.center());
        let aim = Aim { clip, index, ids, width, from, pan: at(ids.pan), tilt: at(ids.tilt), roll: at(ids.roll), fov, begun: false, recorded: i64::MIN };
        ui.data_mut(|d| d.insert_temp(id, aim));
    }
    let aim: Option<Aim> = ui.data(|d| d.get_temp(id)).filter(|a: &Aim| a.clip == clip && a.index == index);
    if let Some(mut a) = aim
        && resp.dragged_by(left)
        && resp.drag_delta() != egui::Vec2::ZERO
    {
        let cur = resp.interact_pointer_pos().unwrap_or(a.from);
        let (pan, tilt, roll) = dragged(&a, cur - a.from, ui.input(|i| i.modifiers.alt));
        // a new undo step, except while adding to a Manual preview
        let begin = !a.begun && !(manual && previewing);
        if record_step(app, &e, &mut a, &mut actions) {
            for (n, (param, value)) in [(a.ids.pan, pan), (a.ids.tilt, tilt), (a.ids.roll, roll)].into_iter().enumerate() {
                actions.push((
                    "effects.setParam".into(),
                    json!({"clip": clip.0, "effect": index, "param": param, "value": value, "merge": aim_merge, "begin": begin && n == 0}),
                ));
            }
        }
        ui.data_mut(|d| d.insert_temp(id, Aim { begun: true, ..a }));
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
            actions.push(("effects.setParam".into(), json!({"clip": clip.0, "effect": index, "param": zoom_id, "value": v, "merge": zoom_merge})));
        }
    }
    let aimed = !actions.is_empty();
    crate::panels::effect_controls::run(app, ui.ctx(), actions);
    if manual && aimed {
        app.ui.vr_pending = Some(VrPending { clip: clip.0, index, time: app.session.playhead(), depth: app.session.history.undo.len() });
    }
    hint_and_mode(app, ui, pic);
}

/// What the picture does (on a backing: the picture under it can be any colour), and the
/// keyframing chip: Auto / Manual (click to switch), with Set Keyframe while a Manual aim waits.
/// Automation ids: `program.vrReframe.keyframing`, `program.vrReframe.setKeyframe`.
fn hint_and_mode(app: &mut FilmcraftApp, ui: &mut egui::Ui, pic: Rect) {
    let t = app.tokens;
    let chip = |ui: &mut egui::Ui, at: Pos2, text: &str, accent: bool| -> (Rect, egui::Galley) {
        let g = ui.painter().layout_no_wrap(text.to_string(), Tokens::ui(10.0), if accent { t.hot_text } else { t.text });
        let r = Rect::from_min_size(at, g.size()).expand2(vec2(5.0, 2.0));
        ui.painter().rect_filled(r, 3.0, Color32::from_black_alpha(170));
        (r, (*g).clone())
    };
    let hint = crate::i18n::t("VR Reframe: drag to aim, ⌥ drag to roll, scroll to zoom");
    let y = pic.max.y - 6.0 - 13.0;
    let (_, g) = chip(ui, pos2(pic.min.x + 8.0, y), hint, false);
    ui.painter().galley(pos2(pic.min.x + 8.0, y), g.into(), t.text);
    let mode = if app.ui.vr_keyframing_manual { tl!("Keyframes: Manual") } else { tl!("Keyframes: Auto") };
    let mw = ui.painter().layout_no_wrap(mode.to_string(), Tokens::ui(10.0), t.text).size().x;
    let mat = pos2(pic.max.x - 13.0 - mw, y);
    let (mr, g) = chip(ui, mat, mode, app.ui.vr_keyframing_manual);
    ui.painter().galley(mat, g.into(), t.text);
    let tip = tl!("Auto: aiming sets keyframes at once and records while playing. Manual: aiming only previews until Set VR Keyframe (⌥⇧K).");
    if ui.interact(mr, egui::Id::unique("vr-keyframing-mode"), Sense::click()).on_hover_text(tip).clicked() {
        app.ui.vr_keyframing_manual = !app.ui.vr_keyframing_manual;
    }
    app.auto.add("program.vrReframe.keyframing", mr, mode);
    if app.ui.vr_pending.is_some() {
        let set = tl!("Set Keyframe ⌥⇧K");
        let sw = ui.painter().layout_no_wrap(set.to_string(), Tokens::ui(10.0), t.text).size().x;
        let sat = pos2(mr.min.x - 14.0 - sw, y);
        let (sr, g) = chip(ui, sat, set, true);
        ui.painter().galley(sat, g.into(), t.hot_text);
        if ui.interact(sr, egui::Id::unique("vr-set-keyframe"), Sense::click()).clicked() {
            let _ = set_keyframe(app, ui.ctx());
        }
        app.auto.add("program.vrReframe.setKeyframe", sr, set);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aim() -> Aim {
        Aim {
            clip: ClipId(1),
            index: 0,
            ids: VIEW_A,
            width: 200.0,
            from: pos2(0.0, 0.0),
            pan: 170.0,
            tilt: 10.0,
            roll: 0.0,
            fov: 100.0,
            begun: false,
            recorded: i64::MIN,
        }
    }

    #[test]
    fn dragging_follows_the_pointer_and_wraps() {
        // a whole picture width to the left turns the view right by its field of view (and wraps)
        let (pan, tilt, roll) = dragged(&aim(), vec2(-200.0, 0.0), false);
        assert!((pan - -90.0).abs() < 1e-9 && tilt == 10.0 && roll == 0.0, "{pan}");
        // down looks up; tilt stops at ±180°
        let (_, tilt, _) = dragged(&aim(), vec2(0.0, 1e6), false);
        assert_eq!(tilt, 180.0);
        // ⌥ rolls instead
        let (pan, _, roll) = dragged(&aim(), vec2(50.0, 0.0), true);
        assert!(pan == 170.0 && (roll - 25.0).abs() < 1e-9);
        // hostile sizes and values
        let (pan, tilt, roll) = dragged(&Aim { pan: f64::NAN, fov: f64::INFINITY, width: 0.0, ..aim() }, vec2(f32::NAN, 3.0), false);
        assert!(pan.is_finite() && tilt.is_finite() && roll.is_finite());
    }

    #[test]
    fn split_screens_aim_the_view_under_the_pointer() {
        let mut e = filmcraft_project::find_effect("vr_reframe").unwrap().instance();
        let pic = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 200.0));
        let (top, bottom) = (pos2(200.0, 20.0), pos2(200.0, 180.0));
        // no split: always A, the whole width
        assert_eq!(view_at(&e, Tick::ZERO, pic, bottom), (VIEW_A, 400.0));
        e.params.insert("split".into(), filmcraft_project::Param::new(filmcraft_project::ParamValue::Choice(1)));
        assert_eq!(view_at(&e, Tick::ZERO, pic, top).0, VIEW_A);
        assert_eq!(view_at(&e, Tick::ZERO, pic, bottom).0, VIEW_B);
        // swapped
        e.params.insert("split_order".into(), filmcraft_project::Param::new(filmcraft_project::ParamValue::Choice(1)));
        assert_eq!(view_at(&e, Tick::ZERO, pic, top).0, VIEW_B);
        // side by side, divider at 25%: the left view is a quarter of the width
        e.params.insert("split".into(), filmcraft_project::Param::new(filmcraft_project::ParamValue::Choice(2)));
        e.params.insert("split_position".into(), filmcraft_project::Param::new(filmcraft_project::ParamValue::Float(25.0)));
        assert_eq!(view_at(&e, Tick::ZERO, pic, pos2(50.0, 100.0)), (VIEW_B, 100.0));
        assert_eq!(view_at(&e, Tick::ZERO, pic, pos2(300.0, 100.0)), (VIEW_A, 300.0));
        // B's wheel zooms its own Field of View, in every View
        assert_eq!(zoom_param(&e, Tick::ZERO, VIEW_B).0, "fov2");
    }

    #[test]
    fn zooming_stays_in_range() {
        assert!(zoomed(100.0, 100.0, 10.0, 360.0) < 100.0);
        assert!(zoomed(100.0, -100.0, 10.0, 360.0) > 100.0);
        assert_eq!(zoomed(350.0, -1e6, 10.0, 360.0), 360.0);
        assert_eq!(zoomed(f64::NAN, 0.0, 90.0, 360.0), 90.0);
    }
}
