//! An editable tree of ISO BMFF boxes (ISO/IEC 14496-12) for rewriting a `moov` box: add boxes,
//! change a table, and write it back with every size recomputed.
//!
//! Only the containers on the way to a video track's sample entry are opened (`moov`, `trak`,
//! `mdia`, `minf`, `stbl`, `edts`, `dinf`, `udta`, `stsd` and visual sample entries); every other
//! box is kept as its bytes. Parsing is bounded (depth, count) and every size is checked, so a
//! damaged box is an error, never a panic.

use std::fmt;

/// Deepest nesting opened (a `moov` sample entry is 7 levels down).
const MAX_DEPTH: usize = 12;
/// Most boxes in one tree.
const MAX_BOXES: usize = 1 << 20;

/// A box: its four-character type, the bytes of a full box's version/flags or a sample entry's
/// fixed fields before its children (`prefix`), then either children or raw payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub kind: [u8; 4],
    pub prefix: Vec<u8>,
    pub body: Body,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    Children(Vec<Node>),
    Data(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeError(pub String);

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MP4 structure: {}", self.0)
    }
}

impl std::error::Error for TreeError {}

fn err<T>(m: &str) -> Result<T, TreeError> {
    Err(TreeError(m.into()))
}

/// Visual sample entry formats whose children (`avcC`, `colr`, `st3d`, …) are opened.
const VISUAL: &[&[u8; 4]] =
    &[b"avc1", b"avc3", b"hvc1", b"hev1", b"av01", b"vp09", b"apch", b"apcn", b"apcs", b"apco", b"ap4h", b"ap4x", b"mp4v", b"dvh1", b"dvhe"];
/// Size of a visual sample entry's fixed fields (SampleEntry 8 + VisualSampleEntry 70).
pub const VISUAL_FIELDS: usize = 78;

/// How many bytes of `kind`'s body come before its children, or None for a leaf.
fn container_prefix(kind: &[u8; 4]) -> Option<usize> {
    match kind {
        b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"edts" | b"dinf" | b"udta" => Some(0),
        // version/flags and the entry count
        b"stsd" => Some(8),
        k if VISUAL.contains(&k) => Some(VISUAL_FIELDS),
        _ => None,
    }
}

impl Node {
    pub fn container(kind: &[u8; 4], prefix: Vec<u8>, children: Vec<Node>) -> Node {
        Node { kind: *kind, prefix, body: Body::Children(children) }
    }

    pub fn leaf(kind: &[u8; 4], data: Vec<u8>) -> Node {
        Node { kind: *kind, prefix: Vec::new(), body: Body::Data(data) }
    }

    /// A full box (version, 24-bit flags) holding `data`.
    pub fn full(kind: &[u8; 4], version: u8, flags: u32, data: &[u8]) -> Node {
        let mut d = Vec::with_capacity(4 + data.len());
        d.push(version);
        d.extend_from_slice(&flags.to_be_bytes()[1..]);
        d.extend_from_slice(data);
        Node::leaf(kind, d)
    }

    pub fn children(&self) -> &[Node] {
        match &self.body {
            Body::Children(c) => c,
            Body::Data(_) => &[],
        }
    }

    pub fn children_mut(&mut self) -> Option<&mut Vec<Node>> {
        match &mut self.body {
            Body::Children(c) => Some(c),
            Body::Data(_) => None,
        }
    }

    pub fn data(&self) -> Option<&[u8]> {
        match &self.body {
            Body::Data(d) => Some(d),
            Body::Children(_) => None,
        }
    }

    pub fn child(&self, kind: &[u8; 4]) -> Option<&Node> {
        self.children().iter().find(|c| &c.kind == kind)
    }

    pub fn child_mut(&mut self, kind: &[u8; 4]) -> Option<&mut Node> {
        self.children_mut()?.iter_mut().find(|c| &c.kind == kind)
    }

