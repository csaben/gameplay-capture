//! Focus tracking and game identity: is the target game window in the
//! foreground, and which executable / bundle id is it?

use cap_types::FocusRecord;
use crossbeam_channel::Sender;

pub use cap_types::GameIdentity;

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
    Err(FocusError::Unsupported("no backend implemented yet".into()))
}
