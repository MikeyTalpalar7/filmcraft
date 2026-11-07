//! Insta360 dual-fisheye video (`.insv`, and the `.lrv` low-resolution proxies the cameras record
//! alongside), stitched into equirectangular 360° video.
//!
//! Insta360 publishes no specification of these files; what follows was observed in files from an
//! Insta360 X5 (firmware v1.11.6), never taken from other software:
//!
//! - They are ISO BMFF files. An `.insv` holds one video track per lens (square HEVC pictures): the
//!   second video track is the front lens (the side with the camera's screen), the first the back
//!   lens. An `.lrv` holds one 2:1 track with the two lenses side by side, front on the left.
//! - A top-level `inst` box after `moov` carries sensor data and, near its end, the lens
//!   calibration as text: `2_R_cx_cy_a_b_c_R_cx_cy_a_b_c_W_H_…` (2 lenses; for each an image-circle
//!   radius, its centre in a `W`×`H` canvas holding both sensors side by side, and three angles;
//!   then the canvas size). The pictures in the tracks are those sensors scaled to the track size.
//!
//! The lens curve ([`LENS_ALPHA_DEG`], [`LENS_K`]) was measured on that footage by matching the
//! two lenses' overlapping edges; the small rotation of the back lens against the front one is
//! measured the same way for every clip when it opens ([`align`]), because the calibration's
//! angles could not be read with certainty. The two pictures are blended across a band of
//! [`BLEND_DEG`] where they meet. Things close to the camera (a hand, the selfie stick) show the
//! parallax of the 3 cm between the lenses, as with any dual-lens stitch.

use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_frame::{AudioBuffer, Chroma, PixelData, VideoFrame};
use filmcraft_media::{FrameRequest, MediaError, MediaInfo, MediaSource, Projection, SharedReader, SharedSource, Spherical, StereoLayout};
use filmcraft_time::Tick;
use rayon::prelude::*;

use crate::mp4::Mp4Source;

/// The angle from a lens's axis at the calibration radius, degrees (X5, measured).
pub const LENS_ALPHA_DEG: f64 = 99.188;
/// Lens curve: r = R (t + K (t³ − t)) with t = θ / α (X5, measured).
pub const LENS_K: f64 = -0.1;
/// Widest angle from a lens's axis that its picture is used for, degrees (the image circle ends
/// a little past it).
pub const MAX_LENS_DEG: f64 = 101.0;
/// Half-width of the band where the two lenses are blended, degrees.
pub const BLEND_DEG: f64 = 3.0;
/// Largest back-lens rotation [`align`] looks for, degrees.
const MAX_ALIGN_DEG: f64 = 4.0;
/// How much of the end of a file is searched for the calibration.
const TAIL: usize = 256 * 1024;

/// One lens's image circle in calibration-canvas pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LensCal {
    pub radius: f64,
    pub cx: f64,
    pub cy: f64,
}

/// The two lenses' calibration; `canvas` is the side of one lens's square sensor picture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Calibration {
    pub front: LensCal,
    pub back: LensCal,
    pub canvas: f64,
}

impl Calibration {
    /// Centred circles of the X5's proportions, for files without a readable calibration.
    pub fn nominal() -> Self {
        let lens = LensCal { radius: 2646.0, cx: 2688.0, cy: 2688.0 };
        Calibration { front: lens, back: lens, canvas: 5376.0 }
    }
}

/// The calibration in one `2_R_cx_cy_a_b_c_R_cx_cy_a_b_c_W_H…` string (a leading `p` allowed).
pub fn parse_calibration(s: &str) -> Option<Calibration> {
    let f: Vec<f64> = s.trim_start_matches('p').split('_').take(15).map(|v| v.parse::<f64>().ok().filter(|x| x.is_finite())).collect::<Option<_>>()?;
    let [n, r1, x1, y1, _, _, _, r2, x2, y2, _, _, _, w, h] = <[f64; 15]>::try_from(f).ok()?;
    if n != 2.0 || !(h > 0.0 && w >= h) {
        return None;
    }
    // the back lens's centre is in the right half of the canvas
    let half = w / 2.0;
    let x2 = if x2 >= half { x2 - half } else { x2 };
    let lens = |r: f64, cx: f64, cy: f64| (r > 0.0 && (0.0..=h).contains(&cx) && (0.0..=h).contains(&cy)).then_some(LensCal { radius: r, cx, cy });
    Some(Calibration { front: lens(r1, x1, y1)?, back: lens(r2, x2, y2)?, canvas: h })
}

