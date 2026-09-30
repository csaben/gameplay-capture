//! Focus tracking and game identity: is the target game window in the
//! foreground, and which executable / bundle id is it?
//!
//! | Platform | Foreground detection | Identity (`game_id`) |
//! |----------|----------------------|----------------------|
//! | Windows  | `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT)` + 250 ms `GetForegroundWindow` safety poll | exe file name via a Toolhelp process snapshot (`CreateToolhelp32Snapshot`); **no `OpenProcess`** on any process |
//! | Linux X11 / XWayland | `_NET_ACTIVE_WINDOW` on the root (PropertyNotify), `GetInputFocus` fallback when no EWMH WM | `_NET_WM_PID` (XRes fallback) -> `/proc/<pid>/exe` basename; Wine/Proton -> the Windows exe from `/proc/<pid>/cmdline` |
//! | Linux Wayland (no X11) | unsupported: no general focus API | - |
//! | macOS    | frontmost on-screen layer-0 window (`CGWindowListCopyWindowInfo`), polled at 10 Hz. **Untested.** | bundle id (`NSRunningApplication`), else process name |
//!
//! Every tracker emits an initial `FocusRecord` at `start`, then one per
//! change of (focused, game_id). `game_id` is `""` when no window is focused.
//! Matching against the target is ASCII-case-insensitive ([`identity_matches`]).
//!
//! [`FocusGate`] turns the record stream into an input filter that also
//! synthesises releases for held keys on focus loss (see [`gate`]).

use cap_types::FocusRecord;
use crossbeam_channel::{Sender, TrySendError};

pub use cap_types::GameIdentity;

pub mod gate;
pub use gate::{FocusGate, HeldInputs};

mod procname;
pub use procname::exe_basename;

#[cfg(target_os = "linux")]
pub mod x11;
#[cfg(target_os = "linux")]
pub use x11::X11Tracker;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::WinTracker;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacTracker;

#[derive(Debug, thiserror::Error)]
pub enum FocusError {
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, FocusError>;

/// A top-level window that could be captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// HWND / X11 window id / CGWindowID.
    pub native_id: u64,
    pub title: String,
    pub pid: u32,
    pub identity: GameIdentity,
}

pub trait FocusTracker: Send {
    /// Visible top-level windows, for choosing a capture target.
    fn list_windows(&self) -> Result<Vec<WindowInfo>>;
    /// Start watching the foreground window. Emits a `FocusRecord` on every
    /// change: `focused` is true iff the foreground window belongs to
    /// `target_game_id`; `game_id` is the foreground window's identity.
    fn start(&mut self, target_game_id: &str, tx: Sender<FocusRecord>) -> Result<()>;
    fn stop(&mut self);
}

pub fn default_tracker() -> Result<Box<dyn FocusTracker>> {
    #[cfg(target_os = "linux")]
    {
        return Ok(Box::new(X11Tracker::new()?));
    }
    #[cfg(windows)]
    {
        return Ok(Box::new(WinTracker::new()));
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(Box::new(MacTracker::new()));
    }
    #[allow(unreachable_code)]
    Err(FocusError::Unsupported(
        "no focus backend for this OS".into(),
    ))
}

/// Does a foreground identity match the recording target? Case-insensitive
/// (Windows exe names are case-insensitive; bundle ids are lowercase by
/// convention). An empty target never matches.
pub fn identity_matches(game_id: &str, target: &str) -> bool {
    !target.is_empty() && game_id.eq_ignore_ascii_case(target)
}

/// Emits a record whenever (focused, game_id) changes. Uses `try_send`; if
/// the channel is full the change is retried on the next observation, so a
/// later record still reports the current state.
#[allow(dead_code)]
pub(crate) struct Emitter {
    tx: Sender<FocusRecord>,
    target: String,
    last: Option<(bool, String)>,
    pub(crate) dropped: u64,
    pending: bool,
}

#[allow(dead_code)]
impl Emitter {
    pub(crate) fn new(target: &str, tx: Sender<FocusRecord>) -> Self {
        Self {
            tx,
            target: target.to_string(),
            last: None,
            dropped: 0,
            pending: false,
        }
    }

    /// A change could not be sent yet (channel full); observe again soon.
    pub(crate) fn pending(&self) -> bool {
        self.pending
    }

    /// Returns false once the receiver is gone.
    pub(crate) fn observe(&mut self, game_id: &str) -> bool {
        let focused = identity_matches(game_id, &self.target);
        if let Some((f, g)) = &self.last {
            if *f == focused && g == game_id {
                return true;
            }
        }
        let rec = FocusRecord {
            t_ns: cap_clock::now_ns(),
            focused,
            game_id: game_id.to_string(),
        };
        match self.tx.try_send(rec) {
            Ok(()) => {
                self.last = Some((focused, game_id.to_string()));
                self.pending = false;
                true
            }
            Err(TrySendError::Full(_)) => {
                self.dropped += 1;
                self.pending = true;
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        assert!(identity_matches("EldenRing.exe", "eldenring.exe"));
        assert!(!identity_matches("eldenring.exe", ""));
        assert!(!identity_matches("", ""));
        assert!(!identity_matches("firefox", "eldenring.exe"));
    }

    #[test]
    fn emitter_dedups_and_retries_when_full() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut e = Emitter::new("game.exe", tx);
        assert!(e.observe("explorer.exe"));
        assert!(e.observe("explorer.exe")); // no change: nothing sent
        assert!(e.observe("GAME.exe")); // full: dropped, retried later
        assert_eq!(e.dropped, 1);
        assert!(e.pending());
        let first = rx.try_recv().unwrap();
        assert_eq!(
            (first.focused, first.game_id.as_str()),
            (false, "explorer.exe")
        );
        assert!(e.observe("GAME.exe"));
        let second = rx.try_recv().unwrap();
        assert!(second.focused);
        assert!(!e.pending());
        assert!(rx.try_recv().is_err());
        drop(rx);
        assert!(!e.observe("x"));
    }
}
