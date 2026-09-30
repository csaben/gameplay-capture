//! Linux focus tracking over X11 (also XWayland) with `x11rb`.
//!
//! * Foreground window: `_NET_ACTIVE_WINDOW` on the root window, watched via
//!   `PropertyNotify` (root selected with `PropertyChangeMask`). Without an
//!   EWMH window manager (e.g. bare Xvfb) the property is absent and
//!   `GetInputFocus` is used instead (walked up to the top-level window).
//!   A 250 ms re-query covers both cases (e.g. focus changes that produce no
//!   property event when there is no WM).
//! * Identity: `_NET_WM_PID`, else the X-Resource extension's client PID;
//!   then `/proc/<pid>/exe` basename (Wine/Proton -> the Windows exe name).
//!   Only `/proc` metadata is read; no process is opened or traced.
//! * Wayland: native Wayland windows are invisible to X11. Under a Wayland
//!   session with XWayland, `_NET_ACTIVE_WINDOW` is `None` whenever a native
//!   Wayland window is focused, which gates input *off* (safe). Without any X
//!   display `new()` returns `Unsupported`.

use crate::procname::linux_exe_for_pid;
use crate::{Emitter, FocusError, FocusTracker, GameIdentity, Result, WindowInfo};
use cap_types::FocusRecord;
use crossbeam_channel::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::res::{ClientIdMask, ClientIdSpec, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, MapState, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

const REQUERY_NS: i64 = 250_000_000;

fn err<E: std::fmt::Display>(e: E) -> FocusError {
    FocusError::Backend(format!("x11: {e}"))
}

struct Atoms {
    net_active_window: Atom,
    net_wm_pid: Atom,
    net_client_list: Atom,
    net_wm_name: Atom,
    utf8_string: Atom,
}

/// An X11 connection plus the queries the tracker needs.
pub struct X11Conn {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
    has_res: bool,
}

impl X11Conn {
    pub fn connect(display: Option<&str>) -> Result<Self> {
        let (conn, screen) = x11rb::connect(display).map_err(err)?;
        let root = conn.setup().roots[screen].root;
        let intern = |name: &[u8]| -> Result<Atom> {
            Ok(conn
                .intern_atom(false, name)
                .map_err(err)?
                .reply()
                .map_err(err)?
                .atom)
        };
        let atoms = Atoms {
            net_active_window: intern(b"_NET_ACTIVE_WINDOW")?,
            net_wm_pid: intern(b"_NET_WM_PID")?,
            net_client_list: intern(b"_NET_CLIENT_LIST")?,
            net_wm_name: intern(b"_NET_WM_NAME")?,
            utf8_string: intern(b"UTF8_STRING")?,
        };
        let has_res = conn
            .extension_information(x11rb::protocol::res::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some();
        Ok(Self {
            conn,
            root,
            atoms,
            has_res,
        })
    }

    fn prop32(&self, win: Window, prop: Atom, ty: impl Into<Atom>, max: u32) -> Option<Vec<u32>> {
        let r = self
            .conn
            .get_property(false, win, prop, ty, 0, max)
            .ok()?
            .reply()
            .ok()?;
        if r.type_ == u32::from(AtomEnum::NONE) {
            return None;
        }
        let v: Vec<u32> = r.value32()?.collect();
        Some(v)
    }

    /// The focused top-level window, or None if nothing (or the root) is focused.
    pub fn active_window(&self) -> Option<Window> {
        if let Some(v) = self.prop32(self.root, self.atoms.net_active_window, AtomEnum::WINDOW, 1) {
            // EWMH WM present: trust it (0 = no active window).
            return v.first().copied().filter(|&w| w != 0);
        }
        // No EWMH: raw X focus. 0 = None, 1 = PointerRoot.
        let focus = self.conn.get_input_focus().ok()?.reply().ok()?.focus;
        if focus <= 1 || focus == self.root {
            return None;
        }
        self.toplevel(focus)
    }

    fn toplevel(&self, mut w: Window) -> Option<Window> {
        for _ in 0..64 {
            let t = self.conn.query_tree(w).ok()?.reply().ok()?;
            if t.parent == self.root || t.parent == 0 {
                return Some(w);
            }
            w = t.parent;
        }
        Some(w)
    }

    pub fn window_pid(&self, win: Window) -> Option<u32> {
        if let Some(v) = self.prop32(win, self.atoms.net_wm_pid, AtomEnum::CARDINAL, 1) {
            if let Some(&pid) = v.first() {
                if pid != 0 {
                    return Some(pid);
                }
            }
        }
        if self.has_res {
            let spec = ClientIdSpec {
                client: win,
                mask: ClientIdMask::LOCAL_CLIENT_PID,
            };
            let r = self.conn.res_query_client_ids(&[spec]).ok()?.reply().ok()?;
            return r
                .ids
                .iter()
                .find_map(|id| id.value.first().copied())
                .filter(|&p| p != 0);
        }
        None
    }

    pub fn window_title(&self, win: Window) -> String {
        let get = |prop: Atom, ty: Atom| -> Option<String> {
            let r = self
                .conn
                .get_property(false, win, prop, ty, 0, 1024)
                .ok()?
                .reply()
                .ok()?;
            if r.value.is_empty() {
                return None;
            }
            Some(String::from_utf8_lossy(&r.value).into_owned())
        };
        get(self.atoms.net_wm_name, self.atoms.utf8_string)
            .or_else(|| get(AtomEnum::WM_NAME.into(), AtomEnum::STRING.into()))
            .unwrap_or_default()
    }

    /// Identity (`game_id`) of a window: exe name of its owning process.
    pub fn identity(&self, win: Window) -> (u32, String) {
        match self.window_pid(win) {
            Some(pid) => (pid, linux_exe_for_pid(pid).unwrap_or_default()),
            None => (0, String::new()),
        }
    }

    pub fn foreground_game_id(&self) -> String {
        self.active_window()
            .map(|w| self.identity(w).1)
            .unwrap_or_default()
    }

    pub fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let wins: Vec<Window> = match self.prop32(
            self.root,
            self.atoms.net_client_list,
            AtomEnum::WINDOW,
            4096,
        ) {
            Some(v) => v,
            None => {
                let tree = self
                    .conn
                    .query_tree(self.root)
                    .map_err(err)?
                    .reply()
                    .map_err(err)?;
                tree.children
                    .into_iter()
                    .filter(|&w| {
                        self.conn
                            .get_window_attributes(w)
                            .ok()
                            .and_then(|c| c.reply().ok())
                            .map(|a| a.map_state == MapState::VIEWABLE && !a.override_redirect)
                            .unwrap_or(false)
                    })
                    .collect()
            }
        };
        let mut out = Vec::new();
        for w in wins {
            let title = self.window_title(w);
            if title.is_empty() {
                continue;
            }
            let (pid, game_id) = self.identity(w);
            out.push(WindowInfo {
                native_id: w as u64,
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

    fn watch_root(&self) -> Result<()> {
        let aux = ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE);
        self.conn
            .change_window_attributes(self.root, &aux)
            .map_err(err)?
            .check()
            .map_err(err)?;
        Ok(())
    }
}

/// X11 focus tracker (see module docs).
pub struct X11Tracker {
    display: Option<String>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl X11Tracker {
    /// Uses `$DISPLAY`. Fails with `Unsupported` on a Wayland session that has
    /// no X display at all.
    pub fn new() -> Result<Self> {
        let display = std::env::var("DISPLAY").ok().filter(|s| !s.is_empty());
        if display.is_none() {
            let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
            return Err(FocusError::Unsupported(if wayland {
                "Wayland session without X11/XWayland: there is no general Wayland API to query the focused \
                 window, so input cannot be focus-gated. Run the game under XWayland (DISPLAY set) or use an \
                 X11 session."
                    .into()
            } else {
                "no X display ($DISPLAY unset); focus tracking needs X11 or XWayland".into()
            }));
        }
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            tracing::warn!("Wayland session: only XWayland windows can be seen; native Wayland windows count as unfocused");
        }
        Ok(Self::with_display(display))
    }

    /// Explicit display name (e.g. `":99"` for Xvfb); `None` = `$DISPLAY`.
    pub fn with_display(display: Option<String>) -> Self {
        Self {
            display,
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    fn connect(&self) -> Result<X11Conn> {
        X11Conn::connect(self.display.as_deref())
    }
}

impl FocusTracker for X11Tracker {
    fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        self.connect()?.list_windows()
    }

    fn start(&mut self, target_game_id: &str, tx: Sender<FocusRecord>) -> Result<()> {
        if self.thread.is_some() {
            return Ok(());
        }
        let x = self.connect()?;
        x.watch_root()?;
        let mut em = Emitter::new(target_game_id, tx);
        em.observe(&x.foreground_game_id()); // initial record
        self.stop.store(false, Ordering::SeqCst);
        let stop = self.stop.clone();
        let h = std::thread::Builder::new()
            .name("cap-focus-x11".into())
            .spawn(move || run(x, em, stop))
            .map_err(|e| FocusError::Backend(format!("spawn x11 thread: {e}")))?;
        self.thread = Some(h);
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for X11Tracker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(x: X11Conn, mut em: Emitter, stop: Arc<AtomicBool>) {
    let mut last_query = cap_clock::now_ns();
    while !stop.load(Ordering::Relaxed) {
        let mut changed = false;
        loop {
            match x.conn.poll_for_event() {
                Ok(Some(Event::PropertyNotify(e))) if e.atom == x.atoms.net_active_window => {
                    changed = true
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => {
                    tracing::error!(
                        "x11 connection lost: {e}; focus tracking stopped (reporting unfocused)"
                    );
                    em.observe("");
                    return;
                }
            }
        }
        let now = cap_clock::now_ns();
        if changed || now - last_query >= REQUERY_NS {
            last_query = now;
            if !em.observe(&x.foreground_game_id()) {
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
