//! macOS keyboard + mouse via IOHIDManager (IOKit FFI).
//!
//! **Best effort, untested on real hardware** (written and cross-checked from
//! Linux). Needs the *Input Monitoring* permission (System Settings > Privacy
//! & Security > Input Monitoring); `start` checks it with `IOHIDCheckAccess`
//! and triggers the system prompt with `IOHIDRequestAccess` when undecided.
//!
//! * Opens the manager with `kIOHIDOptionsTypeNone` (shared, not seized), so
//!   input still reaches the game normally.
//! * Keys: HID keyboard usages (page 0x07) -> set-1 scan codes
//!   ([`crate::scancode::hid_to_code`]); held-set dedup.
//! * Mouse: Generic Desktop X/Y (relative counts), Wheel (0x38) and Consumer
//!   AC Pan (0x0238) in detents, Button page 1..5 -> `cap_types::mouse_button`.
//!   Built-in trackpad gestures are not HID mouse reports and are not logged.
//! * Timestamps: `IOHIDValueGetTimeStamp` (mach absolute time) converted with
//!   `cap_clock::ticks_to_ns`, i.e. the same clock as frames.

use crate::hid_logic::HidTranslator;
use crate::{EventSink, InputError, InputSource, Result};
use cap_types::InputEvent;
use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::array::CFArrayRef;
use core_foundation_sys::base::{
    kCFAllocatorDefault, CFAllocatorRef, CFIndex, CFRelease, CFTypeRef,
};
use core_foundation_sys::runloop::{
    kCFRunLoopDefaultMode, CFRunLoopGetCurrent, CFRunLoopRef, CFRunLoopRunInMode,
};
use core_foundation_sys::string::CFStringRef;
use crossbeam_channel::Sender;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

type IOHIDManagerRef = *mut c_void;
type IOHIDValueRef = *mut c_void;
type IOHIDElementRef = *mut c_void;
type IOReturn = i32;
type IOHIDValueCallback = extern "C" fn(
    context: *mut c_void,
    result: IOReturn,
    sender: *mut c_void,
    value: IOHIDValueRef,
);

const KIOHID_OPTIONS_TYPE_NONE: u32 = 0;
const KIOHID_REQUEST_TYPE_LISTEN_EVENT: u32 = 1;
const KIOHID_ACCESS_TYPE_GRANTED: u32 = 0;
const KIOHID_ACCESS_TYPE_DENIED: u32 = 1;

const PAGE_GENERIC_DESKTOP: u32 = 0x01;
const USAGE_MOUSE: i32 = 0x02;
const USAGE_KEYBOARD: i32 = 0x06;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDManagerCreate(allocator: CFAllocatorRef, options: u32) -> IOHIDManagerRef;
    fn IOHIDManagerSetDeviceMatchingMultiple(manager: IOHIDManagerRef, multiple: CFArrayRef);
    fn IOHIDManagerRegisterInputValueCallback(
        manager: IOHIDManagerRef,
        callback: Option<IOHIDValueCallback>,
        context: *mut c_void,
    );
    fn IOHIDManagerScheduleWithRunLoop(
        manager: IOHIDManagerRef,
        run_loop: CFRunLoopRef,
        mode: CFStringRef,
    );
    fn IOHIDManagerUnscheduleFromRunLoop(
        manager: IOHIDManagerRef,
        run_loop: CFRunLoopRef,
        mode: CFStringRef,
    );
    fn IOHIDManagerOpen(manager: IOHIDManagerRef, options: u32) -> IOReturn;
    fn IOHIDManagerClose(manager: IOHIDManagerRef, options: u32) -> IOReturn;
    fn IOHIDValueGetElement(value: IOHIDValueRef) -> IOHIDElementRef;
    fn IOHIDValueGetIntegerValue(value: IOHIDValueRef) -> CFIndex;
    fn IOHIDValueGetTimeStamp(value: IOHIDValueRef) -> u64;
    fn IOHIDElementGetUsagePage(element: IOHIDElementRef) -> u32;
    fn IOHIDElementGetUsage(element: IOHIDElementRef) -> u32;
    fn IOHIDCheckAccess(request_type: u32) -> u32;
    fn IOHIDRequestAccess(request_type: u32) -> bool;
}

/// Checks (and if undecided, requests) the Input Monitoring permission.
pub fn check_input_monitoring() -> Result<()> {
    // SAFETY: plain IOKit calls without pointers.
    let access = unsafe { IOHIDCheckAccess(KIOHID_REQUEST_TYPE_LISTEN_EVENT) };
    let msg =
        "Input Monitoring permission is required: open System Settings > Privacy & Security > \
               Input Monitoring and enable this app (then restart it)";
    match access {
        KIOHID_ACCESS_TYPE_GRANTED => Ok(()),
        KIOHID_ACCESS_TYPE_DENIED => Err(InputError::PermissionDenied(msg.into())),
        _ => {
            // SAFETY: as above; shows the system prompt once.
            if unsafe { IOHIDRequestAccess(KIOHID_REQUEST_TYPE_LISTEN_EVENT) } {
                Ok(())
            } else {
                Err(InputError::PermissionDenied(msg.into()))
            }
        }
    }
}

