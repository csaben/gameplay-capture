//! Append-only per-segment journal (`journal.jsonl` inside the `.partial`
//! folder). Every frame / input / focus row is appended as it is produced
//! (buffered, flushed at least once per second), so a crash loses at most ~1 s
//! of table rows and `recover_partials` can rebuild the parquet files and the
//! manifest. The journal is deleted when a segment closes normally.

use cap_types::{FocusRecord, FrameRecord, InputEvent, Manifest};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub const JOURNAL_FILE: &str = "journal.jsonl";
const FLUSH_EVERY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Entry {
    /// First line: manifest template (t_end/frame_count/hashes not yet known).
    Header(Box<Manifest>),
    Frame(FrameRecord),
    Input(InputEvent),
    /// Focus state at `t_start` (may be rewritten when a late focus record
    /// arrives; the last one wins).
    FocusInit(FocusRecord),
    Focus(FocusRecord),
}

pub struct JournalWriter {
    w: BufWriter<File>,
    last_flush: Instant,
}

impl JournalWriter {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        Ok(Self { w: BufWriter::with_capacity(64 * 1024, File::create(path)?), last_flush: Instant::now() })
    }

    pub fn append(&mut self, e: &Entry) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.w, e)?;
        self.w.write_all(b"\n")?;
        self.maybe_flush()
    }

    pub fn maybe_flush(&mut self) -> std::io::Result<()> {
        if self.last_flush.elapsed() >= FLUSH_EVERY {
            self.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.last_flush = Instant::now();
        self.w.flush()
    }
}

/// Reads all complete, parseable lines (a torn last line from a crash is skipped).
pub fn read(path: &Path) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let Ok(line) = line else { break };
        if let Ok(e) = serde_json::from_str::<Entry>(&line) {
            out.push(e);
        }
    }
    Ok(out)
}
