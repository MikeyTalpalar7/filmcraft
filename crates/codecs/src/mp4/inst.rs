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
//! - Record 0, just before the footer, is a directory of 10-byte entries: record id, format byte,
//!   data length (u32 LE) and the data's offset in the body (u32 LE). Insta360 Studio finds the
//!   records through it, so a file whose directory disagrees with its records opens as plain
//!   video.
//!
//! After a trim only the kept frames' entries stay in record 9, so the first frame of the trimmed
//! file still meets the motion data of its moment; the directory gets record 9's new length and
//! the new offsets of the records after it; the motion data and every other record are kept whole.
//! Anything that doesn't look as described leaves the box as it was.

/// The magic at the end of the box.
pub const MAGIC: &[u8; 32] = b"8db42d694ccc418790edff439fe026bf";
/// The footer: body length, version, magic.
const FOOTER: usize = 40;
/// The record of per-frame entries.
const FRAME_TIMES: u8 = 9;
/// The directory record, and the size of one of its entries.
const DIRECTORY: u8 = 0;
const ENTRY: usize = 10;
/// How far before the footer the directory is looked for, and its largest size.
const DIRECTORY_SEARCH: usize = 1 << 16;

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

/// A directory entry at `at`: (record id, format, length, offset).
fn entry(b: &[u8], at: usize) -> Option<(u8, u8, usize, usize)> {
    Some((*b.get(at)?, *b.get(at + 1)?, u32_le(b, at + 2)? as usize, u32_le(b, at + 6)? as usize))
}

/// The directory's data (start, end) in `body`: the record 0 near the footer whose entries
/// include `frames` (record 9's format, length and offset).
fn directory(body: &[u8], frames: (u8, usize, usize)) -> Option<(usize, usize)> {
    let end = body.len().checked_sub(FOOTER + 6)?;
    let (format, size, start) = frames;
    (end.saturating_sub(DIRECTORY_SEARCH)..=end).rev().find_map(|h| {
        if body.get(h + 1) != Some(&DIRECTORY) {
            return None;
        }
        let n = u32_le(body, h + 2)? as usize;
        if n == 0 || !n.is_multiple_of(ENTRY) || n > h || n > DIRECTORY_SEARCH {
            return None;
        }
        let s = h - n;
        let hit = (0..n / ENTRY).any(|k| entry(body, s + k * ENTRY) == Some((FRAME_TIMES, format, size, start)));
        hit.then_some((s, h))
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
    let old = h - start;
    let format = *body.get(h)?;
    let (dir_start, dir_end) = directory(body, (format, old, start))?;
    if dir_start < h + 6 {
        return None;
    }
    let data = body.get(start + a * stride..start + b * stride)?;
    let shrink = old - data.len();
    let mut out = Vec::with_capacity(body.len());
    out.extend_from_slice(body.get(..start)?);
    out.extend_from_slice(data);
    out.extend_from_slice(body.get(h..h + 2)?);
    out.extend_from_slice(&u32::try_from(data.len()).ok()?.to_le_bytes());
    out.extend_from_slice(body.get(h + 6..)?);
    // the directory moved back with everything after the frame record
    let new_len = u32::try_from(data.len()).ok()?;
    for at in (dir_start - shrink..dir_end - shrink).step_by(ENTRY) {
        let (id, _, size, offset) = entry(&out, at)?;
        if id == FRAME_TIMES && offset == start {
            out.get_mut(at + 2..at + 6)?.copy_from_slice(&new_len.to_le_bytes());
        } else if size > 0 && offset > start {
            let moved = u32::try_from(offset.checked_sub(shrink)?).ok()?;
            out.get_mut(at + 6..at + 10)?.copy_from_slice(&moved.to_le_bytes());
        }
    }
    let n = out.len();
    let total = u32::try_from(n).ok()?.to_le_bytes();
    out.get_mut(n.checked_sub(FOOTER)?..n - FOOTER + 4)?.copy_from_slice(&total);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body like an X5's: an info record (id 1), the frame record (id 9), a gyro-like record
    /// (id 3) after it, the directory (id 0), and the footer.
    fn body(frames: usize) -> Vec<u8> {
        let mut b = vec![];
        let mut dir = vec![0u8; 10 * ENTRY];
        let mut record = |b: &mut Vec<u8>, id: u8, data: &[u8]| {
            let at = id as usize * ENTRY;
            dir[at..at + 2].copy_from_slice(&[id, 0]);
            dir[at + 2..at + 6].copy_from_slice(&(data.len() as u32).to_le_bytes());
            dir[at + 6..at + 10].copy_from_slice(&(b.len() as u32).to_le_bytes());
            b.extend(data);
            b.extend([0, id]);
            b.extend((data.len() as u32).to_le_bytes());
        };
        record(&mut b, 1, b"info");
        let ft: Vec<u8> = (0..frames as u64).flat_map(|i| (8625 + i * 33).to_le_bytes().into_iter().chain([7; 40])).collect();
        record(&mut b, 9, &ft);
        let gyro: Vec<u8> = (0..50u64).flat_map(|i| (7_000_000 + i * 1000).to_le_bytes().into_iter().chain([1; 12])).collect();
        record(&mut b, 3, &gyro);
        b.extend(&dir);
        b.extend([0, 0]);
        b.extend((dir.len() as u32).to_le_bytes());
        b.extend([0; 32]);
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
        // the directory has record 9's new length, and the gyro record where it now is
        let (ds, _) = directory(&out, (0, 6 * 48, start)).unwrap();
        assert_eq!(entry(&out, ds + 9 * ENTRY), Some((9, 0, 6 * 48, start)));
        assert_eq!(entry(&out, ds + ENTRY), entry(&b, ds + 14 * 48 + ENTRY));
        let (_, _, gyro_len, gyro_at) = entry(&out, ds + 3 * ENTRY).unwrap();
        assert_eq!(gyro_at, h + 6);
        assert_eq!(u64_le(&out, gyro_at), Some(7_000_000));
        assert_eq!(u32_le(&out, gyro_at + gyro_len + 2), Some(gyro_len as u32));
    }

    #[test]
    fn a_box_without_a_directory_is_left_alone() {
        let mut b = body(20);
        // break the directory's entry for the frame record
        let (ds, _) = directory(&b, (0, 20 * 48, frame_record(&b, 20).unwrap().0)).unwrap();
        b[ds + 9 * ENTRY + 2] ^= 1;
        assert!(cut_frames(&b, (4, 10), 20).is_none());
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
