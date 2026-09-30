//! Durable upload queue, retries, integrity hashes.
//!
//! The recorder hands finished segment folders to [`UploadQueue::enqueue`]
//! (sync, cheap, callable from any thread). An async worker ([`UploadQueue::run`])
//! uploads them, manifest last, confirms every object with a size check, then
//! deletes the local folder. State lives in SQLite so the queue survives restarts:
//!
//! ```text
//! pending --> uploading --> uploaded --> verified (local folder deleted)
//!    ^            |
//!    +-- error ---+  (attempts += 1, next_attempt_at = now + backoff w/ jitter)
//!                 +--> failed (permanent: hash mismatch, missing files, API refused)
//! ```

pub mod api;
mod backend;
pub mod disk;
pub mod segment;
pub mod synthetic;

pub use disk::{disk_usage, over_cap, DEFAULT_DISK_CAP_BYTES};
pub use segment::{is_finished_segment, object_key, segment_prefix, verify_segment, VerifiedSegment};

use backend::{Backend, PresignedBackend, StoreBackend};
use object_store::ObjectStore;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("object store: {0}")]
    Store(String),
    #[error("http: {0}")]
    Http(String),
    #[error("api {status} {code}: {message}")]
    Api { status: u16, code: String, message: String },
    #[error("remote size check failed: {0}")]
    SizeMismatch(String),
    /// Not retryable: corrupt/missing segment, rejected by the server.
    #[error("{0}")]
    Permanent(String),
    #[error("invalid segment: {0}")]
    InvalidSegment(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where segments go.
#[derive(Debug, Clone)]
pub enum Target {
    /// Direct S3 API with a per-machine key. Phase 1: Garage over the tailnet
    /// (`region = "garage"`, `allow_http = true`); Phase 2 R2 with keys
    /// (`region = "auto"`).
    S3 { endpoint: String, region: String, bucket: String, access_key: String, secret_key: String, allow_http: bool },
    /// Presigned PUT URLs from `ingest-api` (Phase 2). `token` is a bearer
    /// token from `POST /auth/device`; replace it with [`UploadQueue::set_token`].
    Presigned { api_base: String, token: String },
}

#[derive(Debug, Clone)]
pub struct UploadConfig {
    /// `<user_id>` in `raw/<user_id>/...` for [`Target::S3`]. Phase 1: the
    /// client/machine name. Ignored for presigned uploads (server decides).
    pub user_id: String,
    /// Consent version sent with `POST /sessions` (presigned target only).
    pub consent_version: String,
    /// Files larger than this use multipart upload. Default 64 MiB.
    pub multipart_threshold: u64,
    /// Multipart part size. Default 16 MiB (S3 minimum is 5 MiB).
    pub multipart_part_size: usize,
    /// First retry delay; doubles per attempt up to `max_backoff`. Jitter: 50-100%.
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    /// Wake up at least this often even without new work.
    pub idle_poll: Duration,
    /// Delete the local folder once all objects are confirmed. Default true.
    pub delete_local: bool,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            user_id: "default".into(),
            consent_version: "1".into(),
            multipart_threshold: 64 * 1024 * 1024,
            multipart_part_size: 16 * 1024 * 1024,
            base_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(600),
            idle_poll: Duration::from_secs(30),
            delete_local: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SegmentState {
    Pending,
    Uploading,
    Uploaded,
    Verified,
    Failed,
}

impl SegmentState {
    pub fn as_str(self) -> &'static str {
        match self {
            SegmentState::Pending => "pending",
            SegmentState::Uploading => "uploading",
            SegmentState::Uploaded => "uploaded",
            SegmentState::Verified => "verified",
            SegmentState::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => SegmentState::Pending,
            "uploading" => SegmentState::Uploading,
            "uploaded" => SegmentState::Uploaded,
            "verified" => SegmentState::Verified,
            "failed" => SegmentState::Failed,
            _ => return None,
        })
    }
}

/// One row of the queue.
#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub path: PathBuf,
    pub session_id: String,
    pub segment_idx: u32,
    pub state: SegmentState,
    pub attempts: u32,
    /// Unix millis.
    pub next_attempt_at: i64,
    pub last_error: Option<String>,
}

/// Counts per state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueStats {
    pub pending: u64,
    pub uploading: u64,
    pub uploaded: u64,
    pub verified: u64,
    pub failed: u64,
}

impl QueueStats {
    /// Segments still waiting on the network.
    pub fn outstanding(&self) -> u64 {
        self.pending + self.uploading + self.uploaded
    }
}

