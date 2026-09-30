//! Tray icon for `record --tray` (feature `tray`, Windows and macOS).
//!
//! Red dot = recording, grey = paused (any reason). Menu: Pause/Resume, Quit.
//! Runs the recording session's `tick` from a winit event loop, which also
//! pumps the OS messages tray-icon needs (Win32 message loop / NSApp).

use crate::record::Session;
use anyhow::Result;
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

fn dot(rgb: [u8; 3]) -> Icon {
    const N: u32 = 32;
    let mut px = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let inside = dx * dx + dy * dy <= 13.0 * 13.0;
            px.extend_from_slice(&[rgb[0], rgb[1], rgb[2], if inside { 255 } else { 0 }]);
        }
    }
    Icon::from_rgba(px, N, N).expect("valid icon")
}

struct App {
    session: Option<Session>,
    tray: Option<TrayIcon>,
    pause_item: MenuItem,
    quit_item: MenuItem,
    shown_paused: Option<bool>,
    last_tick: Instant,
}

impl App {
    fn refresh(&mut self) {
        let Some(s) = &self.session else { return };
        let paused = s.pause.is_paused();
        if self.shown_paused != Some(paused) {
            self.shown_paused = Some(paused);
            if let Some(t) = &self.tray {
                let _ = t.set_icon(Some(if paused { dot([140, 140, 140]) } else { dot([220, 30, 30]) }));
            }
            self.pause_item.set_text(if paused { "Resume" } else { "Pause" });
        }
        if let Some(t) = &self.tray {
            let tip = if paused { format!("gamecap: paused ({})", s.pause.describe()) } else { format!("gamecap: recording {}", s.label) };
            let _ = t.set_tooltip(Some(tip));
        }
    }
}

impl ApplicationHandler for App {
    fn new_events(&mut self, _el: &ActiveEventLoop, cause: StartCause) {
        // Create the tray once the loop runs (required on macOS).
        if cause == StartCause::Init {
            let menu = Menu::new();
            let _ = menu.append_items(&[&self.pause_item, &PredefinedMenuItem::separator(), &self.quit_item]);
            self.tray = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("gamecap")
                .with_icon(dot([220, 30, 30]))
                .build()
                .map_err(|e| tracing::error!("tray icon: {e}"))
                .ok();
            self.refresh();
        }
    }

    fn resumed(&mut self, _el: &ActiveEventLoop) {}

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, _event: WindowEvent) {}

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if let Some(s) = &self.session {
                if ev.id == *self.pause_item.id() {
                    s.toggle_user_pause();
                } else if ev.id == *self.quit_item.id() {
                    s.ctrlc.trigger();
                }
            }
        }
        if self.last_tick.elapsed() >= Duration::from_millis(200) {
            self.last_tick = Instant::now();
            let keep = self.session.as_mut().is_some_and(|s| s.tick());
            self.refresh();
            if !keep {
                el.exit();
                return;
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100)));
    }
}

pub fn run(session: Session) -> Result<crate::record::Summary> {
    let event_loop = EventLoop::new()?;
    let mut app = App {
        session: Some(session),
        tray: None,
        pause_item: MenuItem::new("Pause", true, None),
        quit_item: MenuItem::new("Quit", true, None),
        shown_paused: None,
        last_tick: Instant::now(),
    };
    event_loop.run_app(&mut app)?;
    app.tray.take();
    let session = app.session.take().expect("session");
    session.finish()
}
