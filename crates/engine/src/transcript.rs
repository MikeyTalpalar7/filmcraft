//! Text-based editing commands (`transcript.*`): the Text panel ▸ Transcript tab.
//!
//! Transcripts belong to media items (`Project::transcripts`, media time); the sequence transcript
//! is derived from them ([`filmcraft_edit::transcript::sequence_words`]). Words of the sequence
//! transcript are addressed by index (`from`, `to`, inclusive), as `transcript.inspect` lists them.
//!
//! Speech recognition goes through a [`Transcriber`]: [`Session::transcriber`] when a host or a
//! test installed one, else the Whisper model named by `model` from `<data dir>/models` (needs the
//! engine feature `whisper`; without it `transcript.generate` fails with a clear error, and agents
//! can still bring their own transcript with `transcript.set`).

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};

use filmcraft_edit as edit;
use filmcraft_edit::transcript::{self as tx, CaptionRules, SeqWord};
use filmcraft_project::{CaptionFormat, CaptionTrack, ClipId, ItemId, ItemKind, Project, TrackId, Transcript};
use filmcraft_speech::{Options, SpeechError, TICKS_PER_SAMPLE, Transcriber};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};

use crate::commands::{CommandSpec, always, bad, bool_p, f64_p, has_seq, str_p, u64_p};
use crate::{EngineError, Result, Session};

type Run = fn(&mut Session, &Value) -> Result<Value>;
type Enabled = fn(&Session) -> std::result::Result<(), String>;

fn spec(id: &'static str, label: &'static str, menu: &'static [&'static str], params: &'static str, enabled: Enabled, run: Run, journal: bool) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut: None, params, enabled, run, journal, named: crate::commands::NamedKeys::NONE }
}

/// Where downloaded speech models live (`<data dir>/models`).
pub fn models_dir() -> Option<std::path::PathBuf> {
    crate::autosave::default_data_dir().map(|d| d.join("models"))
}

/// Whether this build can transcribe with Whisper (feature `whisper`).
pub fn speech_available() -> bool {
    filmcraft_speech::available()
}

/// The words of the active sequence's transcript.
pub fn sequence_words(s: &Session) -> Vec<SeqWord> {
    match s.active_sequence() {
        Some(q) => tx::sequence_words(q, &s.project.transcripts),
        None => Vec::new(),
    }
}

fn has_transcript(s: &Session) -> std::result::Result<(), String> {
    has_seq(s)?;
    if sequence_words(s).is_empty() { Err("the sequence has no transcript (Transcribe first)".into()) } else { Ok(()) }
}

fn has_transcripts(s: &Session) -> std::result::Result<(), String> {
    if s.project.transcripts.is_empty() { Err("there are no transcripts".into()) } else { Ok(()) }
}

/// Why speech-to-text can't run in this build (no installed transcriber, built without `whisper`).
pub(crate) const NO_SPEECH: &str =
    "speech-to-text is not available in this build (built without the `whisper` feature); import a transcript with transcript.set instead";

/// `transcript.generate` can run: a host installed a transcriber or the build has speech-to-text
/// (#97: it reported enabled and then always failed).
pub(crate) fn can_transcribe(s: &Session) -> std::result::Result<(), String> {
    if s.transcriber.is_some() || speech_available() { Ok(()) } else { Err(NO_SPEECH.into()) }
}

/// `transcript.downloadModel` can run: built with `speech-download` (#98).
fn can_download(_: &Session) -> std::result::Result<(), String> {
    if cfg!(feature = "speech-download") {
        Ok(())
    } else {
        Err("model downloads are not available in this build (built without the `speech-download` feature)".into())
    }
}

/// The media item behind a project item (subclips resolve to their parent).
fn media_item(s: &Session, item: ItemId) -> Option<ItemId> {
    s.project.resolve_media(item).map(|(root, _, _)| root)
}

fn ids_p(p: &Value, k: &str) -> Option<Vec<ItemId>> {
    p.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(ItemId).collect())
}

/// Items to transcribe: `items` / `item`, else the Project panel selection, else the media of the
/// active sequence's enabled audio clips. Subclips resolve to their media; duplicates are removed.
fn targets(s: &Session, p: &Value) -> Vec<ItemId> {
    let mut raw = ids_p(p, "items").or_else(|| u64_p(p, "item").map(|i| vec![ItemId(i)])).unwrap_or_default();
    if raw.is_empty() {
        raw = s.state.project_selection.clone();
    }
    if raw.is_empty()
        && let Some(q) = s.active_sequence()
    {
        raw = q.audio_tracks.iter().flat_map(|t| t.items.iter()).filter(|it| it.enabled).map(|it| it.item).collect();
    }
    let mut out = Vec::new();
    for i in raw {
        if let Some(m) = media_item(s, i)
            && !out.contains(&m)
        {
            out.push(m);
        }
    }
    out
}

