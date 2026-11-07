//! Zooming Effect Controls' keyframe area, for placing keyframes finely.
//!
//! The area draws the clip `zoom` times its width (a "virtual lane" that starts left of the
//! visible area by the scrolled part), so every row, ruler and drag that maps time to x through
//! the lane works unchanged; what falls outside the visible area is skipped ([`shown`]).
//!
//! - The wheel over the time ruler zooms around the pointer (as does a pinch over the area).
//! - A sideways scroll over the area (or ⇧ + wheel over the ruler) scrolls it.
//! - The bar under the area shows the part in view: drag it to scroll, drag its ends to zoom,
//!   double-click it to see the whole clip again.
//!
//! Automation ids: `effectControls.zoomBar`, `effectControls.zoomBar.thumb`.

use egui::{Rect, Sense, Stroke, pos2};
use filmcraft_project::TrackItem;
use filmcraft_time::FrameRate;

use crate::FilmcraftApp;
use crate::theme::Tokens;

/// The most the area zooms in: a frame at most this many times the visible width / 8.
const MAX_FRAME_SHARE: f64 = 8.0;
/// The zoom bar's thumb is at least this wide (points), so it can be grabbed.
const MIN_THUMB: f32 = 14.0;

/// The deepest zoom for a clip of `frames` frames: one frame an eighth of the area wide.
pub(crate) fn max_zoom(frames: f64) -> f64 {
    if frames.is_finite() { (frames / MAX_FRAME_SHARE).max(1.0) } else { 1.0 }
}

/// A sane (zoom, start): zoom within 1…`max`, the view inside the clip.
pub(crate) fn clamp_view(zoom: f64, start: f64, max: f64) -> (f64, f64) {
    let z = if zoom.is_finite() { zoom.clamp(1.0, max.max(1.0)) } else { 1.0 };
    let s = if start.is_finite() { start.clamp(0.0, 1.0 - 1.0 / z) } else { 0.0 };
    (z, s)
}

/// Zoom by `factor`, keeping the clip position under `anchor` (0…1 across the visible area) still.
pub(crate) fn zoom_about(zoom: f64, start: f64, factor: f64, anchor: f64, max: f64) -> (f64, f64) {
    let (z, s) = clamp_view(zoom, start, max);
    let a = if anchor.is_finite() { anchor.clamp(0.0, 1.0) } else { 0.5 };
    let at = s + a / z;
    let f = if factor.is_finite() && factor > 0.0 { factor } else { 1.0 };
    let nz = clamp_view(z * f, 0.0, max).0;
    clamp_view(nz, at - a / nz, max)
}

/// The clip's length in frames.
fn frames(it: &TrackItem, rate: FrameRate) -> f64 {
    it.duration.0.max(1) as f64 / rate.frame_duration().0.max(1) as f64
}

/// The virtual lane for the visible keyframe area `vis`; remembers `vis` for [`shown`].
pub(crate) fn lane(app: &mut FilmcraftApp, vis: Rect, it: &TrackItem, rate: FrameRate) -> Rect {
    let (z, s) = clamp_view(f64::from(app.ui.effect_controls_zoom), f64::from(app.ui.effect_controls_view), max_zoom(frames(it, rate)));
    app.ui.effect_controls_zoom = z as f32;
    app.ui.effect_controls_view = s as f32;
    app.ui.effect_controls_visible = (vis.min.x, vis.max.x);
    let w = vis.width() * z as f32;
    let x0 = vis.min.x - (s * f64::from(w)) as f32;
    Rect::from_min_max(pos2(x0, vis.min.y), pos2(x0 + w, vis.max.y))
}

/// Whether screen x `x` is in the visible keyframe area (with `slack` points either side).
pub(crate) fn shown(app: &FilmcraftApp, x: f32, slack: f32) -> bool {
    let (a, b) = app.ui.effect_controls_visible;
    x >= a - slack && x <= b + slack
}

/// Wheel, pinch and sideways scrolling: over `ruler` the wheel zooms; over `area` a pinch zooms
/// and a sideways scroll moves the view.
pub(crate) fn input(app: &mut FilmcraftApp, ui: &egui::Ui, vis: Rect, ruler: Rect, area: Rect, it: &TrackItem, rate: FrameRate) {
    let Some(pos) = ui.input(|i| i.pointer.hover_pos()) else { return };
    if !area.contains(pos) && !ruler.contains(pos) {
        return;
    }
    let max = max_zoom(frames(it, rate));
    let (z, s) = (f64::from(app.ui.effect_controls_zoom), f64::from(app.ui.effect_controls_view));
    let anchor = f64::from((pos.x - vis.min.x) / vis.width().max(1.0));
    let (pinch, delta, shift) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.modifiers.shift));
    let mut view = (z, s);
    if (pinch - 1.0).abs() > 1e-4 {
        view = zoom_about(z, s, f64::from(pinch), anchor, max);
    } else if ruler.contains(pos) && !shift && delta.y != 0.0 {
        view = zoom_about(z, s, f64::from(delta.y * 0.01).exp(), anchor, max);
    } else {
        // sideways: a trackpad's x, or ⇧ + wheel over the ruler
        let dx = if delta.x != 0.0 {
            delta.x
        } else if shift && ruler.contains(pos) {
            delta.y
        } else {
            0.0
        };
        if dx != 0.0 {
            view = clamp_view(z, s - f64::from(dx) / (f64::from(vis.width().max(1.0)) * z), max);
        }
    }
    if view != (z, s) {
        app.ui.effect_controls_zoom = view.0 as f32;
        app.ui.effect_controls_view = view.1 as f32;
    }
}

