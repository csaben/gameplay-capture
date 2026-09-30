//! End-to-end: device login (dev provider) -> session -> presigned upload via
//! cap-upload -> complete -> DELETE /me/data removes the objects.
//!
//! Skipped unless INGEST_E2E_DATABASE_URL is set. Also needs a Garage/S3
//! endpoint: GARAGE_TEST_ENDPOINT, GARAGE_TEST_KEY_ID, GARAGE_TEST_SECRET
//! (bucket GARAGE_TEST_BUCKET, default `gameplay`). See server/ingest-api/README.md.

use cap_upload::api::ApiClient;
use cap_upload::synthetic::write_segment;
use cap_upload::{SegmentState, Target, UploadConfig, UploadQueue};
use clap::Parser;
use futures::TryStreamExt;
use ingest_api::{AppState, Config};
use object_store::{path::Path as ObjPath, ObjectStore};
use std::time::Duration;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

#[tokio::test]
async fn full_flow() {
    let Some(db_url) = env("INGEST_E2E_DATABASE_URL") else {
        eprintln!("INGEST_E2E_DATABASE_URL not set; skipping");
        return;
    };
    let endpoint = env("GARAGE_TEST_ENDPOINT").expect("GARAGE_TEST_ENDPOINT");
    let key = env("GARAGE_TEST_KEY_ID").expect("GARAGE_TEST_KEY_ID");
    let secret = env("GARAGE_TEST_SECRET").expect("GARAGE_TEST_SECRET");
    let bucket = env("GARAGE_TEST_BUCKET").unwrap_or_else(|| "gameplay".into());

    let cfg = Config::try_parse_from([
        "ingest-api",
        "--database-url", &db_url,
        "--s3-endpoint", &endpoint,
        "--s3-region", "garage",
        "--s3-bucket", &bucket,
        "--s3-access-key", &key,
        "--s3-secret-key", &secret,
        "--s3-allow-http",
        "--auth-provider", "dev",
        "--dev-subject", &format!("e2e-{}", std::process::id()),
        "--deletion-poll-secs", "1",
    ])
    .unwrap();
    let state = AppState::from_config(cfg).await.unwrap();
    sqlx::query("INSERT INTO games (id, status, notes) VALUES ('blockedgame.exe', 'blocked', 'e2e') ON CONFLICT (id) DO UPDATE SET status = 'blocked'")
        .execute(&state.db)
        .await
        .unwrap();
    let db = state.db.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(ingest_api::serve(state, listener));

    // --- device login
    let api = ApiClient::new(&base).unwrap();
    let mut shown = None;
    let tok = api.device_login("rig-a", |c| shown = Some(c.user_code.clone())).await.unwrap();
    assert!(shown.is_some());
    assert!(tok.access_token.starts_with("gca_"));
    let user_id = tok.user_id.clone();

    // --- config
    let conf = api.config().await.unwrap();
    assert!(conf["blocklist"].as_array().unwrap().iter().any(|g| g["game_id"] == "blockedgame.exe"));
    assert_eq!(conf["rate_hz"], 20);

    // --- auth required
    let r = reqwest::Client::new().delete(format!("{base}/me/data")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = reqwest::Client::new()
        .post(format!("{base}/sessions"))
        .bearer_auth("gca_bogus")
        .json(&serde_json::json!({"game_id": "x", "client_version": "0.1.0", "consent_version": "1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // --- refresh rotates
    let tok2 = api.refresh(&tok.refresh_token).await.unwrap();
    assert!(api.refresh(&tok.refresh_token).await.is_err(), "refresh token is single-use");
    let token = tok2.access_token.clone();

    // --- segments: two good, one for a blocked game
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("sessions");
    let session = format!("e2e-{}", std::process::id());
    let s0 = write_segment(&root, &session, 0, 300_000).unwrap();
    let s1 = write_segment(&root, &session, 1, 150_000).unwrap();
    let blocked_session = format!("{session}-blocked");
    let sb = write_segment(&root, &blocked_session, 0, 1000).unwrap();
    let mut m: cap_types::Manifest = serde_json::from_slice(&std::fs::read(sb.join("manifest.json")).unwrap()).unwrap();
    m.game_id = "blockedgame.exe".into();
    std::fs::write(sb.join("manifest.json"), serde_json::to_vec(&m).unwrap()).unwrap();

    let q = UploadQueue::open(
        tmp.path().join("queue.db"),
        Target::Presigned { api_base: base.clone(), token: "expired-token".into() },
        UploadConfig { base_backoff: Duration::ZERO, ..Default::default() },
    )
    .unwrap();
    assert_eq!(q.scan_dir(&root).unwrap(), 3);
    // Bad token: retryable failure, nothing lost.
    q.process_due().await.unwrap();
    assert_eq!(q.stats().unwrap().pending, 3);
    q.set_token(token.clone());
    for _ in 0..5 {
        q.process_due().await.unwrap();
    }
    let entries = q.entries().unwrap();
    let st = q.stats().unwrap();
    assert_eq!(st.verified, 2, "{entries:?}");
    assert_eq!(st.failed, 1, "{entries:?}");
    let failed = entries.iter().find(|e| e.state == SegmentState::Failed).unwrap();
    assert!(failed.last_error.as_deref().unwrap().contains("game_blocked"), "{failed:?}");
    assert!(!s0.exists() && !s1.exists() && sb.exists());

    // --- objects landed under raw/<user_id>/<session>/seg_<n>/
    let store = object_store::aws::AmazonS3Builder::new()
        .with_endpoint(&endpoint)
        .with_region("garage")
        .with_bucket_name(&bucket)
        .with_access_key_id(&key)
        .with_secret_access_key(&secret)
        .with_allow_http(true)
        .build()
        .unwrap();
    let prefix = ObjPath::from(format!("raw/{user_id}/{session}"));
    let objs: Vec<_> = store.list(Some(&prefix)).try_collect().await.unwrap();
    assert_eq!(objs.len(), 10, "{objs:?}");
    assert!(objs.iter().any(|o| o.location.as_ref() == format!("raw/{user_id}/{session}/seg_000001/manifest.json")));

    let ready: Vec<(i32, String, i64)> = sqlx::query_as(
        "SELECT segment_idx, state, total_bytes FROM segments WHERE session_id = $1 ORDER BY segment_idx",
    )
    .bind(&session)
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(ready.len(), 2);
    assert!(ready.iter().all(|r| r.1 == "ready" && r.2 > 0), "{ready:?}");

    // --- a tampered complete (wrong size) is rejected
    let r = reqwest::Client::new()
        .post(format!("{base}/sessions/{session}/segments/0/complete"))
        .bearer_auth(&token)
        .json(&serde_json::json!({"files": [{"name": "manifest.json", "size": 1, "blake3": "0".repeat(64)}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // --- deletion
    let del = api.delete_my_data(&token).await.unwrap();
    assert_eq!(del.segments, 2);
    let mut status = String::new();
    for _ in 0..100 {
        status = sqlx::query_scalar("SELECT status FROM deletions WHERE id = $1::uuid")
            .bind(&del.deletion_id)
            .fetch_one(&db)
            .await
            .unwrap();
        if status == "raw_deleted" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(status, "raw_deleted");
    let objs: Vec<_> = store.list(Some(&prefix)).try_collect().await.unwrap();
    assert!(objs.is_empty(), "{objs:?}");
    let (n_deleted, n_listed): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT objects_deleted FROM deletions WHERE id = $1::uuid),
                (SELECT count(*) FROM deletion_segments WHERE deletion_id = $1::uuid)",
    )
    .bind(&del.deletion_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!((n_deleted, n_listed), (10, 2));
    let states: Vec<String> = sqlx::query_scalar("SELECT state FROM segments WHERE session_id = $1")
        .bind(&session)
        .fetch_all(&db)
        .await
        .unwrap();
    assert!(states.iter().all(|s| s == "deleted"), "{states:?}");

    // Re-uploading a deleted segment is refused.
    let r = reqwest::Client::new()
        .post(format!("{base}/sessions/{session}/segments/0/upload"))
        .bearer_auth(&token)
        .json(&serde_json::json!({"files": [{"name": "manifest.json", "size": 1, "blake3": "0".repeat(64)}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);

    server.abort();
}
