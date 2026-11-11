//! Lossless trim of an MP4 / MOV / Insta360 `.insv`: the samples between two times copied as they
//! are, no decoding or encoding, every track kept (both lenses of an `.insv`, its audio), and every
//! other box the file carries kept too (the camera's metadata, the Insta360 data after the movie).
//!
//! A compressed picture depends on the ones since its keyframe, so the trim starts at the last
//! keyframe at or before the start time and ends at the first keyframe at or after the end time
//! (the first video track sets them; the other video tracks take their keyframes nearest those
//! times, audio and other tracks the samples between them). The result can be a little longer than
//! asked; [`TrimReport`] says by how much.
//!
//! The `moov` is the original's with each kept track's sample tables rebuilt (`stts`, `ctts`,
//! `stss`, `stsz`, `stsc`, `stco`/`co64`), tables that index samples and can't be rebuilt dropped
//! (`sdtp`, sample groups, …), durations and the edit list set to the new length. The new file is
//! `ftyp`, `moov`, `mdat` (one chunk per sample, in time order), then the original's other
//! top-level boxes and any bytes after its last box, in order.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use filmcraft_isobmff::{Sample, TrackKind};

use super::tree::{self, Body, Node, TreeError};

/// What a trim kept, in seconds of the source (the first video track's presentation time).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrimReport {
    pub start: f64,
    pub end: f64,
    pub bytes: u64,
}

fn err<T>(m: impl Into<String>) -> Result<T, TreeError> {
    Err(TreeError(m.into()))
}

fn io(e: std::io::Error) -> TreeError {
    TreeError(e.to_string())
}

/// Sample-table boxes rebuilt for the kept samples, and ones indexing samples that are dropped.
const REBUILT: &[&[u8; 4]] = &[
    b"stts", b"ctts", b"stss", b"stsz", b"stz2", b"stsc", b"stco", b"co64", b"sdtp", b"sbgp", b"sgpd", b"subs", b"saiz", b"saio", b"stps", b"cslg", b"padb",
    b"stdp",
];

/// Top-level boxes, and where the bytes that aren't boxes start (a trailer, or the file's end).
fn top_level(f: &mut File, len: u64) -> Result<(Vec<([u8; 4], u64, u64)>, u64), TreeError> {
    let mut out = Vec::new();
    let mut off = 0u64;
    while off + 8 <= len && out.len() < 100_000 {
        let mut h = [0u8; 16];
        f.seek(SeekFrom::Start(off)).map_err(io)?;
        let n = f.read(&mut h).map_err(io)?;
        let size32 = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
        let kind = [h[4], h[5], h[6], h[7]];
        let size = match size32 {
            0 => len - off,
            1 if n >= 16 => u64::from_be_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]),
            _ => u64::from(size32),
        };
        // something that isn't a box (a camera's trailer): copied as bytes
        if size < 8 || off.checked_add(size).is_none_or(|e| e > len) || !kind.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
            break;
        }
        out.push((kind, off, size));
        off += size;
    }
    Ok((out, off))
}

/// One kept track: its id, timescale and kept samples (decode order).
struct Kept {
    id: u32,
    timescale: u32,
    samples: Vec<Sample>,
    /// The original had a sync-sample table (not every sample a keyframe).
    has_stss: bool,
}

fn secs(t: i64, ts: u32) -> f64 {
    t as f64 / f64::from(ts.max(1))
}

/// The video sample range [s, e) from the keyframe at or before `from` to the keyframe at or after
/// `to` (`slack`: how close counts as "at", seconds).
fn video_range(samples: &[Sample], ts: u32, from: f64, to: f64, slack: f64) -> (usize, usize) {
    let s = samples.iter().enumerate().filter(|(_, x)| x.is_sync && secs(x.pts, ts) <= from + slack).map(|(i, _)| i).next_back();
    let s = s.or_else(|| samples.iter().position(|x| x.is_sync)).unwrap_or(0);
    let e = samples.iter().enumerate().skip(s + 1).find(|(_, x)| x.is_sync && secs(x.pts, ts) >= to - slack).map_or(samples.len(), |(i, _)| i);
    (s, e.max(s))
}