/// Margin kept around each used part of a recording (a word cut by the edit is heard whole).
const USED_PAD_SECONDS: i64 = 1;
/// Silence put between the parts joined for transcription, in samples (half a second).
const PART_GAP: usize = filmcraft_speech::SAMPLE_RATE as usize / 2;

/// The parts of media `item` (`dur` long) that the project's sequences use on audio tracks, in 16
/// kHz samples: each clip's span of the media, a second wider either side, merged where they meet.
/// Empty when no sequence uses it (then all of it is transcribed).
pub(crate) fn used_ranges(p: &Project, item: ItemId, dur: Tick) -> Vec<(usize, usize)> {
    let sr = filmcraft_speech::SAMPLE_RATE as i64;
    let total = dur.to_units_floor(sr).max(0) as usize;
    let pad = (USED_PAD_SECONDS * sr) as usize;
    let mut spans: Vec<(usize, usize)> = audio_placements(p)
        .filter_map(|(seq, clip, placed)| {
            if p.resolve_media(placed).map(|(root, _, _)| root) != Some(item) {
                return None;
            }
            let (_, it) = p.sequence(seq)?.find_item(clip)?;
            if it.duration <= Tick::ZERO {
                return None;
            }
            let (a, b) = (it.source_time_at(it.start), it.source_time_at(it.end() - Tick(1)));
            let (lo, hi) = (a.min(b), a.max(b) + Tick(1));
            let lo = lo.to_units_floor(sr).max(0) as usize;
            let hi = (hi.0.saturating_add(TICKS_PER_SAMPLE - 1) / TICKS_PER_SAMPLE).max(0) as usize;
            Some((lo.saturating_sub(pad), hi.saturating_add(pad).min(total)))
        })
        .filter(|(a, b)| a < b)
        .collect();
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (a, b) in spans {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
}

/// One used part in the joined audio: where it starts there, where in the media, how long (samples).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Part {
    at: usize,
    from: usize,
    len: usize,
}

impl Part {
    #[cfg(test)]
    pub(crate) fn new(at: usize, from: usize, len: usize) -> Self {
        Part { at, from, len }
    }
}

/// Mono 16 kHz audio of `src` (`dur` long): the `ranges` joined with [`PART_GAP`] of silence
/// between them, or all of it without ranges. None: no audio.
fn read_parts(src: &filmcraft_media::SharedSource, dur: Tick, ranges: &[(usize, usize)]) -> Option<(Vec<f32>, Vec<Part>)> {
    if !src.info().has_audio() {
        return None;
    }
    let sr = filmcraft_speech::SAMPLE_RATE;
    let whole = [(0usize, dur.to_units_floor(sr as i64).max(0) as usize)];
    let ranges = if ranges.is_empty() { &whole[..] } else { ranges };
    let (mut audio, mut parts) = (Vec::new(), Vec::new());
    for &(a, b) in ranges {
        if !parts.is_empty() {
            audio.resize(audio.len() + PART_GAP, 0.0);
        }
        let buf = src.audio(i64::try_from(a).ok()?, b.saturating_sub(a), sr).ok()?;
        let mono = filmcraft_speech::downmix(&buf.channels);
        parts.push(Part { at: audio.len(), from: a, len: mono.len() });
        audio.extend(mono);
    }
    Some((audio, parts))
}

/// A time in the joined audio of `parts` as media time: moved by its part's offset (a time in the
/// silence after a part is held at that part's end).
fn media_time(parts: &[Part], t: Tick) -> Tick {
    let n = (t.0 / TICKS_PER_SAMPLE).max(0) as usize;
    let i = parts.partition_point(|p| p.at <= n).saturating_sub(1);
    let Some(p) = parts.get(i) else { return t };
    let offset = sample_tick_of(p.from).0 - sample_tick_of(p.at).0;
    let end = sample_tick_of(p.at + p.len);
    let held = if i + 1 < parts.len() && t > end { end } else { t };
    Tick(held.0.saturating_add(offset).max(0))
}

fn sample_tick_of(n: usize) -> Tick {
    filmcraft_speech::sample_tick(i64::try_from(n).unwrap_or(i64::MAX / TICKS_PER_SAMPLE))
}

/// The words of a transcript of joined `parts` moved to their media times.
pub(crate) fn to_media_times(tr: &mut Transcript, parts: &[Part]) {
    // all of the media, from its start: nothing moves
    if matches!(parts, [Part { at: 0, from: 0, .. }]) {
        return;
    }
    for w in &mut tr.words {
        let (start, last) = (media_time(parts, w.start), media_time(parts, Tick((w.end.0 - 1).max(w.start.0))));
        w.start = start;
        w.end = Tick(last.0.max(start.0) + 1);
    }
}

