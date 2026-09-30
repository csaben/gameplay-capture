//! Windows keyboard + mouse via Raw Input.
//!
//! A hidden message-only window (`HWND_MESSAGE` parent) on its own thread
//! registers for keyboard (usage page 1, usage 6) and mouse (usage page 1,
//! usage 2) with `RIDEV_INPUTSINK`, so it receives `WM_INPUT` even while the
//! game has focus. This is the passive, documented Raw Input path: no
//! `SetWindowsHookEx`, no DLL injection, no `OpenProcess`, nothing sent to
//! the game.
//!
//! * Keys: `MakeCode` with `RI_KEY_E0`/`RI_KEY_E1` folded into the code
//!   (`0xE000 | make`, Pause = `0xE11D`), i.e. native set-1 scan codes (see
//!   [`crate::scancode`]). Fake shifts (`VKey == 0xFF`, sent around
//!   Print Screen / Num Lock-dependent navigation keys) and the trailing `45`
//!   of the Pause sequence are dropped. Autorepeat is suppressed by tracking
//!   the set of held keys.
//! * Mouse: relative `lLastX/lLastY`. For absolute devices (RDP sessions,
//!   tablets, some VMs: `MOUSE_MOVE_ABSOLUTE`) the normalised 0..65535
//!   coordinates are scaled to virtual-desktop pixels and differenced.
//!   Buttons map to `cap_types::mouse_button`; wheel/hwheel are divided by
//!   `WHEEL_DELTA` (120) to give detents.
//! * Timestamp: `cap_clock::now_ns()` (QPC) when the message is processed.
//!   `GetMessageTime()` is only millisecond resolution (and a different time
//!   base), so it is not used; the receipt latency is typically well under
//!   1 ms on this dedicated thread.

use crate::rawinput_logic::RawState;
use crate::{EventSink, InputError, InputSource, Result};
use cap_types::InputEvent;
use crossbeam_channel::Sender;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_VIRTUAL_DESKTOP, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RIDEV_REMOVE, RID_INPUT, RIM_TYPEKEYBOARD,
    RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetSystemMetrics, PostThreadMessageW, RegisterClassW, TranslateMessage, HWND_MESSAGE, MSG,
    SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_INPUT, WM_QUIT, WNDCLASSW,
};

const HID_USAGE_PAGE_GENERIC: u16 = 0x01;
const HID_USAGE_GENERIC_MOUSE: u16 = 0x02;
const HID_USAGE_GENERIC_KEYBOARD: u16 = 0x06;

struct ThreadState {
    sink: EventSink,
    raw: RawState,
    buf: Vec<u64>,
    out: Vec<InputEvent>,
}

