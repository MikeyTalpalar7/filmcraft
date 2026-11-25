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
    /// The tilt the view itself starts from (0 Custom, −90 Little Planet, 90 Rabbit Hole): Tilt
    /// stops where the two together look straight up or down.
    base: f64,
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

/// The largest Pan or Roll a drag reaches (Effect Controls' angle range).
const MAX_TURN: f64 = 36_000.0;

/// The new angles after a drag of `moved` points across a picture `width` points wide: the
/// picture follows the pointer (dragging left looks right), by the field of view per width. Pan
/// and Roll keep counting past ±180° (to Effect Controls' ±36000°), so a drag across the back of
/// the sphere keyframes a short, continuous turn: Pan and Roll turn the way their keyframes say.
fn dragged(a: &Aim, moved: egui::Vec2, roll: bool) -> (f64, f64, f64) {
    let width = a.width;
    let per = if width > 0.0 { a.fov / f64::from(width) } else { 0.0 };
    let finite = |v: f64| if v.is_finite() { v } else { 0.0 };
    let (dx, dy) = (finite(f64::from(moved.x) * per), finite(f64::from(moved.y) * per));
    let wrap = |v: f64| if v.is_finite() { v.clamp(-MAX_TURN, MAX_TURN) } else { 0.0 };
    if roll {
        return (a.pan, a.tilt, wrap(a.roll + dx));
    }
    // the drag in the rolled picture's own directions (x right, y up), so the picture follows the
    // pointer however the view is rolled
    let (s, c) = if a.roll.is_finite() { a.roll.to_radians().sin_cos() } else { (0.0, 1.0) };
    let (u, v) = (dx, -dy);
    let (u, v) = (u * c - v * s, u * s + v * c);
    // never past straight up or down: the view would turn upside down and every drag after it
    // would move the picture the wrong way
    let base = if a.base.is_finite() { a.base } else { 0.0 };
    let tilt = if a.tilt.is_finite() { a.tilt } else { 0.0 };
    // looking up or down, a turn sweeps less of the picture across: turn more to keep up (up to
    // twice, 60° off level, so the poles don't spin)
    let across = (base + tilt).to_radians().cos().abs().max(0.5);
    (wrap(a.pan - u / across), (tilt - v).clamp(-90.0 - base, 90.0 - base), a.roll)
}