/// Pick the samples of every track for `from`…`to` seconds.
fn choose(file: &filmcraft_isobmff::Mp4File, from: f64, to: f64) -> Result<(Vec<Kept>, f64, f64), TreeError> {
    let master = file.tracks.iter().position(|t| t.kind == TrackKind::Video && !t.samples.is_empty()).ok_or(TreeError("no video track".into()))?;
    let m = &file.tracks[master];
    let (s, e) = video_range(&m.samples, m.timescale, from, to, 1e-6);
    let start = m.samples.get(s).map_or(0.0, |x| secs(x.pts, m.timescale));
    let end = m
        .samples
        .get(e)
        .map_or_else(|| m.samples.iter().map(|x| secs(x.pts + i64::from(x.duration), m.timescale)).fold(0.0, f64::max), |x| secs(x.pts, m.timescale));
    if e <= s {
        return err("nothing to keep between those times");
    }
    let mut kept = Vec::new();
    for t in &file.tracks {
        let samples: Vec<Sample> = if t.kind == TrackKind::Video {
            // the other lens: its keyframes nearest the master's (half a frame either way)
            let frame = t.samples.first().map_or(0.0, |x| secs(i64::from(x.duration), t.timescale)) * 0.5;
            let (a, b) = video_range(&t.samples, t.timescale, start, end, frame);
            t.samples.get(a..b).unwrap_or(&[]).to_vec()
        } else {
            t.samples.iter().filter(|x| (start..end).contains(&secs(x.dts, t.timescale))).cloned().collect()
        };
        let has_stss = !t.samples.iter().all(|x| x.is_sync);
        kept.push(Kept { id: t.id, timescale: t.timescale, samples, has_stss });
    }
    Ok((kept, start, end))
}

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

/// Run-length (count, value) pairs.
fn runs<T: PartialEq + Copy>(values: impl Iterator<Item = T>) -> Vec<(u32, T)> {
    let mut out: Vec<(u32, T)> = Vec::new();
    for v in values {
        match out.last_mut() {
            Some((n, last)) if *last == v => *n += 1,
            _ => out.push((1, v)),
        }
    }
    out
}

/// The rebuilt sample tables of `k`, its samples at the file offsets `offsets`.
fn tables(k: &Kept, offsets: &[u64], wide: bool) -> Vec<Node> {
    let s = &k.samples;
    let mut out = Vec::new();
    let stts = runs(s.iter().map(|x| x.duration));
    let mut d = be32(stts.len() as u32).to_vec();
    stts.iter().for_each(|(n, v)| d.extend(be32(*n).iter().chain(&be32(*v))));
    out.push(Node::full(b"stts", 0, 0, &d));
    let ctts = runs(s.iter().map(|x| i32::try_from(x.pts - x.dts).unwrap_or(0)));
    if ctts.iter().any(|(_, o)| *o != 0) {
        let neg = ctts.iter().any(|(_, o)| *o < 0);
        let mut d = be32(ctts.len() as u32).to_vec();
        ctts.iter().for_each(|(n, o)| d.extend(be32(*n).iter().chain(&o.to_be_bytes())));
        out.push(Node::full(b"ctts", u8::from(neg), 0, &d));
    }
    if k.has_stss {
        let sync: Vec<u32> = s.iter().enumerate().filter(|(_, x)| x.is_sync).map(|(i, _)| i as u32 + 1).collect();
        let mut d = be32(sync.len() as u32).to_vec();
        sync.iter().for_each(|i| d.extend(be32(*i)));
        out.push(Node::full(b"stss", 0, 0, &d));
    }
    let mut d = vec![0, 0, 0, 0];
    d.extend(be32(s.len() as u32));
    s.iter().for_each(|x| d.extend(be32(x.size)));
    out.push(Node::full(b"stsz", 0, 0, &d));
    // one sample per chunk; a new stsc entry where the sample description changes
    let mut stsc: Vec<(u32, u32)> = Vec::new();
    for (i, x) in s.iter().enumerate() {
        if stsc.last().is_none_or(|(_, sdi)| *sdi != x.description_index.max(1)) {
            stsc.push((i as u32 + 1, x.description_index.max(1)));
        }
    }
    let mut d = be32(stsc.len() as u32).to_vec();
    stsc.iter().for_each(|(chunk, sdi)| d.extend(be32(*chunk).iter().chain(&be32(1)).chain(&be32(*sdi))));
    out.push(Node::full(b"stsc", 0, 0, &d));
    let mut d = be32(offsets.len() as u32).to_vec();
    for o in offsets {
        if wide {
            d.extend(o.to_be_bytes());
        } else {
            d.extend(be32(u32::try_from(*o).unwrap_or(u32::MAX)));
        }
    }
    out.push(Node::full(if wide { b"co64" } else { b"stco" }, 0, 0, &d));
    out
}

/// A full box's version byte.
fn version(n: &Node) -> u8 {
    n.data().and_then(|d| d.first()).copied().unwrap_or(0)
}