/// The first calibration string in `tail` (the end of a file).
pub fn find_calibration(tail: &[u8]) -> Option<Calibration> {
    let mut at = 0;
    while let Some(i) = tail.get(at..)?.windows(3).position(|w| w == b"p2_") {
        let start = at + i;
        let end = tail.get(start + 1..)?.iter().position(|c| !(c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'))).map_or(tail.len(), |e| start + 1 + e);
        if let Some(c) = std::str::from_utf8(tail.get(start..end)?).ok().and_then(parse_calibration) {
            return Some(c);
        }
        at = start + 3;
    }
    None
}

type V3 = [f64; 3];
type M3 = [[f64; 3]; 3];

/// Rotation from yaw (about Y), pitch (about X) and roll (about Z), degrees: Ry · Rx · Rz.
pub fn rotation(yaw: f64, pitch: f64, roll: f64) -> M3 {
    let (sy, cy) = yaw.to_radians().sin_cos();
    let (sp, cp) = pitch.to_radians().sin_cos();
    let (sr, cr) = roll.to_radians().sin_cos();
    let ry = [[cy, 0.0, sy], [0.0, 1.0, 0.0], [-sy, 0.0, cy]];
    let rx = [[1.0, 0.0, 0.0], [0.0, cp, -sp], [0.0, sp, cp]];
    let rz = [[cr, -sr, 0.0], [sr, cr, 0.0], [0.0, 0.0, 1.0]];
    let mul = |a: M3, b: M3| -> M3 { std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum())) };
    mul(ry, mul(rx, rz))
}

/// How the two lenses sit: the back lens's rotation against the ideal (exactly opposite).
#[derive(Clone, Copy, Debug)]
pub struct Rig {
    pub cal: Calibration,
    pub back: M3,
}

impl Rig {
    pub fn new(cal: Calibration, back_ypr: [f64; 3]) -> Self {
        Rig { cal, back: rotation(back_ypr[0], back_ypr[1], back_ypr[2]) }
    }
    /// A world direction (x right, y up, z forward) in the back lens's frame.
    fn in_back(&self, d: V3) -> V3 {
        let m = &self.back;
        let v = [-d[0], d[1], -d[2]];
        // Rᵀ · v
        std::array::from_fn(|j| (0..3).map(|i| m[i][j] * v[i]).sum())
    }
}

/// The distance from the image-circle centre (calibration pixels) at `theta` radians from a
/// lens's axis.
pub fn lens_radius(theta: f64, cal_radius: f64) -> f64 {
    let t = theta / LENS_ALPHA_DEG.to_radians();
    cal_radius * (t + LENS_K * (t * t * t - t))
}

/// Where direction `d` (in the lens's frame, z its axis) lands in a lens picture `size` pixels
/// across (its left edge at `x0`), and the angle from the axis in degrees.
fn lens_point(l: &LensCal, canvas: f64, size: f64, x0: f64, d: V3) -> (f64, f64, f64) {
    let theta = d[2].clamp(-1.0, 1.0).acos();
    let rho = d[0].hypot(d[1]);
    let scale = size / canvas;
    let (ux, uy) = if rho > 1e-12 { (d[0] / rho, d[1] / rho) } else { (0.0, 0.0) };
    let r = lens_radius(theta, l.radius);
    let x = (l.cx + r * ux + 0.5) * scale - 0.5 + x0;
    let y = (l.cy - r * uy + 0.5) * scale - 0.5;
    (x, y, theta.to_degrees())
}

/// One 8-bit plane of a lens picture: the plane, and the lens's square within it.
#[derive(Clone, Copy)]
struct Plane<'a> {
    px: &'a [u8],
    w: usize,
    h: usize,
    x0: f64,
    size: f64,
}

