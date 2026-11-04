//! 360° video metadata in MP4 / MOV, from the Spherical Video V2 RFC (google/spatial-media,
//! `docs/spherical-video-v2-rfc.md`) and its predecessor, the Spherical Video V1 RFC
//! (`docs/spherical-video-rfc.md`).
//!
//! - V2: `st3d` (stereo mode) and `sv3d` ▸ `proj` ▸ `prhd` (pose) + `equi` / `cbmp` / `mshp`
//!   (projection) are children of the video track's visual sample entry.
//! - V1: a `uuid` box (`ffcc8263-f855-4a93-8814-587a02521fdd`) in the video `trak` holding RDF/XML
//!   with `GSpherical:Spherical`, `GSpherical:ProjectionType` and `GSpherical:StereoMode`.
//!
//! The demuxer doesn't model these boxes, so the `moov` box is re-read here. Everything is
//! optional: a missing, truncated or nonsensical box means "not spherical", never an error.

use filmcraft_isobmff::ByteSource;
use filmcraft_isobmff::boxes::{BoxHeader, BoxIter, BoxRef, Reader};
use filmcraft_media::{Projection, Spherical, StereoLayout};

/// The Spherical Video V1 `uuid` box's extended type.
const V1_UUID: [u8; 16] = [0xff, 0xcc, 0x82, 0x63, 0xf8, 0x55, 0x4a, 0x93, 0x88, 0x14, 0x58, 0x7a, 0x02, 0x52, 0x1f, 0xdd];
/// Largest `moov` box read for the metadata (the sample tables of a long file can be big).
const MAX_MOOV: u64 = 256 << 20;
/// Size of the fixed part of a visual sample entry: SampleEntry (8) + VisualSampleEntry (70).
const VISUAL_ENTRY_FIELDS: usize = 78;

/// The spherical metadata of the video track with id `track_id`, if any.
pub(crate) fn read(src: &dyn ByteSource, moov_offset: u64, track_id: u32) -> Option<Spherical> {
    let left = src.len().checked_sub(moov_offset)?;
    let mut head = vec![0u8; usize::try_from(left.min(BoxHeader::MAX_LEN as u64)).ok()?];
    src.read_at(moov_offset, &mut head).ok()?;
    let h = BoxHeader::parse(&head).ok()?;
    if h.kind != *b"moov" {
        return None;
    }
    // a `moov` that runs to the end of the file has no size of its own
    let size = h.size.unwrap_or(left).min(left);
    if size == 0 || size > MAX_MOOV {
        return None;
    }
    let mut moov = vec![0u8; usize::try_from(size).ok()?];
    src.read_at(moov_offset, &mut moov).ok()?;
    parse_moov(&moov, track_id)
}

/// Spherical metadata of the track `track_id` in a `moov` box (header included).
pub(crate) fn parse_moov(moov: &[u8], track_id: u32) -> Option<Spherical> {
    let root = BoxIter::new(moov).next()?.ok()?;
    let trak = children(moov, &root).find(|t| t.kind == *b"trak" && tkhd_id(moov, t) == Some(track_id))?;
    let entry = child(moov, &trak, &[b"mdia", b"minf", b"stbl", b"stsd"]).and_then(|stsd| first_visual_entry(moov, &stsd));
    let v2 = entry.and_then(|range| v2(moov, range));
    v2.or_else(|| {
        let uuid = children(moov, &trak).find(|b| b.kind == *b"uuid" && b.user_type == Some(V1_UUID))?;
        v1(uuid.payload)
    })
}

fn children<'a>(data: &'a [u8], b: &BoxRef<'a>) -> impl Iterator<Item = BoxRef<'a>> + use<'a> {
    BoxIter::within(data, b.payload_range()).map_while(Result::ok)
}

/// Descend through `path` (one child of each kind), starting below `b`.
fn child<'a>(data: &'a [u8], b: &BoxRef<'a>, path: &[&[u8; 4]]) -> Option<BoxRef<'a>> {
    let mut cur = *b;
    for kind in path {
        cur = children(data, &cur).find(|c| c.kind == **kind)?;
    }
    Some(cur)
}

