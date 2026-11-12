use super::*;

const X5: &str = "p2_2646.011_2696.370_2674.760_0.675_-0.251_90.363_2640.688_8070.730_2688.640_-0.578_-0.408_89.151_10752_5376_11378";

#[test]
fn reads_the_x5_calibration() {
    let c = parse_calibration(X5).unwrap();
    assert_eq!(c.canvas, 5376.0);
    assert_eq!(c.front, LensCal { radius: 2646.011, cx: 2696.370, cy: 2674.760 });
    // the back lens's centre comes back into its own half of the canvas
    assert!((c.back.cx - (8070.730 - 5376.0)).abs() < 1e-9 && c.back.cy == 2688.640 && c.back.radius == 2640.688);
    // found among other bytes, after a candidate that doesn't parse
    let mut tail = b"\x00\x01p2_garbage\xffINS".to_vec();
    tail.extend_from_slice(X5.as_bytes());
    tail.extend_from_slice(b"\x00standard");
    assert_eq!(find_calibration(&tail), Some(c));
    assert_eq!(find_calibration(b"no calibration here"), None);
}

#[test]
fn hostile_calibration_text_is_refused_without_panicking() {
    for s in [
        "",
        "p2_",
        "p2_1_2_3",
        "3_1_1_1_0_0_0_1_1_1_0_0_0_10_5",
        "2_-5_1_1_0_0_0_1_1_1_0_0_0_10_5",
        "2_NaN_1_1_0_0_0_1_1_1_0_0_0_10_5",
        "2_1_1e300_1_0_0_0_1_1_1_0_0_0_10_5",
        "2_1_1_1_0_0_0_1_1_1_0_0_0_5_10",
    ] {
        assert_eq!(parse_calibration(s), None, "{s}");
    }
    let bytes: Vec<u8> = X5.bytes().collect();
    for cut in 0..bytes.len() {
        let _ = find_calibration(&bytes[..cut]);
    }
    for i in 0..bytes.len() {
        let mut b = bytes.clone();
        b[i] ^= 0x55;
        let _ = find_calibration(&b);
    }
}

#[test]
fn lens_curve_reaches_the_calibration_radius_at_alpha_and_grows() {
    let r = 2646.0;
    assert!((lens_radius(LENS_ALPHA_DEG.to_radians(), r) - r).abs() < 1e-9);
    assert_eq!(lens_radius(0.0, r), 0.0);
    let mut last = 0.0;
    for d in 1..=110 {
        let v = lens_radius(f64::from(d).to_radians(), r);
        assert!(v > last, "{d}°");
        last = v;
    }
}

/// Square lens pictures of one value each (front bright, back dark), `size` pixels.
fn flat(size: usize, v: u8) -> Vec<u8> {
    vec![v; size * size]
}

#[test]
fn front_lens_fills_the_middle_and_back_lens_the_sides() {
    let (size, cal) = (96usize, Calibration::nominal());
    let (f, b): (&'static [u8], &'static [u8]) = (flat(size, 200).leak(), flat(size, 40).leak());
    let plane = |px: &'static [u8]| Plane { px, w: size, h: size, x0: 0.0, size: size as f64 };
    let rig = Rig::new(cal, [0.0; 3]);
    let (ow, oh) = (128, 64);
    let o = stitch_plane(&rig, plane(f), plane(b), ow, oh);
    let at = |x: usize, y: usize| o[y * ow + x];
    // longitude 0 (the middle) is the front lens, ±180° (the edges) the back lens
    assert_eq!(at(ow / 2, oh / 2), 200);
    assert_eq!(at(0, oh / 2), 40);
    assert_eq!(at(ow - 1, oh / 2), 40);
    // near ±90° the two are blended, the same way on both sides
    let (l, r) = (at(ow / 4, oh / 2), at(ow - 1 - ow / 4, oh / 2));
    assert!(l > 40 && l < 200 && l.abs_diff(r) <= 1, "{l} {r}");
    // side by side in one picture (front left), as in an .lrv
    let mut sbs = vec![0u8; size * 2 * size];
    for y in 0..size {
        sbs[y * size * 2..y * size * 2 + size].fill(200);
        sbs[y * size * 2 + size..(y + 1) * size * 2].fill(40);
    }
    let left = Plane { px: &sbs, w: size * 2, h: size, x0: 0.0, size: size as f64 };
    let right = Plane { x0: size as f64, ..left };
    let o = stitch_plane(&rig, left, right, ow, oh);
    assert_eq!(o[(oh / 2) * ow + ow / 2], 200);
    assert_eq!(o[(oh / 2) * ow], 40);
}