impl Plane<'_> {
    /// Bilinear sample at pixel coordinates (centres on integers), clamped to the lens's square.
    fn sample(&self, x: f64, y: f64) -> f64 {
        if self.w == 0 || self.h == 0 || !(x.is_finite() && y.is_finite()) {
            return 0.0;
        }
        let lo = self.x0.max(0.0);
        let hi = (self.x0 + self.size - 1.0).min((self.w - 1) as f64).max(lo);
        let x = x.clamp(lo, hi);
        let y = y.clamp(0.0, (self.h - 1) as f64);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (tx, ty) = (x - x0 as f64, y - y0 as f64);
        let g = |xx: usize, yy: usize| self.px.get(yy * self.w + xx).copied().map_or(0.0, f64::from);
        let top = g(x0, y0) + (g(x1, y0) - g(x0, y0)) * tx;
        let bot = g(x0, y1) + (g(x1, y1) - g(x0, y1)) * tx;
        top + (bot - top) * ty
    }
}

/// The blend weight of the front lens for a direction `tf` / `tb` degrees from the two axes.
fn front_weight(tf: f64, tb: f64) -> f64 {
    if tf > MAX_LENS_DEG {
        return 0.0;
    }
    if tb > MAX_LENS_DEG {
        return 1.0;
    }
    ((tb - tf) / (4.0 * BLEND_DEG) + 0.5).clamp(0.0, 1.0)
}

/// One plane of the stitched equirectangular picture, `ow`×`oh`, from the matching planes of the
/// two lenses.
fn stitch_plane(rig: &Rig, front: Plane, back: Plane, ow: usize, oh: usize) -> Vec<u8> {
    let mut out = vec![0u8; ow.saturating_mul(oh)];
    if ow == 0 || oh == 0 {
        return out;
    }
    let cal = rig.cal;
    let cols: Vec<(f64, f64)> = (0..ow).map(|i| ((i as f64 + 0.5) / ow as f64 * std::f64::consts::TAU - std::f64::consts::PI).sin_cos()).collect();
    out.par_chunks_mut(ow).enumerate().for_each(|(j, row)| {
        let lat = std::f64::consts::FRAC_PI_2 - (j as f64 + 0.5) / oh as f64 * std::f64::consts::PI;
        let (sl, cl) = lat.sin_cos();
        for (px, &(so, co)) in row.iter_mut().zip(&cols) {
            let d = [cl * so, sl, cl * co];
            let (fx, fy, tf) = lens_point(&cal.front, cal.canvas, front.size, front.x0, d);
            let (bx, by, tb) = lens_point(&cal.back, cal.canvas, back.size, back.x0, rig.in_back(d));
            let w = front_weight(tf, tb);
            let v = match w {
                w if w >= 1.0 => front.sample(fx, fy),
                w if w <= 0.0 => back.sample(bx, by),
                w => front.sample(fx, fy) * w + back.sample(bx, by) * (1.0 - w),
            };
            *px = v.round().clamp(0.0, 255.0) as u8;
        }
    });
    out
}

