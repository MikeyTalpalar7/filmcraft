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
