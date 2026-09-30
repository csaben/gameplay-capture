//! Crash recovery for `.partial` segment folders.
//!
//! For every `sessions/<session_id>/seg_NNNNNN.partial/`:
//! - `manifest.json` present (crash between manifest write and rename): just
//!   drop the journal and rename.
//! - otherwise, if `video.mp4` is non-empty and `journal.jsonl` has a header and
//!   at least one frame: rebuild the three parquet files from the journal,
//!   write the manifest (`encoder_params["recovered"] = "true"`) and rename.
//!   The journal (flushed every second) and the fragmented MP4 (one fragment
//!   per keyframe, i.e. per second) end at different points after a crash, so
//!   both are cut back to a common length: the video is truncated to the last
//!   complete fragment whose cumulative sample count is <= the journaled frame
//!   count, and frames.parquet keeps exactly that many rows. So
//!   `frame_count` == decodable frames, as the pipeline validation requires
//!   (up to ~1-2 s at the end of the segment are lost).
//!   `t_end_ns` = last kept tick + one tick; inputs are clipped to
//!   `[t_start, t_end)`; `dropped_frames` is derived from gaps in the tick grid.
//!   Non-MP4 video (the test fake muxer) is kept as is.
//! - anything else is renamed to `seg_NNNNNN.broken` so it is not retried and
//!   the uploader ignores it.

use crate::finalize::{finalize_segment, publish_partial, SegmentTables};
use crate::journal::{self, Entry, JOURNAL_FILE};
use cap_types::{files, FocusRecord, NANOS_PER_SEC};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct RecoveryReport {
    /// Final segment folders, ready for the upload queue.
    pub finalized: Vec<PathBuf>,
    /// Unrecoverable partials (renamed to `*.broken`), with the reason.
    pub broken: Vec<(PathBuf, String)>,
}

/// Scan `sessions_root/*/*.partial` and finalize or quarantine each. Call on
/// startup before creating a `Recorder`, then hand `finalized` to the uploader.
pub fn recover_partials(sessions_root: &Path) -> RecoveryReport {
    let mut report = RecoveryReport::default();
    let Ok(sessions) = fs::read_dir(sessions_root) else { return report };
    let mut partials: Vec<PathBuf> = Vec::new();
    for s in sessions.flatten() {
        if !s.path().is_dir() {
            continue;
        }
        if let Ok(entries) = fs::read_dir(s.path()) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(files::PARTIAL_SUFFIX)) {
                    partials.push(p);
                }
            }
        }
    }
    partials.sort();
    for p in partials {
        match recover_one(&p) {
            Ok(fin) => {
                tracing::info!(path = %fin.display(), "recovered partial segment");
                report.finalized.push(fin);
            }
            Err(why) => {
                tracing::warn!(path = %p.display(), %why, "unrecoverable partial segment");
                let broken = p.with_extension("broken");
                let _ = fs::rename(&p, &broken);
                report.broken.push((broken, why));
            }
        }
    }
    report
}