#[test]
fn alignment_keeps_zero_without_detail_and_hostile_planes_dont_panic() {
    let size = 64usize;
    let (f, b) = (flat(size, 120), flat(size, 120));
    fn p(px: &[u8]) -> Plane<'_> {
        Plane { px, w: 64, h: 64, x0: 0.0, size: 64.0 }
    }
    assert_eq!(align(Calibration::nominal(), p(&f), p(&b)), [0.0; 3]);
    let rig = Rig::new(Calibration::nominal(), [f64::NAN, 1e300, -1e300]);
    let empty = Plane { px: &[], w: 0, h: 0, x0: 0.0, size: 0.0 };
    let short = Plane { px: &f[..10], w: size, h: size, x0: -5.0, size: 1e9 };
    for (a, b) in [(empty, empty), (short, empty), (p(&f), short)] {
        let o = stitch_plane(&rig, a, b, 16, 8);
        assert_eq!(o.len(), 128);
    }
    assert!(stitch_plane(&rig, empty, empty, 0, 0).is_empty());
}

/// A real clip, when `FILMCRAFT_INSV_SAMPLE` names one: it opens as 2:1 360° video and its stitched
/// frame at 2 s is written to `FILMCRAFT_INSV_OUT` (a PPM) when that is set.
#[test]
#[ignore = "needs an Insta360 clip (FILMCRAFT_INSV_SAMPLE)"]
fn stitches_a_real_clip() {
    let Ok(path) = std::env::var("FILMCRAFT_INSV_SAMPLE") else { return };
    let bytes: Arc<[u8]> = std::fs::read(&path).unwrap().into();
    let src = opener(&path, bytes).expect("an Insta360 clip").unwrap();
    let v = src.info().video.clone().unwrap();
    assert_eq!(v.width, v.height * 2);
    assert_eq!(v.spherical.map(|s| s.projection), Some(Projection::Equirectangular));
    // the first frame decodes from the clip's start and measures the back lens's alignment
    let started = std::time::Instant::now();
    src.video_frame(FrameRequest { time: Tick::ZERO, scale: 0.25 }).unwrap();
    eprintln!("first frame (with alignment) in {:?}", started.elapsed());
    let started = std::time::Instant::now();
    let f = src.video_frame(FrameRequest { time: Tick::from_seconds_f64(2.0), scale: 0.5 }).unwrap();
    eprintln!("stitched {}×{} in {:?}", f.width, f.height, started.elapsed());
    assert_eq!(f.width, f.height * 2);
    // the same lens pictures again (decoded already): the stitch alone; then the next frame
    for (t, scale) in [(2.0, 0.25), (2.0, 1.0), (2.0 + 1.0 / 29.97, 0.5)] {
        let started = std::time::Instant::now();
        let g = src.video_frame(FrameRequest { time: Tick::from_seconds_f64(t), scale }).unwrap();
        eprintln!("{}×{} at {t:.3} s in {:?}", g.width, g.height, started.elapsed());
    }
    if let Ok(out) = std::env::var("FILMCRAFT_INSV_OUT") {
        let mut ppm = format!("P6\n{} {}\n255\n", f.width, f.height).into_bytes();
        ppm.extend(f.to_rgba8().as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]));
        std::fs::write(out, ppm).unwrap();
    }
}
