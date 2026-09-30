//! Background worker for `DELETE /me/data`: deletes raw objects, then hands
//! off to the shard pipeline by setting `deletions.status = 'raw_deleted'`.

use crate::AppState;
use std::time::Duration;
use uuid::Uuid;

/// Run forever: process pending deletions whenever notified or every
/// `DELETION_POLL_SECS`.
pub async fn run_worker(state: AppState) {
    let poll = Duration::from_secs(state.cfg.deletion_poll_secs.max(1));
    loop {
        loop {
            match process_one(&state).await {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    tracing::error!(error = %e, "deletion worker error");
                    break;
                }
            }
        }
        tokio::select! {
            _ = state.deletion_notify.notified() => {}
            _ = tokio::time::sleep(poll) => {}
        }
    }
}

/// Process the oldest pending deletion. Returns `Ok(true)` if one was handled
/// (successfully or with a recorded error that will be retried later).
pub async fn process_one(state: &AppState) -> anyhow::Result<bool> {
    let mut tx = state.db.begin().await?;
    // Retry failed ones with a crude backoff: attempts * 1 min.
    let row: Option<(Uuid, i32)> = sqlx::query_as(
        "SELECT id, attempts FROM deletions
         WHERE status = 'pending'
           AND (attempts = 0 OR requested_at + make_interval(mins => attempts) < now())
         ORDER BY requested_at LIMIT 1 FOR UPDATE SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some((deletion_id, _)) = row else {
        return Ok(false);
    };
    let segs: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT segment_id, key_prefix FROM deletion_segments WHERE deletion_id = $1")
            .bind(deletion_id)
            .fetch_all(&mut *tx)
            .await?;

    let mut deleted = 0u64;
    let mut err = None;
    for (seg_id, prefix) in &segs {
        match state.storage.delete_prefix(prefix).await {
            Ok(n) => {
                deleted += n;
                sqlx::query("UPDATE segments SET state = 'deleted', deleted_at = now() WHERE id = $1")
                    .bind(seg_id)
                    .execute(&mut *tx)
                    .await?;
            }
            Err(e) => {
                err = Some(format!("{prefix}: {e}"));
                break;
            }
        }
    }
    match err {
        None => {
            sqlx::query(
                "UPDATE deletions SET status = 'raw_deleted', raw_deleted_at = now(),
                        objects_deleted = objects_deleted + $2, last_error = NULL
                 WHERE id = $1",
            )
            .bind(deletion_id)
            .bind(deleted as i64)
            .execute(&mut *tx)
            .await?;
            tracing::info!(%deletion_id, segments = segs.len(), objects = deleted, "raw objects deleted");
        }
        Some(e) => {
            tracing::warn!(%deletion_id, error = %e, "deletion failed; will retry");
            sqlx::query(
                "UPDATE deletions SET attempts = attempts + 1, last_error = $2, objects_deleted = objects_deleted + $3
                 WHERE id = $1",
            )
            .bind(deletion_id)
            .bind(e)
            .bind(deleted as i64)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(true)
}
