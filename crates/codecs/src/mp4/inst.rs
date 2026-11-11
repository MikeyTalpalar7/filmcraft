//! The Insta360 `inst` box at the end of an `.insv`, as far as a lossless trim needs it. Observed
//! in Insta360 X5 files (no published specification):
//!
//! - It ends with a 40-byte footer: the length of the box's body (u32, little-endian), a version
//!   (u32) and the ASCII magic `8db42d694ccc418790edff439fe026bf`.
//! - Before it, records each followed by a 6-byte header: a format byte, a record id byte and the
//!   data's length (u32 LE), so they are found from the end backwards.
//! - Record 9 holds one entry per video frame, each starting with the frame's time on the
//!   camera's clock (u64 LE, milliseconds); record 3 the gyro / accelerometer samples on the same
//!   clock (which Insta360 Studio matches to the frames by time for its stabilization).
//!
//! After a trim only the kept frames' entries stay in record 9, so the first frame of the trimmed
//! file still meets the motion data of its moment; the motion data and every other record are
//! kept whole. Anything that doesn't look as described leaves the box as it was.

/// The magic at the end of the box.
pub const MAGIC: &[u8; 32] = b"8db42d694ccc418790edff439fe026bf";
/// The footer: body length, version, magic.
const FOOTER: usize = 40;
/// The record of per-frame entries.
const FRAME_TIMES: u8 = 9;

fn u32_le(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
}

fn u64_le(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8).map(|x| u64::from_le_bytes([x[0], x[1], x[2], x[3], x[4], x[5], x[6], x[7]]))
}

/// The per-frame record of `body` for a video of `frames` frames: (data start, entry size,
/// header position).
fn frame_record(body: &[u8], frames: usize) -> Option<(usize, usize, usize)> {
    let len = body.len();
    if frames == 0 || len < FOOTER + 6 || !body.ends_with(MAGIC) || u32_le(body, len - FOOTER)? as usize != len {
        return None;
    }
    // the last such header before the footer whose data is `frames` rising times
    (0..len - FOOTER - 5).rev().find_map(|h| {
        if body.get(h + 1) != Some(&FRAME_TIMES) {
            return None;
        }
        let size = u32_le(body, h + 2)? as usize;
        let stride = size.checked_div(frames)?;
        if size == 0 || !size.is_multiple_of(frames) || !(8..=4096).contains(&stride) || size > h {
            return None;
        }
        let start = h - size;
        let rising = (1..frames).all(|i| matches!((u64_le(body, start + (i - 1) * stride), u64_le(body, start + i * stride)), (Some(a), Some(b)) if b > a));
        rising.then_some((start, stride, h))
    })
}

/// `body` with the per-frame record cut to frames `keep.0..keep.1` of `frames`, the record's and
/// the footer's lengths updated. None when the box isn't as described (keep it as it was).
pub fn cut_frames(body: &[u8], keep: (usize, usize), frames: usize) -> Option<Vec<u8>> {
    let (start, stride, h) = frame_record(body, frames)?;
    let (a, b) = (keep.0.min(frames), keep.1.min(frames));
    if a >= b {
        return None;
    }
    let data = body.get(start + a * stride..start + b * stride)?;
    let mut out = Vec::with_capacity(body.len());
    out.extend_from_slice(body.get(..start)?);
    out.extend_from_slice(data);
    out.extend_from_slice(body.get(h..h + 2)?);
    out.extend_from_slice(&u32::try_from(data.len()).ok()?.to_le_bytes());
    out.extend_from_slice(body.get(h + 6..)?);
    let n = out.len();
    let total = u32::try_from(n).ok()?.to_le_bytes();
    out.get_mut(n - FOOTER..n - FOOTER + 4)?.copy_from_slice(&total);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body like an X5's: some other data, a gyro-like record (id 3), the frame record (id 9),
    /// a small record, and the footer.
    fn body(frames: usize) -> Vec<u8> {
        let mut b = vec![0u8; 6];
        b.extend(b"info");
        let gyro: Vec<u8> = (0..50u64).flat_map(|i| (7_000_000 + i * 1000).to_le_bytes().into_iter().chain([1; 12])).collect();
        let n = gyro.len() as u32;
        b.extend(&gyro);
        b.extend([0, 3]);
        b.extend(n.to_le_bytes());
        let ft: Vec<u8> = (0..frames as u64).flat_map(|i| (8625 + i * 33).to_le_bytes().into_iter().chain([7; 40])).collect();
        let n = ft.len() as u32;
        b.extend(&ft);
        b.extend([0, 9]);
        b.extend(n.to_le_bytes());
        b.extend([5, 5, 5, 0, 1, 3, 0, 0, 0]);
        let total = (b.len() + FOOTER) as u32;
        b.extend(total.to_le_bytes());
        b.extend(3u32.to_le_bytes());
        b.extend(MAGIC);
        b
    }

    #[test]
    fn keeps_only_the_kept_frames_times() {
        let b = body(20);
        let out = cut_frames(&b, (4, 10), 20).unwrap();
        assert_eq!(out.len(), b.len() - 14 * 48);
        let (start, stride, h) = frame_record(&out, 6).unwrap();
        assert_eq!(stride, 48);
        assert_eq!(u64_le(&out, start), Some(8625 + 4 * 33));
        assert_eq!(u64_le(&out, start + 5 * 48), Some(8625 + 9 * 33));
        assert_eq!(u32_le(&out, h + 2), Some(6 * 48));
        // the footer's length follows; the gyro record and the rest are untouched
        assert_eq!(u32_le(&out, out.len() - FOOTER), Some(out.len() as u32));
        assert_eq!(&out[..start], &b[..start]);
        assert!(out.ends_with(MAGIC));
    }

    #[test]
    fn anything_unexpected_leaves_the_box_alone() {
        let b = body(20);
        // the wrong frame count, an empty range, a damaged footer, no magic
        assert!(cut_frames(&b, (0, 5), 21).is_none());
        assert!(cut_frames(&b, (5, 5), 20).is_none());
        let mut bad = b.clone();
        let at = bad.len() - FOOTER;
        bad[at] ^= 1;
        assert!(cut_frames(&bad, (0, 5), 20).is_none());
        assert!(cut_frames(&b[..b.len() - 1], (0, 5), 20).is_none());
        // hostile bytes never panic
        for i in 0..b.len() {
            let mut x = b.clone();
            x[i] ^= 0xff;
            let _ = cut_frames(&x, (2, 9), 20);
        }
        assert!(cut_frames(&[], (0, 1), 1).is_none());
    }
}
