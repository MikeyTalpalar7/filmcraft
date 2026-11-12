//! Immersive Video (VR) effects on equirectangular frames.
//!
//! Every effect treats each view (the whole frame, or the two halves of an over/under or
//! side-by-side stereoscopic layout) as a full sphere: x is longitude −180°…180° (wrapping), y is
//! latitude 90°…−90°. Filters are seam-free across the ±180° edge and widen horizontally towards
//! the poles (an equirectangular row at latitude φ covers cos φ of the equator's length).

use std::f64::consts::{PI, TAU};

use filmcraft_project::EffectInstance;
use rayon::prelude::*;

use super::{FxCtx, Image, add_light, blend_simple, bv, chv, cv, fbm, fv, hash01, highlights, lerp3, lin, luma, smoothstep};

type V3 = [f64; 3];

#[inline]
fn dir(lon: f64, lat: f64) -> V3 {
    [lat.cos() * lon.sin(), lat.sin(), lat.cos() * lon.cos()]
}
#[inline]
fn lonlat(d: V3) -> (f64, f64) {
    (d[0].atan2(d[2]), d[1].clamp(-1.0, 1.0).asin())
}
#[inline]
fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
#[inline]
fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

type M3 = [[f64; 3]; 3];
fn mmul(a: M3, b: M3) -> M3 {
    let mut o = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            o[i][j] = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    o
}
fn mvec(m: &M3, v: V3) -> V3 {
    [dot(m[0], v), dot(m[1], v), dot(m[2], v)]
}
fn transpose(m: M3) -> M3 {
    [[m[0][0], m[1][0], m[2][0]], [m[0][1], m[1][1], m[2][1]], [m[0][2], m[1][2], m[2][2]]]
}
/// Rotation from tilt (about X), pan (about Y) and roll (about Z), degrees: R = Ry · Rx · Rz.
fn rotation(tilt: f64, pan: f64, roll: f64) -> M3 {
    let (sx, cx) = tilt.to_radians().sin_cos();
    let (sy, cy) = pan.to_radians().sin_cos();
    let (sz, cz) = roll.to_radians().sin_cos();
    let rx = [[1.0, 0.0, 0.0], [0.0, cx, -sx], [0.0, sx, cx]];
    let ry = [[cy, 0.0, sy], [0.0, 1.0, 0.0], [-sy, 0.0, cy]];
    let rz = [[cz, -sz, 0.0], [sz, cz, 0.0], [0.0, 0.0, 1.0]];
    mmul(ry, mmul(rx, rz))
}

#[inline]
fn px_to_lonlat(x: f64, y: f64, w: usize, h: usize) -> (f64, f64) {
    ((x / w as f64 - 0.5) * TAU, (0.5 - y / h as f64) * PI)
}

/// Bilinear sample with horizontal wrap (vertical clamp), pixel centres at +0.5.
fn sample_wrap(img: &Image, x: f64, y: f64) -> [f32; 4] {
    let (w, h) = (img.w as isize, img.h as isize);
    let fx = x - 0.5;
    let fy = (y - 0.5).clamp(0.0, (h - 1) as f64);
    let (x0, y0) = (fx.floor(), fy.floor());
    let (tx, ty) = ((fx - x0) as f32, (fy - y0) as f32);
    let (x0, y0) = (x0 as isize, y0 as isize);
    let g = |xx: isize, yy: isize| img.get(xx.rem_euclid(w) as usize, yy.clamp(0, h - 1) as usize);
    let (a, b, c, d) = (g(x0, y0), g(x0 + 1, y0), g(x0, y0 + 1), g(x0 + 1, y0 + 1));
    let mut o = [0.0; 4];
    for k in 0..4 {
        let top = a[k] + (b[k] - a[k]) * tx;
        let bot = c[k] + (d[k] - c[k]) * tx;
        o[k] = top + (bot - top) * ty;
    }
    o
}
fn sample_dir(img: &Image, d: V3) -> [f32; 4] {
    let (lon, lat) = lonlat(d);
    sample_wrap(img, (lon / TAU + 0.5) * img.w as f64, (0.5 - lat / PI) * img.h as f64)
}

/// Widest view [`view_dir`] draws, degrees (a whole turn would put the antipode on every edge).
pub const MAX_VIEW_FOV: f64 = 359.0;

/// How a flat view maps the sphere ([`lens_dir`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Lens {
    /// Straight when narrow, curving just enough to keep shapes in proportion as the view widens,
    /// and [`Lens::Curve`]`(1.0)` by 180° ([`view_dir`]).
    Auto,
    /// The general perspective projection from a point `d` (0…1) behind the centre of the sphere:
    /// 0 is straight (rectilinear: straight lines stay straight, up to 179°), 1 curved
    /// (stereographic: round shapes stay round, the whole sphere but its antipode).
    Curve(f64),
    /// Equidistant fisheye: the distance from the centre is the angle from the view axis.
    Fisheye,
    /// Equisolid (a mirror ball's reflection): at 360° the whole sphere fills a disc.
    MirrorBall,
    /// Cylindrical panorama strip: longitude across, verticals stay vertical.
    Panorama,
}

