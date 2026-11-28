//! Copy, cut and paste of keyframes (Effect Controls ⌘C / ⌘X / ⌘V with keyframes selected).
//!
//! Copy keeps each keyframe whole (value, interpolation, easing) with its effect, parameter and
//! its time after the earliest one copied. Paste puts them back at the playhead (the earliest at
//! the playhead, the rest keeping their spacing), one undo step, on the clip given or the one they
//! came from: each into the same effect instance on the same clip, else into the first effect of
//! the same kind on the target clip. A parameter that wasn't animated starts animating; a
//! keyframe already at a pasted time is replaced. Keyframes with nowhere to go are skipped.

use std::collections::HashSet;

use filmcraft_project::{ClipId, Keyframe, Param};
use filmcraft_time::Tick;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::keyframe_move::Key;
use crate::{Result, Session, commands::bad};

const MAX_KEYS: usize = 4096;

/// One copied keyframe.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CopiedKeyframe {
    /// The clip, effect instance (index) and effect kind it was copied from.
    pub clip: u64,
    pub effect_index: usize,
    pub effect: String,
    pub mask: Option<usize>,
    pub param: String,
    /// Clip time after the earliest copied keyframe.
    pub offset: Tick,
    pub key: Keyframe,
}

fn keys(p: &Value, command: &str) -> Result<Vec<Key>> {
    let raw = p.get("keyframes").and_then(Value::as_array).ok_or_else(|| bad(command, "need keyframes"))?;
    if raw.is_empty() || raw.len() > MAX_KEYS {
        return Err(bad(command, "keyframe count must be between 1 and 4096"));
    }
    let keys: Vec<Key> = serde_json::from_value(Value::Array(raw.clone())).map_err(|_| bad(command, "invalid keyframe reference"))?;
    let mut seen = HashSet::new();
    Ok(keys.into_iter().filter(|k| seen.insert(k.clone())).collect())
}

/// `effects.copyKeyframes`: put the keyframes on the keyframe clipboard.
pub(crate) fn copy(s: &mut Session, p: &Value) -> Result<Value> {
    const COPY: &str = "effects.copyKeyframes";
    let keys = keys(p, COPY)?;
    let seq = s.active_sequence().ok_or(crate::EngineError::NoSequence)?;
    let mut out = Vec::with_capacity(keys.len());
    for k in &keys {
        let (_, it) = seq.find_item(ClipId(k.clip)).ok_or_else(|| bad(COPY, "clip does not exist"))?;
        let e = it.effects.get(k.effect).ok_or_else(|| bad(COPY, "effect does not exist"))?;
        let param = match k.mask {
            Some(m) => e.masks.get(m).and_then(|m| m.param(&k.param)),
            None => e.params.get(&k.param),
        }
        .ok_or_else(|| bad(COPY, "parameter does not exist"))?;
        let key = param.keyframes.iter().find(|kf| kf.time == k.time).ok_or_else(|| bad(COPY, "keyframe does not exist"))?;
        out.push(CopiedKeyframe {
            clip: k.clip,
            effect_index: k.effect,
            effect: e.effect.clone(),
            mask: k.mask,
            param: k.param.clone(),
            offset: Tick::ZERO,
            key: key.clone(),
        });
    }
    let first = out.iter().map(|c| c.key.time).min().unwrap_or(Tick::ZERO);
    for c in &mut out {
        c.offset = Tick(c.key.time.0.saturating_sub(first.0));
    }
    let n = out.len();
    s.state.keyframe_clipboard = out;
    Ok(json!({"copied": n}))
}

/// `effects.cutKeyframes`: copy, then delete them (one undo step for the delete).
pub(crate) fn cut(s: &mut Session, p: &Value) -> Result<Value> {
    let copied = copy(s, p)?;
    crate::keyframe_move::delete_many(s, p)?;
    Ok(copied)
}

/// `effects.pasteKeyframes`: the keyframe clipboard at the playhead (or `time`) on `clip`.
pub(crate) fn paste(s: &mut Session, p: &Value) -> Result<Value> {
    const PASTE: &str = "effects.pasteKeyframes";
    let copied = s.state.keyframe_clipboard.clone();
    let first = copied.first().ok_or_else(|| bad(PASTE, "no keyframes have been copied"))?;
    let clip = ClipId(p.get("clip").and_then(Value::as_u64).unwrap_or(first.clip));
    let at = p.get("time").and_then(Value::as_i64).map(Tick).unwrap_or_else(|| s.playhead());
    let mut pasted: Vec<Value> = Vec::new();
    let mut skipped = 0usize;
    s.edit_sequence("Paste Keyframes", |seq, _, _| {
        let (_, it) = seq.find_item_mut(clip).ok_or_else(|| bad(PASTE, "clip does not exist"))?;
        // the earliest lands at the playhead, held inside the clip
        let last = Tick((it.end().0 - 1).max(it.start.0));
        let base = it.effect_time_at(at.clamp(it.start, last));
        for c in &copied {
            let index = match it.effects.get(c.effect_index) {
                Some(e) if c.clip == clip.0 && e.effect == c.effect => Some(c.effect_index),
                _ => it.effects.iter().position(|e| e.effect == c.effect),
            };
            let Some(e) = index.and_then(|i| it.effects.get_mut(i)) else {
                skipped += 1;
                continue;
            };
            let default = e.def().and_then(|d| d.param(&c.param)).map(|d| d.default.clone());
            let param: Option<&mut Param> = match c.mask {
                // a mask's keyframes go back only to the same mask of the same instance
                Some(m) if index == Some(c.effect_index) && c.clip == clip.0 => e.masks.get_mut(m).and_then(|m| m.param_mut(&c.param)),
                Some(_) => None,
                None => match (e.params.contains_key(&c.param), default) {
                    (true, _) => e.params.get_mut(&c.param),
                    (false, Some(d)) => Some(e.params.entry(c.param.clone()).or_insert_with(|| Param::new(d))),
                    (false, None) => None,
                },
            };
            let Some(param) = param else {
                skipped += 1;
                continue;
            };
            let time = Tick(base.0.saturating_add(c.offset.0));
            let key = Keyframe { time, ..c.key.clone() };
            match param.keyframes.binary_search_by_key(&time, |k| k.time) {
                Ok(i) => param.keyframes[i] = key,
                Err(i) => param.keyframes.insert(i, key),
            }
            pasted.push(json!({"clip": clip.0, "effect": index, "param": c.param, "mask": c.mask, "time": time.0}));
        }
        Ok(())
    })?;
    Ok(json!({"pasted": pasted.len(), "skipped": skipped, "keyframes": pasted}))
}
