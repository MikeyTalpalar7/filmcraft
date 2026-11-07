//! VR Video view of the monitors: an equirectangular (360°) picture shown as a view that the
//! user looks around in. It is the projection of the VR Reframe effect ([`filmcraft_render::vfx::view_dir`]):
//! a normal perspective view up to 90°, easing into a stereographic one as it zooms out (up to
//! 359°), so looking straight down at a wide view gives a little planet and straight up a rabbit
//! hole (`view.vrVideo.littlePlanet` / `view.vrVideo.rabbitHole`).
//!
//! The frame texture is drawn as a tessellated mesh: every grid vertex of the view is turned
//! into a direction on the sphere and from there into equirectangular texture coordinates. Triangles that cross the ±180° seam are split there, so
//! the texture needs no wrapping. Stereoscopic media shows its left eye ([`Spherical::eye_region`]).
//!
//! Drag in the picture to look around, scroll to change the field of view, double-click to go
//! back to the initial view. State is [`VrView`] (serde: `ui.set {"program": {"vr": {…}}}`),
//! toggled with `view.vrVideo` (wrench menu: `<monitor>.settings.vrVideo`).

use egui::epaint::{Mesh, Vertex};
use egui::{Color32, Painter, Pos2, Rect, Response, Shape, TextureId, pos2};
use filmcraft_media::Spherical;

use crate::state::VrView;
use crate::theme::Tokens;

pub const DEFAULT_FOV: f32 = 90.0;
pub const MIN_FOV: f32 = 30.0;
pub const MAX_FOV: f32 = filmcraft_render::vfx::MAX_VIEW_FOV as f32;
/// The field of view of the little planet and rabbit hole presets.
pub const PLANET_FOV: f32 = 270.0;
/// Mesh resolution (cells across and down): fine enough for the strong curvature of wide views.
const COLS: usize = 96;
const ROWS: usize = 64;

/// Equirectangular texture coordinates (u 0…1 = longitude −180°…180°, v 0…1 = latitude 90°…−90°)
/// seen at the view point (`nx`, `ny`), each −1…1 across the view (y up), for a view `aspect`
/// (width / height). `None` past the edge of the sphere (the corners of the widest views).
pub fn view_uv(view: &VrView, aspect: f32, nx: f32, ny: f32) -> Option<(f32, f32)> {
    use filmcraft_render::vfx::{equirect_uv, rotate_view, view_dir, view_rotation};
    let finite = |v: f32, or: f32| if v.is_finite() { v } else { or };
    let aspect = if aspect.is_finite() && aspect > 0.0 { aspect } else { 1.0 };
    let fov = finite(view.fov, DEFAULT_FOV).clamp(MIN_FOV, MAX_FOV);
    let d = view_dir(f64::from(nx), f64::from(ny / aspect), f64::from(fov))?;
    let r = view_rotation(f64::from(wrap_yaw(view.yaw)), f64::from(finite(view.pitch, 0.0).clamp(-90.0, 90.0)), 0.0);
    let (u, v) = equirect_uv(rotate_view(&r, d));
    Some((u as f32, v as f32))
}

/// One vertex of a view triangle: screen position and unwrapped equirectangular uv.
#[derive(Clone, Copy, Debug)]
struct V {
    pos: Pos2,
    u: f32,
    v: f32,
}

fn lerp(a: V, b: V, t: f32) -> V {
    V { pos: a.pos + (b.pos - a.pos) * t, u: a.u + (b.u - a.u) * t, v: a.v + (b.v - a.v) * t }
}

/// The part of a convex polygon on one side of `u = c` (`keep_below`: u ≤ c).
fn clip(poly: &[V], c: f32, keep_below: bool) -> Vec<V> {
    let inside = |p: &V| if keep_below { p.u <= c } else { p.u >= c };
    let mut out = Vec::with_capacity(poly.len() + 2);
    for (i, a) in poly.iter().enumerate() {
        let Some(b) = poly.get((i + 1) % poly.len()) else { continue };
        if inside(a) {
            out.push(*a);
        }
        if inside(a) != inside(b) {
            let d = b.u - a.u;
            if d.abs() > f32::EPSILON {
                out.push(lerp(*a, *b, (c - a.u) / d));
            }
        }
    }
    out
}

/// A view triangle split at the seam into polygons whose u is all within 0…1.
fn split_seam(tri: [V; 3]) -> Vec<Vec<V>> {
    // unwrap the corners next to the first, so the triangle doesn't span the whole texture
    let mut t = tri;
    for k in 1..3 {
        let d = t[k].u - t[0].u;
        if d > 0.5 {
            t[k].u -= 1.0;
        } else if d < -0.5 {
            t[k].u += 1.0;
        }
    }
    let shift = |p: Vec<V>, by: f32| p.into_iter().map(|v| V { u: v.u + by, ..v }).collect::<Vec<_>>();
    let (lo, hi) = t.iter().fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(v.u), hi.max(v.u)));
    if hi > 1.0 {
        vec![clip(&t, 1.0, true), shift(clip(&t, 1.0, false), -1.0)]
    } else if lo < 0.0 {
        vec![clip(&t, 0.0, false), shift(clip(&t, 0.0, true), 1.0)]
    } else {
        vec![t.to_vec()]
    }
}

