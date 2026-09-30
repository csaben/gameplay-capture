//! Small OS helpers: live window preview (DWM thumbnail), foreground window, beeps.
//! The preview is composited by DWM onto our own window; nothing touches the
//! target process.

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{BOOL, HWND, RECT};
    use windows::Win32::Graphics::Dwm::*;
    use windows::Win32::System::Diagnostics::Debug::MessageBeep;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsIconic, MB_ICONASTERISK, MB_ICONHAND, MB_OK};

    fn hwnd(id: u64) -> HWND {
        HWND(id as usize as *mut core::ffi::c_void)
    }

    pub struct Preview {
        thumb: isize,
        pub source: u64,
    }

    impl Preview {
        pub fn new(dest: u64, source: u64) -> Option<Self> {
            // SAFETY: plain Win32 calls on window handles; failures are returned.
            let thumb = unsafe { DwmRegisterThumbnail(hwnd(dest), hwnd(source)) }.ok()?;
            Some(Self { thumb, source })
        }

        /// Source size in pixels (None while minimised / gone).
        pub fn source_size(&self) -> Option<(f32, f32)> {
            // SAFETY: valid thumbnail handle.
            let s = unsafe { DwmQueryThumbnailSourceSize(self.thumb) }.ok()?;
            (s.cx > 0 && s.cy > 0).then_some((s.cx as f32, s.cy as f32))
        }

        /// Show the thumbnail in `rect` (physical pixels, client coordinates).
        pub fn show(&self, rect: Option<[i32; 4]>) {
            let mut p = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_VISIBLE | DWM_TNP_RECTDESTINATION | DWM_TNP_SOURCECLIENTAREAONLY | DWM_TNP_OPACITY,
                opacity: 255,
                fSourceClientAreaOnly: BOOL(1),
                ..Default::default()
            };
            match rect {
                Some([l, t, r, b]) => {
                    p.fVisible = BOOL(1);
                    p.rcDestination = RECT { left: l, top: t, right: r, bottom: b };
                }
                None => p.fVisible = BOOL(0),
            }
            // SAFETY: valid handle and properties struct.
            let _ = unsafe { DwmUpdateThumbnailProperties(self.thumb, &p) };
        }
    }

    impl Drop for Preview {
        fn drop(&mut self) {
            // SAFETY: handle from DwmRegisterThumbnail.
            let _ = unsafe { DwmUnregisterThumbnail(self.thumb) };
        }
    }

    pub fn foreground_window() -> Option<u64> {
        // SAFETY: no arguments.
        let h = unsafe { GetForegroundWindow() };
        (!h.0.is_null()).then_some(h.0 as usize as u64)
    }

    pub fn is_minimized(id: u64) -> bool {
        // SAFETY: IsIconic tolerates stale handles.
        unsafe { IsIconic(hwnd(id)) }.as_bool()
    }

    /// Is this keyboard shortcut (modifiers + one key, set-1 scan codes)
    /// registered by another app via RegisterHotKey? Windows routes those to
    /// that app and they never show up in Raw Input, so we'd never see them.
    /// Probes by registering it ourselves for an instant.
    pub fn shortcut_taken(keys: &[u32]) -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        let mut mods = HOT_KEY_MODIFIERS(0);
        let mut main = None;
        for &k in keys {
            match k {
                0x1D | 0xE01D => mods |= MOD_CONTROL,
                0x2A | 0x36 => mods |= MOD_SHIFT,
                0x38 | 0xE038 => mods |= MOD_ALT,
                0xE05B | 0xE05C => mods |= MOD_WIN,
                other if main.is_none() => main = Some(other),
                _ => return false, // two non-modifier keys: not a RegisterHotKey shape
            }
        }
        let Some(sc) = main else { return false };
        // SAFETY: plain calls; the probe registration is undone immediately.
        unsafe {
            let vk = MapVirtualKeyW(sc, MAPVK_VSC_TO_VK_EX);
            if vk == 0 {
                return false;
            }
            const ID: i32 = 0x6763; // arbitrary, thread-local id space
            match RegisterHotKey(None, ID, mods | MOD_NOREPEAT, vk) {
                Ok(()) => {
                    let _ = UnregisterHotKey(None, ID);
                    false
                }
                Err(e) => e.code().0 as u32 == 0x8007_0581, // ERROR_HOTKEY_ALREADY_REGISTERED
            }
        }
    }

    pub fn beep(start: bool) {
        // SAFETY: plain call.
        let _ = unsafe { MessageBeep(if start { MB_ICONASTERISK } else { MB_ICONHAND }) };
        let _ = MB_OK;
    }
}

#[cfg(not(windows))]
mod imp {
    pub struct Preview {
        pub source: u64,
    }
    impl Preview {
        pub fn new(_dest: u64, _source: u64) -> Option<Self> {
            None
        }
        pub fn source_size(&self) -> Option<(f32, f32)> {
            None
        }
        pub fn show(&self, _rect: Option<[i32; 4]>) {}
    }
    pub fn foreground_window() -> Option<u64> {
        None
    }
    pub fn is_minimized(_id: u64) -> bool {
        false
    }
    pub fn beep(_start: bool) {}
    pub fn shortcut_taken(_keys: &[u32]) -> bool {
        false
    }
}

pub use imp::*;