/// The back lens's rotation (yaw, pitch, roll in degrees) that best matches the two lenses where
/// they overlap, from the luma planes `front` and `back`; zero when matching doesn't find a
/// clearly better one.
fn align(cal: Calibration, front: Plane, back: Plane) -> [f64; 3] {
    // the overlap: 84°…96° from the front axis, all round
    let rows: Vec<f64> = (0..25).map(|i| (84.0 + i as f64 * 0.5).to_radians()).collect();
    let dirs: Vec<V3> = rows
        .iter()
        .flat_map(|&t| {
            (0..720).map(move |k| {
                let p = (k as f64 * 0.5).to_radians();
                [t.sin() * p.cos(), t.sin() * p.sin(), t.cos()]
            })
        })
        .collect();
    let loss = |ypr: [f64; 3]| -> f64 {
        let rig = Rig::new(cal, ypr);
        let mut diffs: Vec<f64> = Vec::with_capacity(dirs.len());
        for ring in dirs.chunks(720) {
            let a: Vec<f64> = ring.iter().map(|&d| lens_point(&cal.front, cal.canvas, front.size, front.x0, d)).map(|(x, y, _)| front.sample(x, y)).collect();
            let b: Vec<f64> =
                ring.iter().map(|&d| lens_point(&cal.back, cal.canvas, back.size, back.x0, rig.in_back(d))).map(|(x, y, _)| back.sample(x, y)).collect();
            // exposure differs between the lenses: compare each ring around its own mean
            let (ma, mb) = (a.iter().sum::<f64>() / a.len().max(1) as f64, b.iter().sum::<f64>() / b.len().max(1) as f64);
            diffs.extend(a.iter().zip(&b).map(|(x, y)| ((x - ma) - (y - mb)).abs()));
        }
        // trimmed: parallax of things close to the camera
        diffs.sort_by(f64::total_cmp);
        let keep = diffs.len() * 4 / 5;
        diffs.iter().take(keep).sum::<f64>() / keep.max(1) as f64
    };
    let zero = loss([0.0; 3]);
    let (mut x, mut cur, mut step) = ([0.0f64; 3], zero, 0.5f64);
    for _ in 0..200 {
        let mut improved = false;
        for i in 0..3 {
            for sgn in [1.0, -1.0] {
                let mut y = x;
                y[i] = (y[i] + sgn * step).clamp(-MAX_ALIGN_DEG, MAX_ALIGN_DEG);
                let l = loss(y);
                if l < cur {
                    (x, cur, improved) = (y, l, true);
                }
            }
        }
        if !improved {
            step *= 0.5;
            if step < 0.02 {
                break;
            }
        }
    }
    if cur.is_finite() && cur < zero * 0.95 { x } else { [0.0; 3] }
}

/// The lens pictures of one frame: two square pictures (one per lens) or one side-by-side.
enum Lenses {
    Dual { front: Box<Mp4Source>, back: Box<Mp4Source> },
    SideBySide(Box<Mp4Source>),
}

/// 8-bit planes of a frame (Y, Cb, Cr with their sizes, or R, G, B), and whether they are YUV.
fn planes_of(f: &VideoFrame) -> (Vec<(Arc<Vec<u8>>, usize, usize)>, Option<Chroma>) {
    let (w, h) = (f.width as usize, f.height as usize);
    match &f.data {
        PixelData::Yuv8 { planes, chroma, .. } => {
            let (sx, sy) = chroma.shifts();
            let (cw, ch) = (w.div_ceil(1 << sx), h.div_ceil(1 << sy));
            (vec![(planes[0].clone(), w, h), (planes[1].clone(), cw, ch), (planes[2].clone(), cw, ch)], Some(*chroma))
        }
        _ => {
            let rgba = f.to_rgba8();
            let ch = |c: usize| Arc::new(rgba.iter().skip(c).step_by(4).copied().collect::<Vec<u8>>());
            (vec![(ch(0), w, h), (ch(1), w, h), (ch(2), w, h)], None)
        }
    }
}

/// An Insta360 clip as stitched equirectangular video.
pub struct InsvSource {
    info: MediaInfo,
    lenses: Lenses,
    cal: Calibration,
    /// The back lens's rotation, measured on the first stitched frame.
    rig: Mutex<Option<Rig>>,
    /// The last stitched frame: (time, height, frame).
    last: Mutex<Option<(Tick, u32, Arc<VideoFrame>)>>,
}

/// Whether `name` is an Insta360 clip by its extension.
pub fn is_insta360(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".insv") || n.ends_with(".lrv")
}

