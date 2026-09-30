//! Upload queue wiring (cap-upload), bearer tokens for the presigned target,
//! and the `upload` / `status` / `retry-failed` commands.

use crate::config::{load_token, save_json, Config, Paths, StoredToken, UploadTargetCfg};
use crate::consent::CONSENT_VERSION;
use crate::util::{fmt_bytes, try_lock, unix_now, CtrlC};
use anyhow::{bail, Context, Result};
use cap_upload::api::{ApiClient, TokenResponse};
use cap_upload::{QueueStats, Target, UploadConfig, UploadQueue};
use std::path::Path;
use std::time::{Duration, Instant};

pub fn token_from_response(api_base: &str, t: &TokenResponse) -> StoredToken {
    let now = unix_now();
    StoredToken {
        api_base: api_base.to_string(),
        access_token: t.access_token.clone(),
        refresh_token: t.refresh_token.clone(),
        access_expires_at: now + t.expires_in,
        refresh_expires_at: now + t.refresh_expires_in,
        user_id: t.user_id.clone(),
        device_id: t.device_id.clone(),
    }
}

/// A valid access token for `api_base` (refreshing it if it expires within
/// `margin`), or an error telling the user to log in.
pub async fn fresh_token(paths: &Paths, api_base: &str, margin: Duration) -> Result<StoredToken> {
    let Some(tok) = load_token(&paths.token)? else {
        bail!("not logged in: run `gamecap login` first");
    };
    if tok.api_base.trim_end_matches('/') != api_base.trim_end_matches('/') {
        bail!("stored token is for {} but [upload] api_base is {api_base}: run `gamecap login` again", tok.api_base);
    }
    let now = unix_now();
    if tok.access_expires_at > now + margin.as_secs() {
        return Ok(tok);
    }
    if tok.refresh_expires_at <= now {
        bail!("login expired: run `gamecap login` again");
    }
    let api = ApiClient::new(api_base)?;
    let t = api.refresh(&tok.refresh_token).await.context("refreshing access token")?;
    let new = token_from_response(api_base, &t);
    save_json(&paths.token, &new, true)?;
    tracing::info!("access token refreshed");
    Ok(new)
}

/// Keeps the queue's bearer token fresh (presigned target).
pub async fn token_refresher(paths: Paths, api_base: String, queue: UploadQueue) {
    loop {
        let wait = match load_token(&paths.token) {
            Ok(Some(t)) => t.access_expires_at.saturating_sub(unix_now() + 120),
            _ => 60,
        };
        tokio::time::sleep(Duration::from_secs(wait.clamp(5, 3600))).await;
        match fresh_token(&paths, &api_base, Duration::from_secs(180)).await {
            Ok(t) => queue.set_token(t.access_token),
            Err(e) => tracing::error!("token refresh failed: {e:#}"),
        }
    }
}

pub fn upload_config(cfg: &Config) -> UploadConfig {
    UploadConfig { user_id: cfg.user_id(), consent_version: CONSENT_VERSION.into(), ..Default::default() }
}

/// Open the queue for the configured target (None if no `[upload]`).
/// Presigned targets need a runtime to refresh the token first.
pub fn open_queue(cfg: &Config, paths: &Paths, rt: &tokio::runtime::Runtime) -> Result<Option<(UploadQueue, Option<String>)>> {
    let Some(target) = cfg.upload.as_ref().map(|u| u.target()).transpose()?.flatten() else {
        return Ok(None);
    };
    let (target, api_base) = match target {
        UploadTargetCfg::S3 { endpoint, region, bucket, access_key, secret_key, allow_http } => {
            (Target::S3 { endpoint, region, bucket, access_key, secret_key, allow_http }, None)
        }
        UploadTargetCfg::Presigned { api_base } => {
            let tok = rt.block_on(fresh_token(paths, &api_base, Duration::from_secs(180)))?;
            (Target::Presigned { api_base: api_base.clone(), token: tok.access_token }, Some(api_base))
        }
    };
    let q = UploadQueue::open(&paths.queue_db, target, upload_config(cfg)).context("opening upload queue")?;
    Ok(Some((q, api_base)))
}

/// Read-only queue access (stats, retry) that works without a target.
pub fn open_queue_offline(cfg: &Config, paths: &Paths) -> Result<UploadQueue> {
    let store = std::sync::Arc::new(object_store::memory::InMemory::new());
    Ok(UploadQueue::with_store(&paths.queue_db, store, upload_config(cfg))?)
}

pub fn fmt_stats(s: &QueueStats) -> String {
    format!("pending {} uploading {} uploaded {} verified {} failed {}", s.pending, s.uploading, s.uploaded, s.verified, s.failed)
}

pub fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().thread_name("gamecap-upload").build()?)
}

/// Is a `gamecap record` running (holding the record lock)?
pub fn record_running(paths: &Paths) -> bool {
    matches!(try_lock(&paths.record_lock), Ok(None))
}

pub struct UploadOpts {
    /// Keep running after the queue is empty.
    pub follow: bool,
    /// Give up after this long (None = until empty).
    pub timeout: Option<Duration>,
}

