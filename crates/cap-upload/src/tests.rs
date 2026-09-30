use super::*;
use crate::synthetic::write_segment;
use async_trait::async_trait;
use futures::stream::BoxStream;
use object_store::memory::InMemory;
use object_store::path::Path as ObjPath;
use object_store::{
    GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, PutMultipartOpts, PutOptions, PutPayload, PutResult,
};
use std::sync::atomic::AtomicUsize;

/// Wraps InMemory: records put order and fails the first `fail_puts` puts.
#[derive(Debug)]
struct Flaky {
    inner: InMemory,
    fail_puts: AtomicUsize,
    puts: Mutex<Vec<String>>,
}

impl Flaky {
    fn new(fail_puts: usize) -> Arc<Self> {
        Arc::new(Self { inner: InMemory::new(), fail_puts: AtomicUsize::new(fail_puts), puts: Mutex::new(vec![]) })
    }
}

impl std::fmt::Display for Flaky {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Flaky")
    }
}

#[async_trait]
impl ObjectStore for Flaky {
    async fn put_opts(&self, location: &ObjPath, payload: PutPayload, opts: PutOptions) -> object_store::Result<PutResult> {
        if self.fail_puts.load(Ordering::SeqCst) > 0 {
            self.fail_puts.fetch_sub(1, Ordering::SeqCst);
            return Err(object_store::Error::Generic { store: "flaky", source: "injected failure".into() });
        }
        self.puts.lock().unwrap().push(location.to_string());
        self.inner.put_opts(location, payload, opts).await
    }
    async fn put_multipart_opts(&self, location: &ObjPath, opts: PutMultipartOpts) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.puts.lock().unwrap().push(format!("multipart:{location}"));
        self.inner.put_multipart_opts(location, opts).await
    }
    async fn get_opts(&self, location: &ObjPath, options: GetOptions) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }
    async fn delete(&self, location: &ObjPath) -> object_store::Result<()> {
        self.inner.delete(location).await
    }
    fn list(&self, prefix: Option<&ObjPath>) -> BoxStream<'_, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&ObjPath>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy(&self, from: &ObjPath, to: &ObjPath) -> object_store::Result<()> {
        self.inner.copy(from, to).await
    }
    async fn copy_if_not_exists(&self, from: &ObjPath, to: &ObjPath) -> object_store::Result<()> {
        self.inner.copy_if_not_exists(from, to).await
    }
}

fn cfg() -> UploadConfig {
    UploadConfig { user_id: "rig1".into(), base_backoff: Duration::ZERO, ..Default::default() }
}

async fn get(store: &dyn ObjectStore, key: &str) -> Vec<u8> {
    store.get(&ObjPath::from(key)).await.unwrap().bytes().await.unwrap().to_vec()
}

#[tokio::test]
async fn uploads_manifest_last_then_deletes_local() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("sessions");
    let seg = write_segment(&root, "sess1", 42, 10_000).unwrap();
    let video = std::fs::read(seg.join("video.mp4")).unwrap();
    let store = Flaky::new(0);
    let q = UploadQueue::with_store(tmp.path().join("q.db"), store.clone(), cfg()).unwrap();
    assert!(q.enqueue(&seg).unwrap());
    assert!(!q.enqueue(&seg).unwrap(), "second enqueue is a no-op");
    assert_eq!(q.stats().unwrap().pending, 1);

    assert_eq!(q.process_due().await.unwrap(), 1);
    let st = q.stats().unwrap();
    assert_eq!(st.verified, 1);
    assert_eq!(st.outstanding(), 0);
    assert!(!seg.exists(), "local folder deleted after verification");

    let puts = store.puts.lock().unwrap().clone();
    assert_eq!(puts.len(), 5);
    assert_eq!(puts.last().unwrap(), "raw/rig1/sess1/seg_000042/manifest.json");
    assert_eq!(puts[0], "raw/rig1/sess1/seg_000042/video.mp4");
    assert_eq!(get(store.as_ref(), "raw/rig1/sess1/seg_000042/video.mp4").await, video);
}