/// Mono 16 kHz audio of the parts of media `item` that sequences use (all of it when none do).
/// An installed transcriber (tests, agents bringing a transcript of the whole media) hears it all.
fn item_audio(s: &Session, item: ItemId) -> Option<(Vec<f32>, Vec<Part>)> {
    let dur = match &s.project.item(item)?.kind {
        ItemKind::Media(m) => m.duration(),
        _ => return None,
    };
    let src = s.source(item)?;
    let ranges = if s.transcriber.is_some() { Vec::new() } else { used_ranges(&s.project, item, dur) };
    read_parts(&src, dur, &ranges)
}

fn speech_err(e: SpeechError) -> EngineError {
    EngineError::Other(e.to_string())
}

/// The transcriber to use: the installed one, else the named catalogue model.
fn transcriber(s: &Session, p: &Value) -> Result<Arc<dyn Transcriber>> {
    if let Some(t) = &s.transcriber {
        return Ok(t.clone());
    }
    // Settings ▸ Media Analysis & Transcription ▸ Speech model
    let model = str_p(p, "model").unwrap_or(&s.prefs.media_analysis.whisper_model);
    if filmcraft_speech::models::find(model).is_none() {
        return Err(speech_err(SpeechError::UnknownModel(model.into())));
    }
    if !filmcraft_speech::available() {
        return Err(EngineError::Other(NO_SPEECH.into()));
    }
    let dir = models_dir().ok_or_else(|| EngineError::Other("no data directory for speech models".into()))?;
    filmcraft_speech::load(&dir, model).map_err(speech_err)
}

fn generate(s: &mut Session, p: &Value) -> Result<Value> {
    let items = targets(s, p);
    if items.is_empty() {
        return Err(bad("transcript.generate", "nothing to transcribe (pass `items`, select clips, or open a sequence with audio)"));
    }
    // Speech to text takes minutes on a long sequence: in the background, unless asked to wait
    // (automatic transcription folds into the edit that placed the clips) or a transcriber is
    // installed (tests, agents bringing their own transcripts)
    if !bool_p(p, "wait").unwrap_or(false) && s.transcriber.is_none() && !cfg!(target_arch = "wasm32") {
        return generate_in_background(s, p, items);
    }
    let t = transcriber(s, p)?;
    // Settings ▸ Media Analysis & Transcription: language (or auto-detect) and speaker labelling
    let ma = &s.prefs.media_analysis;
    let default_language = if ma.language_auto_detect { None } else { Some(ma.default_language.clone()) };
    let opts = Options {
        language: match str_p(p, "language") {
            Some(l) => Some(l).filter(|l| !l.is_empty() && *l != "auto").map(str::to_string),
            None => default_language,
        },
        diarize: bool_p(p, "diarize").unwrap_or(ma.speaker_labeling != "off"),
        max_speakers: u64_p(p, "maxSpeakers").map(|n| n.clamp(1, 32) as usize).unwrap_or(Options::default().max_speakers),
    };
    let mut done: Vec<(ItemId, Transcript)> = Vec::new();
    let mut skipped = Vec::new();
    for item in items {
        let Some((audio, parts)) = item_audio(s, item) else {
            skipped.push(item.0);
            continue;
        };
        let mut tr = t.transcribe(&audio, &opts, &mut |_, _| true).map_err(speech_err)?;
        to_media_times(&mut tr, &parts);
        tr.normalize();
        done.push((item, tr));
    }
    if done.is_empty() {
        return Err(EngineError::Other("none of the clips has audio to transcribe".into()));
    }
    let report: Vec<Value> = done
        .iter()
        .map(|(i, t)| json!({"item": i.0, "words": t.words.len(), "speakers": t.speakers.len(), "language": t.language, "source": t.source}))
        .collect();
    s.edit("Transcribe", move |pr, _| {
        for (i, t) in done {
            pr.transcripts.insert(i, Arc::new(t));
        }
        Ok(())
    })?;
    Ok(json!({"items": report, "skipped": skipped}))
}

/// The transcription options of `p` over Settings ▸ Media Analysis & Transcription.
fn options(s: &Session, p: &Value) -> Options {
    let ma = &s.prefs.media_analysis;
    let default_language = if ma.language_auto_detect { None } else { Some(ma.default_language.clone()) };
    Options {
        language: match str_p(p, "language") {
            Some(l) => Some(l).filter(|l| !l.is_empty() && *l != "auto").map(str::to_string),
            None => default_language,
        },
        diarize: bool_p(p, "diarize").unwrap_or(ma.speaker_labeling != "off"),
        max_speakers: u64_p(p, "maxSpeakers").map(|n| n.clamp(1, 32) as usize).unwrap_or(Options::default().max_speakers),
    }
}

