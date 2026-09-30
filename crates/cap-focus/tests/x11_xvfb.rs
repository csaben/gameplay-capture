//! X11 focus tracking against a real (virtual) X server.
//!
//! Needs an X server without a window manager, e.g.
//! `Xvfb :99 -screen 0 640x480x24 & CAP_FOCUS_XVFB=:99 cargo test -p cap-focus --test x11_xvfb -- --ignored`
//! Exercises both the no-WM `GetInputFocus` fallback and the EWMH
//! `_NET_ACTIVE_WINDOW` path (the test plays the WM by setting the property).
#![cfg(target_os = "linux")]

use cap_focus::{FocusTracker, X11Tracker};
use cap_types::FocusRecord;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

fn wait_for(
    rx: &crossbeam_channel::Receiver<FocusRecord>,
    pred: impl Fn(&FocusRecord) -> bool,
) -> FocusRecord {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let r = rx
            .recv_timeout(left)
            .expect("timed out waiting for focus record");
        if pred(&r) {
            return r;
        }
    }
}

#[test]
#[ignore]
fn xvfb_two_windows() {
    let Ok(display) = std::env::var("CAP_FOCUS_XVFB") else {
        eprintln!("CAP_FOCUS_XVFB not set; skipping");
        return;
    };
    let me = cap_focus::exe_basename(&std::env::current_exe().unwrap().to_string_lossy());
    let mut other_proc = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();

    let (conn, screen) = x11rb::connect(Some(&display)).unwrap();
    let root = conn.setup().roots[screen].root;
    let atom = |n: &[u8]| conn.intern_atom(false, n).unwrap().reply().unwrap().atom;
    let net_wm_pid = atom(b"_NET_WM_PID");
    let net_active = atom(b"_NET_ACTIVE_WINDOW");
    let mk = |title: &str, pid: Option<u32>| {
        let w = conn.generate_id().unwrap();
        conn.create_window(
            0,
            w,
            root,
            0,
            0,
            100,
            100,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new(),
        )
        .unwrap();
        conn.change_property8(
            PropMode::REPLACE,
            w,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        )
        .unwrap();
        if let Some(pid) = pid {
            conn.change_property32(PropMode::REPLACE, w, net_wm_pid, AtomEnum::CARDINAL, &[pid])
                .unwrap();
        }
        conn.map_window(w).unwrap();
        w
    };
    let a = mk("Game A", Some(std::process::id()));
    let b = mk("Other B", Some(other_proc.id()));
    let c = mk("NoPid C", None); // identity must come from the X-Resource extension
    conn.sync().unwrap();

    let mut tracker = X11Tracker::with_display(Some(display.clone()));
    let wins = tracker.list_windows().unwrap();
    println!("list_windows: {wins:#?}");
    let wa = wins
        .iter()
        .find(|w| w.native_id == a as u64)
        .expect("window A listed");
    let wb = wins
        .iter()
        .find(|w| w.native_id == b as u64)
        .expect("window B listed");
    assert_eq!(wa.identity.game_id, me);
    assert_eq!(wa.title, "Game A");
    assert_eq!(wb.identity.game_id, "sleep");
    assert_eq!(wb.pid, other_proc.id());
    let wc = wins
        .iter()
        .find(|w| w.native_id == c as u64)
        .expect("window C listed");
    assert_eq!(
        (wc.pid, wc.identity.game_id.as_str()),
        (std::process::id(), me.as_str()),
        "XRes fallback"
    );

    // --- no WM: GetInputFocus fallback ---
    let (tx, rx) = crossbeam_channel::bounded(64);
    tracker.start(&me, tx).unwrap();
    let init = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("initial record");
    println!("initial: {init:?}");
    assert!(!init.focused);

    conn.set_input_focus(InputFocus::PARENT, a, x11rb::CURRENT_TIME)
        .unwrap();
    conn.sync().unwrap();
    let r = wait_for(&rx, |r| r.focused);
    println!("focus A (GetInputFocus): {r:?}");
    assert_eq!(r.game_id, me);

    conn.set_input_focus(InputFocus::PARENT, b, x11rb::CURRENT_TIME)
        .unwrap();
    conn.sync().unwrap();
    let r = wait_for(&rx, |r| !r.focused);
    println!("focus B (GetInputFocus): {r:?}");
    assert_eq!(r.game_id, "sleep");

    // --- EWMH: act as the WM and publish _NET_ACTIVE_WINDOW ---
    let t0 = cap_clock::now_ns();
    conn.change_property32(PropMode::REPLACE, root, net_active, AtomEnum::WINDOW, &[a])
        .unwrap();
    conn.sync().unwrap();
    let r = wait_for(&rx, |r| r.focused);
    println!(
        "focus A (_NET_ACTIVE_WINDOW): {r:?} latency {} us",
        (r.t_ns - t0) / 1000
    );
    assert!(
        r.t_ns - t0 < 200_000_000,
        "PropertyNotify path should be fast"
    );

    conn.change_property32(PropMode::REPLACE, root, net_active, AtomEnum::WINDOW, &[0])
        .unwrap();
    conn.sync().unwrap();
    let r = wait_for(&rx, |r| !r.focused);
    println!("no active window: {r:?}");
    assert_eq!(r.game_id, "");

    tracker.stop();
    let _ = other_proc.kill();
    let _ = other_proc.wait();
}