/// Shaping applied with a [`Lens`]: `twist` (degrees at the view's edge, turning with the
/// distance from the centre), `stretch` (−100…100: wider or taller, keeping the area) and
/// `center` (−100…100: the middle of the view, a little planet's ground, made bigger or smaller
/// against its surroundings).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Shape {
    pub twist: f64,
    pub stretch: f64,
    pub center: f64,
}

/// The direction seen at a point of a flat view of the sphere, looking down +Z with +Y up: `x`
/// runs −1…1 across the view's width, `y` (up) is scaled the same (−h/w…h/w), `fov` is the
/// horizontal field of view in degrees (1…[`MAX_VIEW_FOV`]).
///
/// Narrow views are rectilinear (pinhole); as the view widens it curves just enough to keep
/// shapes near its edge in proportion ([`auto_curve`]), and by 180° it is the
/// stereographic projection, which reaches the whole sphere but its antipode, so the view keeps
/// zooming out without running off the picture: pointed at the nadir that is a "little planet"
/// (outside in), at the zenith a "rabbit hole" (inside out). `None` past the sphere.
pub fn view_dir(x: f64, y: f64, fov: f64) -> Option<[f64; 3]> {
    lens_dir(x, y, fov, Lens::Auto, Shape::default())
}

/// [`view_dir`] through any [`Lens`], with [`Shape`]. `None` past what the lens reaches (the
/// corners of a wide fisheye or mirror ball).
pub fn lens_dir(x: f64, y: f64, fov: f64, lens: Lens, shape: Shape) -> Option<[f64; 3]> {
    if !(x.is_finite() && y.is_finite()) {
        return None;
    }
    let fin = |v: f64, lo: f64, hi: f64| if v.is_finite() { v.clamp(lo, hi) } else { 0.0 };
    let fov = if fov.is_finite() { fov.clamp(1.0, 360.0) } else { 90.0 };
    // Stretch (area-preserving), then Twist (a swirl growing towards the edge)
    let k = 2f64.powf(fin(shape.stretch, -100.0, 100.0) / 100.0).sqrt();
    let (x, y) = (x / k, y * k);
    let r = x.hypot(y);
    let (sa, ca) = (fin(shape.twist, -3600.0, 3600.0).to_radians() * r).sin_cos();
    let (x, y) = (x * ca - y * sa, x * sa + y * ca);
    let d = match lens {
        Lens::Panorama => {
            let half = (fov.min(MAX_VIEW_FOV) * 0.5).to_radians();
            return Some(center_size(dir(x * half, (y * half).atan()), shape.center));
        }
        Lens::Fisheye => radial(x, y, (fov * 0.5).to_radians().min(PI), Some),
        Lens::MirrorBall => {
            let edge = 2.0 * (fov * 0.25).to_radians().sin();
            radial(x, y, edge, |rho| ((rho * 0.5) <= 1.0).then(|| 2.0 * (rho * 0.5).asin()))
        }
        Lens::Auto => general(x, y, fov.min(MAX_VIEW_FOV), auto_curve(fov)),
        Lens::Curve(d) => {
            let d = fin(d, 0.0, 1.0);
            // the widest view this curve reaches (2·acos(−d)), less a degree
            let widest = (2.0 * (-d).acos().to_degrees() - 1.0).min(MAX_VIEW_FOV);
            general(x, y, fov.min(widest), d)
        }
    }?;
    Some(center_size(d, shape.center))
}

/// How much shapes may widen at the edge of an Automatic view: 15% (the ratio of the radial to
/// the sideways magnification there).
const AUTO_EDGE_STRETCH: f64 = 1.15;

/// The curve (`d` of [`general`]) of an Automatic view `fov` degrees wide: straight while that
/// keeps shapes at the view's edge within [`AUTO_EDGE_STRETCH`] (up to about 60°), then just
/// curved enough to hold them there, and fully curved (stereographic) by 180°.
///
/// A straight (rectilinear) view widens shapes at its edge by 1 / cos θ (θ is the angle out from
/// the view's centre): 41% at 90°, double at 120°, so zooming a straight view out stretches
/// people near the edges. The general perspective projection widens them by
/// (1 + d cos θ) / (d + cos θ); the smallest `d` that keeps that within the limit at the edge is
/// (1 − A cos θ) / (A − cos θ).
fn auto_curve(fov: f64) -> f64 {
    let c = (fov.clamp(0.0, 360.0) * 0.5).to_radians().cos();
    let a = AUTO_EDGE_STRETCH;
    let natural = if fov >= 180.0 { 1.0 } else { ((1.0 - a * c) / (a - c)).clamp(0.0, 1.0) };
    natural.max(f64::from(smoothstep(90.0, 180.0, fov as f32)))
}