/// Write `value` as the duration field of `mvhd` / `mdhd` (after creation / modification times
/// and the timescale) or `tkhd` (after the times, track id and a reserved word).
fn set_duration(n: &mut Node, value: u64) {
    let v1 = version(n) == 1;
    let at = match (&n.kind, v1) {
        (b"mvhd" | b"mdhd", false) => 16,
        (b"mvhd" | b"mdhd", true) => 24,
        (b"tkhd", false) => 20,
        (b"tkhd", true) => 28,
        _ => return,
    };
    let Body::Data(d) = &mut n.body else { return };
    if v1 {
        if let Some(f) = d.get_mut(at..at + 8) {
            f.copy_from_slice(&value.to_be_bytes());
        }
    } else if let Some(f) = d.get_mut(at..at + 4) {
        f.copy_from_slice(&u32::try_from(value).unwrap_or(u32::MAX).to_be_bytes());
    }
}

fn tkhd_id(trak: &Node) -> Option<u32> {
    let t = trak.child(b"tkhd")?;
    let d = t.data()?;
    let at = if version(t) == 1 { 20 } else { 12 };
    let b = d.get(at..at + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn mvhd_timescale(moov: &Node) -> u32 {
    let Some(m) = moov.child(b"mvhd") else { return 1000 };
    let at = if version(m) == 1 { 20 } else { 12 };
    m.data().and_then(|d| d.get(at..at + 4)).map_or(1000, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])).max(1)
}

/// The original `moov` with the kept tracks' tables, durations and edit lists replaced (tracks
/// that weren't read are removed).
fn rebuild_moov(orig: &Node, kept: &[Kept], offsets: &[Vec<u64>], wide: bool) -> Result<Node, TreeError> {
    let mut moov = orig.clone();
    let movie_ts = mvhd_timescale(&moov);
    let mut longest = 0u64;
    let kids = moov.children_mut().ok_or(TreeError("moov".into()))?;
    kids.retain(|c| &c.kind != b"trak" || tkhd_id(c).is_some_and(|id| kept.iter().any(|k| k.id == id && !k.samples.is_empty())));
    // fragments and iTunes-style chapter references would point at the old samples
    kids.retain(|c| &c.kind != b"mvex");
    for trak in kids.iter_mut().filter(|c| &c.kind == b"trak") {
        let id = tkhd_id(trak).ok_or(TreeError("track id".into()))?;
        let (i, k) = kept.iter().enumerate().find(|(_, k)| k.id == id).ok_or(TreeError("track".into()))?;
        let media: u64 = k.samples.iter().map(|x| u64::from(x.duration)).sum();
        let movie = (u128::from(media) * u128::from(movie_ts) / u128::from(k.timescale.max(1))) as u64;
        longest = longest.max(movie);
        if let Some(t) = trak.child_mut(b"tkhd") {
            set_duration(t, movie);
        }
        // one edit: the whole track, from its first presented sample
        let first_pts = k.samples.iter().map(|x| x.pts).min().unwrap_or(0);
        let first_dts = k.samples.first().map_or(0, |x| x.dts);
        let media_time = (first_pts - first_dts).max(0);
        let mut e = be32(1).to_vec();
        e.extend((movie).to_be_bytes());
        e.extend(media_time.to_be_bytes());
        e.extend([0, 1, 0, 0]);
        let edts = Node::container(b"edts", vec![], vec![Node::full(b"elst", 1, 0, &e)]);
        let tk = trak.children_mut().ok_or(TreeError("trak".into()))?;
        tk.retain(|c| &c.kind != b"edts");
        let at = tk.iter().position(|c| &c.kind == b"tkhd").map_or(0, |p| p + 1);
        tk.insert(at, edts);
        let mdia = trak.child_mut(b"mdia").ok_or(TreeError("mdia".into()))?;
        if let Some(h) = mdia.child_mut(b"mdhd") {
            set_duration(h, media);
        }
        let stbl = mdia.child_mut(b"minf").and_then(|m| m.child_mut(b"stbl")).ok_or(TreeError("stbl".into()))?;
        let st = stbl.children_mut().ok_or(TreeError("stbl".into()))?;
        st.retain(|c| !REBUILT.contains(&&c.kind));
        st.extend(tables(k, offsets.get(i).map_or(&[][..], |o| o), wide));
    }
    if let Some(m) = moov.child_mut(b"mvhd") {
        set_duration(m, longest);
    }
    Ok(moov)
}

