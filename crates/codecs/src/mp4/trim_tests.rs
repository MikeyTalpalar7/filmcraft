use super::*;
use std::path::PathBuf;

/// A dual-lens-like movie: two H.264 video tracks (keyframe every 10 frames, 25 fps, 3 s) and AAC
/// audio, made by ffmpeg, with a camera-style trailer of raw bytes after its last box.
fn fixture(dir: &Path) -> Option<(PathBuf, Vec<u8>)> {
    let ff = filmcraft_testkit::ffmpeg_or_skip("lossless trim")?;
    let path = dir.join("dual.mp4");
    let ok = std::process::Command::new(ff)
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=64x64:r=25:d=3",
            "-f",
            "lavfi",
            "-i",
            "testsrc=s=64x64:r=25:d=3",
            "-f",
            "lavfi",
            "-i",
            "sine=f=440:d=3",
        ])
        .args(["-map", "0:v", "-map", "1:v", "-map", "2:a", "-c:v", "libx264", "-g", "10", "-bf", "0", "-c:a", "aac", "-movflags", "+faststart"])
        .arg(&path)
        .status()
        .ok()?
        .success();
    if !ok {
        filmcraft_testkit::skip("lossless trim", "ffmpeg couldn't make the fixture (libx264 / aac)");
        return None;
    }
    let trailer: Vec<u8> = (0..300u32).map(|i| (i * 7 % 251) as u8).chain(*b"\x8d\xb4\x2d\x69\x4c\xcc\x41\x87\x90\xed\xff\x43\x9f\xe0\x26\xbf").collect();
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).ok()?;
    f.write_all(&trailer).ok()?;
    Some((path, trailer))
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fc-trim-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn trims_every_track_on_keyframes_without_touching_the_samples() {
    let d = dir("ok");
    let Some((src, trailer)) = fixture(&d) else { return };
    let dst = d.join("cut.mp4");
    let r = trim_file(&src, &dst, 1.1, 2.1).unwrap();
    // widened to keyframes (every 0.4 s): the one at or before 1.1 s is 0.8 s, at or after 2.1 s 2.4 s
    assert!((r.start - 0.8).abs() < 1e-6 && (r.end - 2.4).abs() < 1e-6, "{r:?}");
    let a = std::fs::read(&src).unwrap();
    let b = std::fs::read(&dst).unwrap();
    let (fa, fb) = (filmcraft_isobmff::open(&a[..]).unwrap(), filmcraft_isobmff::open(&b[..]).unwrap());
    assert_eq!(fb.tracks.len(), 3);
    for (ta, tb) in fa.tracks.iter().zip(&fb.tracks) {
        assert!(!tb.samples.is_empty());
        if tb.kind == filmcraft_isobmff::TrackKind::Video {
            // 0.8 s … 2.4 s at 25 fps: frames 20…59, the first a keyframe
            assert_eq!(tb.samples.len(), 40);
            assert!(tb.samples[0].is_sync);
            for (i, x) in tb.samples.iter().enumerate() {
                let orig = &ta.samples[20 + i];
                assert_eq!(&b[x.offset as usize..][..x.size as usize], &a[orig.offset as usize..][..orig.size as usize]);
                assert_eq!(x.is_sync, orig.is_sync);
            }
        }
    }
    // the trailer is still at the end
    assert!(b.ends_with(&trailer));
    // ffmpeg decodes it all without an error
    if let Some(ff) = filmcraft_testkit::ffmpeg() {
        let out = std::process::Command::new(ff).args(["-v", "error", "-i"]).arg(&dst).args(["-map", "0", "-f", "null", "-"]).output().unwrap();
        assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn hostile_trims_are_errors_not_panics() {
    let d = dir("bad");
    let Some((src, _)) = fixture(&d) else { return };
    let dst = d.join("cut.mp4");
    for (a, b) in [(2.0, 1.0), (f64::NAN, 1.0), (0.0, f64::INFINITY), (-5.0, -1.0)] {
        let _ = trim_file(&src, &dst, a, b);
    }
    assert!(trim_file(&src, &src, 0.0, 1.0).is_err(), "never over the original");
    // damaged copies of the file
    let bytes = std::fs::read(&src).unwrap();
    let bad = d.join("bad.mp4");
    for cut in [8, 100, bytes.len() / 2, bytes.len() - 400] {
        std::fs::write(&bad, &bytes[..cut]).unwrap();
        let _ = trim_file(&bad, &dst, 0.5, 1.5);
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// Your own Insta360 clip (set `FILMCRAFT_INSV_SAMPLE`): trimmed, both lenses and the audio keep
/// their samples byte for byte, the `inst` box's frame times follow the kept frames, and FilmCraft
/// opens the result as a stitched 360° clip.
#[test]
#[ignore = "needs a real .insv (FILMCRAFT_INSV_SAMPLE)"]
fn trims_a_real_insv() {
    let Some(src) = std::env::var_os("FILMCRAFT_INSV_SAMPLE").map(PathBuf::from) else { return };
    let d = dir("insv");
    let dst = d.join("VID_trim_00_001.insv");
    let r = trim_file(&src, &dst, 3.0, 6.0).unwrap();
    eprintln!("{r:?}");
    assert!(r.frame_times_cut, "the frame times were found and cut");
    let fa = filmcraft_isobmff::open(File::open(&src).unwrap()).unwrap();
    let fb = filmcraft_isobmff::open(File::open(&dst).unwrap()).unwrap();
    assert_eq!(fa.tracks.len(), fb.tracks.len());
    let (mut a, mut b) = (File::open(&src).unwrap(), File::open(&dst).unwrap());
    for (ta, tb) in fa.tracks.iter().zip(&fb.tracks) {
        if ta.kind != filmcraft_isobmff::TrackKind::Video {
            continue;
        }
        let first =
            ta.samples.iter().position(|x| x.offset != 0 && fa.read_sample(&a, 0, 0).is_ok() && x.pts as f64 / ta.timescale as f64 >= r.start - 1e-6).unwrap();
        for (i, x) in tb.samples.iter().enumerate() {
            let o = &ta.samples[first + i];
            let read = |f: &mut File, off: u64, n: u32| {
                let mut v = vec![0; n as usize];
                f.seek(SeekFrom::Start(off)).unwrap();
                f.read_exact(&mut v).unwrap();
                v
            };
            assert_eq!(read(&mut b, x.offset, x.size), read(&mut a, o.offset, o.size), "sample {i}");
        }
    }
    // FilmCraft opens it as a stitched 360° clip
    let bytes: std::sync::Arc<[u8]> = std::fs::read(&dst).unwrap().into();
    let src2 = crate::open_bytes("trim.insv", bytes).unwrap();
    let v = src2.info().video.clone().unwrap();
    assert!(v.spherical.is_some() && v.width == 2 * v.height, "{v:?}");
    if let Some(ff) = filmcraft_testkit::ffmpeg() {
        let out = std::process::Command::new(ff).args(["-v", "error", "-i"]).arg(&dst).args(["-map", "0", "-f", "null", "-"]).output().unwrap();
        assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    if let Some(keep) = std::env::var_os("FILMCRAFT_INSV_OUT") {
        std::fs::copy(&dst, keep).unwrap();
    }
    let _ = std::fs::remove_dir_all(&d);
}