/// A radially symmetric lens: `theta` turns the image-plane radius (the view's edge at `edge`)
/// into the angle from the view axis.
fn radial(x: f64, y: f64, edge: f64, theta: impl Fn(f64) -> Option<f64>) -> Option<V3> {
    let r = x.hypot(y);
    if r < 1e-12 {
        return Some([0.0, 0.0, 1.0]);
    }
    let t = theta(r * edge).filter(|t| t.is_finite() && *t <= PI + 1e-9)?;
    let st = t.sin();
    Some([st * x / r, st * y / r, t.cos()])
}

/// The general perspective projection from `d` behind the centre: ρ = (d + 1) sin θ / (d + cos θ).
fn general(x: f64, y: f64, fov: f64, d: f64) -> Option<V3> {
    let half = (fov * 0.5).to_radians();
    let k = d + 1.0;
    let denom = d + half.cos();
    if denom <= 1e-9 {
        return None;
    }
    // ρ (d + cos θ) = k sin θ  ⇒  θ = atan2(ρ, k) + asin(ρ d / √(k² + ρ²))
    radial(x, y, k * half.sin() / denom, |rho| {
        let s = rho * d / k.hypot(rho);
        (s <= 1.0).then(|| rho.atan2(k) + s.asin())
    })
}

/// Planet Size: the angle from the view axis bent by a power curve (θ' = π (θ/π)^γ), so the
/// middle of the view takes more (positive) or less of it.
fn center_size(d: V3, amount: f64) -> V3 {
    if !amount.is_finite() || amount.abs() < 1e-9 {
        return d;
    }
    let gamma = 2f64.powf(amount.clamp(-100.0, 100.0) / 50.0);
    let t = d[2].clamp(-1.0, 1.0).acos();
    let r = d[0].hypot(d[1]);
    if r < 1e-12 {
        return d;
    }
    let t2 = PI * (t / PI).powf(gamma);
    let st = t2.sin();
    [st * d[0] / r, st * d[1] / r, t2.cos()]
}

/// The camera rotation of a view: roll about the view axis, then tilt (positive looks up), then
/// pan (positive looks right), degrees.
pub fn view_rotation(pan: f64, tilt: f64, roll: f64) -> [[f64; 3]; 3] {
    rotation(-tilt, pan, roll)
}

/// Apply a [`view_rotation`] to a view direction (world direction = R · view direction).
pub fn rotate(r: &[[f64; 3]; 3], d: [f64; 3]) -> [f64; 3] {
    mvec(r, d)
}

/// Equirectangular coordinates (u 0…1 = longitude −180°…180°, v 0…1 = latitude 90°…−90°) of a
/// direction.
pub fn equirect_uv(d: [f64; 3]) -> (f64, f64) {
    let (lon, lat) = lonlat(d);
    (lon / TAU + 0.5, 0.5 - lat / PI)
}

/// A VR Reframe Lens option.
fn lens_of(choice: u32, curve: f64) -> Lens {
    match choice {
        1 => Lens::Curve(0.0),
        2 => Lens::Curve(1.0),
        3 => Lens::Fisheye,
        4 => Lens::MirrorBall,
        5 => Lens::Panorama,
        6 => Lens::Curve(curve / 100.0),
        _ => Lens::Auto,
    }
}

/// A VR Reframe View option's tilt and field of view: Custom, Little Planet (looking straight
/// down), Rabbit Hole (straight up).
fn view_of(choice: u32, fov: f64, planet_fov: f64) -> (f64, f64) {
    match choice {
        1 => (-90.0, planet_fov),
        2 => (90.0, planet_fov),
        _ => (0.0, fov),
    }
}

/// The direction seen at (`x`, `y`) part-way (`f` 0…1) through a morph from lens `a` to lens
/// `b`, and how much of the picture is there (a lens that can't reach the point fades out).
fn morph_dir(x: f64, y: f64, fov: f64, (a, b, f): (Lens, Lens, f64), shape: Shape) -> Option<(V3, f32)> {
    if f <= 0.0 || a == b {
        return lens_dir(x, y, fov, a, shape).map(|d| (d, 1.0));
    }
    // a straight lens can't show a view as wide as a planet (it pinches to a point well before
    // 180°): mid-morph, while the view widens, it curves at least as much as Automatic would
    let fit = |l: Lens| match l {
        Lens::Curve(d) => Lens::Curve(d.max(auto_curve(fov))),
        other => other,
    };
    let (a, b) = (fit(a), fit(b));
    match (lens_dir(x, y, fov, a, shape), lens_dir(x, y, fov, b, shape)) {
        (Some(da), Some(db)) => {
            let m = [0, 1, 2].map(|i| da[i] + (db[i] - da[i]) * f);
            let n = dot(m, m).sqrt();
            // opposite directions (a quarter of the antipode): take the nearer lens
            if n < 1e-6 { Some((if f < 0.5 { da } else { db }, 1.0)) } else { Some((m.map(|c| c / n), 1.0)) }
        }
        (Some(da), None) => Some((da, (1.0 - f) as f32)),
        (None, Some(db)) => Some((db, f as f32)),
        (None, None) => None,
    }
}