/// The tilt view `ids`' View starts from at media time `mt` (heading to the next one mid-morph):
/// 0 for Custom, −90 for Little Planet (looking down), 90 for Rabbit Hole (up).
fn base_tilt(e: &EffectInstance, mt: Tick, ids: Ids) -> f64 {
    let (from, to, f) = filmcraft_render::vfx::reframe_choice(e, ids.view, mt, 0);
    match if f > 0.0 { to } else { from } {
        1 => -90.0,
        2 => 90.0,
        _ => 0.0,
    }
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

/// The Manual aim waiting to be set on VR Reframe `index` of `clip`, if its undo step is still
/// the last one.
fn pending_on(app: &FilmcraftApp, clip: ClipId, index: usize) -> Option<VrPending> {
    app.ui.vr_pending.clone().filter(|p| (p.clip, p.index) == (clip.0, index) && p.depth == app.session.history.undo.len())
}

/// Take a Manual aim's undo step back (the keyframes it held still come back), leaving no redo.
fn undo_preview(app: &mut FilmcraftApp) {
    if let Some(p) = app.ui.vr_pending.take()
        && p.depth == app.session.history.undo.len()
        && app.session.execute("edit.undo", json!({})).is_ok()
    {
        app.session.history.redo.pop();
    }
}

/// Manual keyframing: an aim that wasn't set as a keyframe stays while playback runs (so the view
/// can be steered live and set with K) and where playback stops; it goes away when the playhead
/// is moved while stopped, or keyframing switches to Auto. If something else was edited since,
/// the aim stays as it is.
fn drop_stale_preview(app: &mut FilmcraftApp) {
    let Some(mut p) = app.ui.vr_pending.clone() else { return };
    if app.session.history.undo.len() != p.depth {
        app.ui.vr_pending = None;
        return;
    }
    let ph = app.session.playhead();
    if !app.ui.vr_keyframing_manual || (!app.playback.playing && !p.playing && ph != p.time) {
        undo_preview(app);
        return;
    }
    (p.time, p.playing) = (ph, app.playback.playing);
    app.ui.vr_pending = Some(p);
}

/// Show a Manual aim: `set` (and the values aimed before it) held still on VR Reframe `index` of
/// `clip`, over its keyframes, as one undo step that replaces the previous aim's.
fn preview(app: &mut FilmcraftApp, clip: ClipId, index: usize, set: &[(&'static str, f64)]) {
    let mut values = pending_on(app, clip, index).map(|p| p.values).unwrap_or_default();
    for (id, v) in set {
        match values.iter_mut().find(|(i, _)| i == id) {
            Some(slot) => slot.1 = *v,
            None => values.push((*id, *v)),
        }
    }
    if pending_on(app, clip, index).is_some() {
        undo_preview(app);
    }
    app.ui.vr_pending = None;
    let Some(e) = app.session.active_sequence().and_then(|s| s.find_item(clip)).and_then(|(_, it)| it.effects.get(index)).cloned() else { return };
    let r = app.session.edit_group("Aim VR Reframe", |s| {
        for (id, v) in &values {
            if animated(&e, id) {
                s.execute("effects.toggleAnimation", json!({"clip": clip.0, "effect": index, "param": id}))?;
            }
            s.execute("effects.setParam", json!({"clip": clip.0, "effect": index, "param": id, "value": v}))?;
        }
        Ok(())
    });
    match r {
        Ok(()) => {
            let (time, depth, playing) = (app.session.playhead(), app.session.history.undo.len(), app.playback.playing);
            app.ui.vr_pending = Some(VrPending { clip: clip.0, index, time, depth, values, playing });
        }
        Err(e) => app.ui.status = e.to_string(),
    }
}

/// Whether K keyframes rather than stops: playback is stopped on a VR Reframe, or a Manual aim
/// is waiting to be set (steered while playing).
pub(crate) fn k_keyframes(app: &FilmcraftApp) -> bool {
    match target(app) {
        Some((clip, index, _)) => !app.playback.playing || pending_on(app, clip, index).is_some(),
        None => false,
    }
}

/// Set VR Keyframe (K while stopped, or ⌥⇧K): keyframes every animatable property of the VR
/// Reframe under the playhead at the playhead, as it is shown there (View, Lens, the angles and
/// Fields of View, the shaping, Split Screen's B view and Order…), animating the ones that
/// aren't yet. One undo step. A Manual aim waiting to be set is what gets keyframed (the
/// keyframes it held still come back first).
pub fn set_keyframe(app: &mut FilmcraftApp, _ctx: &egui::Context) -> Result<Value, String> {
    let (clip, index, mt) = target(app).ok_or("no VR Reframe under the playhead")?;
    // a Manual aim: its keyframes come back, and it is keyframed among them
    let aimed = pending_on(app, clip, index).map(|p| p.values).unwrap_or_default();
    undo_preview(app);
    let e = app.session.active_sequence().and_then(|s| s.find_item(clip)).and_then(|(_, it)| it.effects.get(index)).cloned().ok_or("no VR Reframe")?;
    let mut writes = key_all(&e, mt);
    for (id, v) in &aimed {
        if let Some(w) = writes.iter_mut().find(|(i, _)| i == id) {
            w.1 = json!(v);
        }
    }
    let ids: Vec<&str> = writes.iter().map(|(id, _)| *id).collect();
    app.session
        .edit_group("Set VR Keyframe", |s| {
            for (id, value) in &writes {
                s.execute("effects.setParam", json!({"clip": clip.0, "effect": index, "param": id, "value": value, "keyframe": true}))?;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    Ok(json!({"keyframed": ids}))
}

/// Every animatable property of VR Reframe `e` with the value it has at media time `mt`, as it
/// is drawn (a View, Lens or Order change in the middle of its morph counts as the nearer one).
fn key_all(e: &EffectInstance, mt: Tick) -> Vec<(&'static str, Value)> {
    use filmcraft_project::ParamValue;
    let Some(def) = e.def() else { return Vec::new() };
    def.params
        .iter()
        .filter(|d| d.animatable)
        .filter_map(|d| {
            let value = match &d.default {
                ParamValue::Choice(c) => {
                    let (from, to, f) = filmcraft_render::vfx::reframe_choice(e, d.id, mt, *c);
                    json!(if f >= 0.5 { to } else { from })
                }
                ParamValue::Float(_) => json!(filmcraft_render::vfx::reframe_value(e, d.id, mt)),
                _ => return None,
            };
            Some((d.id, value))
        })
        .collect()
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
    drop_stale_preview(app);
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
    // Manual keyframing previews an aim of an animated view, held still over its keyframes,
    // until it is set (K)
    let previewing = pending_on(app, clip, index).is_some();
    let manual = app.ui.vr_keyframing_manual && (previewing || aim_params(&e).iter().any(|id| animated(&e, id)));
    let mut aimed: Vec<(&'static str, f64)> = Vec::new();
    let mut actions: Vec<(String, Value)> = Vec::new();
    let left = egui::PointerButton::Primary;
    if resp.drag_started_by(left) {
        let from = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()).unwrap_or(pic.center());
        let base = base_tilt(&e, mt, ids);
        let aim = Aim { clip, index, ids, width, from, pan: at(ids.pan), tilt: at(ids.tilt), roll: at(ids.roll), fov, base, begun: false, recorded: i64::MIN };
        ui.data_mut(|d| d.insert_temp(id, aim));
    }
    let aim: Option<Aim> = ui.data(|d| d.get_temp(id)).filter(|a: &Aim| a.clip == clip && a.index == index);
    if let Some(mut a) = aim
        && resp.dragged_by(left)
        && resp.drag_delta() != egui::Vec2::ZERO
    {
        let cur = resp.interact_pointer_pos().unwrap_or(a.from);
        let (pan, tilt, roll) = dragged(&a, cur - a.from, ui.input(|i| i.modifiers.alt));
        let begin = !a.begun;
        if manual {
            aimed.extend([(a.ids.pan, pan), (a.ids.tilt, tilt), (a.ids.roll, roll)]);
        } else if record_step(app, &e, &mut a, &mut actions) {
            for (n, (param, value)) in [(a.ids.pan, pan), (a.ids.tilt, tilt), (a.ids.roll, roll)].into_iter().enumerate() {
                actions.push((
                    "effects.setParam".into(),
                    json!({"clip": clip.0, "effect": index, "param": param, "value": value, "merge": AIM_MERGE, "begin": begin && n == 0}),
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
            if manual {
                aimed.push((zoom_id, v));
            } else {
                actions.push(("effects.setParam".into(), json!({"clip": clip.0, "effect": index, "param": zoom_id, "value": v, "merge": ZOOM_MERGE})));
            }
        }
    }
    crate::panels::effect_controls::run(app, ui.ctx(), actions);
    if !aimed.is_empty() {
        preview(app, clip, index, &aimed);
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
    let y = pic.max.y - 6.0 - 13.0;
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
    let mut left = mr.min.x;
    if app.ui.vr_pending.is_some() {
        let set = tl!("Set Keyframe (K)");
        let sw = ui.painter().layout_no_wrap(set.to_string(), Tokens::ui(10.0), t.text).size().x;
        let sat = pos2(mr.min.x - 14.0 - sw, y);
        let (sr, g) = chip(ui, sat, set, true);
        ui.painter().galley(sat, g.into(), t.hot_text);
        if ui.interact(sr, egui::Id::unique("vr-set-keyframe"), Sense::click()).clicked() {
            let _ = set_keyframe(app, ui.ctx());
        }
        app.auto.add("program.vrReframe.setKeyframe", sr, set);
        left = sr.min.x;
    }
    // the hint at the bottom left, a row higher when it would run into the buttons (a narrow,
    // vertical frame)
    // (wrapped to the picture's width)
    let hint = crate::i18n::t("VR Reframe: drag to aim, ⌥ drag to roll, scroll to zoom");
    let g = ui.painter().layout(hint.to_string(), Tokens::ui(10.0), t.text, (pic.width() - 26.0).max(40.0));
    let fits = pic.min.x + 8.0 + g.size().x + 5.0 + 8.0 <= left;
    let at = pos2(pic.min.x + 8.0, if fits { y } else { y - 10.0 - g.size().y });
    ui.painter().rect_filled(Rect::from_min_size(at, g.size()).expand2(vec2(5.0, 2.0)), 3.0, Color32::from_black_alpha(170));
    ui.painter().galley(at, g, t.text);
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
            base: 0.0,
            begun: false,
            recorded: i64::MIN,
        }
    }

    /// Where a world direction seen at view point `p` (x −1…1 across, y up) of a view with these
    /// angles shows up in a view with the new angles, by searching the view (the view's own maths
    /// is one way only).
    fn seen_at(world: [f64; 3], (pan, tilt, roll): (f64, f64, f64), fov: f64) -> (f64, f64) {
        use filmcraft_render::vfx::{rotate_view, view_dir, view_rotation};
        let r = view_rotation(pan, tilt, roll);
        let mut best = (f64::MAX, (0.0, 0.0));
        for i in -200..=200 {
            for j in -200..=200 {
                let (x, y) = (f64::from(i) / 200.0, f64::from(j) / 200.0);
                let Some(d) = view_dir(x, y, fov) else { continue };
                let w = rotate_view(&r, d);
                let e = (0..3).map(|k| (w[k] - world[k]).powi(2)).sum::<f64>();
                if e < best.0 {
                    best = (e, (x, y));
                }
            }
        }
        best.1
    }

    /// The picture follows the pointer, rolled or not, looking up or down: what was under the
    /// pointer moves the way the pointer did (the drag maps a turn, not a pixel shift, so only the
    /// direction is checked).
    #[test]
    fn the_picture_follows_the_pointer_however_the_view_is_rolled() {
        use filmcraft_render::vfx::{rotate_view, view_dir, view_rotation};
        for roll in [0.0, 30.0, 90.0, -135.0] {
            for tilt in [0.0, 40.0, -60.0] {
                let a = Aim { pan: 20.0, tilt, roll, fov: 90.0, width: 400.0, ..aim() };
                // a small drag right and down (screen points, y down)
                for (mx, my) in [(12.0f32, 0.0f32), (0.0, 12.0), (-9.0, 7.0)] {
                    let world = rotate_view(&view_rotation(a.pan, a.tilt, a.roll), view_dir(0.0, 0.0, 90.0).unwrap());
                    let after = dragged(&a, vec2(mx, my), false);
                    let (x, y) = seen_at(world, after, 90.0);
                    // view x right, y up: the screen drag (mx, −my)
                    let (ex, ey) = (f64::from(mx), -f64::from(my));
                    let dot = (x * ex + y * ey) / ((x * x + y * y).sqrt() * (ex * ex + ey * ey).sqrt());
                    assert!(dot > 0.95, "roll {roll} tilt {tilt} drag ({mx}, {my}): moved to ({x:.3}, {y:.3})");
                }
            }
        }
    }

    /// Dragging never tips the view past straight up or down (it would turn upside down and the
    /// drags after it would go the wrong way), in a planet view too.
    #[test]
    fn tilt_stops_at_straight_up_and_down() {
        assert_eq!(dragged(&aim(), vec2(0.0, 1e6), false).1, 90.0);
        assert_eq!(dragged(&aim(), vec2(0.0, -1e6), false).1, -90.0);
        let planet = Aim { base: -90.0, tilt: 30.0, ..aim() };
        assert_eq!(dragged(&planet, vec2(0.0, -1e6), false).1, 0.0, "a little planet stops looking straight down");
        assert_eq!(dragged(&planet, vec2(0.0, 1e6), false).1, 180.0);
        // a view already upside down comes back the right way up on the next drag
        assert_eq!(dragged(&Aim { tilt: 150.0, ..aim() }, vec2(1.0, 0.0), false).1, 90.0);
    }

    #[test]
    fn dragging_follows_the_pointer_and_keeps_counting() {
        // a whole picture width to the left turns the view right by its field of view (a little
        // more, tilted 10°), past 180° (about 270°, not −90°: keyframed, it turns on to the right)
        let (pan, tilt, roll) = dragged(&aim(), vec2(-200.0, 0.0), false);
        assert!((pan - (170.0 + 100.0 / 10f64.to_radians().cos())).abs() < 1e-9 && tilt == 10.0 && roll == 0.0, "{pan}");
        let (_, _, roll) = dragged(&Aim { roll: -170.0, ..aim() }, vec2(-100.0, 0.0), true);
        assert!((roll - -220.0).abs() < 1e-9, "{roll}");
        let (pan, _, _) = dragged(&Aim { pan: 35_990.0, ..aim() }, vec2(-1e6, 0.0), false);
        assert_eq!(pan, MAX_TURN);
        // down looks up; tilt stops at straight up
        let (_, tilt, _) = dragged(&aim(), vec2(0.0, 1e6), false);
        assert_eq!(tilt, 90.0);
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

    /// Manual keyframing over keyframes: an aim holds still while playing (it isn't undone each
    /// frame and snapped back to the keyframes), K sets it among them without stopping, and an
    /// aim not set goes away when the playhead is moved while stopped.
    #[test]
    fn manual_aims_hold_over_keyframes_while_playing_and_k_sets_them() {
        let mut s = filmcraft_engine::Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id;
        s.execute("effects.apply", json!({"clips": [clip.0], "effect": "VR Reframe"})).unwrap();
        let index = s.active_sequence().unwrap().find_item(clip).unwrap().1.effects.iter().position(|e| e.effect == EFFECT).unwrap();
        for (seconds, pan) in [(0.5, 0.0), (2.5, 90.0)] {
            s.execute("playhead.set", json!({"seconds": seconds})).unwrap();
            s.execute("effects.setParam", json!({"clip": clip.0, "effect": index, "param": "pan", "value": pan, "keyframe": true})).unwrap();
        }
        s.execute("playhead.set", json!({"seconds": 1.0})).unwrap();
        let mut app = FilmcraftApp::new(s);
        let ctx = egui::Context::default();
        app.ui.vr_keyframing_manual = true;
        let pan = |app: &FilmcraftApp| {
            let it = app.session.active_sequence().unwrap().find_item(clip).unwrap().1.clone();
            let e = it.effects[index].clone();
            (filmcraft_render::vfx::reframe_value(&e, "pan", it.effect_time_at(app.session.playhead())), e.param("pan").unwrap().keyframes.len())
        };
        // aim to 45° and play on: the aim holds
        preview(&mut app, clip, index, &[("pan", 45.0)]);
        app.play(1.0);
        app.session.execute("playhead.set", json!({"seconds": 1.5})).unwrap();
        drop_stale_preview(&mut app);
        assert!(app.ui.vr_pending.is_some(), "the aim survives playback");
        assert_eq!(pan(&app).0, 45.0);
        // K while playing with an aim waiting: keyframed at 1.5 s among the others, still playing
        assert!(k_keyframes(&app));
        crate::menus::invoke_command(&mut app, &ctx, "playback.stop", json!({})).unwrap();
        assert!(app.playback.playing);
        assert!(app.ui.vr_pending.is_none());
        assert_eq!(pan(&app), (45.0, 3), "the old keyframes are back, with the new one");
        // without an aim waiting, K stops
        assert!(!k_keyframes(&app));
        app.stop();
        // an aim made while stopped goes away when the playhead is moved
        preview(&mut app, clip, index, &[("pan", -30.0)]);
        assert_eq!(pan(&app).0, -30.0);
        app.session.execute("playhead.set", json!({"seconds": 2.0})).unwrap();
        drop_stale_preview(&mut app);
        assert!(app.ui.vr_pending.is_none());
        assert_eq!(pan(&app).1, 3, "the keyframes are back");
        assert!(app.session.history.redo.is_empty(), "no stray redo");
    }
}