/// A transcription running in the background ([`generate_in_background`]): its job and the
/// document it belongs to; `out` holds the transcripts (and the items without audio) when done.
pub struct PendingTranscribe {
    job: u64,
    epoch: u64,
    out: Arc<std::sync::Mutex<Option<TranscribeOut>>>,
}

impl PendingTranscribe {
    /// The job (in [`Session::jobs`]) that shows its progress.
    pub fn job(&self) -> u64 {
        self.job
    }
}

type TranscribeOut = (Vec<(ItemId, Transcript)>, Vec<u64>);

/// `transcript.generate` on a worker thread, as a job of the Progress panel (cancel stops it): the
/// model loads, each clip's audio is read and transcribed there, and [`poll`] applies the
/// transcripts as one undo step when it is done.
fn generate_in_background(s: &mut Session, p: &Value, items: Vec<ItemId>) -> Result<Value> {
    use std::sync::atomic::Ordering;
    let model = str_p(p, "model").unwrap_or(&s.prefs.media_analysis.whisper_model).to_string();
    if filmcraft_speech::models::find(&model).is_none() {
        return Err(speech_err(SpeechError::UnknownModel(model)));
    }
    if !filmcraft_speech::available() {
        return Err(EngineError::Other(NO_SPEECH.into()));
    }
    let dir = models_dir().ok_or_else(|| EngineError::Other("no data directory for speech models".into()))?;
    let opts = options(s, p);
    // the sources, lengths and used parts now; their audio is read on the worker
    type Src = (filmcraft_media::SharedSource, Tick, Vec<(usize, usize)>);
    let sources: Vec<(ItemId, Option<Src>)> = items
        .iter()
        .map(|&i| {
            let dur = match s.project.item(i).map(|it| &it.kind) {
                Some(ItemKind::Media(m)) => Some(m.duration()),
                _ => None,
            };
            (i, dur.and_then(|d| Some((s.source(i).filter(|src| src.info().has_audio())?, d, used_ranges(&s.project, i, d)))))
        })
        .collect();
    let n = sources.len();
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let label = if n == 1 { "Transcribe".to_string() } else { format!("Transcribe {n} clips") };
    let job = crate::Job { id, label: label.clone(), progress: Default::default(), result: Default::default() };
    job.progress.total.store(n as u64 * 100, Ordering::Relaxed);
    let out: Arc<std::sync::Mutex<Option<TranscribeOut>>> = Arc::default();
    let (prog, res, sink) = (job.progress.clone(), job.result.clone(), out.clone());
    let run = move || {
        let lock = |m: &std::sync::Mutex<String>, v: String| *m.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = v;
        let t0 = web_time::Instant::now();
        lock(&prog.status, format!("Loading the speech model ({model})"));
        let finish = |r: std::result::Result<(), String>| {
            prog.finished.store(true, Ordering::Relaxed);
            *res.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(r.map(|()| filmcraft_export::Report {
                path: String::new(),
                frames: n as u64,
                seconds: t0.elapsed().as_secs_f64(),
                bytes: 0,
                render_fps: 0.0,
                extra_files: Vec::new(),
            }));
        };
        let t = match filmcraft_speech::load(&dir, &model) {
            Ok(t) => t,
            Err(e) => return finish(Err(e.to_string())),
        };
        let (mut done, mut skipped) = (Vec::new(), Vec::new());
        for (k, (item, src)) in sources.into_iter().enumerate() {
            if prog.cancel.load(Ordering::Relaxed) {
                lock(&prog.status, "Stopped".into());
                return finish(Err("stopped".into()));
            }
            let audio = src.and_then(|(src, dur, ranges)| {
                lock(&prog.status, format!("Reading the audio of clip {} of {n}", k + 1));
                read_parts(&src, dur, &ranges)
            });
            let Some((audio, parts)) = audio else {
                skipped.push(item.0);
                continue;
            };
            lock(&prog.status, if n == 1 { "Transcribing".to_string() } else { format!("Transcribing clip {} of {n}", k + 1) });
            let base = k as u64 * 100;
            let r = t.transcribe(&audio, &opts, &mut |f, _| {
                let f = if f.is_finite() { f.clamp(0.0, 1.0) } else { 0.0 };
                prog.done.store(base + (f * 100.0) as u64, Ordering::Relaxed);
                !prog.cancel.load(Ordering::Relaxed)
            });
            match r {
                Ok(mut tr) => {
                    to_media_times(&mut tr, &parts);
                    tr.normalize();
                    done.push((item, tr));
                }
                Err(e) => return finish(Err(e.to_string())),
            }
            prog.done.store(base + 100, Ordering::Relaxed);
        }
        let words: usize = done.iter().map(|(_, t)| t.words.len()).sum();
        lock(&prog.status, format!("Transcribed {} clip(s), {words} words", done.len()));
        *sink.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((done, skipped));
        finish(Ok(()));
    };
    let run = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), run);
    s.jobs.push(job);
    s.transcribe_jobs.push(PendingTranscribe { job: id, epoch: s.document_epoch, out });
    std::thread::Builder::new().name("filmcraft-transcribe".into()).spawn(run).map_err(|e| EngineError::Other(e.to_string()))?;
    Ok(json!({"job": id, "items": n, "background": true}))
}