/// One VR Reframe view (A, or the split screen's B) at a moment: where it looks, how wide, and
/// through which lens (or part-way between two).
struct Cam {
    r: M3,
    fov: f64,
    lenses: (Lens, Lens, f64),
}

/// The parameter ids of a view: View, Lens, Field of View, Planet Field of View, Pan, Tilt, Roll.
const CAM_A: [&str; 7] = ["view", "lens", "fov", "planet_fov", "pan", "tilt", "roll"];
/// The split screen's second view (its Field of View sizes its planets too).
const CAM_B: [&str; 7] = ["view2", "lens2", "fov2", "fov2", "pan2", "tilt2", "roll2"];

fn cam(e: &EffectInstance, cx: &FxCtx, ids: &[&str; 7]) -> Cam {
    use super::reframe_motion::{choice_morph, reframe_value};
    // (keyframe and pasted values are untrusted: reframe_value is always finite)
    let deg = |id| reframe_value(e, id, cx.t);
    let [view, lens, fov, planet_fov, pan, tilt, roll] = *ids;
    let (va, vb, vf) = choice_morph(e, view, cx.t, 0);
    let ((ta, fa), (tb, fb)) = (view_of(va, deg(fov), deg(planet_fov)), view_of(vb, deg(fov), deg(planet_fov)));
    let (base_tilt, fov) = (ta + (tb - ta) * vf, fa + (fb - fa) * vf);
    let r = view_rotation(deg(pan), base_tilt + deg(tilt), deg(roll));
    let (la, lb, lf) = choice_morph(e, lens, cx.t, 0);
    Cam { r, fov, lenses: (lens_of(la, deg("curve")), lens_of(lb, deg("curve")), lf) }
}

/// The colour `cam` sees at (`nx`, `ny`) of its view (x −1…1 across, y scaled the same).
fn shade(src: &Image, cam: &Cam, shape: Shape, nx: f64, ny: f64) -> [f32; 4] {
    match morph_dir(nx, ny, cam.fov, cam.lenses, shape) {
        Some((d, a)) => sample_dir(src, mvec(&cam.r, d)).map(|c| c * a),
        None => [0.0; 4],
    }
}

/// The split screen at a moment: `axis` 1 top and bottom, 2 side by side; the divider at `at`
/// (0…1 along the axis) softened over `soft` either side; which view is first (`order`: 0 A, 1
/// B, part-way while they swap).
struct Split {
    axis: u32,
    at: f64,
    soft: f64,
    order: (u32, u32, f64),
}

/// The colour at (`fx`, `fy`), 0…1 across the frame `aspect` (height / width) high, of the
/// split screen with views `a` and `b`.
fn split_px(src: &Image, (a, b): (&Cam, &Cam), sp: &Split, shape: Shape, (fx, fy): (f64, f64), aspect: f64) -> [f32; 4] {
    let along = if sp.axis == 1 { fy } else { fx };
    let first = if sp.soft > 0.0 {
        1.0 - f64::from(smoothstep((sp.at - sp.soft) as f32, (sp.at + sp.soft) as f32, along as f32))
    } else {
        f64::from(u8::from(along < sp.at))
    };
    // a region's own view coordinates: x −1…1 across its width
    let local = |lo: f64, hi: f64| {
        let span = (hi - lo).max(1e-6);
        if sp.axis == 1 {
            (fx * 2.0 - 1.0, (1.0 - (fy - lo) / span * 2.0) * aspect * span)
        } else {
            ((fx - lo) / span * 2.0 - 1.0, (1.0 - fy * 2.0) * aspect / span)
        }
    };
    let mix = |order: u32| {
        let (c1, c2) = if order == 0 { (a, b) } else { (b, a) };
        let mut o = [0.0f32; 4];
        for (cam, lo, hi, w) in [(c1, 0.0, sp.at, first), (c2, sp.at, 1.0, 1.0 - first)] {
            if w > 0.0 {
                let (nx, ny) = local(lo, hi);
                let c = shade(src, cam, shape, nx, ny);
                for k in 0..4 {
                    o[k] += c[k] * w as f32;
                }
            }
        }
        o
    };
    let (oa, ob, f) = sp.order;
    let ca = mix(oa);
    if f <= 0.0 || oa == ob {
        return ca;
    }
    let cb = mix(ob);
    [0, 1, 2, 3].map(|k| ca[k] + (cb[k] - ca[k]) * f as f32)
}

