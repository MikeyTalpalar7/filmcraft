//! Marking a finished MP4 / MOV as 360° video (Spherical Video V2 and V1, google/spatial-media),
//! so players, YouTube and editors show it as a sphere: monoscopic equirectangular, no pose.
//!
//! - V2: `st3d` (mono) and `sv3d` ▸ `svhd` + `proj` ▸ `prhd` + `equi` in the video track's
//!   visual sample entry.
//! - V1: the `uuid` box with the GSpherical RDF/XML in the video `trak`, for older tools.
//!
//! The `moov` box grows, so the file is rewritten: when the media data comes after `moov` (a
//! fast-start file) its chunk offsets move by what `moov` grew. The new file is written beside the
//! old one and renamed over it, so a failure leaves the export as it was.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::tree::{self, Node, TreeError};

/// The Spherical Video V1 `uuid` box's extended type.
const V1_UUID: [u8; 16] = [0xff, 0xcc, 0x82, 0x63, 0xf8, 0x55, 0x4a, 0x93, 0x88, 0x14, 0x58, 0x7a, 0x02, 0x52, 0x1f, 0xdd];
/// Largest `moov` box rewritten.
const MAX_MOOV: u64 = 512 << 20;
const V1_XML: &str = concat!(
    "<?xml version=\"1.0\"?><rdf:SphericalVideo xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" ",
    "xmlns:GSpherical=\"http://ns.google.com/videos/1.0/spherical/\">",
    "<GSpherical:Spherical>true</GSpherical:Spherical><GSpherical:Stitched>true</GSpherical:Stitched>",
    "<GSpherical:StitchingSoftware>FilmCraft</GSpherical:StitchingSoftware>",
    "<GSpherical:ProjectionType>equirectangular</GSpherical:ProjectionType></rdf:SphericalVideo>"
);

fn io(e: std::io::Error) -> TreeError {
    TreeError(e.to_string())
}

/// The top-level boxes of the file: (type, offset, size).
fn top_level(f: &mut File, len: u64) -> Result<Vec<([u8; 4], u64, u64)>, TreeError> {
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
            1 => return Err(TreeError("truncated box header".into())),
            s => u64::from(s),
        };
        if size < 8 || off.checked_add(size).is_none_or(|e| e > len) {
            return Err(TreeError("a top-level box runs past the end of the file".into()));
        }
        out.push((kind, off, size));
        off += size;
    }
    Ok(out)
}

/// The `moov` with the spherical boxes added to its first video track (replacing any there).
pub fn tag_moov(moov: &mut Node) -> Result<(), TreeError> {
    let traks = moov.children_mut().ok_or(TreeError("moov has no tracks".into()))?;
    let trak = traks.iter_mut().find(|t| &t.kind == b"trak" && tree::handler(t) == Some(*b"vide")).ok_or(TreeError("no video track".into()))?;
    // V1, in the track
    let kids = trak.children_mut().ok_or(TreeError("track".into()))?;
    kids.retain(|c| !(&c.kind == b"uuid" && c.data().is_some_and(|d| d.starts_with(&V1_UUID))));
    let mut v1 = V1_UUID.to_vec();
    v1.extend_from_slice(V1_XML.as_bytes());
    kids.push(Node::leaf(b"uuid", v1));
    // V2, in the sample entry
    let entry = trak
        .child_mut(b"mdia")
        .and_then(|m| m.child_mut(b"minf"))
        .and_then(|m| m.child_mut(b"stbl"))
        .and_then(|s| s.child_mut(b"stsd"))
        .and_then(|s| s.children_mut())
        .and_then(|e| e.first_mut())
        .ok_or(TreeError("the video track has no sample description".into()))?;
    let boxes = entry.children_mut().ok_or(TreeError("this video format can't carry 360° metadata".into()))?;
    boxes.retain(|c| &c.kind != b"st3d" && &c.kind != b"sv3d");
    boxes.push(Node::full(b"st3d", 0, 0, &[0]));
    let svhd = Node::full(b"svhd", 0, 0, b"FilmCraft\0");
    let prhd = Node::full(b"prhd", 0, 0, &[0; 12]);
    let equi = Node::full(b"equi", 0, 0, &[0; 16]);
    boxes.push(Node::container(b"sv3d", vec![], vec![svhd, Node::container(b"proj", vec![], vec![prhd, equi])]));
    Ok(())
}

/// Mark the MP4 / MOV at `path` as monoscopic equirectangular 360° video.
pub fn tag_file(path: &Path) -> Result<(), TreeError> {
    let mut f = File::open(path).map_err(io)?;
    let len = f.metadata().map_err(io)?.len();
    let boxes = top_level(&mut f, len)?;
    let &(_, moov_off, moov_size) = boxes.iter().find(|b| &b.0 == b"moov").ok_or(TreeError("no moov box".into()))?;
    if moov_size > MAX_MOOV {
        return Err(TreeError("the moov box is too large".into()));
    }
    let mut raw = vec![0u8; usize::try_from(moov_size).map_err(|_| TreeError("moov".into()))?];
    f.seek(SeekFrom::Start(moov_off)).map_err(io)?;
    f.read_exact(&mut raw).map_err(io)?;
    let mut moov = tree::parse(&raw)?;
    tag_moov(&mut moov)?;
    let grown = i64::try_from(moov.to_bytes().len()).unwrap_or(i64::MAX) - i64::try_from(moov_size).unwrap_or(0);
    // the media after moov moves by what it grew (moov's size changes the offsets' basis again
    // only if crossing 4 GiB, which it can't at this size)
    let moov_end = moov_off + moov_size;
    tree::shift_chunk_offsets(&mut moov, moov_end, grown)?;
    let new_moov = moov.to_bytes();
    let tmp = path.with_extension("vrtag.tmp");
    let result = (|| -> Result<(), TreeError> {
        let mut out = BufWriter::new(File::create(&tmp).map_err(io)?);
        f.seek(SeekFrom::Start(0)).map_err(io)?;
        std::io::copy(&mut (&mut f).take(moov_off), &mut out).map_err(io)?;
        out.write_all(&new_moov).map_err(io)?;
        f.seek(SeekFrom::Start(moov_end)).map_err(io)?;
        std::io::copy(&mut f, &mut out).map_err(io)?;
        out.into_inner().map_err(|e| io(e.into_error()))?.sync_all().map_err(io)?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    drop(f);
    std::fs::rename(&tmp, path).map_err(io)
}
