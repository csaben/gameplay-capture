//! End-to-end: synthetic CPU frames -> Encoder -> SegmentMuxer -> ffprobe.
//!
//! NVENC tests skip (pass with a note) when no NVIDIA GPU / hevc_nvenc is
//! available. The libx265 tests need an FFmpeg built with libx265 and skip
//! otherwise. ffprobe/ffmpeg CLIs are needed for verification.

use cap_capture::synthetic::draw_frame;
use cap_capture::{CapturedFrame, FramePayload, PixelFormat};
use cap_encode::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cap-encode-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn have_tool(t: &str) -> bool {
    Command::new(t).arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn cpu_frame(w: u32, h: u32, n: u64) -> CapturedFrame {
    CapturedFrame {
        capture_ns: n as i64 * 50_000_000,
        width: w,
        height: h,
        format: PixelFormat::Bgra8,
        payload: FramePayload::Cpu { data: draw_frame(w, h, n), stride: (w * 4) as usize },
    }
}

fn open_or_skip(cfg: &EncoderConfig) -> Option<Encoder> {
    match Encoder::open(cfg) {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("SKIP: cannot open {:?}: {e}", cfg.force_encoder);
            None
        }
    }
}

struct Probe {
    codec: String,
    width: u32,
    height: u32,
    has_b_frames: u32,
    stream_line: String,
    /// (pts, key, pict_type) per decoded frame.
    frames: Vec<(i64, bool, String)>,
}

fn ffprobe(path: &Path) -> Probe {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name,width,height,has_b_frames,codec_tag_string,pix_fmt,color_space,color_range,profile"])
        .args(["-show_entries", "frame=pts,key_frame,pict_type", "-of", "compact=p=0:nk=0"])
        .arg(path)
        .output()
        .expect("ffprobe");
    assert!(out.status.success(), "ffprobe failed: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    let mut p = Probe { codec: String::new(), width: 0, height: 0, has_b_frames: 99, stream_line: String::new(), frames: vec![] };
    for line in text.lines() {
        let kv: Vec<(&str, &str)> = line.split('|').filter_map(|f| f.split_once('=')).collect();
        let get = |k: &str| kv.iter().find(|(a, _)| *a == k).map(|(_, v)| *v);
        if let Some(c) = get("codec_name") {
            p.stream_line = line.to_string();
            p.codec = c.to_string();
            p.width = get("width").unwrap().parse().unwrap();
            p.height = get("height").unwrap().parse().unwrap();
            p.has_b_frames = get("has_b_frames").unwrap().parse().unwrap();
        } else if let Some(k) = get("key_frame") {
            p.frames.push((get("pts").unwrap().parse().unwrap_or(-1), k == "1", get("pict_type").unwrap().to_string()));
        }
    }
    p
}

/// Decode the whole file; returns (success, decoded frame count, stderr).
fn decode(path: &Path) -> (bool, usize, String) {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-f", "framemd5", "-"])
        .output()
        .expect("ffmpeg");
    let n = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.starts_with('#')).count();
    (out.status.success(), n, String::from_utf8_lossy(&out.stderr).into_owned())
}

/// Encode `total` frames at 20 Hz into segments split at `boundaries`
/// (forcing a keyframe at each boundary). Returns segment paths + stats.
fn record(enc: &mut Encoder, dir: &Path, src_w: u32, src_h: u32, total: i64, boundaries: &[i64]) -> Vec<(PathBuf, i64)> {
    let params = enc.params().clone();
    let mut segs = Vec::new();
    let mut seg_start = 0i64;
    let mut path = dir.join("seg_000000.mp4");
    let mut mux = SegmentMuxer::create(&path, &params, 0).unwrap();
    let mut frames_in_seg = 0i64;
    for pts in 0..total {
        let boundary = boundaries.contains(&pts);
        if boundary {
            mux.finish().unwrap();
            segs.push((path.clone(), frames_in_seg));
            seg_start = pts;
            path = dir.join(format!("seg_{:06}.mp4", segs.len()));
            mux = SegmentMuxer::create(&path, &params, seg_start).unwrap();
            frames_in_seg = 0;
        }
        let f = cpu_frame(src_w, src_h, pts as u64);
        let pkts = enc.encode(&f, pts, boundary).unwrap();
        for p in &pkts {
            if p.pts == seg_start {
                assert!(p.keyframe, "first packet of segment must be a keyframe");
            }
            mux.write(p).unwrap();
            frames_in_seg += 1;
        }
    }
    for p in enc.flush().unwrap() {
        mux.write(&p).unwrap();
        frames_in_seg += 1;
    }
    mux.finish().unwrap();
    segs.push((path, frames_in_seg));
    segs
}