/// VR Reframe: a flat view of the sphere (a normal shot, a wide view, a little planet or a rabbit
/// hole) that fills the sequence frame (the whole layer without a sequence, and for each eye of
/// a stereoscopic layout). The layer is centred in the sequence 1:1 by Motion's defaults, so the
/// view covers the sequence frame's rectangle in layer pixels and is transparent outside it.
///
/// The numbers move through their keyframes as [`super::reframe_motion`] says, and a change of
/// View or Lens morphs over the time between its keyframes.
///
/// Split Screen shows a second view (B, with its own View, Lens, Field of View and aim) under or
/// beside the first, each filling its part of the frame; the Order can be keyframed to swap them.
fn reframe(img: &mut Image, e: &EffectInstance, cx: &FxCtx, layout: u32) {
    use super::reframe_motion::{choice_morph, reframe_value};
    let deg = |id| reframe_value(e, id, cx.t);
    let target = match (layout, cx.env) {
        (0, Some(env)) => {
            let (sw, sh) = env.sequence_size();
            let k = img.w as f64 / env.source_size().0.max(1) as f64;
            Some((sw as f64 * k, sh as f64 * k))
        }
        _ => None,
    };
    let a = cam(e, cx, &CAM_A);
    let shape = Shape { twist: deg("twist"), stretch: deg("stretch"), center: deg("planet_size") };
    let split = match chv(e, "split") {
        axis @ (1 | 2) => Some((
            cam(e, cx, &CAM_B),
            Split {
                axis,
                at: (deg("split_position") / 100.0).clamp(0.05, 0.95),
                soft: (deg("split_feather") / 100.0).clamp(0.0, 0.25),
                order: choice_morph(e, "split_order", cx.t, 0),
            },
        )),
        _ => None,
    };
    per_view(img, layout, |v| {
        let src = v.clone();
        let (w, h) = (v.w, v.h);
        let (tw, th) = target.filter(|(tw, th)| *tw >= 1.0 && *th >= 1.0).unwrap_or((w as f64, h as f64));
        let (x0, y0) = ((w as f64 - tw) * 0.5, (h as f64 - th) * 0.5);
        v.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (fx, fy) = ((x as f64 + 0.5 - x0) / tw, (y as f64 + 0.5 - y0) / th);
                *px = if !((0.0..=1.0).contains(&fx) && (0.0..=1.0).contains(&fy)) {
                    [0.0; 4]
                } else if let Some((b, sp)) = &split {
                    split_px(&src, (&a, b), sp, shape, (fx, fy), th / tw)
                } else {
                    shade(&src, &a, shape, fx * 2.0 - 1.0, (1.0 - fy * 2.0) * th / tw)
                };
            }
        });
    });
}

/// Run `f` on each view of the frame layout.
fn per_view(img: &mut Image, layout: u32, f: impl Fn(&mut Image)) {
    let (w, h) = (img.w, img.h);
    let views: Vec<(usize, usize, usize, usize)> = match layout {
        1 if h >= 2 => vec![(0, 0, w, h / 2), (0, h / 2, w, h - h / 2)],
        2 if w >= 2 => vec![(0, 0, w / 2, h), (w / 2, 0, w - w / 2, h)],
        _ => vec![(0, 0, w, h)],
    };
    if views.len() == 1 {
        f(img);
        return;
    }
    for (x0, y0, vw, vh) in views {
        let mut v = Image::new(vw, vh);
        for y in 0..vh {
            let s = ((y0 + y) * w + x0) * 4;
            v.px[y * vw * 4..(y + 1) * vw * 4].copy_from_slice(&img.px[s..s + vw * 4]);
        }
        f(&mut v);
        for y in 0..vh {
            let s = ((y0 + y) * w + x0) * 4;
            img.px[s..s + vw * 4].copy_from_slice(&v.px[y * vw * 4..(y + 1) * vw * 4]);
        }
    }
}