/// Track id from `tkhd` (version 0: 32-bit times, version 1: 64-bit).
fn tkhd_id(data: &[u8], trak: &BoxRef) -> Option<u32> {
    let tkhd = children(data, trak).find(|b| b.kind == *b"tkhd")?;
    let mut r = Reader::new(tkhd.payload);
    let (version, _) = r.full_header().ok()?;
    r.skip(if version == 1 { 16 } else { 8 }).ok()?;
    r.u32().ok()
}

/// The child-box range of the first sample entry in `stsd` (after its fixed visual fields).
fn first_visual_entry(data: &[u8], stsd: &BoxRef) -> Option<std::ops::Range<usize>> {
    let r = stsd.payload_range();
    // version/flags + entry_count
    let start = r.start.checked_add(8)?;
    let entry = BoxIter::within(data, start..r.end).next()?.ok()?;
    let p = entry.payload_range();
    let kids = p.start.checked_add(VISUAL_ENTRY_FIELDS)?;
    (kids <= p.end).then_some(kids..p.end)
}

/// Spherical Video V2 boxes among a sample entry's children.
fn v2(data: &[u8], range: std::ops::Range<usize>) -> Option<Spherical> {
    let mut stereo = None;
    let mut sv3d = None;
    // some writers leave trailing padding after the last child box
    for b in BoxIter::within(data, range).allow_truncated().map_while(Result::ok) {
        match &b.kind.0 {
            b"st3d" => stereo = st3d(b.payload),
            b"sv3d" => sv3d = Some(b),
            _ => {}
        }
    }
    let sv3d = sv3d?;
    let proj = children(data, &sv3d).find(|b| b.kind == *b"proj")?;
    let mut s = Spherical { stereo: stereo.unwrap_or_default(), projection: Projection::Other, ..Default::default() };
    for b in children(data, &proj) {
        match &b.kind.0 {
            b"prhd" => {
                let mut r = Reader::new(b.payload);
                r.full_header().ok()?;
                // 16.16 fixed-point degrees
                let fixed = |v: i32| v as f32 / 65536.0;
                let (y, p, ro) = (r.i32().ok()?, r.i32().ok()?, r.i32().ok()?);
                (s.yaw, s.pitch, s.roll) = (fixed(y), fixed(p), fixed(ro));
            }
            b"equi" => s.projection = Projection::Equirectangular,
            b"cbmp" => s.projection = Projection::Cubemap,
            _ => {}
        }
    }
    Some(s)
}

fn st3d(payload: &[u8]) -> Option<StereoLayout> {
    let mut r = Reader::new(payload);
    r.full_header().ok()?;
    Some(match r.u8().ok()? {
        1 => StereoLayout::TopBottom,
        2 => StereoLayout::LeftRight,
        // 0 = mono; 3 (stereo-custom) is described by a mesh FilmCraft doesn't read
        _ => StereoLayout::Mono,
    })
}