impl InsvSource {
    pub fn open_reader(name: &str, reader: SharedReader) -> crate::Result<Self> {
        let len = reader.len();
        let start = len.saturating_sub(TAIL as u64);
        let tail = filmcraft_media::reader::read_range(&*reader, start, TAIL).unwrap_or_default();
        let cal = find_calibration(&tail).unwrap_or_else(Calibration::nominal);
        let first = Mp4Source::open_reader_video(name, reader.clone(), 0)?;
        let second = Mp4Source::open_reader_video(name, reader, 1).ok().filter(|s| s.info().video.is_some());
        let (lenses, size) = match second {
            // (the second video track is the front lens)
            Some(front) => {
                let size = front.info().video.as_ref().map_or(0, |v| v.height);
                (Lenses::Dual { front: Box::new(front), back: Box::new(first) }, size)
            }
            None => {
                let v = first.info().video.as_ref().ok_or_else(|| crate::CodecError::Unsupported("Insta360 clip without video".into()))?;
                if v.width != v.height.saturating_mul(2) {
                    return Err(crate::CodecError::Unsupported(format!("Insta360 clip of {}×{}: not two lenses side by side", v.width, v.height)));
                }
                let size = v.height;
                (Lenses::SideBySide(Box::new(first)), size)
            }
        };
        if size < 16 {
            return Err(crate::CodecError::Unsupported("Insta360 clip with no usable picture".into()));
        }
        let base = match &lenses {
            Lenses::Dual { front, .. } => front.info().clone(),
            Lenses::SideBySide(s) => s.info().clone(),
        };
        let mut info = base;
        info.container = "Insta360".into();
        if let Some(v) = info.video.as_mut() {
            v.width = size.saturating_mul(2);
            v.height = size;
            v.par = (1, 1);
            v.codec = format!("{} · Insta360 dual fisheye", v.codec);
            v.spherical = Some(Spherical { projection: Projection::Equirectangular, stereo: StereoLayout::Mono, ..Default::default() });
        }
        Ok(InsvSource { info, lenses, cal, rig: Mutex::new(None), last: Mutex::new(None) })
    }

    /// The two lens pictures at `req` (front, back), and where each lens sits in its picture.
    fn lens_frames(&self, req: FrameRequest) -> Result<[(Arc<VideoFrame>, f64, f64); 2], MediaError> {
        Ok(match &self.lenses {
            Lenses::Dual { front, back } => {
                let (f, b) = (front.video_frame(req)?, back.video_frame(req)?);
                let (fs, bs) = (f64::from(f.height), f64::from(b.height));
                [(f, 0.0, fs), (b, 0.0, bs)]
            }
            Lenses::SideBySide(s) => {
                let f = s.video_frame(req)?;
                let size = f64::from(f.height);
                [(f.clone(), 0.0, size), (f, size, size)]
            }
        })
    }