#[tokio::test]
async fn retries_with_backoff_after_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("sessions");
    let seg = write_segment(&root, "sess1", 0, 5000).unwrap();
    let store = Flaky::new(2);
    let mut c = cfg();
    c.base_backoff = Duration::from_secs(3600);
    c.max_backoff = Duration::from_secs(7200);
    let q = UploadQueue::with_store(tmp.path().join("q.db"), store.clone(), c).unwrap();
    q.enqueue(&seg).unwrap();

    assert_eq!(q.process_due().await.unwrap(), 0);
    let e = &q.entries().unwrap()[0];
    assert_eq!(e.state, SegmentState::Pending);
    assert_eq!(e.attempts, 1);
    assert!(e.last_error.as_deref().unwrap().contains("injected"));
    // Backoff of >= 30 min (jitter 50-100% of 1h) means nothing is due now.
    assert!(e.next_attempt_at > now_ms() + 29 * 60 * 1000);
    assert_eq!(q.process_due().await.unwrap(), 0);
    assert_eq!(q.entries().unwrap()[0].attempts, 1);
    assert!(seg.exists());
}

#[tokio::test]
async fn zero_backoff_eventually_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let seg = write_segment(&tmp.path().join("s"), "sess1", 1, 5000).unwrap();
    let store = Flaky::new(3);
    let q = UploadQueue::with_store(tmp.path().join("q.db"), store.clone(), cfg()).unwrap();
    q.enqueue(&seg).unwrap();
    for _ in 0..5 {
        q.process_due().await.unwrap();
    }
    let e = &q.entries().unwrap()[0];
    assert_eq!(e.state, SegmentState::Verified);
    assert_eq!(e.attempts, 3);
}

#[tokio::test]
async fn hash_mismatch_fails_permanently_and_keeps_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let seg = write_segment(&tmp.path().join("s"), "sess1", 3, 5000).unwrap();
    // Corrupt one byte without changing size.
    let mut v = std::fs::read(seg.join("inputs.parquet")).unwrap();
    v[0] ^= 0xff;
    std::fs::write(seg.join("inputs.parquet"), v).unwrap();
    let store = Flaky::new(0);
    let q = UploadQueue::with_store(tmp.path().join("q.db"), store.clone(), cfg()).unwrap();
    q.enqueue(&seg).unwrap();
    q.process_due().await.unwrap();
    let e = &q.entries().unwrap()[0];
    assert_eq!(e.state, SegmentState::Failed);
    assert!(e.last_error.as_deref().unwrap().contains("blake3 mismatch"));
    assert!(seg.exists());
    assert!(store.puts.lock().unwrap().is_empty(), "nothing uploaded");
}

#[tokio::test]
async fn restart_requeues_uploading_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let seg = write_segment(&tmp.path().join("s"), "sess1", 5, 5000).unwrap();
    let db = tmp.path().join("q.db");
    {
        let q = UploadQueue::with_store(&db, Flaky::new(0), cfg()).unwrap();
        q.enqueue(&seg).unwrap();
        // Simulate a crash mid-upload.
        let p = std::fs::canonicalize(&seg).unwrap();
        q.set_state(&p, SegmentState::Uploading, None).unwrap();
        assert_eq!(q.stats().unwrap().uploading, 1);
    }
    let store = Flaky::new(0);
    let q = UploadQueue::with_store(&db, store.clone(), cfg()).unwrap();
    assert_eq!(q.stats().unwrap().pending, 1);
    assert_eq!(q.process_due().await.unwrap(), 1);
    assert!(!seg.exists());
}

