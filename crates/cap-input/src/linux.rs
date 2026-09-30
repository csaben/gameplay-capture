//! Linux keyboard + mouse via evdev (`/dev/input/event*`).
//!
//! * Read-only and non-grabbing: `EVIOCGRAB` is never issued, so the game and
//!   desktop receive input exactly as before.
//! * `EVIOCSCLOCKID(CLOCK_MONOTONIC)` is set on every device, so the kernel's
//!   per-event `timeval` is on the same clock as `cap_clock::now_ns()` and is
//!   used as `t_ns` directly (sub-millisecond accurate, independent of when
//!   this thread wakes up).
//! * Keys: evdev `KEY_*` codes are translated to set-1 scan codes via
//!   [`crate::scancode::evdev_to_code`]; autorepeat (`value == 2`) is dropped.
//! * Mouse: `REL_X`/`REL_Y` raw counts; `REL_WHEEL_HI_RES`/`REL_HWHEEL_HI_RES`
//!   (1/120 detent units) when the device has them, else `REL_WHEEL`/
//!   `REL_HWHEEL` in detents; `BTN_LEFT/RIGHT/MIDDLE/SIDE/EXTRA/BACK/FORWARD`.
//! * Joysticks/gamepads are skipped here (gilrs handles them).
//! * Hotplug: `/dev/input` is rescanned every [`RESCAN_SECS`] seconds; devices
//!   that vanish (ENODEV) are dropped.
//! * Requires read access to `/dev/input/event*`: add the user to the `input`
//!   group (`sudo usermod -aG input $USER`, then log out and back in).

use crate::scancode;
use crate::{EventSink, InputError, InputSource, Result};
use cap_types::{mouse_axis, mouse_button, wheel_axis, Device, EventKind, InputEvent, Nanos};
use crossbeam_channel::Sender;
use evdev::raw_stream::RawDevice;
use evdev::{KeyCode, RelativeAxisCode};
use std::collections::{HashMap, HashSet};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

pub const RESCAN_SECS: i64 = 3;
const INPUT_DIR: &str = "/dev/input";

/// `_IOW('E', 0xa0, int)`
const EVIOCSCLOCKID: libc::c_ulong = 0x4004_45a0;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const SYN_DROPPED: u16 = 3;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;
const BTN_MISC: u16 = 0x100;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
const BTN_FORWARD: u16 = 0x115;
const BTN_BACK: u16 = 0x116;

/// evdev `BTN_*` -> `cap_types::mouse_button`.
pub fn mouse_button_code(code: u16) -> Option<u32> {
    Some(match code {
        BTN_LEFT => mouse_button::LEFT,
        BTN_RIGHT => mouse_button::RIGHT,
        BTN_MIDDLE => mouse_button::MIDDLE,
        BTN_SIDE | BTN_BACK => mouse_button::BACK,
        BTN_EXTRA | BTN_FORWARD => mouse_button::FORWARD,
        _ => return None,
    })
}

