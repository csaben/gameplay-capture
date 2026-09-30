//! End-to-end X11 capture against a private Xvfb server.
//!
//! Uses `$CAP_XVFB` (path to an Xvfb binary) or `Xvfb` from `$PATH`; the tests
//! are skipped (pass with a note) when neither exists.
#![cfg(target_os = "linux")]

use cap_capture::linux_x11::{X11Config, X11Source};
use cap_capture::*;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::COPY_DEPTH_FROM_PARENT;

struct Xvfb {
    child: Child,
    display: String,
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_xvfb() -> Option<Xvfb> {
    let bin = std::env::var("CAP_XVFB").ok().or_else(|| {
        std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p).map(|d| d.join("Xvfb")).find(|p| p.is_file()).map(|p| p.display().to_string())
        })
    })?;
    // -displayfd 1: Xvfb picks a free display number and prints it on stdout when ready.
    let mut child = Command::new(bin)
        .args(["-displayfd", "1", "-screen", "0", "640x480x24", "-nolisten", "tcp", "+extension", "Composite"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).ok()?;
    let n: u32 = line.trim().parse().ok()?;
    Some(Xvfb { child, display: format!(":{n}") })
}

/// Create and map a window filled with `rgb` (0xRRGGBB).
fn make_window(conn: &RustConnection, screen: usize, x: i16, y: i16, w: u16, h: u16, rgb: u32) -> Window {
    let root = conn.setup().roots[screen].root;
    let win = conn.generate_id().unwrap();
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        win,
        root,
        x,
        y,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().background_pixel(rgb).override_redirect(1),
    )
    .unwrap();
    conn.map_window(win).unwrap();
    conn.sync().unwrap();
    win
}

fn target(win: Window) -> WindowTarget {
    WindowTarget { native_id: win as u64, title: "test".into(), game_id: "test".into(), pid: 0 }
}

/// Wait for a frame newer than `after_seq` that satisfies `pred`.
fn wait_frame(sink: &LatestFrame, pred: impl Fn(&CapturedFrame) -> bool) -> Arc<CapturedFrame> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let (_, Some(f)) = sink.latest() {
            if pred(&f) {
                return f;
            }
        }
        assert!(Instant::now() < deadline, "no matching frame within 5 s; latest: {:?}", sink.latest().1);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn pixel(f: &CapturedFrame, x: u32, y: u32) -> [u8; 4] {
    match &f.payload {
        FramePayload::Cpu { data, stride } => {
            let i = y as usize * stride + x as usize * 4;
            [data[i], data[i + 1], data[i + 2], data[i + 3]]
        }
        other => panic!("expected CPU payload, got {other:?}"),
    }
}

fn run_capture_checks(cfg_mut: impl Fn(&mut X11Config)) {
    let Some(x) = start_xvfb() else {
        eprintln!("SKIP: Xvfb not found (set CAP_XVFB=/path/to/Xvfb)");
        return;
    };
    let (conn, screen) = RustConnection::connect(Some(&x.display)).unwrap();
    // Target: orange-ish 0xFF8040 at 200x120, partly covered by a blue window
    // and hanging off the right edge of the 640-wide screen.
    let win = make_window(&conn, screen, 500, 50, 200, 120, 0xFF8040);
    let _cover = make_window(&conn, screen, 520, 60, 50, 50, 0x0000FF);

    let mut cfg = X11Config { display: Some(x.display.clone()), poll_hz: 100, ..Default::default() };
    cfg_mut(&mut cfg);
    let composite = cfg.use_composite;
    let mut src = X11Source::new(cfg);
    let sink = LatestFrame::new();
    let before = cap_clock::now_ns();
    src.start(&target(win), sink.clone()).unwrap();
    assert_eq!(src.info().unwrap().backend, "x11");

    let f = wait_frame(&sink, |f| f.width == 200 && f.height == 120);
    assert_eq!(f.format, PixelFormat::Bgra8);
    assert!(f.capture_ns >= before && f.capture_ns <= cap_clock::now_ns(), "capture_ns on cap_clock base");
    if composite {
        // Uncovered, covered-by-another-window and off-screen pixels all show the target's colour.
        for (px, py) in [(0, 0), (199, 119), (30, 20), (190, 60)] {
            assert_eq!(pixel(&f, px, py), [0x40, 0x80, 0xFF, 0xFF], "pixel ({px},{py})");
        }
    }

    // Resize: new frames follow the new geometry.
    conn.configure_window(win, &ConfigureWindowAux::new().x(10).width(320).height(200)).unwrap();
    conn.sync().unwrap();
    let f = wait_frame(&sink, |f| f.width == 320 && f.height == 200);
    if composite {
        assert_eq!(pixel(&f, 319, 199), [0x40, 0x80, 0xFF, 0xFF]);
    }
    assert_eq!(src.info().unwrap().width, 320);

    // Destroying the window ends the grab thread with an error, not a hang.
    conn.destroy_window(win).unwrap();
    conn.sync().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while src.is_running() {
        assert!(Instant::now() < deadline, "grab thread did not exit after DestroyNotify");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(src.last_error().is_some());
    src.stop();
}

#[test]
fn x11_composite_shm() {
    run_capture_checks(|_| {});
}

#[test]
fn x11_composite_no_shm() {
    run_capture_checks(|c| c.use_shm = false);
}

#[test]
fn x11_no_composite_visible_window() {
    let Some(x) = start_xvfb() else {
        eprintln!("SKIP: Xvfb not found");
        return;
    };
    let (conn, screen) = RustConnection::connect(Some(&x.display)).unwrap();
    let win = make_window(&conn, screen, 10, 10, 64, 48, 0x10A020);
    let mut src = X11Source::new(X11Config { display: Some(x.display.clone()), use_composite: false, ..Default::default() });
    let sink = LatestFrame::new();
    src.start(&target(win), sink.clone()).unwrap();
    let f = wait_frame(&sink, |f| f.width == 64);
    assert_eq!(pixel(&f, 5, 5), [0x20, 0xA0, 0x10, 0xFF]);
    src.stop();
}

#[test]
fn x11_missing_window_is_reported() {
    let Some(x) = start_xvfb() else {
        eprintln!("SKIP: Xvfb not found");
        return;
    };
    let mut src = X11Source::new(X11Config { display: Some(x.display.clone()), ..Default::default() });
    let err = src.start(&target(0x7fff_fff0), LatestFrame::new()).unwrap_err();
    assert!(matches!(err, CaptureError::WindowNotFound(_)), "{err:?}");
}
