//! Pause reasons and the hotkey / focus-policy observer.
//!
//! Several independent things can pause recording (one-key pause, chat pause,
//! disk cap, a blocked game in front). [`PauseState`] keeps one bit per reason
//! and mirrors "any bit set" into the recorder's shared `paused` flag.
//!
//! [`Hotkeys`] is the pure state machine behind the recorder observer
//! (`cap_recorder::Sources::observer`): it sees the raw input stream from
//! the passive input backends (no global hooks) and the focus records.

use crate::blocklist::Blocklist;
use crate::config::ChatRule;
use cap_recorder::Observed;
use cap_types::{Device, EventKind, InputEvent};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Reason {
    /// One-key pause (Scroll Lock by default) or tray Pause.
    User = 1,
    /// Chat key opened a text box.
    Chat = 2,
    /// Local segments reached `disk_cap_gb`.
    DiskCap = 4,
    /// A blocked game is in the foreground.
    BlockedGame = 8,
}

const ALL: [Reason; 4] = [Reason::User, Reason::Chat, Reason::DiskCap, Reason::BlockedGame];

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::User => "paused by user",
            Reason::Chat => "chat pause",
            Reason::DiskCap => "disk cap reached",
            Reason::BlockedGame => "blocked game in foreground",
        }
    }
}

/// Shared between the main loop, the observer (input-gate thread) and the tray.
#[derive(Clone)]
pub struct PauseState {
    bits: Arc<Mutex<u32>>,
    flag: Arc<AtomicBool>,
}

impl PauseState {
    /// `flag` is `RecorderConfig::paused`.
    pub fn new(flag: Arc<AtomicBool>) -> Self {
        let bits = if flag.load(SeqCst) { Reason::User as u32 } else { 0 };
        Self { bits: Arc::new(Mutex::new(bits)), flag }
    }

    /// Returns true if the reason changed.
    pub fn set(&self, r: Reason, on: bool) -> bool {
        let mut b = self.bits.lock().unwrap();
        let before = *b;
        if on {
            *b |= r as u32;
        } else {
            *b &= !(r as u32);
        }
        self.flag.store(*b != 0, SeqCst);
        before != *b
    }

    pub fn toggle(&self, r: Reason) -> bool {
        let on = !self.is_set(r);
        self.set(r, on);
        on
    }

    pub fn is_set(&self, r: Reason) -> bool {
        *self.bits.lock().unwrap() & r as u32 != 0
    }

    pub fn is_paused(&self) -> bool {
        *self.bits.lock().unwrap() != 0
    }

    pub fn reasons(&self) -> Vec<Reason> {
        let b = *self.bits.lock().unwrap();
        ALL.into_iter().filter(|r| b & *r as u32 != 0).collect()
    }

    pub fn describe(&self) -> String {
        self.reasons().iter().map(|r| r.label()).collect::<Vec<_>>().join(", ")
    }
}

/// Hotkey + focus policy state machine (pure; no OS calls).
pub struct Hotkeys {
    pub pause: PauseState,
    /// 0 = disabled.
    pub pause_key: u32,
    pub chat: Option<ChatRule>,
    pub blocklist: Blocklist,
}

impl Hotkeys {
    fn is_key_down(ev: &InputEvent, code: u32) -> bool {
        code != 0 && ev.device == Device::Keyboard && ev.kind == EventKind::KeyDown && ev.code == code
    }

    pub fn on_input(&mut self, ev: &InputEvent, focused: bool) {
        if Self::is_key_down(ev, self.pause_key) {
            let on = self.pause.toggle(Reason::User);
            tracing::info!("{} (pause key)", if on { "paused" } else { "resumed" });
            return;
        }
        // Chat keys only count while the game has focus (Enter in a browser
        // must not toggle anything).
        let Some(rule) = &self.chat else { return };
        if !focused {
            return;
        }
        if self.pause.is_set(Reason::Chat) {
            if rule.close_keys.iter().any(|&k| Self::is_key_down(ev, k)) {
                self.pause.set(Reason::Chat, false);
                tracing::info!("chat pause ended");
            }
        } else if Self::is_key_down(ev, rule.open_key) {
            self.pause.set(Reason::Chat, true);
            tracing::info!("chat pause started (inputs not logged until chat closes)");
        }
    }