/// Kernel `timeval` (CLOCK_MONOTONIC after EVIOCSCLOCKID) -> ns.
pub fn timeval_to_ns(sec: i64, usec: i64) -> Nanos {
    sec * 1_000_000_000 + usec * 1_000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Class {
    keyboard: bool,
    mouse: bool,
    hires_wheel: bool,
    hires_hwheel: bool,
}

fn classify(dev: &RawDevice) -> Class {
    let keys = dev.supported_keys();
    let has_key = |k: KeyCode| keys.map(|s| s.contains(k)).unwrap_or(false);
    let rel = dev.supported_relative_axes();
    let has_rel = |r: RelativeAxisCode| rel.map(|s| s.contains(r)).unwrap_or(false);
    let is_joystick = has_key(KeyCode::BTN_SOUTH) || has_key(KeyCode::BTN_TRIGGER);
    let keyboard = !is_joystick
        && has_key(KeyCode::KEY_A)
        && has_key(KeyCode::KEY_Z)
        && has_key(KeyCode::KEY_SPACE)
        && has_key(KeyCode::KEY_ENTER);
    let mouse = !is_joystick
        && has_rel(RelativeAxisCode::REL_X)
        && has_rel(RelativeAxisCode::REL_Y)
        && has_key(KeyCode::BTN_LEFT);
    Class {
        keyboard,
        mouse,
        hires_wheel: has_rel(RelativeAxisCode::REL_WHEEL_HI_RES),
        hires_hwheel: has_rel(RelativeAxisCode::REL_HWHEEL_HI_RES),
    }
}

struct OpenDevice {
    dev: RawDevice,
    class: Class,
    name: String,
}

/// Summary of one enumerated device (for diagnostics / smoke tests).
#[derive(Debug, Clone)]
pub struct DeviceSummary {
    pub path: PathBuf,
    pub name: String,
    pub keyboard: bool,
    pub mouse: bool,
    pub monotonic_clock: bool,
}

fn event_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(INPUT_DIR)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("event"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn set_monotonic(dev: &RawDevice) -> bool {
    let clk: libc::c_int = libc::CLOCK_MONOTONIC;
    // SAFETY: valid fd and pointer to an int, as EVIOCSCLOCKID expects.
    unsafe { libc::ioctl(dev.as_raw_fd(), EVIOCSCLOCKID as _, &clk) == 0 }
}

fn set_nonblocking(dev: &RawDevice) -> bool {
    let fd = dev.as_raw_fd();
    // SAFETY: fcntl on an fd we own.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        fl >= 0 && libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) == 0
    }
}

#[allow(clippy::large_enum_variant)]
enum OpenOutcome {
    Opened(OpenDevice),
    Ignored,
    Denied,
    Failed(std::io::Error),
}

fn open_device(path: &Path) -> OpenOutcome {
    let dev = match RawDevice::open(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return OpenOutcome::Denied,
        Err(e) => return OpenOutcome::Failed(e),
    };
    let class = classify(&dev);
    if !class.keyboard && !class.mouse {
        return OpenOutcome::Ignored;
    }
    if !set_monotonic(&dev) {
        // Should never fail on kernels >= 3.4; without it the timestamps
        // would be CLOCK_REALTIME, so refuse the device rather than mislabel.
        return OpenOutcome::Failed(std::io::Error::last_os_error());
    }
    if !set_nonblocking(&dev) {
        return OpenOutcome::Failed(std::io::Error::last_os_error());
    }
    let name = dev.name().unwrap_or("?").to_string();
    OpenOutcome::Opened(OpenDevice { dev, class, name })
}

/// Enumerate keyboards/mice without reading any events (diagnostics).
pub fn enumerate() -> Vec<DeviceSummary> {
    event_paths()
        .into_iter()
        .filter_map(|p| match open_device(&p) {
            OpenOutcome::Opened(d) => Some(DeviceSummary {
                path: p,
                name: d.name,
                keyboard: d.class.keyboard,
                mouse: d.class.mouse,
                monotonic_clock: true,
            }),
            _ => None,
        })
        .collect()
}

struct Scanner {
    devices: HashMap<PathBuf, OpenDevice>,
    /// Paths already classified as irrelevant (not re-opened every rescan).
    ignored: HashSet<PathBuf>,
}

struct ScanResult {
    denied: usize,
}

impl Scanner {
    fn rescan(&mut self) -> ScanResult {
        let mut denied = 0;
        let present: HashSet<PathBuf> = event_paths().into_iter().collect();
        self.devices.retain(|p, _| present.contains(p));
        self.ignored.retain(|p| present.contains(p));
        for p in present {
            if self.devices.contains_key(&p) || self.ignored.contains(&p) {
                continue;
            }
            match open_device(&p) {
                OpenOutcome::Opened(d) => {
                    tracing::info!(path = %p.display(), name = %d.name, kb = d.class.keyboard, mouse = d.class.mouse, "evdev device opened");
                    self.devices.insert(p, d);
                }
                OpenOutcome::Ignored => {
                    self.ignored.insert(p);
                }
                OpenOutcome::Denied => denied += 1,
                OpenOutcome::Failed(e) => {
                    tracing::debug!(path = %p.display(), "evdev open failed: {e}")
                }
            }
        }
        ScanResult { denied }
    }
}

fn permission_message(denied: usize) -> String {
    format!(
        "cannot open any keyboard/mouse under {INPUT_DIR} ({denied} device(s) denied). \
         Add your user to the `input` group: `sudo usermod -aG input $USER`, then log out and back in \
         (check with `groups`). Input devices are only read, never grabbed."
    )
}

pub struct EvdevSource {
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl EvdevSource {
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
            thread: None,
        }
    }

    /// Keyboards/mice that could be opened right now.
    pub fn enumerate() -> Vec<DeviceSummary> {
        enumerate()
    }
}