fn verify_segments(segs: &[(PathBuf, i64)], codec: &str, gop: i64, expect_frames: &[i64]) {
    assert_eq!(segs.len(), expect_frames.len());
    for ((path, written), want) in segs.iter().zip(expect_frames) {
        assert_eq!(written, want, "{path:?} packets written");
        let p = ffprobe(path);
        assert_eq!(p.codec, codec);
        eprintln!("stream: {}", p.stream_line);
        assert!(p.stream_line.contains("pix_fmt=yuv420p"), "{}", p.stream_line);
        assert_eq!((p.width, p.height), (640, 360));
        assert_eq!(p.has_b_frames, 0, "no B-frame reordering");
        assert_eq!(p.frames.len() as i64, *want, "{path:?} frame count");
        assert!(p.frames[0].1, "{path:?} first frame keyframe");
        assert_eq!(p.frames[0].0, 0, "{path:?} starts at pts 0");
        for (i, (_, key, pict)) in p.frames.iter().enumerate() {
            assert_ne!(pict, "B", "B-frame at {i}");
            assert_eq!(*key, i as i64 % gop == 0, "{path:?} frame {i}: key={key}");
        }
        let (ok, n, err) = decode(path);
        assert!(ok && n as i64 == *want, "decode {path:?}: ok={ok} n={n} {err}");
        eprintln!("verified {path:?}: {want} frames, {} bytes", std::fs::metadata(path).unwrap().len());
    }
}

/// Truncate a copy at `frac` of its size; it must still decode at least one GOP.
fn verify_truncated(path: &Path, frac: f64, min_frames: usize) {
    let data = std::fs::read(path).unwrap();
    let cut = (data.len() as f64 * frac) as usize;
    let t = path.with_extension("trunc.mp4");
    std::fs::write(&t, &data[..cut]).unwrap();
    let (ok, n, err) = decode(&t);
    eprintln!("truncated {path:?} to {cut}/{} bytes: ffmpeg ok={ok}, decoded {n} frames, stderr: {}", data.len(), err.trim());
    assert!(n >= min_frames, "truncated file decoded only {n} frames");
    let out = Command::new("ffprobe").args(["-v", "error", "-count_frames", "-show_entries", "stream=nb_read_frames"]).arg(&t).output().unwrap();
    eprintln!("ffprobe truncated: {}", String::from_utf8_lossy(&out.stdout).trim());
}

fn nvenc_cfg() -> EncoderConfig {
    let mut cfg = EncoderConfig::new(640, 360, 20);
    cfg.force_encoder = Some("hevc_nvenc".into());
    cfg
}