thread_local! {
    static STATE: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

pub struct RawInputSource {
    dropped: Arc<AtomicU64>,
    thread: Option<(JoinHandle<()>, u32)>,
}

impl RawInputSource {
    pub fn new() -> Self {
        Self {
            dropped: Arc::new(AtomicU64::new(0)),
            thread: None,
        }
    }
}

impl Default for RawInputSource {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSource for RawInputSource {
    fn start(&mut self, tx: Sender<InputEvent>) -> Result<()> {
        if self.thread.is_some() {
            return Ok(());
        }
        let sink = EventSink::new(tx, self.dropped.clone());
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<u32>>(1);
        let h = std::thread::Builder::new()
            .name("cap-input-rawinput".into())
            .spawn(move || {
                // SAFETY: all Win32 calls below are made on this thread with
                // valid arguments; the window lives until the loop exits.
                unsafe { run(sink, ready_tx) }
            })
            .map_err(|e| InputError::Backend(format!("spawn raw input thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(tid)) => {
                self.thread = Some((h, tid));
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = h.join();
                Err(e)
            }
            Err(_) => {
                let _ = h.join();
                Err(InputError::Backend(
                    "raw input thread exited during init".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        if let Some((h, tid)) = self.thread.take() {
            // SAFETY: posting WM_QUIT to a thread id we own.
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            let _ = h.join();
        }
    }

    fn name(&self) -> &'static str {
        "raw-input"
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for RawInputSource {
    fn drop(&mut self) {
        self.stop();
    }
}

fn devices(
    flags: windows::Win32::UI::Input::RAWINPUTDEVICE_FLAGS,
    hwnd: HWND,
) -> [RAWINPUTDEVICE; 2] {
    [
        RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_GENERIC,
            usUsage: HID_USAGE_GENERIC_KEYBOARD,
            dwFlags: flags,
            hwndTarget: hwnd,
        },
        RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_GENERIC,
            usUsage: HID_USAGE_GENERIC_MOUSE,
            dwFlags: flags,
            hwndTarget: hwnd,
        },
    ]
}

unsafe fn run(sink: EventSink, ready: Sender<Result<u32>>) {
    let fail = |msg: String| {
        let _ = ready.send(Err(InputError::Backend(msg)));
    };
    let hinst = match GetModuleHandleW(PCWSTR::null()) {
        Ok(h) => h,
        Err(e) => return fail(format!("GetModuleHandleW: {e}")),
    };
    let class = w!("cap_input_raw_sink");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinst.into(),
        lpszClassName: class,
        ..Default::default()
    };
    // Re-registering after a restart fails with ERROR_CLASS_ALREADY_EXISTS; harmless.
    RegisterClassW(&wc);
    let hwnd = match CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        w!("cap-input"),
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        HWND_MESSAGE,
        None,
        hinst,
        None,
    ) {
        Ok(h) => h,
        Err(e) => return fail(format!("CreateWindowExW(HWND_MESSAGE): {e}")),
    };
    STATE.with(|s| {
        *s.borrow_mut() = Some(ThreadState {
            sink,
            raw: RawState::default(),
            buf: vec![0u64; 16],
            out: Vec::with_capacity(8),
        })
    });
    let devs = devices(RIDEV_INPUTSINK, hwnd);
    if let Err(e) = RegisterRawInputDevices(&devs, std::mem::size_of::<RAWINPUTDEVICE>() as u32) {
        let _ = DestroyWindow(hwnd);
        return fail(format!("RegisterRawInputDevices: {e}"));
    }
    // The window exists, so this thread has a message queue for WM_QUIT.
    let _ = ready.send(Ok(GetCurrentThreadId()));

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    let remove = devices(RIDEV_REMOVE, HWND::default());
    let _ = RegisterRawInputDevices(&remove, std::mem::size_of::<RAWINPUTDEVICE>() as u32);
    let _ = DestroyWindow(hwnd);
    STATE.with(|s| s.borrow_mut().take());
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        let t_ns = cap_clock::now_ns();
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                handle_input(st, t_ns, HRAWINPUT(lparam.0 as _));
            }
        });
    }
    // DefWindowProc must see WM_INPUT so the system can free the data.
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn screen_size(virtual_desktop: bool) -> (f64, f64) {
    // SAFETY: GetSystemMetrics has no preconditions.
    unsafe {
        if virtual_desktop {
            (
                GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64,
                GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64,
            )
        } else {
            (
                GetSystemMetrics(SM_CXSCREEN) as f64,
                GetSystemMetrics(SM_CYSCREEN) as f64,
            )
        }
    }
}

unsafe fn handle_input(st: &mut ThreadState, t_ns: i64, h: HRAWINPUT) {
    let header = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut size = 0u32;
    if GetRawInputData(h, RID_INPUT, None, &mut size, header) != 0 || size == 0 {
        return;
    }
    let words = (size as usize).div_ceil(8);
    if st.buf.len() < words {
        st.buf.resize(words, 0);
    }
    let got = GetRawInputData(
        h,
        RID_INPUT,
        Some(st.buf.as_mut_ptr() as *mut _),
        &mut size,
        header,
    );
    if got == u32::MAX || (got as usize) < std::mem::size_of::<RAWINPUTHEADER>() {
        return;
    }
    let raw = &*(st.buf.as_ptr() as *const RAWINPUT);
    if raw.header.dwType == RIM_TYPEKEYBOARD.0 {
        let k = raw.data.keyboard;
        if let Some(ev) = st.raw.keyboard(t_ns, k.MakeCode, k.Flags, k.VKey) {
            st.sink.send(ev);
        }
    } else if raw.header.dwType == RIM_TYPEMOUSE.0 {
        let m = raw.data.mouse;
        let bf = m.Anonymous.Anonymous.usButtonFlags;
        let bd = m.Anonymous.Anonymous.usButtonData;
        let us_flags = m.usFlags.0;
        let screen = screen_size(us_flags & MOUSE_VIRTUAL_DESKTOP.0 != 0);
        st.out.clear();
        st.raw.mouse(
            t_ns,
            us_flags,
            bf,
            bd,
            m.lLastX,
            m.lLastY,
            screen,
            &mut st.out,
        );
        for ev in st.out.drain(..) {
            st.sink.send(ev);
        }
    }
}