/// Three-pass box approximation of a Gaussian along each row with wrap-around; the sigma of a
/// row is `sigma / cos(latitude)` (capped at a quarter turn).
fn blur_rows_wrap(img: &mut Image, sigma: f32) {
    let (w, h) = (img.w, img.h);
    img.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let lat = (0.5 - (y as f64 + 0.5) / h as f64) * PI;
        let s = (sigma as f64 / lat.cos().max(1e-3)).min(w as f64 / 4.0) as f32;
        if s <= 0.3 {
            return;
        }
        let r = ((((12.0 * s * s / 3.0 + 1.0).sqrt() - 1.0) / 2.0).round().max(1.0) as isize).min((w as isize - 1) / 2).max(0);
        if r == 0 {
            return;
        }
        let wi = w as isize;
        let mut tmp = row.to_vec();
        for _ in 0..3 {
            let inv = 1.0 / (2 * r + 1) as f32;
            // sliding window sum with wrap-around
            let mut acc = [0f32; 4];
            for j in -r..=r {
                let i = j.rem_euclid(wi) as usize;
                for k in 0..4 {
                    acc[k] += row[i * 4 + k];
                }
            }
            for x in 0..wi {
                for k in 0..4 {
                    tmp[x as usize * 4 + k] = acc[k] * inv;
                }
                let (out_i, in_i) = ((x - r).rem_euclid(wi) as usize, (x + r + 1).rem_euclid(wi) as usize);
                for k in 0..4 {
                    acc[k] += row[in_i * 4 + k] - row[out_i * 4 + k];
                }
            }
            row.copy_from_slice(&tmp);
        }
    });
}

fn vr_blur_image(img: &mut Image, sigma: f32) {
    if sigma <= 0.3 {
        return;
    }
    blur_rows_wrap(img, sigma);
    crate::effects::gaussian(img, 0.0, sigma, true);
}