#[test]
fn nvenc_two_segments_ffprobe() {
    if !have_tool("ffprobe") {
        eprintln!("SKIP: ffprobe not installed");
        return;
    }
    let Some(mut enc) = open_or_skip(&nvenc_cfg()) else { return };
    eprintln!("params: {:#?}", enc.params().params);
    let dir = tmpdir("nvenc2seg");
    // 3 s at 20 Hz; segment boundary at 2 s (a GOP boundary, as in production
    // where 60 s segments are a multiple of the 1 s GOP).
    let segs = record(&mut enc, &dir, 1280, 720, 60, &[40]);
    // Windows NVENC runs on a D3D11 device: CPU frames take swscale + upload (GPU
    // scaling there is the D3D11 VP, which needs a texture input).
    let want = if cfg!(windows) { "cpu:swscale+hwupload" } else { "gpu:hwupload+scale_cuda" };
    assert_eq!(enc.last_frame_path(), Some(want), "expected scale path used");
    verify_segments(&segs, "hevc", 20, &[40, 20]);
    verify_truncated(&segs[0].0, 0.7, 20);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nvenc_forced_keyframe_mid_gop() {
    if !have_tool("ffprobe") {
        return;
    }
    let Some(mut enc) = open_or_skip(&nvenc_cfg()) else { return };
    let dir = tmpdir("nvencforce");
    // Boundary at 30 is mid-GOP: frame 30 must still be an IDR.
    let segs = record(&mut enc, &dir, 1280, 720, 50, &[30]);
    let p = ffprobe(&segs[1].0);
    assert!(p.frames[0].1, "forced keyframe at segment start");
    assert_eq!(p.frames[0].2, "I");
    let keys: Vec<usize> = ffprobe(&segs[0].0).frames.iter().enumerate().filter(|f| f.1 .1).map(|f| f.0).collect();
    assert_eq!(keys, vec![0, 20]);
    let (ok, n, _) = decode(&segs[1].0);
    assert!(ok && n == 20);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nvenc_throughput() {
    let Some(mut enc) = open_or_skip(&nvenc_cfg()) else { return };
    // Rgba8 takes the CPU fallback (swscale + upload) for comparison.
    for &(w, h, fmt) in &[
        (1280u32, 720u32, PixelFormat::Bgra8),
        (1920, 1080, PixelFormat::Bgra8),
        (2560, 1440, PixelFormat::Bgra8),
        (1920, 1080, PixelFormat::Rgba8),
    ] {
        // Pre-render a few frames so timing measures the encoder, not draw_frame.
        let frames: Vec<CapturedFrame> = (0..8)
            .map(|n| CapturedFrame { format: fmt, ..cpu_frame(w, h, n) })
            .collect();
        let n = 200i64;
        for (i, f) in frames.iter().enumerate().take(4) {
            enc.encode(f, 1_000_000 * w as i64 + 1000 * fmt as i64 - 10 + i as i64, i == 0).unwrap(); // warm-up (graph rebuild on size change)
        }
        let mut bytes = 0usize;
        let t = Instant::now();
        for i in 0..n {
            let f = &frames[(i % 8) as usize];
            for p in enc.encode(f, 1_000_000 * w as i64 + 1000 * fmt as i64 + i, false).unwrap() {
                bytes += p.data.len();
            }
        }
        let dt = t.elapsed().as_secs_f64();
        eprintln!(
            "THROUGHPUT hevc_nvenc {w}x{h} {fmt:?} cpu -> 640x360 via {:?}: {:.0} fps ({:.2} ms/frame), {:.1} KB/frame",
            enc.last_frame_path(),
            n as f64 / dt,
            dt * 1000.0 / n as f64,
            bytes as f64 / n as f64 / 1024.0
        );
    }
}

#[test]
fn libx265_software_when_forced() {
    if !have_tool("ffprobe") {
        return;
    }
    let mut cfg = EncoderConfig::new(640, 360, 20);
    cfg.force_encoder = Some("libx265".into());
    let Some(mut enc) = open_or_skip(&cfg) else { return };
    assert_eq!(enc.params().encoder_name, "libx265");
    let dir = tmpdir("x265");
    let segs = record(&mut enc, &dir, 1280, 720, 60, &[40]);
    assert_eq!(enc.last_frame_path(), Some("cpu:swscale"));
    verify_segments(&segs, "hevc", 20, &[40, 20]);
    verify_truncated(&segs[0].0, 0.7, 20);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_forced_encoder_refuses() {
    let mut cfg = EncoderConfig::new(640, 360, 20);
    cfg.force_encoder = Some("hevc_doesnotexist_nvenc".into());
    match Encoder::open(&cfg) {
        Err(EncodeError::NoHardwareEncoder(msg)) => eprintln!("refused as expected: {msg}"),
        Err(e) => panic!("wrong error {e}"),
        Ok(_) => panic!("opened a nonexistent encoder"),
    }
}

#[test]
fn av1_probe_reports_all_candidates() {
    // RTX 3090 (Ampere) has no AV1 encoder; on RTX 40+ this opens av1_nvenc.
    let mut cfg = EncoderConfig::new(640, 360, 20);
    cfg.codec = Codec::Av1;
    match Encoder::open(&cfg) {
        Ok(e) => eprintln!("AV1 available: {}", e.params().encoder_name),
        Err(EncodeError::NoHardwareEncoder(msg)) => {
            eprintln!("no AV1 hw encoder: {msg}");
            assert!(msg.contains("av1_nvenc"));
        }
        Err(e) => panic!("wrong error {e}"),
    }
}

#[test]
fn default_probe_picks_hardware() {
    // Without force_encoder only hardware encoders are considered.
    match Encoder::open(&EncoderConfig::new(640, 360, 20)) {
        Ok(e) => {
            eprintln!("probe picked {}", e.params().encoder_name);
            assert!(probe::is_hardware_name(&e.params().encoder_name));
        }
        Err(EncodeError::NoHardwareEncoder(msg)) => eprintln!("SKIP: no hw encoder: {msg}"),
        Err(e) => panic!("wrong error {e}"),
    }
}

#[test]
fn cpu_fallback_rgba_and_hdr_into_nvenc() {
    // Rgba8 cannot take the CUDA graph (encoder frames are BGR0) and Rgba16f
    // cannot be uploaded at all: both must use swscale + hwupload.
    if !have_tool("ffprobe") {
        return;
    }
    let Some(mut enc) = open_or_skip(&nvenc_cfg()) else { return };
    let dir = tmpdir("fallback");
    let path = dir.join("v.mp4");
    let mut mux = SegmentMuxer::create(&path, enc.params(), 0).unwrap();
    let (w, h) = (1280u32, 720u32);
    for pts in 0..40i64 {
        let f = if pts < 20 {
            let mut data = draw_frame(w, h, pts as u64);
            for px in data.chunks_mut(4) {
                px.swap(0, 2);
            }
            CapturedFrame { capture_ns: 0, width: w, height: h, format: PixelFormat::Rgba8, payload: FramePayload::Cpu { data, stride: w as usize * 4 } }
        } else {
            // Half-float RGBA; values up to 2.0 exercise the clamp "tone-map".
            let mut data = Vec::with_capacity((w * h * 8) as usize);
            for y in 0..h {
                for x in 0..w {
                    let r = half_bits(x as f32 / w as f32 * 2.0);
                    let g = half_bits(y as f32 / h as f32);
                    let b = half_bits(pts as f32 / 40.0);
                    for c in [r, g, b, half_bits(1.0)] {
                        data.extend_from_slice(&c.to_le_bytes());
                    }
                }
            }
            CapturedFrame { capture_ns: 0, width: w, height: h, format: PixelFormat::Rgba16f, payload: FramePayload::Cpu { data, stride: w as usize * 8 } }
        };
        for p in enc.encode(&f, pts, false).unwrap() {
            mux.write(&p).unwrap();
        }
        assert_eq!(enc.last_frame_path(), Some("cpu:swscale+hwupload"), "frame {pts}");
    }
    mux.finish().unwrap();
    let p = ffprobe(&path);
    assert_eq!(p.frames.len(), 40);
    let _ = std::fs::remove_dir_all(&dir);
}

/// f32 -> IEEE half bits (normal range only; enough for test data).
fn half_bits(v: f32) -> u16 {
    if v == 0.0 {
        return 0;
    }
    let b = v.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32 - 127 + 15;
    let man = ((b >> 13) & 0x3ff) as u16;
    if exp <= 0 {
        return sign;
    }
    sign | ((exp as u16) << 10) | man
}

#[test]
fn nvenc_lossless_opens_and_encodes() {
    let mut cfg = nvenc_cfg();
    cfg.lossless = true;
    let Some(mut enc) = open_or_skip(&cfg) else { return };
    assert_eq!(enc.params().params.get("opt.tune").map(String::as_str), Some("lossless"));
    let mut n = 0;
    for pts in 0..5 {
        n += enc.encode(&cpu_frame(1280, 720, pts as u64), pts, pts == 0).unwrap().len();
    }
    n += enc.flush().unwrap().len();
    assert_eq!(n, 5);
}

#[test]
fn nvenc_flush_then_continue() {
    let Some(mut enc) = open_or_skip(&nvenc_cfg()) else { return };
    let extradata = enc.params().extradata.clone();
    let mut pk = Vec::new();
    for pts in 0..3 {
        pk.extend(enc.encode(&cpu_frame(1280, 720, pts as u64), pts, pts == 0).unwrap());
    }
    pk.extend(enc.flush().unwrap());
    // Encoding continues after a flush (encoder is reopened, same parameters).
    let p = enc.encode(&cpu_frame(1280, 720, 3), 3, true).unwrap();
    assert!(p.iter().all(|p| p.keyframe));
    pk.extend(p);
    pk.extend(enc.flush().unwrap());
    assert_eq!(pk.len(), 4);
    assert_eq!(enc.params().extradata, extradata);
}