/// The view's mesh over `rect`, texturing from the `eye` region (x, y, w, h fractions) of `tex`.
pub fn mesh(tex: TextureId, rect: Rect, view: &VrView, eye: [f32; 4]) -> Mesh {
    let mut m = Mesh::with_texture(tex);
    if !(rect.width() > 0.0 && rect.height() > 0.0) {
        return m;
    }
    let aspect = rect.width() / rect.height();
    let grid: Vec<Option<V>> = (0..=ROWS)
        .flat_map(|r| (0..=COLS).map(move |c| (c, r)))
        .map(|(c, r)| {
            let (fx, fy) = (c as f32 / COLS as f32, r as f32 / ROWS as f32);
            let (u, v) = view_uv(view, aspect, fx * 2.0 - 1.0, 1.0 - fy * 2.0)?;
            Some(V { pos: pos2(rect.min.x + fx * rect.width(), rect.min.y + fy * rect.height()), u, v })
        })
        .collect();
    // (cells with a corner past the sphere are left out)
    let at = |c: usize, r: usize| grid.get(r * (COLS + 1) + c).copied().flatten();
    let [ex, ey, ew, eh] = eye;
    for r in 0..ROWS {
        for c in 0..COLS {
            let (Some(a), Some(b), Some(d), Some(e)) = (at(c, r), at(c + 1, r), at(c, r + 1), at(c + 1, r + 1)) else { continue };
            for tri in [[a, b, e], [a, e, d]] {
                for poly in split_seam(tri) {
                    let Ok(base) = u32::try_from(m.vertices.len()) else { return m };
                    for p in &poly {
                        let uv = pos2(ex + p.u.clamp(0.0, 1.0) * ew, ey + p.v.clamp(0.0, 1.0) * eh);
                        m.vertices.push(Vertex { pos: p.pos, uv, color: Color32::WHITE });
                    }
                    // the clipped pieces are convex: fan them
                    for k in 1..poly.len().saturating_sub(1) as u32 {
                        m.indices.extend_from_slice(&[base, base + k, base + k + 1]);
                    }
                }
            }
        }
    }
    m
}

/// How much more detail of the frame the view needs than a picture of the whole frame the same
/// size: it spreads `fov` of the frame's 360° across the picture (1 when off, or `shown` is false).
pub fn detail_zoom(view: &VrView, shown: bool) -> f32 {
    if !(view.enabled && shown) || !view.fov.is_finite() {
        return 1.0;
    }
    (360.0 / view.fov.max(1.0)).max(1.0)
}

/// A wide view looking straight down (`up` false: a little planet, the scene seen from outside)
/// or straight up (a rabbit hole, seen from inside), keeping the heading.
pub fn planet(view: &VrView, up: bool) -> VrView {
    VrView { enabled: true, yaw: view.yaw, pitch: if up { 90.0 } else { -90.0 }, fov: PLANET_FOV }
}

/// The view's starting point: the media's initial pose, else straight ahead.
pub fn initial(meta: Option<&Spherical>, fov: f32) -> VrView {
    let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
    let (yaw, pitch) = meta.map(|s| (finite(s.yaw), finite(s.pitch))).unwrap_or_default();
    VrView { enabled: true, yaw: wrap_yaw(yaw), pitch: pitch.clamp(-90.0, 90.0), fov }
}

/// Yaw folded into −180°…180°.
pub fn wrap_yaw(yaw: f32) -> f32 {
    if !yaw.is_finite() {
        return 0.0;
    }
    let y = (yaw + 180.0).rem_euclid(360.0) - 180.0;
    if y.is_finite() { y } else { 0.0 }
}

/// Paints the view of `tex` into `rect`, with its heading readout.
pub fn paint(painter: &Painter, tex: TextureId, rect: Rect, view: &VrView, meta: Option<&Spherical>, tokens: &Tokens) {
    let eye = meta.map(Spherical::eye_region).unwrap_or([0.0, 0.0, 1.0, 1.0]);
    painter.add(Shape::mesh(mesh(tex, rect, view, eye)));
    let label = format!("VR  {:.0}°  {:.0}°  FOV {:.0}°", view.yaw, view.pitch, view.fov);
    // on a backing: the picture under it can be any colour
    let galley = painter.layout_no_wrap(label, Tokens::ui(10.0), tokens.text);
    let at = pos2(rect.min.x + 8.0, rect.max.y - 6.0 - galley.size().y);
    painter.rect_filled(Rect::from_min_size(at, galley.size()).expand2(egui::vec2(5.0, 2.0)), 3.0, Color32::from_black_alpha(160));
    painter.galley(at, galley, tokens.text);
}