struct Inner {
    db: Mutex<Connection>,
    backend: Backend,
    cfg: UploadConfig,
    notify: Notify,
    shutdown: AtomicBool,
    token: Option<Arc<RwLock<String>>>,
}

/// Cheap to clone; all clones share one DB connection and worker wakeup.
#[derive(Clone)]
pub struct UploadQueue {
    inner: Arc<Inner>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS segments (
    path            TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    segment_idx     INTEGER NOT NULL,
    state           TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    enqueued_at     INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS segments_due ON segments(state, next_attempt_at);
";

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

impl UploadQueue {
    /// Open (or create) the queue DB and build the backend for `target`.
    /// Rows left in `uploading` by a crash go back to `pending`.
    pub fn open(db_path: impl AsRef<Path>, target: Target, cfg: UploadConfig) -> Result<Self> {
        match target {
            Target::S3 { endpoint, region, bucket, access_key, secret_key, allow_http } => {
                let store = backend::build_s3(&endpoint, &region, &bucket, &access_key, &secret_key, allow_http)?;
                Self::with_store(db_path, store, cfg)
            }
            Target::Presigned { api_base, token } => {
                let token = Arc::new(RwLock::new(token));
                let backend = Backend::Presigned(PresignedBackend {
                    api: api::ApiClient::new(api_base)?,
                    token: token.clone(),
                    consent_version: cfg.consent_version.clone(),
                    sessions: Mutex::new(HashSet::new()),
                });
                Self::with_backend(db_path.as_ref(), backend, cfg, Some(token))
            }
        }
    }

    /// Like [`open`](Self::open) with an arbitrary `object_store` (tests use `InMemory`).
    pub fn with_store(db_path: impl AsRef<Path>, store: Arc<dyn ObjectStore>, cfg: UploadConfig) -> Result<Self> {
        let backend = Backend::Store(StoreBackend {
            store,
            user_id: cfg.user_id.clone(),
            multipart_threshold: cfg.multipart_threshold,
            part_size: cfg.multipart_part_size.max(5 * 1024 * 1024),
        });
        Self::with_backend(db_path.as_ref(), backend, cfg, None)
    }

    fn with_backend(db_path: &Path, backend: Backend, cfg: UploadConfig, token: Option<Arc<RwLock<String>>>) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(db_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        let reset = conn.execute(
            "UPDATE segments SET state = 'pending', next_attempt_at = 0, updated_at = ?1 WHERE state = 'uploading'",
            params![now_ms()],
        )?;
        if reset > 0 {
            tracing::info!(reset, "requeued segments interrupted mid-upload");
        }
        Ok(Self {
            inner: Arc::new(Inner {
                db: Mutex::new(conn),
                backend,
                cfg,
                notify: Notify::new(),
                shutdown: AtomicBool::new(false),
                token,
            }),
        })
    }

    /// Queue a finished segment folder. Sync and cheap (reads the small
    /// manifest, one INSERT); safe to call from the recorder's writer thread.
    /// Returns `false` if it was already queued. Hashes are checked later by
    /// the worker, not here.
    pub fn enqueue(&self, segment_dir: impl AsRef<Path>) -> Result<bool> {
        let dir = segment_dir.as_ref();
        if !is_finished_segment(dir) {
            return Err(Error::InvalidSegment(format!(
                "{} is not a finished segment (needs manifest.json, no .partial suffix)",
                dir.display()
            )));
        }
        let manifest = segment::read_manifest(dir).map_err(|e| Error::InvalidSegment(e.to_string()))?;
        let path = std::fs::canonicalize(dir)?;
        let now = now_ms();
        let n = self.inner.db.lock().unwrap().execute(
            "INSERT OR IGNORE INTO segments (path, session_id, segment_idx, state, attempts, next_attempt_at, enqueued_at, updated_at)
             VALUES (?1, ?2, ?3, 'pending', 0, 0, ?4, ?4)",
            params![path.to_string_lossy(), manifest.session_id, manifest.segment_idx, now],
        )?;
        if n > 0 {
            tracing::debug!(path = %path.display(), "segment queued");
            self.inner.notify.notify_one();
        }
        Ok(n > 0)
    }

    /// Startup sweep: queue every finished segment under
    /// `<sessions_root>/<session_id>/seg_*` that is not in the DB yet
    /// (skips `.partial` folders and folders without `manifest.json`).
    /// Returns how many were newly queued.
    pub fn scan_dir(&self, sessions_root: impl AsRef<Path>) -> Result<usize> {
        let root = sessions_root.as_ref();
        let mut added = 0;
        let sessions = match std::fs::read_dir(root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut dirs = Vec::new();
        for s in sessions.flatten() {
            if !s.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(segs) = std::fs::read_dir(s.path()) else { continue };
            for seg in segs.flatten() {
                let p = seg.path();
                if seg.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && seg.file_name().to_string_lossy().starts_with("seg_")
                    && is_finished_segment(&p)
                {
                    dirs.push(p);
                }
            }
        }
        dirs.sort();
        for d in dirs {
            match self.enqueue(&d) {
                Ok(true) => added += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(path = %d.display(), error = %e, "skipping segment during scan"),
            }
        }
        Ok(added)
    }

    /// Replace the bearer token used by [`Target::Presigned`] (after a refresh).
    pub fn set_token(&self, token: impl Into<String>) {
        if let Some(t) = &self.inner.token {
            *t.write().unwrap() = token.into();
            self.inner.notify.notify_one();
        }
    }

    /// Worker loop: processes due segments until [`shutdown`](Self::shutdown).
    /// Spawn it once on a tokio runtime: `tokio::spawn(queue.clone().run_owned())`
    /// or `queue.run().await`.
    pub async fn run(&self) {
        while !self.inner.shutdown.load(Ordering::SeqCst) {
            match self.process_due().await {
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "upload queue pass failed"),
            }
            if self.inner.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let wait = self.next_wake().unwrap_or(self.inner.cfg.idle_poll);
            tokio::select! {
                _ = self.inner.notify.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Owned version of [`run`](Self::run) for `tokio::spawn`.
    pub async fn run_owned(self) {
        self.run().await
    }

    /// Ask [`run`](Self::run) to return after the current segment.
    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.inner.notify.notify_one();
    }

    /// Process every segment that is due now (pending with elapsed backoff,
    /// or uploaded but not yet deleted). Returns how many reached `verified`.
    pub async fn process_due(&self) -> Result<usize> {
        let mut verified = 0;
        let mut seen = HashSet::new();
        loop {
            if self.inner.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let Some(entry) = self.next_due(now_ms())? else { break };
            if !seen.insert(entry.path.clone()) {
                break; // don't spin on a row that got rescheduled with zero backoff
            }
            if self.process_entry(entry).await? == SegmentState::Verified {
                verified += 1;
            }
        }
        Ok(verified)
    }

    fn next_due(&self, now: i64) -> Result<Option<QueueEntry>> {
        let db = self.inner.db.lock().unwrap();
        let row = db
            .query_row(
                "SELECT path, session_id, segment_idx, state, attempts, next_attempt_at, last_error FROM segments
                 WHERE (state = 'pending' OR state = 'uploaded') AND next_attempt_at <= ?1
                 ORDER BY next_attempt_at, enqueued_at, session_id, segment_idx LIMIT 1",
                params![now],
                row_to_entry,
            )
            .optional()?;
        Ok(row)
    }

    /// Milliseconds until the next scheduled retry, if any.
    fn next_wake(&self) -> Option<Duration> {
        let db = self.inner.db.lock().unwrap();
        let next: Option<i64> = db
            .query_row(
                "SELECT MIN(next_attempt_at) FROM segments WHERE state IN ('pending', 'uploaded')",
                [],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        next.map(|t| {
            let ms = (t - now_ms()).max(0) as u64;
            Duration::from_millis(ms).min(self.inner.cfg.idle_poll)
        })
    }

    fn set_state(&self, path: &Path, state: SegmentState, err: Option<&str>) -> Result<()> {
        self.inner.db.lock().unwrap().execute(
            "UPDATE segments SET state = ?2, last_error = ?3, updated_at = ?4 WHERE path = ?1",
            params![path.to_string_lossy(), state.as_str(), err, now_ms()],
        )?;
        Ok(())
    }

    fn schedule_retry(&self, entry: &QueueEntry, state: SegmentState, err: &str) -> Result<()> {
        let attempts = entry.attempts + 1;
        let delay = backoff(&self.inner.cfg, attempts);
        self.inner.db.lock().unwrap().execute(
            "UPDATE segments SET state = ?2, attempts = ?3, next_attempt_at = ?4, last_error = ?5, updated_at = ?6 WHERE path = ?1",
            params![
                entry.path.to_string_lossy(),
                state.as_str(),
                attempts,
                now_ms() + delay.as_millis() as i64,
                err,
                now_ms()
            ],
        )?;
        Ok(())
    }

    async fn process_entry(&self, entry: QueueEntry) -> Result<SegmentState> {
        if entry.state == SegmentState::Uploaded {
            return self.finish_local(&entry).await;
        }
        self.set_state(&entry.path, SegmentState::Uploading, None)?;
        let dir = entry.path.clone();
        let verified = match tokio::task::spawn_blocking(move || verify_segment(&dir)).await {
            Ok(r) => r,
            Err(e) => Err(Error::Io(std::io::Error::other(e.to_string()))),
        };
        let result = match verified {
            Ok(seg) => self.inner.backend.upload_and_confirm(&seg).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => {
                self.set_state(&entry.path, SegmentState::Uploaded, None)?;
                tracing::info!(session = %entry.session_id, idx = entry.segment_idx, "segment uploaded");
                self.finish_local(&QueueEntry { state: SegmentState::Uploaded, ..entry }).await
            }
            Err(Error::Permanent(msg)) => {
                tracing::error!(path = %entry.path.display(), error = %msg, "segment failed permanently");
                self.set_state(&entry.path, SegmentState::Failed, Some(&msg))?;
                Ok(SegmentState::Failed)
            }
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(path = %entry.path.display(), attempts = entry.attempts + 1, error = %msg, "upload failed; will retry");
                self.schedule_retry(&entry, SegmentState::Pending, &msg)?;
                Ok(SegmentState::Pending)
            }
        }
    }

    /// uploaded -> verified: remove the local folder (sizes were already confirmed).
    async fn finish_local(&self, entry: &QueueEntry) -> Result<SegmentState> {
        if self.inner.cfg.delete_local {
            match tokio::fs::remove_dir_all(&entry.path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    let msg = format!("delete local folder: {e}");
                    self.schedule_retry(entry, SegmentState::Uploaded, &msg)?;
                    return Ok(SegmentState::Uploaded);
                }
            }
        }
        self.set_state(&entry.path, SegmentState::Verified, None)?;
        Ok(SegmentState::Verified)
    }

    pub fn stats(&self) -> Result<QueueStats> {
        let db = self.inner.db.lock().unwrap();
        let mut st = db.prepare("SELECT state, COUNT(*) FROM segments GROUP BY state")?;
        let mut out = QueueStats::default();
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (s, n) = row?;
            let n = n as u64;
            match SegmentState::parse(&s) {
                Some(SegmentState::Pending) => out.pending = n,
                Some(SegmentState::Uploading) => out.uploading = n,
                Some(SegmentState::Uploaded) => out.uploaded = n,
                Some(SegmentState::Verified) => out.verified = n,
                Some(SegmentState::Failed) => out.failed = n,
                None => {}
            }
        }
        Ok(out)
    }

    /// All rows (for status UIs and tests).
    pub fn entries(&self) -> Result<Vec<QueueEntry>> {
        let db = self.inner.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT path, session_id, segment_idx, state, attempts, next_attempt_at, last_error FROM segments
             ORDER BY session_id, segment_idx",
        )?;
        let rows = st.query_map([], row_to_entry)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Put `failed` rows back to `pending` (e.g. after the user fixed something).
    pub fn retry_failed(&self) -> Result<usize> {
        let n = self.inner.db.lock().unwrap().execute(
            "UPDATE segments SET state = 'pending', attempts = 0, next_attempt_at = 0, updated_at = ?1 WHERE state = 'failed'",
            params![now_ms()],
        )?;
        self.inner.notify.notify_one();
        Ok(n)
    }
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<QueueEntry> {
    let state: String = r.get(3)?;
    Ok(QueueEntry {
        path: PathBuf::from(r.get::<_, String>(0)?),
        session_id: r.get(1)?,
        segment_idx: r.get::<_, i64>(2)? as u32,
        state: SegmentState::parse(&state).unwrap_or(SegmentState::Pending),
        attempts: r.get::<_, i64>(4)? as u32,
        next_attempt_at: r.get(5)?,
        last_error: r.get(6)?,
    })
}

/// Exponential backoff with jitter: `base * 2^(attempts-1)`, capped, then
/// scaled by a random factor in [0.5, 1.0].
pub fn backoff(cfg: &UploadConfig, attempts: u32) -> Duration {
    let exp = attempts.saturating_sub(1).min(30);
    let raw = cfg.base_backoff.saturating_mul(1u32 << exp.min(20));
    let capped = raw.min(cfg.max_backoff);
    let jitter: f64 = rand::Rng::gen_range(&mut rand::thread_rng(), 0.5..=1.0);
    capped.mul_f64(jitter)
}

#[cfg(test)]
mod tests;