struct CallbackCtx {
    sink: EventSink,
    tr: HidTranslator,
}

extern "C" fn on_value(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
    value: IOHIDValueRef,
) {
    if context.is_null() || value.is_null() {
        return;
    }
    // SAFETY: context is the Box<CallbackCtx> owned by the run-loop thread,
    // alive until after the manager is unscheduled; value is valid for the
    // duration of the callback.
    unsafe {
        let ctx = &mut *(context as *mut CallbackCtx);
        let el = IOHIDValueGetElement(value);
        if el.is_null() {
            return;
        }
        let page = IOHIDElementGetUsagePage(el);
        let usage = IOHIDElementGetUsage(el);
        let v = IOHIDValueGetIntegerValue(value) as i64;
        let ts = IOHIDValueGetTimeStamp(value);
        let t_ns = if ts != 0 {
            cap_clock::ticks_to_ns(ts as i64)
        } else {
            cap_clock::now_ns()
        };
        if let Some(ev) = ctx.tr.translate(t_ns, page, usage, v) {
            ctx.sink.send(ev);
        }
    }
}

fn matching() -> CFArray<CFDictionary<CFString, CFNumber>> {
    let dict = |usage: i32| {
        CFDictionary::from_CFType_pairs(&[
            (
                CFString::new("DeviceUsagePage"),
                CFNumber::from(PAGE_GENERIC_DESKTOP as i32),
            ),
            (CFString::new("DeviceUsage"), CFNumber::from(usage)),
        ])
    };
    CFArray::from_CFTypes(&[dict(USAGE_KEYBOARD), dict(USAGE_MOUSE)])
}

pub struct HidSource {
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl HidSource {
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
            thread: None,
        }
    }
}

impl Default for HidSource {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSource for HidSource {
    fn start(&mut self, tx: Sender<InputEvent>) -> Result<()> {
        if self.thread.is_some() {
            return Ok(());
        }
        check_input_monitoring()?;
        self.stop.store(false, Ordering::SeqCst);
        let sink = EventSink::new(tx, self.dropped.clone());
        let stop = self.stop.clone();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<()>>(1);
        let h = std::thread::Builder::new()
            .name("cap-input-iohid".into())
            .spawn(move || {
                // SAFETY: all IOKit/CF objects are created, used and released on this thread.
                unsafe { run(sink, stop, ready_tx) }
            })
            .map_err(|e| InputError::Backend(format!("spawn IOHID thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.thread = Some(h);
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = h.join();
                Err(e)
            }
            Err(_) => {
                let _ = h.join();
                Err(InputError::Backend(
                    "IOHID thread exited during init".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }

    fn name(&self) -> &'static str {
        "iohid"
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for HidSource {
    fn drop(&mut self) {
        self.stop();
    }
}

unsafe fn run(sink: EventSink, stop: Arc<AtomicBool>, ready: Sender<Result<()>>) {
    let mgr = IOHIDManagerCreate(kCFAllocatorDefault, KIOHID_OPTIONS_TYPE_NONE);
    if mgr.is_null() {
        let _ = ready.send(Err(InputError::Backend("IOHIDManagerCreate failed".into())));
        return;
    }
    let m = matching();
    IOHIDManagerSetDeviceMatchingMultiple(mgr, m.as_concrete_TypeRef());
    let ctx = Box::into_raw(Box::new(CallbackCtx {
        sink,
        tr: HidTranslator::default(),
    }));
    IOHIDManagerRegisterInputValueCallback(mgr, Some(on_value), ctx as *mut c_void);
    let rl = CFRunLoopGetCurrent();
    IOHIDManagerScheduleWithRunLoop(mgr, rl, kCFRunLoopDefaultMode);
    let r = IOHIDManagerOpen(mgr, KIOHID_OPTIONS_TYPE_NONE);
    if r != 0 {
        IOHIDManagerUnscheduleFromRunLoop(mgr, rl, kCFRunLoopDefaultMode);
        CFRelease(mgr as CFTypeRef);
        drop(Box::from_raw(ctx));
        // kIOReturnNotPermitted (0xE00002E2) = no Input Monitoring permission.
        let e = if r as u32 == 0xE000_02E2 {
            InputError::PermissionDenied(
                "IOHIDManagerOpen: not permitted (grant Input Monitoring)".into(),
            )
        } else {
            InputError::Backend(format!("IOHIDManagerOpen failed: {r:#x}"))
        };
        let _ = ready.send(Err(e));
        return;
    }
    let _ = ready.send(Ok(()));
    while !stop.load(Ordering::Relaxed) {
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.2, 0);
    }
    IOHIDManagerRegisterInputValueCallback(mgr, None, std::ptr::null_mut());
    IOHIDManagerUnscheduleFromRunLoop(mgr, rl, kCFRunLoopDefaultMode);
    IOHIDManagerClose(mgr, KIOHID_OPTIONS_TYPE_NONE);
    CFRelease(mgr as CFTypeRef);
    drop(Box::from_raw(ctx));
}
