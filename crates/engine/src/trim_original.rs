//! File ▸ Export ▸ Trimmed Original (No Re-encode)…: the Source monitor clip's original file cut to
//! its In and Out points without re-encoding, every track and the camera's metadata kept (both
//! lenses and the Insta360 data of an `.insv`), through [`filmcraft_codecs::mp4::trim`].
//!
//! `file.exportTrimmedOriginal {item?, path, in?, out?}`: `item` defaults to the Source monitor's
//! clip; `in` / `out` (seconds of the media) to its In and Out marks, else its start and end. The
//! cut widens to keyframes; the reply says what was kept.

use filmcraft_project::{ItemKind, MediaRef};
use filmcraft_time::TICKS_PER_SECOND;
use serde_json::{Value, json};

use crate::commands::{bad, f64_p, item_p, str_p};
use crate::{Result, Session};

const CMD: &str = "file.exportTrimmedOriginal";

/// Enabled with a clip in the Source monitor.
pub(crate) fn enabled(s: &Session) -> std::result::Result<(), String> {
    s.state.source_item.map(|_| ()).ok_or_else(|| "no clip in the Source monitor".into())
}

/// The file extensions it can trim (MP4 family and Insta360).
pub const EXTENSIONS: &[&str] = &["mp4", "mov", "m4v", "insv", "lrv"];

pub(crate) fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let id = item_p(p, "item").or(s.state.source_item).ok_or_else(|| bad(CMD, "no clip in the Source monitor"))?;
    let item = s.project.item(id).ok_or_else(|| bad(CMD, "no such item"))?;
    let ItemKind::Media(m) = &item.kind else { return Err(bad(CMD, "the item isn't a media clip")) };
    let MediaRef::File { path: src } = &m.media else { return Err(bad(CMD, "the clip isn't a file")) };
    let ext = std::path::Path::new(src).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if !EXTENSIONS.contains(&ext.as_str()) {
        return Err(bad(CMD, "only MP4, MOV and Insta360 (.insv / .lrv) files can be trimmed without re-encoding"));
    }
    let dst = str_p(p, "path").ok_or_else(|| bad(CMD, "need `path`"))?;
    let secs = |t: filmcraft_time::Tick| t.0 as f64 / TICKS_PER_SECOND as f64;
    let length = secs(m.info.duration);
    let from = f64_p(p, "in").or(m.mark_in.map(secs)).unwrap_or(0.0);
    let to = f64_p(p, "out").or(m.mark_out.map(secs)).unwrap_or(length);
    if !(from.is_finite() && to.is_finite() && to > from) {
        return Err(bad(CMD, "the Out point must come after the In point"));
    }
    let r = filmcraft_codecs::mp4::trim::trim_file(std::path::Path::new(src), std::path::Path::new(dst), from, to).map_err(|e| bad(CMD, e.to_string()))?;
    Ok(json!({"path": dst, "start": r.start, "end": r.end, "bytes": r.bytes, "insta360FrameTimes": r.frame_times_cut}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_what_it_cant_trim() {
        let mut s = Session::default();
        // no clip in the Source monitor
        assert!(s.execute(CMD, json!({"path": "/tmp/x.mp4"})).is_err());
        s.execute("file.openDemoProject", json!({})).unwrap();
        let generated = s.project.items.values().find(|i| matches!(&i.kind, ItemKind::Media(m) if matches!(m.media, MediaRef::Generator(_)))).map(|i| i.id);
        if let Some(g) = generated {
            let e = s.execute(CMD, json!({"item": g.0, "path": "/tmp/x.mp4"})).unwrap_err().to_string();
            assert!(e.contains("file"), "{e}");
        }
        for p in [json!({"item": 999_999, "path": "/tmp/x.mp4"}), json!({"item": -1}), json!({"item": "x", "path": 3}), json!({"in": f64::MAX, "out": 0})] {
            assert!(s.execute(CMD, p).is_err());
        }
    }
}
