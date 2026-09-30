//! Global hotkeys from passive input: the same Raw Input (`RIDEV_INPUTSINK`)
//! and gilrs sources the recorder uses, so there are no hooks and no
//! `RegisterHotKey` (which would also swallow the combo from the game).
//!
//! A binding is a set of keyboard keys, mouse side/middle buttons and gamepad
//! buttons. It fires once when its last element goes down while all the others
//! are held; other held keys (e.g. W while running) don't block it.

use cap_input::gamepad_code::button as pad;
use cap_types::{Device, EventKind, InputEvent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum Elem {
    /// PS/2 set-1 scan code (0xE0xx for extended keys).
    Key(u32),
    /// Mouse button code (2 middle, 3 back, 4 forward; left/right are not bindable).
    Mouse(u32),
    /// Gamepad button code (cap_input::gamepad_code).
    Pad(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Action {
    StartStop,
    Pause,
}

impl Action {
    pub const ALL: [Action; 2] = [Action::StartStop, Action::Pause];
    pub fn label(self) -> &'static str {
        match self {
            Action::StartStop => "Start / stop recording",
            Action::Pause => "Pause / resume",
        }
    }
}

/// Canonical order: modifiers first, then other keys, mouse, gamepad.
pub fn normalize(set: impl IntoIterator<Item = Elem>) -> Vec<Elem> {
    let mut v: Vec<Elem> = set.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
    v.sort_by_key(|e| {
        let rank = match e {
            Elem::Key(c) if is_modifier(*c) => 0,
            Elem::Key(_) => 1,
            Elem::Mouse(_) => 2,
            Elem::Pad(_) => 3,
        };
        (rank, *e)
    });
    v
}

pub fn is_modifier(code: u32) -> bool {
    matches!(code, 0x1D | 0xE01D | 0x2A | 0x36 | 0x38 | 0xE038 | 0xE05B | 0xE05C)
}

pub fn combo_label(c: &[Elem]) -> String {
    if c.is_empty() {
        return "not set".into();
    }
    c.iter().map(|e| elem_name(*e)).collect::<Vec<_>>().join(" + ")
}

pub fn elem_name(e: Elem) -> String {
    match e {
        Elem::Key(c) => key_name(c),
        Elem::Mouse(c) => match c {
            0 => "Mouse L".into(),
            1 => "Mouse R".into(),
            2 => "Mouse Middle".into(),
            3 => "Mouse Back".into(),
            4 => "Mouse Forward".into(),
            n => format!("Mouse {n}"),
        },
        Elem::Pad(c) => {
            let n = match c {
                pad::SOUTH => "A",
                pad::EAST => "B",
                pad::NORTH => "Y",
                pad::WEST => "X",
                pad::LEFT_TRIGGER => "LB",
                pad::LEFT_TRIGGER2 => "LT",
                pad::RIGHT_TRIGGER => "RB",
                pad::RIGHT_TRIGGER2 => "RT",
                pad::SELECT => "View",
                pad::START => "Menu",
                pad::MODE => "Guide",
                pad::LEFT_THUMB => "LS",
                pad::RIGHT_THUMB => "RS",
                pad::DPAD_UP => "D-pad Up",
                pad::DPAD_DOWN => "D-pad Down",
                pad::DPAD_LEFT => "D-pad Left",
                pad::DPAD_RIGHT => "D-pad Right",
                n => return format!("Pad {n}"),
            };
            format!("Pad {n}")
        }
    }
}

pub fn key_name(code: u32) -> String {
    let s = match code {
        0x01 => "Esc",
        0x0E => "Backspace",
        0x0F => "Tab",
        0x1C => "Enter",
        0x1D => "Ctrl",
        0x2A => "Shift",
        0x36 => "RShift",
        0x38 => "Alt",
        0x39 => "Space",
        0x3A => "CapsLock",
        0x45 => "NumLock",
        0x46 => "ScrollLock",
        0x29 => "`",
        0x0C => "-",
        0x0D => "=",
        0x1A => "[",
        0x1B => "]",
        0x2B => "\\",
        0x27 => ";",
        0x28 => "'",
        0x33 => ",",
        0x34 => ".",
        0x35 => "/",
        0x57 => "F11",
        0x58 => "F12",
        0x47 => "Num7",
        0x48 => "Num8",
        0x49 => "Num9",
        0x4A => "Num-",
        0x4B => "Num4",
        0x4C => "Num5",
        0x4D => "Num6",
        0x4E => "Num+",
        0x4F => "Num1",
        0x50 => "Num2",
        0x51 => "Num3",
        0x52 => "Num0",
        0x53 => "Num.",
        0x37 => "Num*",
        0xE01C => "NumEnter",
        0xE01D => "RCtrl",
        0xE035 => "Num/",
        0xE037 => "PrintScreen",
        0xE038 => "RAlt",
        0xE047 => "Home",
        0xE048 => "Up",
        0xE049 => "PageUp",
        0xE04B => "Left",
        0xE04D => "Right",
        0xE04F => "End",
        0xE050 => "Down",
        0xE051 => "PageDown",
        0xE052 => "Insert",
        0xE053 => "Delete",
        0xE05B => "Win",
        0xE05C => "RWin",
        0xE05D => "Menu",
        0xE11D => "Pause",
        _ => "",
    };
    if !s.is_empty() {
        return s.into();
    }
    const ROWS: [(u32, &str); 4] = [(0x02, "1234567890"), (0x10, "QWERTYUIOP"), (0x1E, "ASDFGHJKL"), (0x2C, "ZXCVBNM")];
    for (start, chars) in ROWS {
        if let Some(ch) = code.checked_sub(start).and_then(|i| chars.chars().nth(i as usize)) {
            return ch.to_string();
        }
    }
    if (0x3B..=0x44).contains(&code) {
        return format!("F{}", code - 0x3A);
    }
    format!("key {code:#x}")
}

/// Map a raw event to (element, pressed). Mouse motion, wheel, axes and the
/// left/right mouse buttons are not bindable.
pub fn classify(e: &InputEvent) -> Option<(Elem, bool)> {
    match (e.device, e.kind) {
        (Device::Keyboard, EventKind::KeyDown) => key_elem(e.code).map(|k| (k, true)),
        (Device::Keyboard, EventKind::KeyUp) => key_elem(e.code).map(|k| (k, false)),
        (Device::Mouse, EventKind::MouseButton) if e.code >= 2 => Some((Elem::Mouse(e.code), e.value > 0.5)),
        (Device::Gamepad, EventKind::Button) => Some((Elem::Pad(e.code), e.value > 0.5)),
        _ => None,
    }
}

fn key_elem(code: u32) -> Option<Elem> {
    // Windows' "fake shift" around extended keys is noise.
    (code != 0xE02A && code != 0xE036).then_some(Elem::Key(code))
}

/// Binding capture: collects everything pressed until all of it is released.
#[derive(Default, Clone)]
pub struct Capture {
    pub pressed: BTreeSet<Elem>,
    pub result: Option<Vec<Elem>>,
}

#[derive(Default)]
pub struct State {
    pub held: BTreeSet<Elem>,
    pub capture: Option<Capture>,
    pub bindings: Vec<(Action, Vec<Elem>)>,
    pub errors: Vec<String>,
    pub sources: Vec<&'static str>,
}

impl State {
    /// Feed one press/release; returns an action to run.
    pub fn feed(&mut self, elem: Elem, pressed: bool) -> Option<Action> {
        if pressed {
            if !self.held.insert(elem) {
                return None; // auto-repeat
            }
            if let Some(c) = &mut self.capture {
                if c.result.is_none() {
                    c.pressed.insert(elem);
                }
                return None;
            }
            self.bindings
                .iter()
                .find(|(_, combo)| !combo.is_empty() && combo.contains(&elem) && combo.iter().all(|e| self.held.contains(e)))
                .map(|(a, _)| *a)
        } else {
            self.held.remove(&elem);
            if let Some(c) = &mut self.capture {
                if c.result.is_none() && !c.pressed.is_empty() && c.pressed.iter().all(|e| !self.held.contains(e)) {
                    c.result = Some(normalize(c.pressed.iter().copied()));
                }
            }
            None
        }
    }
}

pub struct Engine {
    pub state: Arc<Mutex<State>>,
    _sources: Vec<Box<dyn cap_input::InputSource>>,
}

impl Engine {
    /// Start the input sources; `on_action` runs on the engine thread.
    pub fn start(repaint: impl Fn() + Send + 'static, on_action: impl Fn(Action) + Send + 'static) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let (tx, rx) = crossbeam_channel::bounded::<InputEvent>(8192);
        let mut sources = match cap_input::default_sources() {
            Ok(s) => s,
            Err(e) => {
                state.lock().unwrap().errors.push(format!("input: {e}"));
                Vec::new()
            }
        };
        for s in &mut sources {
            match s.start(tx.clone()) {
                Ok(()) => state.lock().unwrap().sources.push(s.name()),
                Err(e) => state.lock().unwrap().errors.push(format!("{}: {e}", s.name())),
            }
        }
        let st = state.clone();
        std::thread::Builder::new()
            .name("hotkeys".into())
            .spawn(move || {
                for ev in rx {
                    let Some((elem, pressed)) = classify(&ev) else { continue };
                    let (action, capturing) = {
                        let mut s = st.lock().unwrap();
                        (s.feed(elem, pressed), s.capture.is_some())
                    };
                    if let Some(a) = action {
                        on_action(a);
                    }
                    if capturing || action.is_some() {
                        repaint();
                    }
                }
            })
            .expect("spawn hotkey thread");
        Self { state, _sources: sources }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: Elem = Elem::Key(0x1D);
    const SHIFT: Elem = Elem::Key(0x2A);
    const F9: Elem = Elem::Key(0x43);
    const W: Elem = Elem::Key(0x11);
    const LB: Elem = Elem::Pad(pad::LEFT_TRIGGER);
    const RB: Elem = Elem::Pad(pad::RIGHT_TRIGGER);

    fn st() -> State {
        State {
            bindings: vec![(Action::StartStop, vec![CTRL, SHIFT, F9]), (Action::Pause, vec![LB, RB])],
            ..Default::default()
        }
    }

    #[test]
    fn fires_once_on_completion_even_with_extra_keys_held() {
        let mut s = st();
        assert_eq!(s.feed(W, true), None);
        assert_eq!(s.feed(CTRL, true), None);
        assert_eq!(s.feed(SHIFT, true), None);
        assert_eq!(s.feed(F9, true), Some(Action::StartStop));
        assert_eq!(s.feed(F9, true), None, "auto-repeat must not re-fire");
        s.feed(F9, false);
        assert_eq!(s.feed(F9, true), Some(Action::StartStop), "re-press fires again");
    }

    #[test]
    fn order_independent_and_gamepad() {
        let mut s = st();
        assert_eq!(s.feed(RB, true), None);
        assert_eq!(s.feed(LB, true), Some(Action::Pause));
        let mut s = st();
        assert_eq!(s.feed(F9, true), None);
        assert_eq!(s.feed(SHIFT, true), None);
        assert_eq!(s.feed(CTRL, true), Some(Action::StartStop));
    }

    #[test]
    fn capture_waits_for_full_release_and_suppresses_actions() {
        let mut s = st();
        s.capture = Some(Capture::default());
        s.feed(CTRL, true);
        s.feed(SHIFT, true);
        assert_eq!(s.feed(F9, true), None, "no actions while capturing");
        s.feed(F9, false);
        assert!(s.capture.as_ref().unwrap().result.is_none(), "still holding ctrl+shift");
        s.feed(CTRL, false);
        s.feed(SHIFT, false);
        assert_eq!(s.capture.as_ref().unwrap().result, Some(vec![CTRL, SHIFT, F9]));
        // Later presses don't change a finished capture.
        s.feed(W, true);
        assert_eq!(s.capture.as_ref().unwrap().result, Some(vec![CTRL, SHIFT, F9]));
    }

    #[test]
    fn mixed_keyboard_gamepad_label() {
        assert_eq!(combo_label(&normalize([Elem::Pad(pad::SELECT), F9, CTRL])), "Ctrl + F9 + Pad View");
        assert_eq!(key_name(0x10), "Q");
        assert_eq!(key_name(0x44), "F10");
        assert_eq!(key_name(0x02), "1");
    }

    #[test]
    fn classify_ignores_motion_and_primary_buttons() {
        let ev = |device, kind, code, value| InputEvent { t_ns: 0, device, kind, code, value };
        assert_eq!(classify(&ev(Device::Mouse, EventKind::MouseMove, 0, 5.0)), None);
        assert_eq!(classify(&ev(Device::Mouse, EventKind::MouseButton, 0, 1.0)), None);
        assert_eq!(classify(&ev(Device::Mouse, EventKind::MouseButton, 4, 1.0)), Some((Elem::Mouse(4), true)));
        assert_eq!(classify(&ev(Device::Gamepad, EventKind::Button, pad::RIGHT_TRIGGER2, 0.8)), Some((Elem::Pad(9), true)));
        assert_eq!(classify(&ev(Device::Keyboard, EventKind::KeyDown, 0xE02A, 1.0)), None);
    }
}