/// The zoom bar `bar` under the area: the thumb is the part in view.
pub(crate) fn bar(app: &mut FilmcraftApp, ui: &egui::Ui, bar: Rect, it: &TrackItem, rate: FrameRate, t: &Tokens) {
    let max = max_zoom(frames(it, rate));
    let (z, s) = clamp_view(f64::from(app.ui.effect_controls_zoom), f64::from(app.ui.effect_controls_view), max);
    let w = bar.width().max(1.0);
    let (a, b) = (bar.min.x + (s as f32) * w, bar.min.x + ((s + 1.0 / z) as f32) * w);
    let (a, b) = if b - a < MIN_THUMB { ((a + b - MIN_THUMB) / 2.0, (a + b + MIN_THUMB) / 2.0) } else { (a, b) };
    let thumb = Rect::from_min_max(pos2(a, bar.min.y), pos2(b, bar.max.y));
    let p = ui.painter();
    p.rect_filled(bar, bar.height() / 2.0, t.tl_bg);
    let id = egui::Id::unique("ec-zoom-bar");
    let resp = ui.interact(bar, id, Sense::click_and_drag());
    // which part the drag holds: the left end, the right end or the middle
    let grab = egui::Id::unique("ec-zoom-grab");
    if resp.drag_started()
        && let Some(p0) = resp.interact_pointer_pos()
    {
        let part: u8 = if (p0.x - thumb.min.x).abs() < 6.0 {
            1
        } else if (p0.x - thumb.max.x).abs() < 6.0 {
            2
        } else {
            0
        };
        ui.data_mut(|d| d.insert_temp(grab, part));
    }
    let part: u8 = ui.data(|d| d.get_temp(grab)).unwrap_or(0);
    let hover_end = resp.hover_pos().is_some_and(|p| (p.x - thumb.min.x).abs() < 6.0 || (p.x - thumb.max.x).abs() < 6.0);
    if hover_end || (resp.dragged() && part != 0) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    let mut view = (z, s);
    if resp.dragged() {
        let dx = f64::from(resp.drag_delta().x / w);
        let (lo, hi) = (s, s + 1.0 / z);
        view = match part {
            1 => {
                let lo = (lo + dx).clamp(0.0, hi - 1.0 / max);
                clamp_view(1.0 / (hi - lo), lo, max)
            }
            2 => {
                let hi = (hi + dx).clamp(lo + 1.0 / max, 1.0);
                clamp_view(1.0 / (hi - lo), lo, max)
            }
            _ => clamp_view(z, s + dx, max),
        };
    } else if resp.clicked()
        && let Some(p0) = resp.interact_pointer_pos()
        && !thumb.contains(p0)
    {
        // a click beside the thumb centres the view there
        view = clamp_view(z, f64::from((p0.x - bar.min.x) / w) - 0.5 / z, max);
    }
    if resp.double_clicked() {
        view = (1.0, 0.0);
    }
    if view != (z, s) {
        app.ui.effect_controls_zoom = view.0 as f32;
        app.ui.effect_controls_view = view.1 as f32;
    }
    let col = if resp.hovered() || resp.dragged() { t.text_dim } else { t.text_faint };
    p.rect_filled(thumb, bar.height() / 2.0, col.gamma_multiply(0.6));
    p.rect_stroke(thumb, bar.height() / 2.0, Stroke::new(1.0, col), egui::StrokeKind::Inside);
    app.auto.add("effectControls.zoomBar", bar, "Keyframe area zoom");
    app.auto.add("effectControls.zoomBar.thumb", thumb, &format!("{:.0}%", z * 100.0));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_stay_inside_the_clip() {
        assert_eq!(clamp_view(0.2, -3.0, 50.0), (1.0, 0.0));
        assert_eq!(clamp_view(4.0, 0.9, 50.0), (4.0, 0.75));
        assert_eq!(clamp_view(1e9, 0.5, 50.0).0, 50.0);
        assert_eq!(clamp_view(f64::NAN, f64::INFINITY, f64::NAN), (1.0, 0.0));
        assert_eq!(max_zoom(4.0), 1.0);
        assert_eq!(max_zoom(800.0), 100.0);
        assert_eq!(max_zoom(f64::NAN), 1.0);
    }

    #[test]
    fn zooming_keeps_the_point_under_the_pointer() {
        // the point a quarter across the view stays a quarter across
        let (z, s) = zoom_about(2.0, 0.25, 2.0, 0.25, 100.0);
        assert_eq!(z, 4.0);
        let before = 0.25 + 0.25 / 2.0;
        assert!((s + 0.25 / z - before).abs() < 1e-12);
        // zooming out all the way shows the whole clip
        assert_eq!(zoom_about(4.0, 0.5, 0.001, 0.9, 100.0), (1.0, 0.0));
        // hostile factors and anchors
        let v = zoom_about(2.0, 0.1, f64::NAN, f64::NEG_INFINITY, 10.0);
        assert!(v.0.is_finite() && v.1.is_finite());
    }
}