    pub fn on_focus(&mut self, game_id: &str) {
        let blocked = self.blocklist.is_blocked_id(game_id);
        if self.pause.set(Reason::BlockedGame, blocked) {
            if blocked {
                tracing::warn!(game_id, "blocked game in the foreground: recording paused");
            } else {
                tracing::info!("blocked game left the foreground: resuming");
            }
        }
    }

    pub fn observe(&mut self, o: Observed<'_>) {
        match o {
            Observed::Input { event, focused } => self.on_input(event, focused),
            Observed::Focus(rec) => self.on_focus(&rec.game_id),
        }
    }

    pub fn into_observer(mut self) -> cap_recorder::Observer {
        Box::new(move |o| self.observe(o))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::scan;

    fn key(code: u32, down: bool) -> InputEvent {
        InputEvent {
            t_ns: 0,
            device: Device::Keyboard,
            kind: if down { EventKind::KeyDown } else { EventKind::KeyUp },
            code,
            value: down as u8 as f32,
        }
    }

    fn hk(chat: bool) -> (Hotkeys, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        let blocklist: Blocklist = toml::from_str("[[game]]\nid = \"bad.exe\"\nstatus = \"blocked\"\n").unwrap();
        let h = Hotkeys {
            pause: PauseState::new(flag.clone()),
            pause_key: scan::SCROLL_LOCK,
            chat: chat.then(|| ChatRule { open_key: scan::ENTER, close_keys: vec![scan::ENTER, scan::ESCAPE] }),
            blocklist,
        };
        (h, flag)
    }

    #[test]
    fn pause_key_toggles_even_unfocused() {
        let (mut h, flag) = hk(false);
        h.on_input(&key(scan::SCROLL_LOCK, true), false);
        assert!(flag.load(SeqCst));
        h.on_input(&key(scan::SCROLL_LOCK, false), true); // key up ignored
        assert!(flag.load(SeqCst));
        h.on_input(&key(scan::ENTER, true), true); // chat disabled
        h.on_input(&key(scan::SCROLL_LOCK, true), true);
        assert!(!flag.load(SeqCst));
    }

    #[test]
    fn chat_pause_cycle() {
        let (mut h, flag) = hk(true);
        h.on_input(&key(scan::ENTER, true), false);
        assert!(!flag.load(SeqCst), "unfocused Enter ignored");
        h.on_input(&key(scan::ENTER, true), true);
        assert!(flag.load(SeqCst) && h.pause.reasons() == vec![Reason::Chat]);
        h.on_input(&key(0x23, true), true); // typing
        assert!(flag.load(SeqCst));
        h.on_input(&key(scan::ESCAPE, true), true);
        assert!(!flag.load(SeqCst));
        // Chat + user pause overlap: both must clear.
        h.on_input(&key(scan::ENTER, true), true);
        h.on_input(&key(scan::SCROLL_LOCK, true), true);
        h.on_input(&key(scan::ENTER, true), true);
        assert!(flag.load(SeqCst) && h.pause.reasons() == vec![Reason::User]);
        assert_eq!(h.pause.describe(), "paused by user");
    }

    #[test]
    fn blocked_game_in_front() {
        let (mut h, flag) = hk(false);
        h.on_focus("game.exe");
        assert!(!flag.load(SeqCst));
        h.on_focus("BAD.exe");
        assert!(flag.load(SeqCst));
        h.pause.set(Reason::DiskCap, true);
        h.on_focus("explorer.exe");
        assert!(flag.load(SeqCst), "disk cap still holds");
        h.pause.set(Reason::DiskCap, false);
        assert!(!flag.load(SeqCst));
    }
}