/// Apply the transcripts of finished background transcriptions (one undo step each) and drop
/// those whose project was replaced. Called with the other job polls, once per UI frame, and
/// before each top-level command of a headless session.
pub fn poll(s: &mut Session) {
    use std::sync::atomic::Ordering;
    let mut i = 0;
    while i < s.transcribe_jobs.len() {
        let job = s.transcribe_jobs[i].job;
        if s.transcribe_jobs[i].epoch != s.document_epoch {
            if let Some(j) = s.jobs.iter().find(|j| j.id == job) {
                j.progress.cancel.store(true, Ordering::Relaxed);
            }
            s.transcribe_jobs.remove(i);
            continue;
        }
        let finished = s.jobs.iter().find(|j| j.id == job).is_none_or(|j| j.progress.finished.load(Ordering::Relaxed));
        if !finished {
            i += 1;
            continue;
        }
        let pending = s.transcribe_jobs.remove(i);
        let out = pending.out.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        match out {
            Some((done, _)) if !done.is_empty() => {
                // (a clip deleted meanwhile has nowhere to keep its transcript)
                let done: Vec<(ItemId, Transcript)> = done.into_iter().filter(|(i, _)| s.project.item(*i).is_some()).collect();
                if let Err(e) = s.edit("Transcribe", move |pr, _| {
                    for (i, t) in done {
                        pr.transcripts.insert(i, Arc::new(t));
                    }
                    Ok(())
                }) {
                    s.error_toast("transcript.generate", e.to_string());
                }
            }
            Some(_) => s.error_toast("transcript.generate", "none of the clips has audio to transcribe"),
            // failed or stopped: the job's own result says why
            None => {}
        }
    }
}

/// Enabled audio placements, including media inside nested sequences, without recursive walks.
fn audio_placements(project: &Project) -> impl Iterator<Item = (ItemId, ClipId, ItemId)> + '_ {
    project.sequences().filter_map(|item| item.as_sequence().map(|q| (item.id, q))).flat_map(|(sequence, q)| {
        q.audio_tracks.iter().flat_map(|track| &track.items).filter(|clip| clip.enabled).map(move |clip| (sequence, clip.id, clip.item))
    })
}

/// Transcribe newly placed audio once per underlying media item. This runs after the outermost
/// editing command so every placement route shares the same scope, including paste and expansion.
pub(crate) fn auto_transcribe_sequence_clips(s: &mut Session, before: &Project) -> Result<()> {
    let old: HashSet<_> = audio_placements(before).collect();
    let items: BTreeSet<_> = audio_placements(&s.project)
        .filter(|placement| !old.contains(placement))
        .filter_map(|(_, _, item)| s.project.resolve_media(item))
        .filter(|(root, media, _)| media.info.has_audio() && !s.project.transcripts.contains_key(root))
        .map(|(root, _, _)| root.0)
        .collect();
    if items.is_empty() {
        return Ok(());
    }
    // Fold the transcript into the placement's undo step. Preserve the whole history, including
    // its bounded oldest entry and merge key; generate's own edit must not consume another slot.
    let history = s.history.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| generate(s, &json!({"items": items, "wait": true}))))
        .unwrap_or_else(|_| Err(EngineError::Other("internal error during automatic transcription (see the crash log); the clips were placed".into())));
    s.history = history;
    result.map(|_| ())
}