    /// The box serialized, sizes recomputed (a 64-bit size when it doesn't fit 32 bits).
    pub fn write(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&self.kind);
        out.extend_from_slice(&self.prefix);
        match &self.body {
            Body::Children(c) => c.iter().for_each(|n| n.write(out)),
            Body::Data(d) => out.extend_from_slice(d),
        }
        let size = out.len() - start;
        match u32::try_from(size) {
            Ok(s) => out.splice(start..start + 4, s.to_be_bytes()).for_each(drop),
            Err(_) => {
                // size 1 and a 64-bit size after the type
                let big = (size as u64 + 8).to_be_bytes();
                out.splice(start..start + 4, 1u32.to_be_bytes()).for_each(drop);
                out.splice(start + 8..start + 8, big).for_each(drop);
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.write(&mut v);
        v
    }
}

/// One box's header at `off` in `b`: (type, header length, total size).
pub fn header(b: &[u8], off: usize, end: usize) -> Result<([u8; 4], usize, usize), TreeError> {
    let h = b.get(off..off.checked_add(8).ok_or(TreeError("offset".into()))?).ok_or(TreeError("truncated box header".into()))?;
    let size32 = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
    let kind = [h[4], h[5], h[6], h[7]];
    let (hl, size) = match size32 {
        0 => (8, end.checked_sub(off).ok_or(TreeError("offset".into()))?),
        1 => {
            let l = b.get(off + 8..off + 16).ok_or(TreeError("truncated large size".into()))?;
            let s = u64::from_be_bytes([l[0], l[1], l[2], l[3], l[4], l[5], l[6], l[7]]);
            (16, usize::try_from(s).map_err(|_| TreeError("box too large".into()))?)
        }
        s => (8, s as usize),
    };
    if size < hl || off.checked_add(size).is_none_or(|e| e > end) {
        return err("a box runs past its parent");
    }
    Ok((kind, hl, size))
}

/// Parse the box at the start of `b` (which must be exactly one box).
pub fn parse(b: &[u8]) -> Result<Node, TreeError> {
    let mut count = 0usize;
    let (node, used) = parse_at(b, 0, b.len(), 0, &mut count)?;
    if used != b.len() {
        return err("bytes after the box");
    }
    Ok(node)
}

fn parse_at(b: &[u8], off: usize, end: usize, depth: usize, count: &mut usize) -> Result<(Node, usize), TreeError> {
    *count += 1;
    if depth > MAX_DEPTH || *count > MAX_BOXES {
        return err("boxes nested too deeply or too many boxes");
    }
    let (kind, hl, size) = header(b, off, end)?;
    let body = b.get(off + hl..off + size).ok_or(TreeError("truncated box".into()))?;
    let node = match container_prefix(&kind) {
        Some(p) if body.len() >= p => {
            let mut children = Vec::new();
            let mut at = p;
            while at < body.len() {
                // a few zero bytes of padding may end a container (QuickTime)
                if body.len() - at < 8 && body[at..].iter().all(|&x| x == 0) {
                    break;
                }
                let (c, used) = parse_at(body, at, body.len(), depth + 1, count)?;
                children.push(c);
                at += used;
            }
            Node { kind, prefix: body[..p].to_vec(), body: Body::Children(children) }
        }
        _ => Node { kind, prefix: Vec::new(), body: Body::Data(body.to_vec()) },
    };
    Ok((node, size))
}

/// The handler type (`vide`, `soun`, …) of a `trak`.
pub fn handler(trak: &Node) -> Option<[u8; 4]> {
    let d = trak.child(b"mdia")?.child(b"hdlr")?.data()?;
    let h = d.get(8..12)?;
    Some([h[0], h[1], h[2], h[3]])
}

/// Add `delta` to every chunk offset (`stco`, `co64`) at or past `from` of every track in `moov`:
/// what moving the media data there by `delta` bytes needs. Fails when an offset would overflow.
pub fn shift_chunk_offsets(moov: &mut Node, from: u64, delta: i64) -> Result<(), TreeError> {
    let Some(traks) = moov.children_mut() else { return Ok(()) };
    for trak in traks.iter_mut().filter(|t| &t.kind == b"trak") {
        let Some(stbl) = trak.child_mut(b"mdia").and_then(|m| m.child_mut(b"minf")).and_then(|m| m.child_mut(b"stbl")) else { continue };
        let Some(kids) = stbl.children_mut() else { continue };
        for k in kids.iter_mut() {
            let wide = match &k.kind {
                b"stco" => false,
                b"co64" => true,
                _ => continue,
            };
            let Body::Data(d) = &mut k.body else { continue };
            let n = d.get(4..8).map_or(0, |c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])) as usize;
            let w = if wide { 8 } else { 4 };
            if d.len() < 8 + n.saturating_mul(w) {
                return err("a chunk offset table is truncated");
            }
            for i in 0..n {
                let at = 8 + i * w;
                if wide {
                    let o = u64::from_be_bytes(d[at..at + 8].try_into().map_err(|_| TreeError("co64".into()))?);
                    if o < from {
                        continue;
                    }
                    let v = o.checked_add_signed(delta).ok_or(TreeError("chunk offset out of range".into()))?;
                    d[at..at + 8].copy_from_slice(&v.to_be_bytes());
                } else {
                    let o = u32::from_be_bytes(d[at..at + 4].try_into().map_err(|_| TreeError("stco".into()))?);
                    if u64::from(o) < from {
                        continue;
                    }
                    let v = u64::from(o).checked_add_signed(delta).and_then(|v| u32::try_from(v).ok()).ok_or(TreeError("chunk offset out of range".into()))?;
                    d[at..at + 4].copy_from_slice(&v.to_be_bytes());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_moov() -> Node {
        let avc1 = Node::container(b"avc1", vec![0; VISUAL_FIELDS], vec![Node::leaf(b"avcC", vec![1, 2, 3])]);
        let stsd = Node::container(b"stsd", vec![0, 0, 0, 0, 0, 0, 0, 1], vec![avc1]);
        let stco = Node::full(b"stco", 0, 0, &[0, 0, 0, 2, 0, 0, 0, 100, 0, 0, 0, 200]);
        let stbl = Node::container(b"stbl", vec![], vec![stsd, stco]);
        let hdlr = Node::full(b"hdlr", 0, 0, &[0, 0, 0, 0, b'v', b'i', b'd', b'e', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let minf = Node::container(b"minf", vec![], vec![stbl]);
        let mdia = Node::container(b"mdia", vec![], vec![hdlr, minf]);
        Node::container(b"moov", vec![], vec![Node::container(b"trak", vec![], vec![mdia])])
    }

    #[test]
    fn round_trips_and_edits() {
        let moov = sample_moov();
        let bytes = moov.to_bytes();
        assert_eq!(parse(&bytes).unwrap(), moov);
        let trak = &moov.children()[0];
        assert_eq!(handler(trak), Some(*b"vide"));
        let mut m = moov.clone();
        shift_chunk_offsets(&mut m, 150, 40).unwrap();
        let stco = m.children()[0].child(b"mdia").unwrap().child(b"minf").unwrap().child(b"stbl").unwrap().child(b"stco").unwrap();
        // only the offsets at or past 150 move
        assert_eq!(&stco.data().unwrap()[8..], &[0, 0, 0, 100, 0, 0, 0, 240]);
        assert!(shift_chunk_offsets(&mut m, 0, i64::from(u32::MAX)).is_err(), "a 32-bit offset can't overflow");
    }

    #[test]
    fn damaged_boxes_are_errors_not_panics() {
        let bytes = sample_moov().to_bytes();
        for cut in 0..bytes.len() {
            let _ = parse(&bytes[..cut]);
        }
        for i in 0..bytes.len() {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut b = bytes.clone();
                b[i] ^= flip;
                let _ = parse(&b);
            }
        }
        // a size that says 1 (64-bit) with nothing after it, and a huge size
        assert!(parse(&[0, 0, 0, 1, b'm', b'o', b'o', b'v']).is_err());
        assert!(parse(&[0xff, 0xff, 0xff, 0xff, b'm', b'o', b'o', b'v']).is_err());
    }
}
