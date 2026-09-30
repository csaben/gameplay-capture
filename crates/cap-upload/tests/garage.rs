//! Integration test against a real Garage (or any S3) endpoint.
//!
//! Skipped unless `GARAGE_TEST_ENDPOINT` is set. Also needs
//! `GARAGE_TEST_KEY_ID`, `GARAGE_TEST_SECRET`, and optionally
//! `GARAGE_TEST_BUCKET` (default `gameplay`) / `GARAGE_TEST_REGION` (default `garage`).
//! See deploy/garage/README.md ("Local test instance").

use cap_upload::synthetic::write_segment;
use cap_upload::{Target, UploadConfig, UploadQueue};
use futures::TryStreamExt;
use object_store::{aws::AmazonS3Builder, path::Path as ObjPath, ObjectStore};
use std::time::Duration;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

#[tokio::test]
async fn upload_to_garage() {
    let Some(endpoint) = env("GARAGE_TEST_ENDPOINT") else {
        eprintln!("GARAGE_TEST_ENDPOINT not set; skipping");
        return;
    };
    let key = env("GARAGE_TEST_KEY_ID").expect("GARAGE_TEST_KEY_ID");
    let secret = env("GARAGE_TEST_SECRET").expect("GARAGE_TEST_SECRET");
    let bucket = env("GARAGE_TEST_BUCKET").unwrap_or_else(|| "gameplay".into());
    let region = env("GARAGE_TEST_REGION").unwrap_or_else(|| "garage".into());

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("sessions");
    let session = format!("it-{}", std::process::id());
    let small = write_segment(&root, &session, 0, 200_000).unwrap();
    let big = write_segment(&root, &session, 1, 13 * 1024 * 1024).unwrap();
    let big_video = std::fs::read(big.join("video.mp4")).unwrap();
    // A half-written segment must be ignored.
    std::fs::create_dir_all(root.join(&session).join("seg_000002.partial")).unwrap();

    let cfg = UploadConfig {
        user_id: "itest-client".into(),
        multipart_threshold: 6 * 1024 * 1024,
        multipart_part_size: 5 * 1024 * 1024,
        base_backoff: Duration::from_millis(200),
        ..Default::default()
    };
    let target = Target::S3 {
        endpoint: endpoint.clone(),
        region: region.clone(),
        bucket: bucket.clone(),
        access_key: key.clone(),
        secret_key: secret.clone(),
        allow_http: true,
    };
    let q = UploadQueue::open(tmp.path().join("queue.db"), target, cfg).unwrap();
    assert_eq!(q.scan_dir(&root).unwrap(), 2);
    let worker = tokio::spawn(q.clone().run_owned());
    for _ in 0..300 {
        if q.stats().unwrap().verified == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    q.shutdown();
    worker.await.unwrap();
    let st = q.stats().unwrap();
    assert_eq!(st.verified, 2, "entries: {:?}", q.entries().unwrap());
    assert!(!small.exists() && !big.exists());

    let store = AmazonS3Builder::new()
        .with_endpoint(endpoint)
        .with_region(region)
        .with_bucket_name(bucket)
        .with_access_key_id(key)
        .with_secret_access_key(secret)
        .with_allow_http(true)
        .build()
        .unwrap();
    let prefix = ObjPath::from(format!("raw/itest-client/{session}"));
    let objs: Vec<_> = store.list(Some(&prefix)).try_collect().await.unwrap();
    assert_eq!(objs.len(), 10, "{objs:?}");
    let got = store
        .get(&ObjPath::from(format!("raw/itest-client/{session}/seg_000001/video.mp4")))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(got.as_ref(), big_video.as_slice());
    let m: cap_types::Manifest = serde_json::from_slice(
        &store
            .get(&ObjPath::from(format!("raw/itest-client/{session}/seg_000000/manifest.json")))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(m.segment_idx, 0);

    for o in objs {
        store.delete(&o.location).await.unwrap();
    }
}