fn set(s: &mut Session, p: &Value) -> Result<Value> {
    let item = u64_p(p, "item").map(ItemId).ok_or_else(|| bad("transcript.set", "`item` is required"))?;
    let item = media_item(s, item).ok_or_else(|| bad("transcript.set", "no such media item"))?;
    let v = p.get("transcript").cloned().ok_or_else(|| bad("transcript.set", "`transcript` is required"))?;
    let mut t: Transcript = serde_json::from_value(v).map_err(|e| bad("transcript.set", e.to_string()))?;
    if t.source.is_empty() {
        t.source = "imported".into();
    }
    t.normalize();
    t.check().map_err(|e| bad("transcript.set", e))?;
    let n = t.words.len();
    s.edit("Set Transcript", move |pr, _| {
        pr.transcripts.insert(item, Arc::new(t));
        Ok(())
    })?;
    Ok(json!({"item": item.0, "words": n}))
}

fn delete(s: &mut Session, p: &Value) -> Result<Value> {
    let items: Vec<ItemId> = match ids_p(p, "items").or_else(|| u64_p(p, "item").map(|i| vec![ItemId(i)])) {
        Some(v) => v.into_iter().filter_map(|i| media_item(s, i)).collect(),
        None => s.project.transcripts.keys().copied().collect(),
    };
    let n = items.iter().filter(|i| s.project.transcripts.contains_key(i)).count();
    if n == 0 {
        return Err(EngineError::Other("no transcript to delete".into()));
    }
    s.edit("Delete Transcript", move |pr, _| {
        for i in items {
            pr.transcripts.remove(&i);
        }
        Ok(())
    })?;
    Ok(json!({"deleted": n}))
}

fn word_json(i: usize, w: &SeqWord) -> Value {
    json!({"i": i, "text": w.text, "start": w.start.0, "end": w.end.0, "speaker": w.speaker, "clip": w.clip.0, "item": w.item.0, "confidence": w.confidence})
}

fn inspect(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let gap = Tick::from_seconds_f64(f64_p(p, "paragraphGapSeconds").unwrap_or(1.5));
    let paras: Vec<Value> = tx::paragraphs(&words, gap)
        .into_iter()
        .map(|r| {
            let text = words[r.clone()].iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
            json!({"from": r.start, "to": r.end - 1, "speaker": words[r.start].speaker, "start": words[r.start].start.0, "end": words[r.end - 1].end.0, "text": text})
        })
        .collect();
    let speakers: Vec<String> = {
        let mut v: Vec<String> = Vec::new();
        for w in &words {
            if let Some(n) = &w.speaker
                && !v.contains(n)
            {
                v.push(n.clone());
            }
        }
        v
    };
    let current = tx::word_at(&words, s.playhead());
    Ok(json!({
        "words": words.iter().enumerate().map(|(i, w)| word_json(i, w)).collect::<Vec<_>>(),
        "paragraphs": paras,
        "speakers": speakers,
        "current": current,
        "items": s.project.transcripts.iter().map(|(i, t)| json!({"item": i.0, "words": t.words.len(), "language": t.language, "source": t.source, "speakers": t.speakers.iter().map(|k| &k.name).collect::<Vec<_>>()})).collect::<Vec<_>>(),
    }))
}

fn search(s: &mut Session, p: &Value) -> Result<Value> {
    let q = str_p(p, "query").ok_or_else(|| bad("transcript.search", "`query` is required"))?;
    let words = sequence_words(s);
    let hits: Vec<Value> = tx::search(&words, q)
        .into_iter()
        .map(|r| json!({"from": r.start, "to": r.end - 1, "start": words[r.start].start.0, "end": words[r.end - 1].end.0}))
        .collect();
    Ok(json!({"matches": hits}))
}

/// Timeline range of the words `from..=to` (frame-snapped outward).
fn range_p(s: &Session, p: &Value, cmd: &str) -> Result<TimeRange> {
    let words = sequence_words(s);
    let from = u64_p(p, "from").ok_or_else(|| bad(cmd, "`from` (word index) is required"))? as usize;
    let to = u64_p(p, "to").map(|n| n as usize).unwrap_or(from);
    tx::word_range(&words, from, to, s.sequence_rate()).ok_or_else(|| bad(cmd, format!("word index out of range (the transcript has {} words)", words.len())))
}

fn range_json(r: TimeRange) -> Value {
    json!({"start": r.start.0, "end": r.end().0})
}

fn select(s: &mut Session, p: &Value) -> Result<Value> {
    let r = range_p(s, p, "transcript.select")?;
    let fd = s.sequence_rate().frame_duration();
    s.edit_sequence("Mark Transcript Selection", |q, _, _| {
        q.mark_in = Some(r.start);
        q.mark_out = Some(r.end() - fd);
        Ok(())
    })?;
    s.set_playhead(r.start);
    Ok(range_json(r))
}