impl Default for EvdevSource {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSource for EvdevSource {
    fn start(&mut self, tx: Sender<InputEvent>) -> Result<()> {
        if self.thread.is_some() {
            return Ok(());
        }
        let mut scanner = Scanner {
            devices: HashMap::new(),
            ignored: HashSet::new(),
        };
        let r = scanner.rescan();
        if scanner.devices.is_empty() {
            if r.denied > 0 {
                return Err(InputError::PermissionDenied(permission_message(r.denied)));
            }
            return Err(InputError::Unsupported(format!(
                "no keyboard or mouse found under {INPUT_DIR}"
            )));
        }
        self.stop.store(false, Ordering::SeqCst);
        let sink = EventSink::new(tx, self.dropped.clone());
        let stop = self.stop.clone();
        let h = std::thread::Builder::new()
            .name("cap-input-evdev".into())
            .spawn(move || read_loop(scanner, sink, stop))
            .map_err(|e| InputError::Backend(format!("spawn evdev thread: {e}")))?;
        self.thread = Some(h);
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }

    fn name(&self) -> &'static str {
        "evdev"
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for EvdevSource {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Converts raw evdev events of one device into dataset events.
#[derive(Default)]
pub(crate) struct Translator {
    pub hires_wheel: bool,
    pub hires_hwheel: bool,
}

impl Translator {
    pub(crate) fn translate(
        &self,
        t_ns: Nanos,
        ty: u16,
        code: u16,
        value: i32,
    ) -> Option<InputEvent> {
        let ev = |device, kind, code, value| {
            Some(InputEvent {
                t_ns,
                device,
                kind,
                code,
                value,
            })
        };
        match ty {
            EV_KEY => {
                if value == 2 {
                    return None; // autorepeat
                }
                let v = if value != 0 { 1.0 } else { 0.0 };
                if (BTN_MISC..0x160).contains(&code) {
                    let b = mouse_button_code(code)?;
                    return ev(Device::Mouse, EventKind::MouseButton, b, v);
                }
                let kind = if value != 0 {
                    EventKind::KeyDown
                } else {
                    EventKind::KeyUp
                };
                ev(Device::Keyboard, kind, scancode::evdev_to_code(code), v)
            }
            EV_REL => match code {
                REL_X => ev(
                    Device::Mouse,
                    EventKind::MouseMove,
                    mouse_axis::X,
                    value as f32,
                ),
                REL_Y => ev(
                    Device::Mouse,
                    EventKind::MouseMove,
                    mouse_axis::Y,
                    value as f32,
                ),
                REL_WHEEL if !self.hires_wheel => ev(
                    Device::Mouse,
                    EventKind::Wheel,
                    wheel_axis::VERTICAL,
                    value as f32,
                ),
                REL_HWHEEL if !self.hires_hwheel => ev(
                    Device::Mouse,
                    EventKind::Wheel,
                    wheel_axis::HORIZONTAL,
                    value as f32,
                ),
                REL_WHEEL_HI_RES => ev(
                    Device::Mouse,
                    EventKind::Wheel,
                    wheel_axis::VERTICAL,
                    value as f32 / 120.0,
                ),
                REL_HWHEEL_HI_RES => ev(
                    Device::Mouse,
                    EventKind::Wheel,
                    wheel_axis::HORIZONTAL,
                    value as f32 / 120.0,
                ),
                _ => None,
            },
            _ => None,
        }
    }
}

fn read_loop(mut scanner: Scanner, sink: EventSink, stop: Arc<AtomicBool>) {
    let mut last_scan = cap_clock::now_ns();
    let mut pollfds: Vec<libc::pollfd> = Vec::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        if cap_clock::now_ns() - last_scan > RESCAN_SECS * 1_000_000_000 {
            scanner.rescan();
            last_scan = cap_clock::now_ns();
        }
        pollfds.clear();
        paths.clear();
        for (p, d) in &scanner.devices {
            pollfds.push(libc::pollfd {
                fd: d.dev.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            paths.push(p.clone());
        }
        if pollfds.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(200));
            continue;
        }
        // SAFETY: pollfds is a valid array of pollfd of the given length.
        let n = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 200) };
        if n <= 0 {
            continue;
        }
        let mut dead: Vec<PathBuf> = Vec::new();
        for (pfd, path) in pollfds.iter().zip(paths.iter()) {
            if pfd.revents == 0 {
                continue;
            }
            let Some(d) = scanner.devices.get_mut(path) else {
                continue;
            };
            if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
                && pfd.revents & libc::POLLIN == 0
            {
                dead.push(path.clone());
                continue;
            }
            let tr = Translator {
                hires_wheel: d.class.hires_wheel,
                hires_hwheel: d.class.hires_hwheel,
            };
            match d.dev.fetch_events() {
                Ok(events) => {
                    for e in events {
                        let raw: &libc::input_event = e.as_ref();
                        #[allow(clippy::unnecessary_cast)] // time_t is 32-bit on some targets
                        let t_ns = timeval_to_ns(raw.time.tv_sec as i64, raw.time.tv_usec as i64);
                        let (ty, code, value) = (e.event_type().0, e.code(), e.value());
                        if ty == EV_SYN && code == SYN_DROPPED {
                            tracing::warn!(dev = %d.name, "evdev SYN_DROPPED: kernel buffer overflowed, events lost");
                            continue;
                        }
                        if let Some(out) = tr.translate(t_ns, ty, code, value) {
                            if !sink.send(out) {
                                return; // receiver gone
                            }
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    tracing::info!(dev = %d.name, "evdev device gone: {e}");
                    dead.push(path.clone());
                }
            }
        }
        for p in dead {
            scanner.devices.remove(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_keys_buttons_wheel() {
        let tr = Translator {
            hires_wheel: true,
            hires_hwheel: false,
        };
        let k = tr.translate(5, EV_KEY, 30, 1).unwrap();
        assert_eq!(
            (k.device, k.kind, k.code, k.value),
            (Device::Keyboard, EventKind::KeyDown, 0x1E, 1.0)
        );
        let k = tr.translate(6, EV_KEY, 97, 0).unwrap();
        assert_eq!((k.kind, k.code, k.value), (EventKind::KeyUp, 0xE01D, 0.0));
        assert!(
            tr.translate(7, EV_KEY, 30, 2).is_none(),
            "autorepeat dropped"
        );
        let b = tr.translate(8, EV_KEY, BTN_SIDE, 1).unwrap();
        assert_eq!(
            (b.device, b.kind, b.code),
            (Device::Mouse, EventKind::MouseButton, mouse_button::BACK)
        );
        assert!(tr.translate(8, EV_KEY, 0x14a /* BTN_TOUCH */, 1).is_none());
        let m = tr.translate(9, EV_REL, REL_Y, -7).unwrap();
        assert_eq!(
            (m.kind, m.code, m.value),
            (EventKind::MouseMove, mouse_axis::Y, -7.0)
        );
        // hi-res vertical wheel preferred; low-res ignored on that device
        assert!(tr.translate(10, EV_REL, REL_WHEEL, 1).is_none());
        let w = tr.translate(10, EV_REL, REL_WHEEL_HI_RES, 60).unwrap();
        assert_eq!(
            (w.kind, w.code, w.value),
            (EventKind::Wheel, wheel_axis::VERTICAL, 0.5)
        );
        let h = tr.translate(11, EV_REL, REL_HWHEEL, -1).unwrap();
        assert_eq!((h.code, h.value), (wheel_axis::HORIZONTAL, -1.0));
        assert_eq!(h.t_ns, 11);
    }

    #[test]
    fn timeval_conversion() {
        assert_eq!(timeval_to_ns(12, 345_678), 12_345_678_000);
    }

    #[test]
    #[allow(clippy::unnecessary_cast)]
    fn ioctl_number() {
        // _IOW('E', 0xa0, int) = dir(1)<<30 | size(4)<<16 | 'E'<<8 | 0xa0
        let v = (1u64 << 30) | (4 << 16) | ((b'E' as u64) << 8) | 0xa0;
        assert_eq!(v, EVIOCSCLOCKID as u64); // c_ulong
    }

    /// Live smoke test: enumerates devices only (no key presses expected).
    /// Run with `cargo test -p cap-input -- --ignored --nocapture evdev_live`.
    #[test]
    #[ignore]
    fn evdev_live_enumerate() {
        let devs = enumerate();
        for d in &devs {
            println!(
                "{} {:?} kb={} mouse={}",
                d.path.display(),
                d.name,
                d.keyboard,
                d.mouse
            );
        }
        let mut src = EvdevSource::new();
        let (tx, _rx) = crossbeam_channel::bounded(1024);
        match src.start(tx) {
            Ok(()) => {
                std::thread::sleep(std::time::Duration::from_millis(300));
                src.stop();
                println!("started+stopped OK with {} device(s)", devs.len());
            }
            Err(e) => println!("start error: {e}"),
        }
    }
}