#[tokio::test]
async fn scan_dir_skips_partial_and_unfinished() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("sessions");
    write_segment(&root, "a", 0, 100).unwrap();
    write_segment(&root, "a", 1, 100).unwrap();
    write_segment(&root, "b", 0, 100).unwrap();
    std::fs::create_dir_all(root.join("a").join("seg_000002.partial")).unwrap();
    std::fs::write(root.join("a").join("seg_000002.partial").join("manifest.json"), b"{}").unwrap();
    std::fs::create_dir_all(root.join("b").join("seg_000001")).unwrap(); // no manifest yet
    let q = UploadQueue::with_store(tmp.path().join("q.db"), Flaky::new(0), cfg()).unwrap();
    assert_eq!(q.scan_dir(&root).unwrap(), 3);
    assert_eq!(q.scan_dir(&root).unwrap(), 0);
    assert!(q.enqueue(root.join("a").join("seg_000002.partial")).is_err());
    assert_eq!(q.scan_dir(tmp.path().join("missing")).unwrap(), 0);
}

#[tokio::test]
async fn multipart_above_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    let seg = write_segment(&tmp.path().join("s"), "sess1", 7, 12 * 1024 * 1024).unwrap();
    let video = std::fs::read(seg.join("video.mp4")).unwrap();
    let store = Flaky::new(0);
    let mut c = cfg();
    c.multipart_threshold = 6 * 1024 * 1024;
    c.multipart_part_size = 5 * 1024 * 1024;
    let q = UploadQueue::with_store(tmp.path().join("q.db"), store.clone(), c).unwrap();
    q.enqueue(&seg).unwrap();
    assert_eq!(q.process_due().await.unwrap(), 1);
    let puts = store.puts.lock().unwrap().clone();
    assert_eq!(puts[0], "multipart:raw/rig1/sess1/seg_000007/video.mp4");
    assert_eq!(get(store.as_ref(), "raw/rig1/sess1/seg_000007/video.mp4").await, video);
}

#[tokio::test]
async fn run_loop_picks_up_enqueued_segments_and_shuts_down() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Flaky::new(0);
    let q = UploadQueue::with_store(tmp.path().join("q.db"), store.clone(), cfg()).unwrap();
    let worker = tokio::spawn(q.clone().run_owned());
    let root = tmp.path().join("s");
    // Enqueue from a plain OS thread, like the recorder does.
    let q2 = q.clone();
    let root2 = root.clone();
    std::thread::spawn(move || {
        for i in 0..3 {
            let seg = write_segment(&root2, "sess9", i, 2000).unwrap();
            q2.enqueue(&seg).unwrap();
        }
    })
    .join()
    .unwrap();
    for _ in 0..100 {
        if q.stats().unwrap().verified == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(q.stats().unwrap().verified, 3);
    q.shutdown();
    tokio::time::timeout(Duration::from_secs(5), worker).await.unwrap().unwrap();
}

#[test]
fn disk_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("sessions");
    assert_eq!(disk_usage(&root).unwrap(), 0);
    let seg = write_segment(&root, "a", 0, 50_000).unwrap();
    let used = disk_usage(&root).unwrap();
    let manifest_len = std::fs::metadata(seg.join("manifest.json")).unwrap().len();
    assert_eq!(used, 50_000 + 2048 + 3072 + 4096 + manifest_len);
    assert!(over_cap(&root, used).unwrap());
    assert!(!over_cap(&root, used + 1).unwrap());
    assert!(!over_cap(&root, DEFAULT_DISK_CAP_BYTES).unwrap());
}

#[test]
fn backoff_grows_and_caps() {
    let c = UploadConfig { base_backoff: Duration::from_secs(2), max_backoff: Duration::from_secs(60), ..Default::default() };
    let b1 = backoff(&c, 1);
    assert!(b1 >= Duration::from_secs(1) && b1 <= Duration::from_secs(2));
    let b4 = backoff(&c, 4);
    assert!(b4 >= Duration::from_secs(8) && b4 <= Duration::from_secs(16));
    let b50 = backoff(&c, 50);
    assert!(b50 >= Duration::from_secs(30) && b50 <= Duration::from_secs(60));
}