fn extract_or_lift(s: &mut Session, p: &Value, extract: bool) -> Result<Value> {
    let cmd = if extract { "transcript.extract" } else { "transcript.lift" };
    let r = range_p(s, p, cmd)?;
    let tg = s.targeting().targeted;
    s.edit_sequence(if extract { "Extract Text" } else { "Lift Text" }, |q, ctx, _| {
        if extract {
            edit::extract(q, &tg, r, ctx);
        } else {
            edit::lift(q, &tg, r, ctx);
        }
        q.mark_in = None;
        q.mark_out = None;
        Ok(())
    })?;
    s.set_playhead(r.start);
    Ok(range_json(r))
}

fn rename_speaker(s: &mut Session, p: &Value) -> Result<Value> {
    let name = str_p(p, "name").map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad("transcript.renameSpeaker", "`name` is required"))?.to_string();
    let item = u64_p(p, "item").map(ItemId).and_then(|i| media_item(s, i));
    // `speaker`: the current name (every transcript), or an index (needs `item`)
    let (old_name, index) = match p.get("speaker") {
        Some(Value::String(n)) => (Some(n.clone()), None),
        Some(v) if v.is_u64() => (None, v.as_u64().map(|n| n as usize)),
        _ => return Err(bad("transcript.renameSpeaker", "`speaker` (name, or index with `item`) is required")),
    };
    if index.is_some() && item.is_none() {
        return Err(bad("transcript.renameSpeaker", "a speaker index needs `item`"));
    }
    let mut n = 0;
    let mut next = s.project.transcripts.clone();
    for (i, t) in next.iter_mut() {
        if item.is_some_and(|x| x != *i) {
            continue;
        }
        let tt = Arc::make_mut(t);
        for (k, sp) in tt.speakers.iter_mut().enumerate() {
            if old_name.as_ref().is_some_and(|o| *o == sp.name) || index == Some(k) {
                sp.name = name.clone();
                n += 1;
            }
        }
    }
    if n == 0 {
        return Err(EngineError::Other("no such speaker".into()));
    }
    s.edit("Rename Speaker", move |pr, _| {
        pr.transcripts = next;
        Ok(())
    })?;
    Ok(json!({"renamed": n}))
}

fn remove_ranges(s: &mut Session, label: &str, ranges: Vec<TimeRange>) -> Result<Value> {
    let n = ranges.len();
    if n == 0 {
        return Ok(json!({"removed": 0, "ticks": 0}));
    }
    let total = s.edit_sequence(label, |q, ctx, _| Ok(tx::ripple_delete_ranges(q, ranges, ctx)))?;
    Ok(json!({"removed": n, "ticks": total.0, "seconds": total.0 as f64 / TICKS_PER_SECOND as f64}))
}

fn remove_pauses(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let min = Tick::from_seconds_f64(f64_p(p, "minSeconds").unwrap_or(1.0));
    let keep = Tick::from_seconds_f64(f64_p(p, "keepSeconds").unwrap_or(0.15));
    let ranges = tx::find_pauses(&words, min, keep, s.sequence_rate());
    remove_ranges(s, "Remove Pauses", ranges)
}

fn remove_fillers(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let fillers: Vec<String> = match p.get("fillers").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        None => tx::DEFAULT_FILLERS.iter().map(|f| f.to_string()).collect(),
    };
    let hits = tx::find_fillers(&words, &fillers);
    let ranges = tx::filler_ranges(&words, &hits, s.sequence_rate());
    remove_ranges(s, "Remove Filler Words", ranges)
}

fn create_captions(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let d = CaptionRules::default();
    let rules = CaptionRules {
        max_chars: u64_p(p, "maxChars").map(|n| n as usize).unwrap_or(d.max_chars),
        lines: u64_p(p, "lines").map(|n| n as usize).unwrap_or(d.lines),
        min_duration: f64_p(p, "minSeconds").map(Tick::from_seconds_f64).unwrap_or(d.min_duration),
        max_duration: f64_p(p, "maxSeconds").map(Tick::from_seconds_f64).unwrap_or(d.max_duration),
        gap_frames: p.get("gapFrames").and_then(Value::as_i64).unwrap_or(d.gap_frames),
        break_pause: d.break_pause,
    };
    let blocks = tx::caption_blocks(&words, &rules, s.sequence_rate());
    let format = str_p(p, "format").and_then(CaptionFormat::from_name).unwrap_or_default();
    let name = str_p(p, "name").unwrap_or("Transcript").to_string();
    let n = blocks.len();
    let tid = s.edit_sequence("Create Captions", |q, ctx, st| {
        let tid = TrackId(ctx.alloc());
        let mut t = CaptionTrack::new(tid, name, format);
        t.captions = tx::blocks_to_captions(&blocks, ctx);
        q.caption_tracks.insert(0, t);
        st.caption_selection.clear();
        Ok(tid)
    })?;
    Ok(json!({"track": tid.0, "captions": n}))
}

