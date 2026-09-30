//! macOS focus tracking. **Best effort, untested on real hardware** (written
//! and cross-checked from Linux).
//!
//! * Foreground app: the owner of the front-most on-screen, layer-0 window in
//!   `CGWindowListCopyWindowInfo` (the list is ordered front to back), polled
//!   at 10 Hz. `NSWorkspace.frontmostApplication` is avoided because it is
//!   only refreshed by notifications on the main run loop, which a CLI
//!   recorder thread does not run.
//! * Identity: bundle id via `NSRunningApplication(processIdentifier:)`
//!   (e.g. `com.foo.game`), else the window owner's process name. No process
//!   is opened.
//! * `list_windows`: same window list (layer 0, on screen). Window titles
//!   (`kCGWindowName`) are only provided when the app has the Screen
//!   Recording permission (which ScreenCaptureKit capture needs anyway);
//!   without it windows fall back to the owner name as title.

use crate::{Emitter, FocusError, FocusTracker, GameIdentity, Result, WindowInfo};
use cap_types::FocusRecord;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_graphics::window::{
    copy_window_info, kCGNullWindowID, kCGWindowLayer, kCGWindowListExcludeDesktopElements,
    kCGWindowListOptionOnScreenOnly, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName,
    kCGWindowOwnerPID,
};
use crossbeam_channel::Sender;
use objc2_app_kit::NSRunningApplication;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

const POLL_MS: u64 = 100;

struct CgWindow {
    number: u32,
    pid: i32,
    owner: String,
    title: String,
}

fn dict_get(
    d: &CFDictionary<CFString, CFType>,
    key: core_foundation_sys::string::CFStringRef,
) -> Option<CFType> {
    // SAFETY: the kCGWindow* keys are valid static CFStrings.
    let k = unsafe { CFString::wrap_under_get_rule(key) };
    d.find(&k).map(|v| v.clone())
}

fn get_i64(
    d: &CFDictionary<CFString, CFType>,
    key: core_foundation_sys::string::CFStringRef,
) -> Option<i64> {
    dict_get(d, key)?.downcast::<CFNumber>()?.to_i64()
}

fn get_string(
    d: &CFDictionary<CFString, CFType>,
    key: core_foundation_sys::string::CFStringRef,
) -> Option<String> {
    Some(dict_get(d, key)?.downcast::<CFString>()?.to_string())
}

/// On-screen, normal-layer windows, front to back.
fn windows() -> Vec<CgWindow> {
    let Some(arr) = copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr.iter() {
        // SAFETY: each element of the window-info array is a CFDictionary.
        let d: CFDictionary<CFString, CFType> =
            unsafe { CFDictionary::wrap_under_get_rule(*item as _) };
        // SAFETY: static CFString keys exported by CoreGraphics.
        let (layer, number, pid, owner, title) = unsafe {
            (
                get_i64(&d, kCGWindowLayer).unwrap_or(-1),
                get_i64(&d, kCGWindowNumber).unwrap_or(0),
                get_i64(&d, kCGWindowOwnerPID).unwrap_or(0),
                get_string(&d, kCGWindowOwnerName).unwrap_or_default(),
                get_string(&d, kCGWindowName).unwrap_or_default(),
            )
        };
        if layer != 0 {
            continue;
        }
        out.push(CgWindow {
            number: number as u32,
            pid: pid as i32,
            owner,
            title,
        });
    }
    out
}

fn identity_for(pid: i32, owner: &str) -> String {
    let bundle = objc2::rc::autoreleasepool(|_| {
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .and_then(|app| app.bundleIdentifier())
            .map(|s| s.to_string())
    });
    bundle
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| owner.to_string())
}

fn foreground_game_id() -> String {
    windows()
        .first()
        .map(|w| identity_for(w.pid, &w.owner))
        .unwrap_or_default()
}

pub struct MacTracker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MacTracker {
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }
}

impl Default for MacTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl FocusTracker for MacTracker {
    fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        Ok(windows()
            .into_iter()
            .map(|w| {
                let game_id = identity_for(w.pid, &w.owner);
                let title = if w.title.is_empty() {
                    w.owner.clone()
                } else {
                    w.title
                };
                WindowInfo {
                    native_id: w.number as u64,
                    title,
                    pid: w.pid.max(0) as u32,
                    identity: GameIdentity {
                        game_id,
                        publisher: None,
                    },
                }
            })
            .collect())
    }

    fn start(&mut self, target_game_id: &str, tx: Sender<FocusRecord>) -> Result<()> {
        if self.thread.is_some() {
            return Ok(());
        }
        let mut em = Emitter::new(target_game_id, tx);
        em.observe(&foreground_game_id());
        self.stop.store(false, Ordering::SeqCst);
        let stop = self.stop.clone();
        let h = std::thread::Builder::new()
            .name("cap-focus-mac".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                    if !em.observe(&foreground_game_id()) {
                        return;
                    }
                }
            })
            .map_err(|e| FocusError::Backend(format!("spawn focus thread: {e}")))?;
        self.thread = Some(h);
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for MacTracker {
    fn drop(&mut self) {
        self.stop();
    }
}
