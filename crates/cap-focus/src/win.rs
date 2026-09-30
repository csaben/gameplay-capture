//! Windows focus tracking.
//!
//! * `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT |
//!   WINEVENT_SKIPOWNPROCESS)` on a dedicated message-loop thread: the hook
//!   callback runs in *our* process (out-of-context), nothing is injected.
//!   A 250 ms thread timer re-checks `GetForegroundWindow` as a safety net.
//! * Identity: `GetWindowThreadProcessId` -> pid, then the exe file name from
//!   a Toolhelp process snapshot (`CreateToolhelp32Snapshot` /
//!   `Process32FirstW`/`Process32NextW`). This reads the system process list
//!   and **never calls `OpenProcess`** on the game (or any process), so no
//!   process handle to the game ever exists, not even
//!   `PROCESS_QUERY_LIMITED_INFORMATION`.
//! * Publisher (code-signing) is not resolved: it would require opening the
//!   exe file / process. `GameIdentity::publisher` is `None`.
//! * `list_windows`: `EnumWindows`, keeping visible, un-owned, non-tool,
//!   non-cloaked windows with a title.

use crate::{Emitter, FocusError, FocusTracker, GameIdentity, Result, WindowInfo};
use cap_types::FocusRecord;
use crossbeam_channel::Sender;
use std::cell::RefCell;
use std::collections::HashMap;
use std::thread::JoinHandle;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumWindows, GetForegroundWindow, GetMessageW, GetWindow, GetWindowLongW,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, KillTimer,
    PeekMessageW, PostThreadMessageW, SetTimer, TranslateMessage, EVENT_SYSTEM_FOREGROUND,
    GWL_EXSTYLE, GW_OWNER, MSG, PM_NOREMOVE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
    WM_QUIT, WM_TIMER, WS_EX_TOOLWINDOW,
};

const RECHECK_MS: u32 = 250;

/// pid -> exe file name for all running processes (one Toolhelp snapshot;
/// no process is opened).
pub fn process_names() -> HashMap<u32, String> {
    let mut out = HashMap::new();
    // SAFETY: snapshot handle is closed below; entry has dwSize set.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut pe = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut pe).is_ok() {
            loop {
                let len = pe
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(pe.szExeFile.len());
                out.insert(
                    pe.th32ProcessID,
                    String::from_utf16_lossy(&pe.szExeFile[..len]),
                );
                if Process32NextW(snap, &mut pe).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: valid out-pointer; hwnd may be stale, then pid stays 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

fn window_title(hwnd: HWND) -> String {
    // SAFETY: buffer sized from GetWindowTextLengthW.
    unsafe {
        let n = GetWindowTextLengthW(hwnd);
        if n <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

fn foreground_game_id() -> String {
    // SAFETY: no preconditions.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return String::new();
    }
    let pid = window_pid(hwnd);
    if pid == 0 {
        return String::new();
    }
    process_names().remove(&pid).unwrap_or_default()
}

struct HookState {
    em: Emitter,
    last_hwnd: isize,
}

thread_local! {
    static HOOK: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

fn recheck(force: bool) {
    HOOK.with(|h| {
        if let Some(st) = h.borrow_mut().as_mut() {
            // SAFETY: no preconditions.
            let hwnd = unsafe { GetForegroundWindow() }.0 as isize;
            // Resolve identity only when the foreground window changed, or a
            // previous record could not be sent (full channel) and must be retried.
            if force || hwnd != st.last_hwnd || st.em.pending() {
                st.last_hwnd = hwnd;
                st.em.observe(&foreground_game_id());
            }
        }
    });
}

unsafe extern "system" fn on_foreground(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    recheck(true);
}

pub struct WinTracker {
    thread: Option<(JoinHandle<()>, u32)>,
}

impl WinTracker {
    pub fn new() -> Self {
        Self { thread: None }
    }
}

impl Default for WinTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl FocusTracker for WinTracker {
    fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let v = &mut *(lparam.0 as *mut Vec<HWND>);
            v.push(hwnd);
            BOOL(1)
        }
        let mut hwnds: Vec<HWND> = Vec::new();
        // SAFETY: the callback only runs during EnumWindows, while `hwnds` is alive.
        unsafe { EnumWindows(Some(collect), LPARAM(&mut hwnds as *mut _ as isize)) }
            .map_err(|e| FocusError::Backend(format!("EnumWindows: {e}")))?;
        let names = process_names();
        let mut out = Vec::new();
        for hwnd in hwnds {
            // SAFETY: plain queries on window handles (may be stale; failures just filter).
            unsafe {
                if !IsWindowVisible(hwnd).as_bool() {
                    continue;
                }
                if GetWindow(hwnd, GW_OWNER)
                    .map(|o| !o.0.is_null())
                    .unwrap_or(false)
                {
                    continue;
                }
                if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
                    continue;
                }
                let mut cloaked = 0u32;
                if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4)
                    .is_ok()
                    && cloaked != 0
                {
                    continue;
                }
            }
            let title = window_title(hwnd);
            if title.is_empty() {
                continue;
            }
            let pid = window_pid(hwnd);
            let game_id = names.get(&pid).cloned().unwrap_or_default();
            out.push(WindowInfo {
                native_id: hwnd.0 as usize as u64,
                title,
                pid,
                identity: GameIdentity {
                    game_id,
                    publisher: None,
                },
            });
        }
        Ok(out)
    }

    fn start(&mut self, target_game_id: &str, tx: Sender<FocusRecord>) -> Result<()> {
        if self.thread.is_some() {
            return Ok(());
        }
        let em = Emitter::new(target_game_id, tx);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<u32>>(1);
        let h = std::thread::Builder::new()
            .name("cap-focus-win".into())
            // SAFETY: Win32 calls on this thread with valid arguments.
            .spawn(move || unsafe { run(em, ready_tx) })
            .map_err(|e| FocusError::Backend(format!("spawn focus thread: {e}")))?;
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
                Err(FocusError::Backend(
                    "focus thread exited during init".into(),
                ))
            }
        }
    }

    fn stop(&mut self) {
        if let Some((h, tid)) = self.thread.take() {
            // SAFETY: posting WM_QUIT to our own thread.
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            let _ = h.join();
        }
    }
}

impl Drop for WinTracker {
    fn drop(&mut self) {
        self.stop();
    }
}

unsafe fn run(em: Emitter, ready: Sender<Result<u32>>) {
    // Force creation of this thread's message queue before publishing the id.
    let mut msg = MSG::default();
    let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
    HOOK.with(|h| *h.borrow_mut() = Some(HookState { em, last_hwnd: 0 }));
    let hook = SetWinEventHook(
        EVENT_SYSTEM_FOREGROUND,
        EVENT_SYSTEM_FOREGROUND,
        None,
        Some(on_foreground),
        0,
        0,
        WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
    );
    if hook.is_invalid() {
        let _ = ready.send(Err(FocusError::Backend(
            "SetWinEventHook(EVENT_SYSTEM_FOREGROUND) failed".into(),
        )));
        return;
    }
    let timer = SetTimer(None, 0, RECHECK_MS, None);
    recheck(true); // initial record
    let _ = ready.send(Ok(GetCurrentThreadId()));
    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
        if msg.message == WM_TIMER && msg.hwnd.0.is_null() {
            recheck(false);
            continue;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    let _ = KillTimer(None, timer);
    let _ = UnhookWinEvent(hook);
    HOOK.with(|h| h.borrow_mut().take());
}
