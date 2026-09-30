//! PipeWire stream half against a GStreamer `videotestsrc ! pipewiresink`
//! video source on the user's PipeWire daemon (the portal is bypassed with
//! `PipeWireSource::start_node`). Skipped when gst-launch-1.0, the pipewire
//! GStreamer plugin, pw-cli or a PipeWire daemon is missing.
#![cfg(all(target_os = "linux", feature = "pipewire"))]

use cap_capture::linux_pipewire::{PipeWireConfig, PipeWireSource};
use cap_capture::*;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Gst(Child);
impl Drop for Gst {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn find_node(name: &str) -> Option<u32> {
    let out = Command::new("pw-cli").args(["ls", "Node"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut current = None;
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("id ") {
            current = rest.split(',').next().and_then(|n| n.trim().parse().ok());
        } else if l.contains("node.name") && l.contains(name) {
            return current;
        }
    }
    None
}

#[test]
fn pipewire_shm_stream_from_videotestsrc() {
    let ok = Command::new("gst-inspect-1.0").arg("pipewiresink").stdout(Stdio::null()).stderr(Stdio::null()).status();
    if !ok.is_ok_and(|s| s.success()) {
        eprintln!("SKIP: gst-launch-1.0 / pipewiresink not available");
        return;
    }
    let name = format!("cap-capture-test-{}", std::process::id());
    let gst = Gst(
        Command::new("gst-launch-1.0")
            .args([
                "-q",
                "videotestsrc",
                "pattern=solid-color",
                "foreground-color=0xFFFF8040",
                "is-live=true",
                "!",
                "video/x-raw,format=BGRx,width=320,height=240,framerate=30/1",
                "!",
                "pipewiresink",
                "mode=provide",
                &format!("stream-properties=props,media.class=Video/Source,node.name={name}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn gst-launch-1.0"),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let node = loop {
        if let Some(n) = find_node(&name) {
            break n;
        }
        if Instant::now() > deadline {
            eprintln!("SKIP: test node never appeared (no PipeWire daemon?)");
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let mut src = PipeWireSource::new(PipeWireConfig::default());
    let sink = LatestFrame::new();
    let t0 = cap_clock::now_ns();
    src.start_node(None, node, sink.clone()).expect("start_node");

    let deadline = Instant::now() + Duration::from_secs(10);
    let frame = loop {
        if let (_, Some(f)) = sink.latest() {
            break f;
        }
        assert!(Instant::now() < deadline, "no frame; last_error={:?}", src.last_error());
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!((frame.width, frame.height), (320, 240));
    assert_eq!(frame.format, PixelFormat::Bgra8);
    assert!(frame.capture_ns >= t0 - 1_000_000_000 && frame.capture_ns <= cap_clock::now_ns());
    match &frame.payload {
        FramePayload::Cpu { data, stride } => {
            assert_eq!(*stride, 320 * 4);
            for (x, y) in [(0usize, 0usize), (319, 239), (160, 120)] {
                let i = y * stride + x * 4;
                assert_eq!(&data[i..i + 4], &[0x40, 0x80, 0xFF, 0xFF], "pixel ({x},{y})");
            }
        }
        other => panic!("expected SHM frame from videotestsrc, got {other:?}"),
    }
    assert_eq!(src.info().unwrap().width, 320);

    // Frames keep flowing.
    let n0 = src.frames_published();
    std::thread::sleep(Duration::from_millis(300));
    assert!(src.frames_published() > n0, "stream stalled");

    src.stop();
    assert!(!src.is_running());
    drop(gst);
}
