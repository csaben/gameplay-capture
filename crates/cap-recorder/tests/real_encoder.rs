//! SyntheticSource -> real hardware encoder (hevc_nvenc) -> fragmented MP4,
//! verified with ffprobe. Needs an NVIDIA GPU + ffprobe, so ignored by default:
//! `cargo test -p cap-recorder --test real_encoder -- --ignored --nocapture`

use cap_capture::synthetic::SyntheticSource;
use cap_capture::WindowTarget;
use cap_recorder::testkit::ScriptedFocusTracker;
use cap_recorder::{tables, Recorder, RecorderConfig, Sources};
use cap_types::{files, CaptureSettings, Manifest};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn ffprobe(path: &Path) -> (String, u32, u32, u64, Vec<bool>) {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-count_frames"])
        .args(["-show_entries", "stream=codec_name,width,height,nb_read_frames", "-show_entries", "frame=key_frame"])
        .args(["-of", "json"])
        .arg(path)
        .output()
        .expect("ffprobe");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let s = &v["streams"][0];
    let keys = v["frames"].as_array().unwrap().iter().map(|f| f["key_frame"].as_i64() == Some(1)).collect();
    (
        s["codec_name"].as_str().unwrap().to_string(),
        s["width"].as_u64().unwrap() as u32,
        s["height"].as_u64().unwrap() as u32,
        s["nb_read_frames"].as_str().unwrap().parse().unwrap(),
        keys,
    )
}

#[test]
#[ignore]
fn synthetic_to_nvenc_segments() {
    let root = std::env::temp_dir().join(format!("caprec-nvenc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let target = WindowTarget { native_id: 0, title: "synthetic".into(), game_id: "synthetic".into(), pid: 0 };
    let settings = CaptureSettings { width: 640, height: 360, rate_hz: 20, segment_secs: 2 };
    let mut cfg = RecorderConfig::new(target, settings, root.clone(), "nvenc".into());
    cfg.encoder.force_encoder = Some("hevc_nvenc".into());
    let (tx, rx) = crossbeam_channel::unbounded();
    let sources = Sources {
        frames: Box::new(SyntheticSource::new(1280, 720, 60)),
        inputs: vec![],
        focus: Box::new(ScriptedFocusTracker::focused()),
    };
    let rec = Recorder::start(cfg, sources, Box::new(move |p| tx.send(p).unwrap())).expect("open hevc_nvenc");
    std::thread::sleep(Duration::from_millis(5200));
    let stats = rec.stop();
    eprintln!("{stats:#?}");
    let segs: Vec<_> = rx.try_iter().collect();
    assert_eq!(segs.len(), 3, "{segs:?}");
    for (i, dir) in segs.iter().enumerate() {
        let m: Manifest = serde_json::from_slice(&std::fs::read(dir.join(files::MANIFEST)).unwrap()).unwrap();
        let frames = tables::read_frames(&dir.join(files::FRAMES)).unwrap();
        let (codec, w, h, n, keys) = ffprobe(&dir.join(files::VIDEO));
        eprintln!(
            "seg {i}: {codec} {w}x{h} decoded {n} frames, manifest frame_count {} dropped {} encoder {} gpu {} os {} size {} B keyframes at {:?}",
            m.frame_count,
            m.dropped_frames,
            m.encoder,
            m.gpu,
            m.os,
            m.sizes[files::VIDEO],
            keys.iter().enumerate().filter(|(_, k)| **k).map(|(i, _)| i).collect::<Vec<_>>()
        );
        assert_eq!(codec, "hevc");
        assert_eq!((w, h), (640, 360));
        assert_eq!(n, frames.len() as u64, "decoded frames == frames.parquet rows");
        assert_eq!(m.frame_count as u64, n);
        assert!(keys[0], "segment starts on a keyframe");
        assert_eq!(m.encoder, "hevc_nvenc");
        if i < 2 {
            assert_eq!(n + m.dropped_frames, 40);
        }
    }
    if std::env::var_os("CAPREC_KEEP").is_none() {
        std::fs::remove_dir_all(&root).unwrap();
    }
}

/// Child half of `nvenc_crash_recovery`: records with hevc_nvenc until killed.
#[test]
#[ignore]
fn nvenc_crash_child() {
    let Ok(dir) = std::env::var("CAPREC_CRASH_DIR") else { return };
    let target = WindowTarget { native_id: 0, title: "synthetic".into(), game_id: "synthetic".into(), pid: 0 };
    let settings = CaptureSettings { width: 640, height: 360, rate_hz: 20, segment_secs: 60 };
    let mut cfg = RecorderConfig::new(target, settings, dir.into(), "crash".into());
    cfg.encoder.force_encoder = Some("hevc_nvenc".into());
    let sources = Sources {
        frames: Box::new(SyntheticSource::new(640, 360, 30)),
        inputs: vec![],
        focus: Box::new(ScriptedFocusTracker::focused()),
    };
    let _rec = Recorder::start(cfg, sources, Box::new(|_| {})).unwrap();
    std::thread::sleep(Duration::from_secs(60));
}

/// SIGKILL a recorder mid-segment, then recover: the recovered video must
/// decode to exactly `frame_count` frames.
#[test]
#[ignore]
fn nvenc_crash_recovery() {
    use cap_recorder::journal;
    let root = std::env::temp_dir().join(format!("caprec-nvenc-crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["nvenc_crash_child", "--exact", "--ignored"])
        .env("CAPREC_CRASH_DIR", &root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let partial = root.join("crash").join("seg_000000.partial");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let n = journal::read(&partial.join(journal::JOURNAL_FILE))
            .map(|e| e.iter().filter(|e| matches!(e, journal::Entry::Frame(_))).count())
            .unwrap_or(0);
        if n >= 70 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "child never journaled");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(230)); // land mid-GOP
    child.kill().unwrap();
    child.wait().unwrap();
    let before = std::fs::metadata(partial.join(files::VIDEO)).unwrap().len();
    let rep = cap_recorder::recover_partials(&root);
    assert!(rep.broken.is_empty(), "{:?}", rep.broken);
    let dir = &rep.finalized[0];
    let m: Manifest = serde_json::from_slice(&std::fs::read(dir.join(files::MANIFEST)).unwrap()).unwrap();
    let frames = tables::read_frames(&dir.join(files::FRAMES)).unwrap();
    let (codec, _, _, n, keys) = ffprobe(&dir.join(files::VIDEO));
    eprintln!(
        "recovered: video {} -> {} B, {codec} decoded {n}, frame_count {}, rows {}, dropped {}, keyframes {:?}",
        before,
        m.sizes[files::VIDEO],
        m.frame_count,
        frames.len(),
        m.dropped_frames,
        keys.iter().enumerate().filter(|(_, k)| **k).map(|(i, _)| i).collect::<Vec<_>>()
    );
    assert_eq!(n, m.frame_count as u64);
    assert_eq!(frames.len() as u64, n);
    assert!(n >= 40, "{n}");
    assert_eq!(n % 20, 0, "cut at a fragment (GOP) boundary");
    println!("RECOVERED_DIR={}", dir.display());
}