/// Trim the movie at `src` to `from`…`to` seconds (widened to keyframes) into `dst`.
pub fn trim_file(src: &Path, dst: &Path, from: f64, to: f64) -> Result<TrimReport, TreeError> {
    if !(from.is_finite() && to.is_finite() && to > from) {
        return err("the end must come after the start");
    }
    if src == dst {
        return err("write the trimmed file somewhere else than the original");
    }
    let mut f = File::open(src).map_err(io)?;
    let len = f.metadata().map_err(io)?.len();
    let file = filmcraft_isobmff::open(&f).map_err(|e| TreeError(e.to_string()))?;
    if file.fragmented {
        return err("fragmented MP4 files can't be trimmed losslessly yet");
    }
    let (boxes, tail) = top_level(&mut f, len)?;
    let &(_, moov_off, moov_size) = boxes.iter().find(|b| &b.0 == b"moov").ok_or(TreeError("no moov box".into()))?;
    let mut raw = vec![0u8; usize::try_from(moov_size).map_err(|_| TreeError("moov".into()))?];
    f.seek(SeekFrom::Start(moov_off)).map_err(io)?;
    f.read_exact(&mut raw).map_err(io)?;
    let orig = tree::parse(&raw)?;
    let (kept, start, end) = choose(&file, from, to)?;
    // the samples in time order (one chunk each)
    let mut order: Vec<(f64, usize, usize)> = Vec::new();
    for (t, k) in kept.iter().enumerate() {
        order.extend(k.samples.iter().enumerate().map(|(i, x)| (secs(x.dts, k.timescale), t, i)));
    }
    order.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let data: u64 = order.iter().filter_map(|&(_, t, i)| kept.get(t)?.samples.get(i)).map(|x| u64::from(x.size)).sum();
    let ftyp = boxes.iter().find(|b| &b.0 == b"ftyp").copied();
    let ftyp_len = ftyp.map_or(0, |b| b.2);
    // layout: offsets depend on the moov's size, which depends on the offsets' width only
    let place = |base: u64| -> Vec<Vec<u64>> {
        let mut offs: Vec<Vec<u64>> = kept.iter().map(|k| vec![0; k.samples.len()]).collect();
        let mut at = base;
        for &(_, t, i) in &order {
            if let (Some(o), Some(x)) = (offs.get_mut(t).and_then(|v| v.get_mut(i)), kept.get(t).and_then(|k| k.samples.get(i))) {
                *o = at;
                at += u64::from(x.size);
            }
        }
        offs
    };
    let probe = rebuild_moov(&orig, &kept, &place(0), true)?.to_bytes().len() as u64;
    let wide = ftyp_len + probe + 16 + data > u64::from(u32::MAX);
    let first = rebuild_moov(&orig, &kept, &place(0), wide)?.to_bytes().len() as u64;
    let base = ftyp_len + first + 16;
    let moov = rebuild_moov(&orig, &kept, &place(base), wide)?.to_bytes();
    if moov.len() as u64 != first {
        return err("internal: the movie header changed size");
    }
    let tmp = dst.with_extension("trim.tmp");
    let result = (|| -> Result<u64, TreeError> {
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&tmp).map_err(io)?);
        let copy = |f: &mut File, out: &mut BufWriter<File>, off: u64, n: u64| -> Result<(), TreeError> {
            f.seek(SeekFrom::Start(off)).map_err(io)?;
            let got = std::io::copy(&mut f.take(n), out).map_err(io)?;
            if got != n { err("the source file ended early") } else { Ok(()) }
        };
        if let Some((_, off, size)) = ftyp {
            copy(&mut f, &mut out, off, size)?;
        }
        out.write_all(&moov).map_err(io)?;
        out.write_all(&1u32.to_be_bytes()).map_err(io)?;
        out.write_all(b"mdat").map_err(io)?;
        out.write_all(&(data + 16).to_be_bytes()).map_err(io)?;
        for &(_, t, i) in &order {
            if let Some(x) = kept.get(t).and_then(|k| k.samples.get(i)) {
                copy(&mut f, &mut out, x.offset, u64::from(x.size))?;
            }
        }
        // the original's other boxes (camera metadata), then whatever follows its last box
        for &(kind, off, size) in &boxes {
            if !matches!(&kind, b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide") {
                copy(&mut f, &mut out, off, size)?;
            }
        }
        if tail < len {
            copy(&mut f, &mut out, tail, len - tail)?;
        }
        let file = out.into_inner().map_err(|e| io(e.into_error()))?;
        file.sync_all().map_err(io)?;
        file.metadata().map(|m| m.len()).map_err(io)
    })();
    match result {
        Ok(bytes) => {
            std::fs::rename(&tmp, dst).map_err(io)?;
            Ok(TrimReport { start, end, bytes })
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
#[path = "trim_tests.rs"]
mod tests;