/// `gamecap upload`: recover partials (unless a recorder is running), queue
/// everything finished on disk and drain.
pub fn cmd_upload(cfg: &Config, paths: &Paths, opts: UploadOpts, ctrlc: &CtrlC) -> Result<()> {
    let Some(_lock) = try_lock(&paths.upload_lock)? else {
        bail!("another gamecap process (record or upload) is already running the uploader");
    };
    let rt = runtime()?;
    let Some((queue, api_base)) = open_queue(cfg, paths, &rt)? else {
        bail!("no [upload] target configured in {}", paths.config_file.display());
    };
    if !record_running(paths) {
        let rep = cap_recorder::recover_partials(&paths.sessions_root);
        for (p, why) in &rep.broken {
            tracing::warn!(path = %p.display(), "unrecoverable partial segment: {why}");
        }
        if !rep.finalized.is_empty() {
            eprintln!("recovered {} partial segment(s)", rep.finalized.len());
        }
    }
    let added = queue.scan_dir(&paths.sessions_root)?;
    eprintln!("queued {added} new segment(s) from {}", paths.sessions_root.display());
    let worker = rt.spawn(queue.clone().run_owned());
    if let Some(api) = api_base {
        rt.spawn(token_refresher(paths.clone(), api, queue.clone()));
    }
    let start = Instant::now();
    let mut last = String::new();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let st = queue.stats()?;
        let line = fmt_stats(&st);
        if line != last {
            eprintln!("[{:>5.1}s] {line}", start.elapsed().as_secs_f64());
            last = line;
        }
        if ctrlc.triggered() {
            eprintln!("interrupted");
            break;
        }
        if !opts.follow && st.outstanding() == 0 {
            break;
        }
        if opts.timeout.is_some_and(|t| start.elapsed() >= t) {
            eprintln!("timeout: {} segment(s) still outstanding", st.outstanding());
            break;
        }
    }
    queue.shutdown();
    let _ = rt.block_on(async { tokio::time::timeout(Duration::from_secs(10), worker).await });
    rt.shutdown_timeout(Duration::from_secs(2));
    let st = queue.stats()?;
    eprintln!("done: {}", fmt_stats(&st));
    if st.failed > 0 {
        eprintln!("{} segment(s) failed permanently; see `gamecap status`, fix, then `gamecap retry-failed`", st.failed);
    }
    Ok(())
}

fn dir_counts(root: &Path) -> (usize, usize, usize) {
    let (mut fin, mut partial, mut broken) = (0, 0, 0);
    for s in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for e in std::fs::read_dir(s.path()).into_iter().flatten().flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !n.starts_with("seg_") {
                continue;
            }
            if n.ends_with(".partial") {
                partial += 1;
            } else if n.ends_with(".broken") {
                broken += 1;
            } else {
                fin += 1;
            }
        }
    }
    (fin, partial, broken)
}

/// `gamecap status`.
pub fn cmd_status(cfg: &Config, paths: &Paths, verbose: bool) -> Result<()> {
    let state: crate::config::State = crate::config::load_json(&paths.state)?;
    println!("config:        {}", paths.config_file.display());
    println!("sessions_root: {}", paths.sessions_root.display());
    println!("queue db:      {}", paths.queue_db.display());
    println!("logs:          {}", paths.log_dir.display());
    let target = match cfg.upload.as_ref().map(|u| u.target()).transpose()?.flatten() {
        Some(UploadTargetCfg::S3 { endpoint, bucket, .. }) => format!("s3 {endpoint} bucket {bucket} as {}", cfg.user_id()),
        Some(UploadTargetCfg::Presigned { api_base }) => {
            let who = load_token(&paths.token)?.map(|t| format!("logged in as {}", t.user_id)).unwrap_or("not logged in".into());
            format!("presigned {api_base} ({who})")
        }
        None => "none (segments stay local)".into(),
    };
    println!("upload target: {target}");
    println!(
        "consent:       {}",
        match (&state.consent_version, &state.consent_accepted_at) {
            (Some(v), Some(t)) => format!("version {v} accepted {t}{}", if v == CONSENT_VERSION { "" } else { " (outdated)" }),
            _ => "not given".into(),
        }
    );
    println!("recording:     {}", if record_running(paths) { "yes (a gamecap record is running)" } else { "no" });
    let used = cap_upload::disk_usage(&paths.sessions_root).unwrap_or(0);
    let cap = cfg.disk_cap_bytes();
    let (fin, partial, broken) = dir_counts(&paths.sessions_root);
    println!(
        "local disk:    {} of {} cap ({:.1}%); {fin} finished, {partial} partial, {broken} broken segment folder(s)",
        fmt_bytes(used),
        fmt_bytes(cap),
        used as f64 * 100.0 / cap as f64
    );
    let q = open_queue_offline(cfg, paths)?;
    println!("upload queue:  {}", fmt_stats(&q.stats()?));
    for e in q.entries()? {
        if verbose || e.state == cap_upload::SegmentState::Failed || e.last_error.is_some() && e.state != cap_upload::SegmentState::Verified {
            println!(
                "  {} seg {:>4} {:<9} attempts {} {}",
                e.session_id,
                e.segment_idx,
                e.state.as_str(),
                e.attempts,
                e.last_error.as_deref().unwrap_or("")
            );
        }
    }
    Ok(())
}

pub fn cmd_retry_failed(cfg: &Config, paths: &Paths) -> Result<()> {
    let q = open_queue_offline(cfg, paths)?;
    let n = q.retry_failed()?;
    println!("{n} failed segment(s) moved back to pending; they upload on the next `gamecap upload` / `gamecap record`");
    Ok(())
}