fn models(_: &mut Session, _: &Value) -> Result<Value> {
    let dir = models_dir();
    Ok(json!({
        "available": filmcraft_speech::available(),
        "default": filmcraft_speech::models::DEFAULT_MODEL,
        "dir": dir.as_ref().map(|d| d.to_string_lossy().to_string()),
        "models": filmcraft_speech::models::catalogue().iter().map(|m| json!({
            "id": m.id, "name": m.name, "multilingual": m.multilingual, "description": m.description,
            "license": m.license, "source": m.source, "size": m.size(),
            "installed": dir.as_ref().is_some_and(|d| filmcraft_speech::models::installed(d, m)),
        })).collect::<Vec<_>>(),
    }))
}

/// Download a catalogue model into `<data dir>/models` (feature `speech-download`). Hosts show
/// the size, source and licence (`transcript.models`) and ask before running this.
fn download_model(_: &mut Session, p: &Value) -> Result<Value> {
    let id = str_p(p, "model").unwrap_or(filmcraft_speech::models::DEFAULT_MODEL);
    let m = filmcraft_speech::models::find(id).ok_or_else(|| speech_err(SpeechError::UnknownModel(id.into())))?;
    let dir = models_dir().ok_or_else(|| EngineError::Other("no data directory for speech models".into()))?;
    #[cfg(feature = "speech-download")]
    {
        filmcraft_speech::models::download(&dir, m, &mut |_, _, _| true).map_err(speech_err)?;
        Ok(json!({"model": m.id, "dir": filmcraft_speech::models::model_dir(&dir, m).to_string_lossy()}))
    }
    #[cfg(not(feature = "speech-download"))]
    {
        let _ = (m, dir);
        Err(EngineError::Other("model downloads are not available in this build (built without the `speech-download` feature)".into()))
    }
}

pub fn commands() -> Vec<CommandSpec> {
    vec![
        spec(
            "transcript.generate",
            "Transcribe…",
            &["Sequence", "Transcript"],
            r#"{"items":[id]?,"model":"whisper-base"?,"language":"en|auto"?,"diarize":bool?,"maxSpeakers":n?}"#,
            can_transcribe,
            generate,
            true,
        ),
        spec(
            "transcript.set",
            "Import Transcript",
            &[],
            r#"{"item":id,"transcript":{"language":str,"speakers":[{"name":str}],"words":[{"text":str,"start":tick,"end":tick,"speaker":n?}]}}"#,
            always,
            set,
            true,
        ),
        spec("transcript.delete", "Delete Transcript", &["Sequence", "Transcript"], r#"{"items":[id]?}"#, has_transcripts, delete, true),
        spec("transcript.inspect", "Inspect Transcript", &[], r#"{"paragraphGapSeconds":f?}"#, always, inspect, false),
        spec("transcript.search", "Search Transcript", &[], r#"{"query":str}"#, always, search, false),
        spec("transcript.models", "List Speech Models", &[], "{}", always, models, false),
        spec("transcript.downloadModel", "Download Speech Model", &[], r#"{"model":"whisper-base"?}"#, can_download, download_model, true),
        spec("transcript.select", "Mark Selected Text", &[], r#"{"from":word,"to":word?}"#, has_transcript, select, true),
        spec("transcript.extract", "Extract Selected Text", &[], r#"{"from":word,"to":word?}"#, has_transcript, |s, p| extract_or_lift(s, p, true), true),
        spec("transcript.lift", "Lift Selected Text", &[], r#"{"from":word,"to":word?}"#, has_transcript, |s, p| extract_or_lift(s, p, false), true),
        spec(
            "transcript.renameSpeaker",
            "Rename Speaker…",
            &[],
            r#"{"speaker":"Speaker 1"|index,"name":str,"item":id?}"#,
            has_transcripts,
            rename_speaker,
            true,
        ),
        spec(
            "transcript.removePauses",
            "Remove Pauses",
            &["Sequence", "Transcript"],
            r#"{"minSeconds":f?,"keepSeconds":f?}"#,
            has_transcript,
            remove_pauses,
            true,
        ),
        spec("transcript.removeFillers", "Remove Filler Words", &["Sequence", "Transcript"], r#"{"fillers":[str]?}"#, has_transcript, remove_fillers, true),
        spec(
            "transcript.createCaptions",
            "Create Captions from Transcript…",
            &["Sequence", "Transcript"],
            r#"{"maxChars":n?,"lines":1|2?,"minSeconds":f?,"maxSeconds":f?,"gapFrames":n?,"format":str?,"name":str?}"#,
            has_transcript,
            create_captions,
            true,
        ),
    ]
}