    fn stitch(&self, req: FrameRequest) -> Result<Arc<VideoFrame>, MediaError> {
        let full = self.info.video.as_ref().map_or(0, |v| v.height);
        let scale = if req.scale.is_finite() { req.scale.clamp(1.0 / 16.0, 1.0) } else { 1.0 };
        let oh = ((full as f32 * scale).round() as u32).max(64) & !1;
        let t = req.time.max(Tick::ZERO);
        if let Some((lt, lh, f)) = self.last.lock().unwrap_or_else(PoisonError::into_inner).as_ref()
            && *lt == t
            && *lh == oh
        {
            return Ok(f.clone());
        }
        // the lens pictures at least as large as the output needs
        let [(ff, fx0, fsize), (bf, bx0, bsize)] = self.lens_frames(FrameRequest { time: t, scale })?;
        let (fp, fchroma) = planes_of(&ff);
        let (bp, bchroma) = planes_of(&bf);
        let rig = {
            let mut g = self.rig.lock().unwrap_or_else(PoisonError::into_inner);
            match *g {
                Some(r) => r,
                None => {
                    let lum = |p: &[(Arc<Vec<u8>>, usize, usize)], x0: f64, size: f64| p.first().map(|(px, w, h)| (px.clone(), *w, *h, x0, size));
                    let ypr = match (lum(&fp, fx0, fsize), lum(&bp, bx0, bsize)) {
                        (Some(a), Some(b)) => {
                            align(self.cal, Plane { px: &a.0, w: a.1, h: a.2, x0: a.3, size: a.4 }, Plane { px: &b.0, w: b.1, h: b.2, x0: b.3, size: b.4 })
                        }
                        _ => [0.0; 3],
                    };
                    let r = Rig::new(self.cal, ypr);
                    *g = Some(r);
                    r
                }
            }
        };
        let ow = oh as usize * 2;
        let yuv = match (fchroma, bchroma) {
            (Some(a), Some(b)) if a == b => Some(a),
            _ => None,
        };
        // (both pictures in one format: YUV planes as they are, else RGB)
        let (fp, bp) = if yuv.is_none() && (fchroma.is_some() || bchroma.is_some()) {
            let rgb = |f: &VideoFrame| {
                let rgba = f.to_rgba8();
                let (w, h) = (f.width as usize, f.height as usize);
                (0..3).map(|c| (Arc::new(rgba.iter().skip(c).step_by(4).copied().collect::<Vec<u8>>()), w, h)).collect::<Vec<_>>()
            };
            (rgb(&ff), rgb(&bf))
        } else {
            (fp, bp)
        };
        let (sx, sy) = yuv.map_or((0, 0), Chroma::shifts);
        let outs: Vec<Vec<u8>> = (0..3)
            .map(|k| {
                let (shx, shy) = if k == 0 { (0, 0) } else { (sx, sy) };
                let (pw, ph) = (ow.div_ceil(1 << shx), (oh as usize).div_ceil(1 << shy));
                let view = |p: &(Arc<Vec<u8>>, usize, usize), x0: f64, size: f64| -> (Arc<Vec<u8>>, usize, usize, f64, f64) {
                    let k = f64::from(1u32 << shx);
                    (p.0.clone(), p.1, p.2, x0 / k, size / k)
                };
                match (fp.get(k), bp.get(k)) {
                    (Some(a), Some(b)) => {
                        let (a, b) = (view(a, fx0, fsize), view(b, bx0, bsize));
                        stitch_plane(
                            &rig,
                            Plane { px: &a.0, w: a.1, h: a.2, x0: a.3, size: a.4 },
                            Plane { px: &b.0, w: b.1, h: b.2, x0: b.3, size: b.4 },
                            pw,
                            ph,
                        )
                    }
                    _ => vec![0; pw * ph],
                }
            })
            .collect();
        let mut outs = outs.into_iter();
        let mut next = || Arc::new(outs.next().unwrap_or_default());
        let data = match yuv {
            Some(chroma) => PixelData::Yuv8 { planes: [next(), next(), next()], chroma, alpha: None },
            None => {
                let (r, g, b) = (next(), next(), next());
                let rgba = r.iter().zip(g.iter()).zip(b.iter()).flat_map(|((r, g), b)| [*r, *g, *b, 255]).collect();
                PixelData::Rgba8(Arc::new(rgba))
            }
        };
        let color = if yuv.is_some() { ff.color } else { filmcraft_color::ColorInfo::SRGB_FULL };
        let frame = Arc::new(VideoFrame { width: ow as u32, height: oh, data, color, par: (1, 1), pts: ff.pts });
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some((t, oh, frame.clone()));
        Ok(frame)
    }

    fn audio_source(&self) -> &Mp4Source {
        match &self.lenses {
            Lenses::Dual { front, .. } => front,
            Lenses::SideBySide(s) => s,
        }
    }
}

impl MediaSource for InsvSource {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn video_frame(&self, req: FrameRequest) -> Result<Arc<VideoFrame>, MediaError> {
        self.stitch(req)
    }
    fn audio(&self, start: i64, frames: usize, sample_rate: u32) -> Result<AudioBuffer, MediaError> {
        self.audio_source().audio(start, frames, sample_rate)
    }
    fn audio_stream(&self, stream: usize, start: i64, frames: usize, sample_rate: u32) -> Result<AudioBuffer, MediaError> {
        self.audio_source().audio_stream(stream, start, frames, sample_rate)
    }
}

/// [`filmcraft_media::Opener`] for Insta360 clips (by extension; they are MP4 files otherwise).
pub fn opener(name: &str, bytes: Arc<[u8]>) -> Option<Result<SharedSource, MediaError>> {
    if !is_insta360(name) || !crate::mp4::sniff(&bytes) {
        return None;
    }
    let reader: SharedReader = Arc::new(filmcraft_media::reader::MemReader(bytes));
    Some(InsvSource::open_reader(name, reader).map(|s| Arc::new(s) as SharedSource).map_err(Into::into))
}

/// [`filmcraft_media::ReaderOpener`] for Insta360 clips.
pub fn reader_opener(name: &str, head: &[u8], reader: &SharedReader) -> Option<Result<SharedSource, MediaError>> {
    if !is_insta360(name) || !crate::mp4::sniff(head) {
        return None;
    }
    Some(InsvSource::open_reader(name, reader.clone()).map(|s| Arc::new(s) as SharedSource).map_err(Into::into))
}

#[cfg(test)]
mod tests;
