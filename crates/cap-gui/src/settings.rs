//! GUI settings, stored as `gui.json` next to gamecap's config.toml (the
//! recorder config rejects unknown keys, so the GUI keeps its own file).

use crate::hotkeys::{Action, Elem};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum HotkeyTarget {
    /// Record whatever window is in the foreground when the hotkey is pressed.
    Foreground,
    /// Record the window selected in the list.
    Selected,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub start_stop: Vec<Elem>,
    pub pause: Vec<Elem>,
    pub hotkey_target: HotkeyTarget,
    pub beep: bool,
    pub upload: bool,
    pub viewer_url: String,
    /// Path to gamecap.exe (default: next to gamecap-gui.exe).
    pub gamecap_exe: String,
    /// Folder with the FFmpeg DLLs if they aren't next to gamecap.exe.
    pub ffmpeg_dir: String,
    /// Game id selected last time (re-selected on start).
    pub last_game: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // Ctrl + Shift + F9
            start_stop: vec![Elem::Key(0x1D), Elem::Key(0x2A), Elem::Key(0x43)],
            pause: vec![],
            hotkey_target: HotkeyTarget::Foreground,
            beep: true,
            upload: true,
            viewer_url: "http://127.0.0.1:8787".into(),
            gamecap_exe: String::new(),
            ffmpeg_dir: String::new(),
            last_game: String::new(),
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        let dir = std::env::var_os("GAMECAP_CONFIG")
            .and_then(|c| PathBuf::from(c).parent().map(|p| p.to_path_buf()))
            .or_else(|| dirs::config_dir().map(|d| d.join("gamecap")))
            .unwrap_or_else(|| PathBuf::from("."));
        dir.join("gui.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let p = Self::path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&p, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn binding(&self, a: Action) -> &Vec<Elem> {
        match a {
            Action::StartStop => &self.start_stop,
            Action::Pause => &self.pause,
        }
    }

    pub fn binding_mut(&mut self, a: Action) -> &mut Vec<Elem> {
        match a {
            Action::StartStop => &mut self.start_stop,
            Action::Pause => &mut self.pause,
        }
    }

    pub fn bindings(&self) -> Vec<(Action, Vec<Elem>)> {
        Action::ALL.iter().map(|a| (*a, self.binding(*a).clone())).collect()
    }
}