fn recover_one(partial: &Path) -> Result<PathBuf, String> {
    if partial.join(files::MANIFEST).is_file() {
        return publish_partial(partial);
    }
    let video_len = fs::metadata(partial.join(files::VIDEO)).map(|m| m.len()).unwrap_or(0);
    if video_len == 0 {
        return Err("no video".into());
    }
    let entries = journal::read(&partial.join(JOURNAL_FILE)).map_err(|e| format!("journal: {e}"))?;
    let mut header = None;
    let mut t = SegmentTables::default();
    let mut init: Option<FocusRecord> = None;
    let mut focus = Vec::new();
    for e in entries {
        match e {
            Entry::Header(m) => header = Some(*m),
            Entry::Frame(f) => t.frames.push(f),
            Entry::Input(i) => t.inputs.push(i),
            Entry::FocusInit(f) => init = Some(f),
            Entry::Focus(f) => focus.push(f),
        }
    }
    let mut m = header.ok_or("journal has no header")?;
    t.frames.sort_by_key(|f| f.frame_idx);
    t.frames.dedup_by_key(|f| f.frame_idx);
    // Keep only the contiguous prefix 0..n.
    let n = t.frames.iter().enumerate().take_while(|(i, f)| f.frame_idx as usize == *i).count();
    t.frames.truncate(n);
    let video = partial.join(files::VIDEO);
    if let Some(frags) = fmp4_fragments(&video) {
        // Largest complete-fragment prefix not longer than the journal.
        let (samples, end) =
            frags.fragments.iter().rev().find(|(c, _)| *c <= t.frames.len() as u64).copied().ok_or("no complete MP4 fragment")?;
        let f = fs::OpenOptions::new().write(true).open(&video).map_err(|e| e.to_string())?;
        f.set_len(end).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        t.frames.truncate(samples as usize);
    }
    let (first, last) = match (t.frames.first(), t.frames.last()) {
        (Some(a), Some(b)) => (*a, *b),
        _ => return Err("journal has no frames".into()),
    };
    let tick = NANOS_PER_SEC / m.rate_hz.max(1) as i64;
    m.t_end_ns = last.tick_ns + tick;
    let slots = ((last.tick_ns - first.tick_ns) / tick + 1) as u64;
    m.dropped_frames = slots.saturating_sub(t.frames.len() as u64);
    let (t0, t1) = (m.t_start_ns, m.t_end_ns);
    t.inputs.retain(|i| i.t_ns >= t0 && i.t_ns < t1);
    t.inputs.sort_by_key(|i| i.t_ns);
    let mut init = init.unwrap_or(FocusRecord { t_ns: t0, focused: false, game_id: String::new() });
    init.t_ns = t0;
    focus.retain(|f| f.t_ns >= t0 && f.t_ns < t1);
    focus.sort_by_key(|f| f.t_ns);
    t.focus = std::iter::once(init).chain(focus).collect();
    m.encoder_params.insert("recovered".into(), "true".into());
    finalize_segment(partial, m, &t)
}

/// Complete fragments of a fragmented MP4: `(cumulative samples, end offset)`
/// after each `moof`+`mdat` pair that is fully on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Fmp4Fragments {
    pub fragments: Vec<(u64, u64)>,
}

fn be32(b: &[u8], i: usize) -> Option<u64> {
    b.get(i..i + 4).map(|x| u32::from_be_bytes(x.try_into().unwrap()) as u64)
}

/// Iterate boxes in `b[start..end]`: yields (type, box start, payload start, box end).
fn boxes(b: &[u8], start: usize, end: usize) -> Vec<([u8; 4], usize, usize, usize)> {
    let mut out = Vec::new();
    let mut i = start;
    while i + 8 <= end {
        let Some(size32) = be32(b, i) else { break };
        let typ: [u8; 4] = b[i + 4..i + 8].try_into().unwrap();
        let (size, hdr) = match size32 {
            0 => ((end - i) as u64, 8),
            1 => match b.get(i + 8..i + 16) {
                Some(x) => (u64::from_be_bytes(x.try_into().unwrap()), 16),
                None => break,
            },
            s => (s, 8),
        };
        if size < hdr as u64 || i as u64 + size > end as u64 {
            break; // truncated box
        }
        let e = i + size as usize;
        out.push((typ, i, i + hdr, e));
        i = e;
    }
    out
}

/// Parses a fragmented MP4 (`ftyp`, `moov`, then `moof`/`mdat` pairs). Returns
/// `None` if the file is not an MP4.
pub fn fmp4_fragments(path: &Path) -> Option<Fmp4Fragments> {
    let b = fs::read(path).ok()?;
    let top = boxes(&b, 0, b.len());
    if top.first().map(|x| &x.0) != Some(b"ftyp") {
        return None;
    }
    let mut frags = Vec::new();
    let mut total = 0u64;
    let mut pending: Option<u64> = None;
    for (typ, _s, p, e) in top {
        match &typ {
            b"moof" => {
                let mut n = 0u64;
                for (t2, _, p2, e2) in boxes(&b, p, e) {
                    if &t2 == b"traf" {
                        for (t3, _, p3, _) in boxes(&b, p2, e2) {
                            if &t3 == b"trun" {
                                n += be32(&b, p3 + 4).unwrap_or(0);
                            }
                        }
                    }
                }
                pending = Some(n);
            }
            b"mdat" => {
                if let Some(n) = pending.take() {
                    total += n;
                    frags.push((total, e as u64));
                }
            }
            _ => {}
        }
    }
    Some(Fmp4Fragments { fragments: frags })
}