/// Spherical Video V1: RDF/XML in the `uuid` box payload (after its 16-byte user type).
fn v1(payload: &[u8]) -> Option<Spherical> {
    let xml = std::str::from_utf8(payload).ok()?;
    let tag = |name: &str| -> Option<&str> {
        let open = format!("<GSpherical:{name}>");
        let start = xml.find(&open)?.checked_add(open.len())?;
        let len = xml.get(start..)?.find("</")?;
        xml.get(start..start.checked_add(len)?).map(str::trim)
    };
    if !tag("Spherical")?.eq_ignore_ascii_case("true") {
        return None;
    }
    let projection = match tag("ProjectionType") {
        Some(p) if p.eq_ignore_ascii_case("equirectangular") => Projection::Equirectangular,
        None => Projection::Equirectangular,
        Some(_) => Projection::Other,
    };
    let stereo = match tag("StereoMode") {
        Some(m) if m.eq_ignore_ascii_case("top-bottom") => StereoLayout::TopBottom,
        Some(m) if m.eq_ignore_ascii_case("left-right") => StereoLayout::LeftRight,
        _ => StereoLayout::Mono,
    };
    let deg = |name: &str| tag(name).and_then(|v| v.parse::<f32>().ok()).filter(|v| v.is_finite()).unwrap_or(0.0);
    Some(Spherical { projection, stereo, yaw: deg("InitialViewHeadingDegrees"), pitch: deg("InitialViewPitchDegrees"), roll: deg("InitialViewRollDegrees") })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }
    fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut p = vec![0, 0, 0, 0];
        p.extend_from_slice(body);
        bx(kind, &p)
    }
    fn tkhd(id: u32) -> Vec<u8> {
        let mut b = vec![0u8; 8];
        b.extend_from_slice(&id.to_be_bytes());
        b.extend_from_slice(&[0u8; 68]);
        full(b"tkhd", &b)
    }
    /// A `moov` with one video track whose `avc1` entry carries `extra` children, plus `trak_extra`.
    fn moov(id: u32, extra: &[u8], trak_extra: &[u8]) -> Vec<u8> {
        let mut entry = vec![0u8; VISUAL_ENTRY_FIELDS];
        entry.extend_from_slice(extra);
        let mut stsd = 1u32.to_be_bytes().to_vec();
        stsd.extend_from_slice(&bx(b"avc1", &entry));
        let stbl = bx(b"stbl", &full(b"stsd", &stsd));
        let mdia = bx(b"mdia", &bx(b"minf", &stbl));
        let mut trak = tkhd(id);
        trak.extend_from_slice(&mdia);
        trak.extend_from_slice(trak_extra);
        bx(b"moov", &bx(b"trak", &trak))
    }
    fn sv3d(yaw: f32, projection: &[u8]) -> Vec<u8> {
        let mut pose = ((yaw * 65536.0) as i32).to_be_bytes().to_vec();
        pose.extend_from_slice(&[0u8; 8]);
        let mut proj = full(b"prhd", &pose);
        proj.extend_from_slice(projection);
        let mut s = full(b"svhd", b"test\0");
        s.extend_from_slice(&bx(b"proj", &proj));
        bx(b"sv3d", &s)
    }

    #[test]
    fn reads_v2_equirectangular_with_stereo_and_pose() {
        let mut kids = full(b"st3d", &[1]);
        kids.extend_from_slice(&sv3d(90.5, &full(b"equi", &[0u8; 16])));
        let s = parse_moov(&moov(1, &kids, &[]), 1).unwrap();
        assert_eq!(s.projection, Projection::Equirectangular);
        assert_eq!(s.stereo, StereoLayout::TopBottom);
        assert!((s.yaw - 90.5).abs() < 1e-4);
    }

    #[test]
    fn reads_v2_cubemap_and_only_the_named_track() {
        let kids = sv3d(0.0, &full(b"cbmp", &[0u8; 8]));
        let m = moov(2, &kids, &[]);
        assert_eq!(parse_moov(&m, 2).unwrap().projection, Projection::Cubemap);
        assert_eq!(parse_moov(&m, 1), None);
    }

    #[test]
    fn reads_v1_xml() {
        let mut p = V1_UUID.to_vec();
        p.extend_from_slice(
            b"<rdf:SphericalVideo><GSpherical:Spherical>true</GSpherical:Spherical>\
              <GSpherical:ProjectionType>equirectangular</GSpherical:ProjectionType>\
              <GSpherical:StereoMode>left-right</GSpherical:StereoMode>\
              <GSpherical:InitialViewHeadingDegrees>45</GSpherical:InitialViewHeadingDegrees></rdf:SphericalVideo>",
        );
        let s = parse_moov(&moov(1, &[], &bx(b"uuid", &p)), 1).unwrap();
        assert_eq!((s.projection, s.stereo, s.yaw), (Projection::Equirectangular, StereoLayout::LeftRight, 45.0));
    }

    #[test]
    fn flat_video_is_not_spherical() {
        assert_eq!(parse_moov(&moov(1, &bx(b"pasp", &[0, 0, 0, 1, 0, 0, 0, 1]), &[]), 1), None);
    }

    #[test]
    fn hostile_moov_never_panics() {
        let mut kids = full(b"st3d", &[2]);
        kids.extend_from_slice(&sv3d(10.0, &full(b"equi", &[0u8; 16])));
        let good = moov(1, &kids, &[]);
        for cut in 0..good.len() {
            let _ = std::panic::catch_unwind(|| parse_moov(&good[..cut], 1)).expect("truncation panicked");
        }
        for i in 0..good.len() {
            for bit in [0x01u8, 0x80, 0xff] {
                let mut m = good.clone();
                m[i] ^= bit;
                let _ = std::panic::catch_unwind(|| parse_moov(&m, 1)).expect("bit flip panicked");
            }
        }
    }
}
