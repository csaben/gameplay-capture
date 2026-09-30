//! Passive input logging: `trait InputSource` plus per-platform backends.
//!
//! Backends emit every event at full rate with `cap_clock` timestamps. Focus
//! gating happens in the recorder, which drops events while the game is not
//! focused. Never use global hooks (`SetWindowsHookEx`, `rdev`), injection,
//! or anything that opens the game process.

use cap_types::InputEvent;
use crossbeam_channel::Sender;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, InputError>;

pub trait InputSource: Send {
    /// Start delivering events. Must never block on `tx`: use `try_send` and
    /// count drops.
    fn start(&mut self, tx: Sender<InputEvent>) -> Result<()>;
    fn stop(&mut self);
    fn name(&self) -> &'static str;
    /// Events dropped because the channel was full.
    fn dropped(&self) -> u64 {
        0
    }
}

/// Keyboard+mouse and gamepad sources for this OS.
pub fn default_sources() -> Result<Vec<Box<dyn InputSource>>> {
    Err(InputError::Unsupported("no backend implemented yet".into()))
}