/// Look-around input on the picture: drag turns the view (the point under the pointer follows
/// it), the wheel changes the field of view, a double-click goes back to the initial view.
pub fn input(ui: &egui::Ui, resp: &Response, rect: Rect, view: &mut VrView, meta: Option<&Spherical>) {
    if rect.width() <= 0.0 {
        return;
    }
    if resp.double_clicked() {
        *view = initial(meta, DEFAULT_FOV);
        return;
    }
    if resp.dragged_by(egui::PointerButton::Primary) {
        let d = resp.drag_delta();
        let per_point = view.fov / rect.width();
        view.yaw = wrap_yaw(view.yaw - d.x * per_point);
        view.pitch = (view.pitch + d.y * per_point).clamp(-90.0, 90.0);
    }
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 && scroll.is_finite() {
            view.fov = (view.fov * (-scroll * 0.002).exp()).clamp(MIN_FOV, MAX_FOV);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look(yaw: f32, pitch: f32) -> VrView {
        VrView { enabled: true, yaw, pitch, fov: 90.0 }
    }

    #[test]
    fn centre_of_the_view_is_where_it_looks() {
        let close = |uv: Option<(f32, f32)>, (eu, ev): (f32, f32)| uv.is_some_and(|(u, v)| (u - eu).abs() < 1e-4 && (v - ev).abs() < 1e-4);
        assert!(close(view_uv(&look(0.0, 0.0), 1.0, 0.0, 0.0), (0.5, 0.5)));
        assert!(close(view_uv(&look(90.0, 0.0), 1.0, 0.0, 0.0), (0.75, 0.5)));
        assert!(close(view_uv(&look(-90.0, 0.0), 1.0, 0.0, 0.0), (0.25, 0.5)));
        assert!(close(view_uv(&look(0.0, 45.0), 1.0, 0.0, 0.0), (0.5, 0.25)));
        // the right edge of a 90° view is 45° to the right
        assert!(close(view_uv(&look(0.0, 0.0), 1.0, 1.0, 0.0), (0.625, 0.5)));
    }

    #[test]
    fn planets_look_at_the_poles_and_wide_views_fill_the_picture() {
        let base = look(30.0, 0.0);
        // little planet: the nadir in the middle; rabbit hole: the zenith
        assert!(view_uv(&planet(&base, false), 1.0, 0.0, 0.0).is_some_and(|(_, v)| v > 0.999));
        assert!(view_uv(&planet(&base, true), 1.0, 0.0, 0.0).is_some_and(|(_, v)| v < 0.001));
        // zoomed all the way out the corners still reach the sphere at 16:9 (no black corners)
        let wide = VrView { fov: MAX_FOV, ..base };
        assert!(view_uv(&wide, 16.0 / 9.0, 1.0, 9.0 / 16.0).is_some());
        let m = mesh(TextureId::default(), Rect::from_min_max(pos2(0.0, 0.0), pos2(320.0, 180.0)), &planet(&base, false), [0.0, 0.0, 1.0, 1.0]);
        assert!(m.indices.len() >= COLS * ROWS * 6);
    }

    #[test]
    fn seam_triangles_are_split_inside_the_texture() {
        // looking backwards puts the ±180° seam in the middle of the view
        let m = mesh(TextureId::default(), Rect::from_min_max(pos2(0.0, 0.0), pos2(400.0, 200.0)), &look(180.0, 0.0), [0.0, 0.0, 1.0, 1.0]);
        assert!(!m.indices.is_empty());
        assert!(m.indices.iter().all(|&i| (i as usize) < m.vertices.len()));
        for t in m.indices.chunks(3) {
            let us: Vec<f32> = t.iter().filter_map(|&i| m.vertices.get(i as usize)).map(|v| v.uv.x).collect();
            let spread = us.iter().cloned().fold(f32::MIN, f32::max) - us.iter().cloned().fold(f32::MAX, f32::min);
            assert!(spread < 0.5, "a triangle stretches across the texture: {us:?}");
        }
    }

    #[test]
    fn hostile_views_never_panic_or_produce_nan() {
        for v in [
            VrView { enabled: true, yaw: f32::NAN, pitch: f32::INFINITY, fov: f32::NAN },
            VrView { enabled: true, yaw: 1e30, pitch: -1e30, fov: 0.0 },
            look(0.0, 90.0),
            look(0.0, -90.0),
        ] {
            let m = mesh(TextureId::default(), Rect::from_min_max(pos2(0.0, 0.0), pos2(64.0, 64.0)), &v, [0.0, 0.0, 1.0, 0.5]);
            assert!(m.vertices.iter().all(|x| x.uv.x.is_finite() && x.uv.y.is_finite()), "{v:?}");
        }
        assert!(mesh(TextureId::default(), Rect::NOTHING, &look(0.0, 0.0), [0.0; 4]).indices.is_empty());
        assert_eq!(wrap_yaw(540.0), -180.0);
        assert_eq!(wrap_yaw(-90.0), -90.0);
        assert_eq!(wrap_yaw(f32::NAN), 0.0);
    }
}