pub fn apply(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let layout = chv(e, "frame_layout");
    match e.effect.as_str() {
        "vr_blur" => {
            let s = fv(e, "blurriness", cx) * cx.px_scale * 0.5;
            per_view(img, layout, |v| vr_blur_image(v, s));
        }
        "vr_sharpen" => {
            let amt = fv(e, "amount", cx) / 100.0;
            if amt <= 0.0 {
                return;
            }
            let s = cx.px_scale.max(0.35) * 1.2;
            per_view(img, layout, |v| {
                let mut b = v.clone();
                vr_blur_image(&mut b, s);
                v.px.par_chunks_mut(4).zip(b.px.par_chunks(4)).for_each(|(p, q)| {
                    for k in 0..3 {
                        p[k] = (p[k] + (p[k] - q[k]) * amt).max(0.0);
                    }
                });
            });
        }
        "vr_denoise" => {
            let level = fv(e, "amount", cx) / 100.0;
            if level <= 0.0 {
                return;
            }
            let slow = chv(e, "noise_type") == 1;
            let show = bv(e, "show_noise");
            let s = (level * if slow { 3.0 } else { 1.8 }) * cx.px_scale.max(0.35);
            let thr = 0.02 + level * 0.12;
            per_view(img, layout, |v| {
                let mut b = v.clone();
                vr_blur_image(&mut b, s);
                v.px.par_chunks_mut(4).zip(b.px.par_chunks(4)).for_each(|(p, q)| {
                    let d = (0..3).map(|k| (p[k] - q[k]).abs()).fold(0.0, f32::max);
                    let k = (-(d * d) / (thr * thr)).exp();
                    let o: Vec<f32> = (0..4).map(|c| p[c] + (q[c] - p[c]) * k).collect();
                    if show {
                        for c in 0..3 {
                            p[c] = ((p[c] - o[c]) * 4.0 + 0.18).max(0.0) * p[3];
                        }
                    } else {
                        p.copy_from_slice(&o);
                    }
                });
            });
        }
        "vr_glow" => {
            let th = fv(e, "threshold", cx) / 100.0;
            let r = fv(e, "radius", cx) * cx.px_scale * 0.5;
            let br = fv(e, "brightness", cx);
            let sat = fv(e, "saturation", cx);
            let tint = bv(e, "use_tint").then(|| lin(cv(e, "tint", cx)));
            per_view(img, layout, |v| {
                let mut hi = highlights(v, th, 0.1);
                vr_blur_image(&mut hi, r);
                v.px.par_chunks_mut(4).zip(hi.px.par_chunks(4)).for_each(|(p, q)| {
                    let l = luma([q[0], q[1], q[2]]);
                    let mut g = [0, 1, 2].map(|k| (l + (q[k] - l) * sat).max(0.0));
                    if let Some(t) = tint {
                        g = t.map(|c| c * l);
                    }
                    add_light(p, g.map(|c| c * br));
                });
            });
        }
        "vr_chromatic_aberrations" => {
            let ab = [fv(e, "red", cx), fv(e, "green", cx), fv(e, "blue", cx)];
            if ab.iter().all(|a| a.abs() < 1e-4) {
                return;
            }
            let center = dir((fv(e, "center_x", cx) as f64).to_radians(), (fv(e, "center_y", cx) as f64).to_radians());
            let p = 0.5 + fv(e, "falloff", cx) as f64 / 50.0;
            let inverse = bv(e, "inverse");
            per_view(img, layout, |v| {
                let src = v.clone();
                let (w, h) = (v.w, v.h);
                v.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
                    for x in 0..w {
                        let (lon, lat) = px_to_lonlat(x as f64 + 0.5, y as f64 + 0.5, w, h);
                        let d = dir(lon, lat);
                        let th = dot(d, center).clamp(-1.0, 1.0).acos();
                        let mut f = (th / PI).powf(p);
                        if inverse {
                            f = 1.0 - f;
                        }
                        let axis = cross(center, d);
                        let al = dot(axis, axis).sqrt();
                        if al > 1e-9 {
                            let k = [axis[0] / al, axis[1] / al, axis[2] / al];
                            let px = &mut row[x * 4..x * 4 + 3];
                            for (c, v) in px.iter_mut().enumerate() {
                                let ang = ab[c] as f64 / 100.0 * 0.15 * f;
                                // rotate d about k by −ang (Rodrigues)
                                let (s, co) = (-ang).sin_cos();
                                let kd = cross(k, d);
                                let kdot = dot(k, d);
                                let r = [0, 1, 2].map(|i| d[i] * co + kd[i] * s + k[i] * kdot * (1.0 - co));
                                *v = sample_dir(&src, r)[c];
                            }
                        }
                    }
                });
            });
        }
        "vr_color_gradients" => {
            let pts: Vec<(V3, [f32; 3])> = (1..=3)
                .map(|i| {
                    let lon = fv(e, ["p1_lon", "p2_lon", "p3_lon"][i - 1], cx) as f64;
                    let lat = fv(e, ["p1_lat", "p2_lat", "p3_lat"][i - 1], cx) as f64;
                    (dir(lon.to_radians(), lat.to_radians()), lin(cv(e, ["c1", "c2", "c3"][i - 1], cx)))
                })
                .collect();
            let expo = 1.0 + fv(e, "blend", cx) as f64 / 25.0;
            let op = fv(e, "opacity", cx) / 100.0;
            let mode = chv(e, "mode");
            per_view(img, layout, |v| {
                let (w, h) = (v.w, v.h);
                v.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
                    for x in 0..w {
                        let (lon, lat) = px_to_lonlat(x as f64 + 0.5, y as f64 + 0.5, w, h);
                        let d = dir(lon, lat);
                        let mut acc = [0.0f64; 3];
                        let mut ws = 0.0;
                        for (pd, c) in &pts {
                            let ang = dot(d, *pd).clamp(-1.0, 1.0).acos().max(1e-3);
                            let wgt = 1.0 / ang.powf(expo);
                            for k in 0..3 {
                                acc[k] += c[k] as f64 * wgt;
                            }
                            ws += wgt;
                        }
                        let g = acc.map(|v| (v / ws) as f32);
                        let p = &mut row[x * 4..x * 4 + 4];
                        let base = Image::unpremul([p[0], p[1], p[2], p[3]]);
                        let o = lerp3(base, blend_simple(mode, base, g), op);
                        let a = p[3] + (1.0 - p[3]) * op;
                        p.copy_from_slice(&[o[0] * a, o[1] * a, o[2] * a, a]);
                    }
                });
            });
        }
        "vr_digital_glitch" => {
            let amp = fv(e, "amplitude", cx) / 100.0;
            if amp <= 0.0 {
                return;
            }
            let cplx = fv(e, "distortion", cx).max(1.0);
            let rate = fv(e, "rate", cx) as f64;
            let colour = fv(e, "color", cx) / 100.0;
            let scan = fv(e, "scanlines", cx) / 100.0;
            let seed = fv(e, "seed", cx).max(0.0) as u64;
            let tick = (cx.seconds * rate).floor() as i64;
            per_view(img, layout, |v| {
                let src = v.clone();
                let (w, h) = (v.w, v.h);
                let band = (h as f32 / cplx).max(1.0);
                v.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
                    let b = (y as f32 / band) as i64;
                    let r = hash01(b, tick, 0, seed);
                    let active = r < amp;
                    let shift = if active { (hash01(b, tick, 1, seed) - 0.5) * w as f32 * 0.3 * amp } else { 0.0 };
                    let cs = colour * amp * 12.0;
                    for x in 0..w {
                        let (fx, fy) = (x as f64 + 0.5 + shift as f64, y as f64 + 0.5);
                        let mut o = sample_wrap(&src, fx, fy);
                        if active && cs > 0.0 {
                            o[0] = sample_wrap(&src, fx + cs as f64, fy)[0];
                            o[2] = sample_wrap(&src, fx - cs as f64, fy)[2];
                        }
                        if scan > 0.0 && y % 2 == 1 {
                            for c in &mut o[..3] {
                                *c *= 1.0 - scan * 0.6;
                            }
                        }
                        row[x * 4..x * 4 + 4].copy_from_slice(&o);
                    }
                });
            });
        }
        "vr_fractal_noise" => {
            let kind = chv(e, "fractal_type");
            let invert = bv(e, "invert");
            let contrast = fv(e, "contrast", cx) / 100.0;
            let bright = fv(e, "brightness", cx) / 100.0;
            let scale = (fv(e, "scale", cx) / 100.0).max(0.01);
            let cplx = fv(e, "complexity", cx);
            let evo = fv(e, "evolution", cx) / 360.0;
            let op = fv(e, "opacity", cx) / 100.0;
            let mode = chv(e, "mode");
            let f = 4.0 / scale;
            per_view(img, layout, |v| {
                let (w, h) = (v.w, v.h);
                v.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
                    for x in 0..w {
                        let (lon, lat) = px_to_lonlat(x as f64 + 0.5, y as f64 + 0.5, w, h);
                        let d = dir(lon, lat);
                        let mut n = fbm(d[0] as f32 * f + evo * 0.31, d[1] as f32 * f + evo * 0.17, d[2] as f32 * f + evo * 0.11, cplx, 3);
                        n = match kind {
                            1 => n.abs().sqrt(),
                            2 => n.abs(),
                            _ => n * 0.5 + 0.5,
                        };
                        let mut g = ((n - 0.5) * contrast + 0.5 + bright).clamp(0.0, 1.0);
                        if invert {
                            g = 1.0 - g;
                        }
                        let gl = filmcraft_color::srgb_to_linear(g);
                        let p = &mut row[x * 4..x * 4 + 4];
                        let base = Image::unpremul([p[0], p[1], p[2], p[3]]);
                        let o = lerp3(base, blend_simple(mode, base, [gl; 3]), op);
                        let a = p[3] + (1.0 - p[3]) * op;
                        p.copy_from_slice(&[o[0] * a, o[1] * a, o[2] * a, a]);
                    }
                });
            });
        }
        "vr_plane_to_sphere" => {
            let fov = (fv(e, "scale", cx) as f64).to_radians().clamp(1e-3, PI * 0.999);
            let r = transpose(rotation(fv(e, "tilt", cx) as f64, fv(e, "pan", cx) as f64, fv(e, "roll", cx) as f64));
            let feather = fv(e, "feather", cx) / 100.0;
            per_view(img, layout, |v| {
                let src = v.clone();
                let (w, h) = (v.w, v.h);
                let tx = (fov / 2.0).tan();
                let ty = tx * h as f64 / w as f64;
                v.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
                    for x in 0..w {
                        let (lon, lat) = px_to_lonlat(x as f64 + 0.5, y as f64 + 0.5, w, h);
                        let d = mvec(&r, dir(lon, lat));
                        let mut o = [0.0f32; 4];
                        if d[2] > 1e-6 {
                            let (u, vv) = (d[0] / d[2] / tx, -d[1] / d[2] / ty);
                            if u.abs() <= 1.0 && vv.abs() <= 1.0 {
                                o = src.sample_bilinear_clamped(((u + 1.0) / 2.0 * w as f64) as f32, ((vv + 1.0) / 2.0 * h as f64) as f32);
                                if feather > 0.0 {
                                    let edge = (1.0 - u.abs().max(vv.abs())) as f32;
                                    let k = smoothstep(0.0, feather, edge);
                                    o = o.map(|c| c * k);
                                }
                            }
                        }
                        row[x * 4..x * 4 + 4].copy_from_slice(&o);
                    }
                });
            });
        }
        "vr_reframe" => reframe(img, e, cx, layout),
        "vr_projection" | "vr_rotate_sphere" => {
            let r = rotation(fv(e, "tilt", cx) as f64, fv(e, "pan", cx) as f64, fv(e, "roll", cx) as f64);
            let fov = if e.effect == "vr_projection" { fv(e, "fov", cx) as f64 } else { 360.0 };
            let stretch = e.effect == "vr_projection" && bv(e, "stretch");
            let ident = (0..3).all(|i| (0..3).all(|j| (r[i][j] - if i == j { 1.0 } else { 0.0 }).abs() < 1e-12));
            if ident && (fov - 360.0).abs() < 1e-9 {
                return;
            }
            let rt = transpose(r);
            per_view(img, layout, |v| {
                let src = v.clone();
                let (w, h) = (v.w, v.h);
                let (kx, ky) = (fov / 360.0, if stretch { (fov / 180.0).min(1.0) } else { 1.0 });
                v.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
                    for x in 0..w {
                        let (lon, lat) = px_to_lonlat(x as f64 + 0.5, y as f64 + 0.5, w, h);
                        let d = mvec(&rt, dir(lon * kx, lat * ky));
                        row[x * 4..x * 4 + 4].copy_from_slice(&sample_dir(&src, d));
                    }
                });
            });
        }
        _ => {}
    }
}
