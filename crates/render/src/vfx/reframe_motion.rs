//! VR Reframe's camera motion between keyframes.
//!
//! With Smooth Motion on (the default), the view's numbers (Pan, Tilt, Roll, Field of View…) move
//! along a smooth curve through their keyframes, the way a camera operator moves: easing out of
//! the first keyframe, keeping its speed through the ones in between (no stop at each keyframe)
//! and easing into the last. The curve never overshoots a keyframe's value (a monotone cubic), and a
//! keyframe set to Hold still holds. View and Lens keyframes morph from one into the next over the
//! time between them instead of cutting (a Hold keyframe still cuts).
//!
//! Pan and Roll take the short way round: a pan from 170° to −170° turns 20°, not 340°.
//!
//! Smooth Motion off, the numbers follow their keyframes' own interpolation (Linear, Bezier…).

use filmcraft_project::{EffectInstance, Interpolation, Keyframe, Param};
use filmcraft_time::Tick;

/// Angles that wrap: they take the short way between keyframes.
const WRAPPING: &[&str] = &["pan", "roll"];

/// Whether the instance moves smoothly through its keyframes (the Smooth Motion checkbox; on by
/// default and when absent).
pub fn smooth(e: &EffectInstance) -> bool {
    e.param("smooth").and_then(|p| p.value.as_bool()).unwrap_or(true)
}

/// The value VR Reframe uses for its number parameter `id` at media time `t` (finite; 0 for a
/// non-finite one).
pub fn reframe_value(e: &EffectInstance, id: &str, t: Tick) -> f64 {
    let v = match e.param(id).filter(|p| p.keyframes.len() >= 2) {
        Some(p) => track(p, t, smooth(e), WRAPPING.contains(&id)).unwrap_or_else(|| e.f64_at(id, t)),
        None => e.f64_at(id, t),
    };
    if v.is_finite() { v } else { 0.0 }
}

/// The keyframes' values, each wrapping one turned by whole turns to within half a turn of the
/// one before it.
fn values(k: &[Keyframe], wrap: bool) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::with_capacity(k.len());
    for kf in k {
        let mut v = kf.value.as_f64().filter(|v| v.is_finite()).unwrap_or(0.0);
        if let (true, Some(&prev)) = (wrap, out.last()) {
            v = prev + (v - prev + 180.0).rem_euclid(360.0) - 180.0;
        }
        out.push(v);
    }
    out
}

/// `p` at `t` (None: let the parameter evaluate itself).
fn track(p: &Param, t: Tick, smooth: bool, wrap: bool) -> Option<f64> {
    let k = &p.keyframes;
    let v = values(k, wrap);
    let (first, last) = (k.first()?, k.last()?);
    if t <= first.time {
        return v.first().copied();
    }
    if t >= last.time {
        return v.last().copied();
    }
    // the segment holding t: k[i].time ≤ t < k[i + 1].time
    let i = k.partition_point(|kf| kf.time <= t).checked_sub(1)?;
    let (a, b) = (k.get(i)?, k.get(i + 1)?);
    let (va, vb) = (*v.get(i)?, *v.get(i + 1)?);
    let span = (b.time.0 - a.time.0) as f64;
    if span <= 0.0 {
        return Some(va);
    }
    let u = ((t.0 - a.time.0) as f64 / span).clamp(0.0, 1.0);
    if a.interp == Interpolation::Hold {
        return Some(va);
    }
    if !smooth {
        if !wrap {
            return None;
        }
        // the parameter's own easing, along the short way round
        let (ra, rb, rt) = (a.value.as_f64()?, b.value.as_f64()?, p.f64_at(t));
        let f = if (rb - ra).abs() > 1e-12 { (rt - ra) / (rb - ra) } else { u };
        return Some(va + (vb - va) * f);
    }
    let (ma, mb) = (slope(k, &v, i), slope(k, &v, i + 1));
    // cubic Hermite in seconds-free units: slopes are per tick, scaled by the span
    let (u2, u3) = (u * u, u * u * u);
    let h = [2.0 * u3 - 3.0 * u2 + 1.0, u3 - 2.0 * u2 + u, -2.0 * u3 + 3.0 * u2, u3 - u2];
    Some(h[0] * va + h[1] * span * ma + h[2] * vb + h[3] * span * mb)
}

/// The speed (value per tick) through keyframe `i`: none at the first and last keyframes, at a
/// turning point and next to a Hold; otherwise the weighted mean of the slopes either side,
/// limited so the curve doesn't overshoot (Fritsch–Carlson).
fn slope(k: &[Keyframe], v: &[f64], i: usize) -> f64 {
    let (Some(prev), Some(cur), Some(next)) = (i.checked_sub(1).and_then(|p| k.get(p)), k.get(i), k.get(i + 1)) else { return 0.0 };
    if prev.interp == Interpolation::Hold || cur.interp == Interpolation::Hold {
        return 0.0;
    }
    let (h0, h1) = ((cur.time.0 - prev.time.0) as f64, (next.time.0 - cur.time.0) as f64);
    let (Some(&vp), Some(&vc), Some(&vn)) = (v.get(i.wrapping_sub(1)), v.get(i), v.get(i + 1)) else { return 0.0 };
    if h0 <= 0.0 || h1 <= 0.0 {
        return 0.0;
    }
    let (s0, s1) = ((vc - vp) / h0, (vn - vc) / h1);
    if s0 * s1 <= 0.0 {
        return 0.0;
    }
    let m = (s0 * h1 + s1 * h0) / (h0 + h1);
    let cap = 3.0 * s0.abs().min(s1.abs());
    m.clamp(-cap, cap)
}

/// A View or Lens choice at `t`: the option before, the one after and how far (0…1, eased) the
/// view has morphed from the first to the second. Without Smooth Motion, or across a Hold
/// keyframe, it cuts at the next keyframe (morph 0).
pub fn choice_morph(e: &EffectInstance, id: &str, t: Tick, default: u32) -> (u32, u32, f64) {
    let opt = |kf: &Keyframe| match kf.value {
        filmcraft_project::ParamValue::Choice(c) => c,
        _ => default,
    };
    let Some(p) = e.param(id) else { return (default, default, 0.0) };
    let k = &p.keyframes;
    let (Some(first), Some(last)) = (k.first(), k.last()) else {
        let c = match p.value {
            filmcraft_project::ParamValue::Choice(c) => c,
            _ => default,
        };
        return (c, c, 0.0);
    };
    if t <= first.time {
        return (opt(first), opt(first), 0.0);
    }
    if t >= last.time {
        return (opt(last), opt(last), 0.0);
    }
    let Some((a, b)) = k.partition_point(|kf| kf.time <= t).checked_sub(1).and_then(|i| Some((k.get(i)?, k.get(i + 1)?))) else {
        return (opt(last), opt(last), 0.0);
    };
    let (ca, cb) = (opt(a), opt(b));
    if ca == cb || !smooth(e) || a.interp == Interpolation::Hold {
        return (ca, ca, 0.0);
    }
    let span = (b.time.0 - a.time.0) as f64;
    let u = if span > 0.0 { ((t.0 - a.time.0) as f64 / span).clamp(0.0, 1.0) } else { 0.0 };
    (ca, cb, u * u * (3.0 - 2.0 * u))
}
