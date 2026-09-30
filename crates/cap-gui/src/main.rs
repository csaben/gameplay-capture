//! `gamecap-gui`: pick a window, start/stop recording, global keyboard / mouse /
//! gamepad hotkeys, and browse local recordings. Recording itself runs in a
//! `gamecap record --control-stdin --status-json` child process.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod child;
mod hotkeys;
mod platform;
mod settings;

use child::{Launcher, Recording};
use crossbeam_channel::Receiver;
use eframe::egui::{
    self, vec2, Align, Align2, Color32, CornerRadius, FontId, Layout, Rect, RichText, Sense, Stroke, StrokeKind, Ui,
};
use hotkeys::{Action, Elem, Engine};
use serde_json::Value;
use settings::{HotkeyTarget, Settings};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const BG: Color32 = Color32::from_rgb(0x12, 0x14, 0x19);
const PANEL: Color32 = Color32::from_rgb(0x18, 0x1b, 0x22);
const CARD: Color32 = Color32::from_rgb(0x20, 0x24, 0x2d);
const BORDER: Color32 = Color32::from_rgb(0x2e, 0x33, 0x3f);
const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe8, 0xee);
const MUTED: Color32 = Color32::from_rgb(0x8b, 0x92, 0xa3);
const ACCENT: Color32 = Color32::from_rgb(0x5b, 0x8d, 0xef);
const REC: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);
const AMBER: Color32 = Color32::from_rgb(0xf0, 0xa0, 0x3c);
const GREEN: Color32 = Color32::from_rgb(0x46, 0xc2, 0x7a);

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("gamecap")
            .with_inner_size([1120.0, 740.0])
            .with_min_inner_size([860.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("gamecap", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Record,
    Hotkeys,
    Recordings,
}

#[derive(Clone, Debug)]
struct WinItem {
    id: u64,
    pid: u32,
    game: String,
    title: String,
}

enum Consent {
    Ok,
    Needed(String),
    Error(String),
}

struct SessionRow {
    id: String,
    game: String,
    segments: usize,
    partial: usize,
    bytes: u64,
    path: PathBuf,
}

struct App {
    settings: Settings,
    launcher: Launcher,
    engine: Engine,
    actions: Receiver<Action>,
    tab: Tab,
    tracker: Option<Box<dyn cap_focus::FocusTracker>>,
    windows: Vec<WinItem>,
    windows_at: Option<Instant>,
    filter: String,
    selected: Option<WinItem>,
    rec: Option<Recording>,
    rec_target: Option<WinItem>,
    last_result: Option<(String, bool)>,
    toast: Option<(String, Instant, bool)>,
    consent: Consent,
    capturing: Option<Action>,
    preview: Option<platform::Preview>,
    preview_rect: Option<[i32; 4]>,
    my_hwnd: Option<u64>,
    sessions: Vec<SessionRow>,
    sessions_root: Option<PathBuf>,
    upload_user: Option<String>,
    sessions_at: Option<Instant>,
    status_text: String,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        style(&cc.egui_ctx);
        let settings = Settings::load();
        let launcher = Launcher::resolve(Some(&settings.gamecap_exe), Some(&settings.ffmpeg_dir));
        let (tx, actions) = crossbeam_channel::unbounded();
        let ctx = cc.egui_ctx.clone();
        let ctx2 = cc.egui_ctx.clone();
        let engine = Engine::start(
            move || ctx.request_repaint(),
            move |a| {
                let _ = tx.send(a);
                ctx2.request_repaint();
            },
        );
        engine.state.lock().unwrap().bindings = settings.bindings();
        let consent = query_consent(&launcher);
        let mut app = Self {
            tracker: cap_focus::default_tracker().ok(),
            settings,
            launcher,
            engine,
            actions,
            tab: Tab::Record,
            windows: Vec::new(),
            windows_at: None,
            filter: String::new(),
            selected: None,
            rec: None,
            rec_target: None,
            last_result: None,
            toast: None,
            consent,
            capturing: None,
            preview: None,
            preview_rect: None,
            my_hwnd: None,
            sessions: Vec::new(),
            sessions_root: None,
            upload_user: None,
            sessions_at: None,
            status_text: String::new(),
        };
        app.refresh_windows();
        if !app.settings.last_game.is_empty() {
            app.selected = app.windows.iter().find(|w| w.game.eq_ignore_ascii_case(&app.settings.last_game)).cloned();
        }
        app
    }

    fn toast(&mut self, msg: impl Into<String>, error: bool) {
        self.toast = Some((msg.into(), Instant::now(), error));
    }

    fn refresh_windows(&mut self) {
        let Some(t) = &self.tracker else { return };
        let me = std::process::id();
        if let Ok(ws) = t.list_windows() {
            let mut v: Vec<WinItem> = ws
                .into_iter()
                .filter(|w| w.pid != me && !w.title.trim().is_empty())
                .map(|w| WinItem { id: w.native_id, pid: w.pid, game: w.identity.game_id, title: w.title })
                .collect();
            v.sort_by_key(|w| (w.game.to_lowercase(), w.title.to_lowercase()));
            self.windows = v;
        }
        self.windows_at = Some(Instant::now());
        // Follow a window whose handle is gone but whose game is back (restart).
        if let Some(sel) = &self.selected {
            if !self.windows.iter().any(|w| w.id == sel.id) {
                self.selected = self.windows.iter().find(|w| w.game == sel.game).cloned().or(self.selected.clone());
            }
        }
    }

    fn recording_active(&self) -> bool {
        self.rec.as_ref().is_some_and(|r| r.exit.is_none())
    }

    fn start(&mut self, target: WinItem) {
        if !matches!(self.consent, Consent::Ok) {
            self.toast("Accept the terms first", true);
            return;
        }
        if self.rec.is_some() {
            return;
        }
        let ctx_none = || {};
        let _ = ctx_none;
        match self.launcher.start_record(target.id, !self.settings.upload, || {}) {
            Ok(r) => {
                if self.settings.beep {
                    platform::beep(true);
                }
                self.last_result = None;
                self.toast(format!("Recording {}", target.game), false);
                self.settings.last_game = target.game.clone();
                let _ = self.settings.save();
                self.rec_target = Some(target);
                self.rec = Some(r);
            }
            Err(e) => {
                if self.settings.beep {
                    platform::beep(false);
                }
                self.toast(format!("Could not start: {e:#}"), true);
            }
        }
    }

    fn stop(&mut self) {
        if let Some(r) = &mut self.rec {
            if self.settings.beep && r.stop_requested.is_none() {
                platform::beep(false);
            }
            r.stop();
        }
    }

    fn hotkey_target(&self) -> Option<WinItem> {
        if self.settings.hotkey_target == HotkeyTarget::Foreground {
            if let Some(fg) = platform::foreground_window() {
                if Some(fg) != self.my_hwnd {
                    if let Some(w) = self.windows.iter().find(|w| w.id == fg) {
                        return Some(w.clone());
                    }
                    // Not in our (2 s old) list yet: ask the tracker directly.
                    if let Some(t) = &self.tracker {
                        if let Ok(ws) = t.list_windows() {
                            if let Some(w) = ws.into_iter().find(|w| w.native_id == fg) {
                                return Some(WinItem { id: w.native_id, pid: w.pid, game: w.identity.game_id, title: w.title });
                            }
                        }
                    }
                }
            }
        }
        self.selected.clone()
    }

    fn on_action(&mut self, a: Action) {
        match a {
            Action::StartStop => {
                if self.rec.as_ref().is_some_and(|r| r.exit.is_none() && r.stop_requested.is_none()) {
                    self.stop();
                } else if self.rec.is_none() {
                    match self.hotkey_target() {
                        Some(t) => self.start(t),
                        None => {
                            if self.settings.beep {
                                platform::beep(false);
                            }
                            self.toast("Hotkey: no window to record (select one first)", true);
                        }
                    }
                }
            }
            Action::Pause => {
                if let Some(r) = &mut self.rec {
                    r.send("pause");
                }
            }
        }
    }

    fn poll_recording(&mut self) {
        let Some(r) = &mut self.rec else { return };
        if r.stop_requested.is_some_and(|t| t.elapsed() > Duration::from_secs(240)) {
            r.kill();
        }
        if !r.poll() {
            return;
        }
        let st = r.state.lock().unwrap();
        let game = self.rec_target.as_ref().map(|t| t.game.clone()).unwrap_or_default();
        let result = match (&st.stopped, r.exit) {
            (Some(s), _) => (
                format!(
                    "{game}: {} segment(s), {} frames, {} dropped, {} inputs{}",
                    s["segments"],
                    s["frames"],
                    s["dropped"],
                    s["inputs"],
                    match s["outstanding"].as_u64() {
                        Some(n) if n > 0 => format!(", {n} upload(s) pending (next run)"),
                        _ => String::new(),
                    }
                ),
                false,
            ),
            (None, code) => {
                let msg = st
                    .log
                    .iter()
                    .rev()
                    .find(|l| l.starts_with("error"))
                    .or(st.log.back())
                    .cloned()
                    .unwrap_or_else(|| format!("gamecap exited with {code:?}"));
                (msg, true)
            }
        };
        drop(st);
        if result.1 && self.settings.beep {
            platform::beep(false);
        }
        self.last_result = Some(result);
        self.rec = None;
        self.sessions_at = None;
    }

    fn refresh_sessions(&mut self) {
        self.sessions_at = Some(Instant::now());
        if self.upload_user.is_none() {
            // "upload target: s3 http://… bucket gameplay as inari"
            self.upload_user = self.launcher.run(&["status"]).ok().and_then(|s| {
                s.lines().find(|l| l.starts_with("upload target:")).and_then(|l| l.rsplit_once(" as ").map(|(_, u)| u.trim().to_string()))
            });
        }
        if self.sessions_root.is_none() {
            self.sessions_root = self.launcher.sessions_root().ok();
        }
        let Some(root) = &self.sessions_root else { return };
        let mut rows = Vec::new();
        if let Ok(rd) = std::fs::read_dir(root) {
            for e in rd.flatten() {
                let p = e.path();
                if !p.is_dir() {
                    continue;
                }
                let mut row = SessionRow {
                    id: e.file_name().to_string_lossy().into_owned(),
                    game: String::new(),
                    segments: 0,
                    partial: 0,
                    bytes: 0,
                    path: p.clone(),
                };
                if let Ok(segs) = std::fs::read_dir(&p) {
                    for s in segs.flatten() {
                        let name = s.file_name().to_string_lossy().into_owned();
                        if !name.starts_with("seg_") {
                            continue;
                        }
                        if name.contains('.') {
                            row.partial += 1;
                            continue;
                        }
                        row.segments += 1;
                        if let Ok(files) = std::fs::read_dir(s.path()) {
                            row.bytes += files.flatten().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum::<u64>();
                        }
                        if row.game.is_empty() {
                            if let Ok(m) = std::fs::read_to_string(s.path().join("manifest.json")) {
                                if let Ok(v) = serde_json::from_str::<Value>(&m) {
                                    row.game = v["game_id"].as_str().unwrap_or_default().to_string();
                                }
                            }
                        }
                    }
                }
                rows.push(row);
            }
        }
        rows.sort_by(|a, b| b.id.cmp(&a.id));
        self.sessions = rows;
    }

    // ---------------------------------------------------------------- UI

    fn top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("gamecap").size(20.0).strong().color(TEXT));
            ui.add_space(18.0);
            for (t, name) in [(Tab::Record, "Record"), (Tab::Hotkeys, "Hotkeys"), (Tab::Recordings, "Recordings")] {
                let sel = self.tab == t;
                let txt = RichText::new(name).size(15.0).color(if sel { TEXT } else { MUTED });
                let b = egui::Button::new(txt).fill(if sel { CARD } else { Color32::TRANSPARENT }).min_size(vec2(96.0, 30.0));
                if ui.add(b).clicked() {
                    self.tab = t;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(6.0);
                let (text, color) = self.pill();
                pill(ui, &text, color);
                if let Some((msg, at, err)) = &self.toast {
                    if at.elapsed() < Duration::from_secs(5) {
                        ui.add_space(10.0);
                        ui.label(RichText::new(msg).color(if *err { REC } else { MUTED }));
                    }
                }
            });
        });
    }

    fn pill(&self) -> (String, Color32) {
        match &self.rec {
            Some(r) => {
                let st = r.state.lock().unwrap();
                let game = self.rec_target.as_ref().map(|t| t.game.as_str()).unwrap_or("");
                if r.stop_requested.is_some() {
                    let q = st.status.as_ref().map(|s| s["queue"]["pending"].as_u64().unwrap_or(0) + s["queue"]["uploading"].as_u64().unwrap_or(0));
                    return (format!("Stopping… uploads left {}", q.unwrap_or(0)), MUTED);
                }
                let secs = r.started_at.elapsed().as_secs();
                let t = format!("{:02}:{:02}", secs / 60, secs % 60);
                match st.status.as_ref().and_then(|s| s["state"].as_str()) {
                    Some("paused") => (format!("PAUSED {t}  {game}"), AMBER),
                    Some("waiting") => (format!("STARTING {t}  {game}"), AMBER),
                    Some(_) => (format!("REC {t}  {game}"), REC),
                    None => ("Starting…".into(), MUTED),
                }
            }
            None => (format!("Idle · {} to record", hotkeys::combo_label(&self.settings.start_stop)), MUTED),
        }
    }

    fn window_list(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Windows").size(16.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("⟳ Refresh").clicked() {
                    self.refresh_windows();
                }
            });
        });
        ui.add_space(4.0);
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter by game or title").desired_width(f32::INFINITY));
        ui.add_space(6.0);
        let f = self.filter.to_lowercase();
        let items: Vec<WinItem> = self
            .windows
            .iter()
            .filter(|w| f.is_empty() || w.game.to_lowercase().contains(&f) || w.title.to_lowercase().contains(&f))
            .cloned()
            .collect();
        let rec_id = self.rec_target.as_ref().filter(|_| self.rec.is_some()).map(|t| t.id);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for w in items {
                let sel = self.selected.as_ref().is_some_and(|s| s.id == w.id);
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::click());
                let p = ui.painter().with_clip_rect(rect);
                let fill = if sel {
                    Color32::from_rgb(0x26, 0x33, 0x4f)
                } else if resp.hovered() {
                    CARD
                } else {
                    Color32::TRANSPARENT
                };
                p.rect_filled(rect.shrink(1.0), CornerRadius::same(6), fill);
                if sel {
                    p.rect_filled(Rect::from_min_size(rect.min + vec2(1.0, 8.0), vec2(3.0, 30.0)), CornerRadius::same(2), ACCENT);
                }
                let mut x = rect.min.x + 12.0;
                if rec_id == Some(w.id) {
                    p.circle_filled(egui::pos2(x + 4.0, rect.min.y + 15.0), 4.0, REC);
                    x += 14.0;
                }
                p.text(egui::pos2(x, rect.min.y + 7.0), Align2::LEFT_TOP, &w.game, FontId::proportional(14.5), TEXT);
                p.text(egui::pos2(rect.min.x + 12.0, rect.min.y + 26.0), Align2::LEFT_TOP, &w.title, FontId::proportional(12.5), MUTED);
                if resp.clicked() {
                    self.selected = Some(w.clone());
                }
                if resp.double_clicked() && self.rec.is_none() {
                    self.start(w.clone());
                }
            }
        });
    }

    fn record_tab(&mut self, ui: &mut Ui) {
        let target = if self.rec.is_some() { self.rec_target.clone() } else { self.selected.clone() };
        let Some(target) = target else {
            ui.add_space(80.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Select a window on the left").size(20.0).color(TEXT));
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!(
                        "…or press {} while a game is in the foreground.",
                        hotkeys::combo_label(&self.settings.start_stop)
                    ))
                    .color(MUTED),
                );
                ui.label(RichText::new("Run games in borderless windowed mode; exclusive fullscreen can capture black frames.").color(MUTED));
            });
            return;
        };
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(&target.game).size(22.0).strong().color(TEXT));
                ui.label(RichText::new(format!("{}   ·   pid {}   ·   {:#x}", target.title, target.pid, target.id)).color(MUTED));
            });
        });
        ui.add_space(8.0);

        // Live preview (DWM thumbnail composited into this rect).
        if self.preview.as_ref().map(|p| p.source) != Some(target.id) {
            self.preview = self.my_hwnd.and_then(|me| platform::Preview::new(me, target.id));
        }
        let avail = ui.available_width();
        let max_h = (ui.available_height() - 230.0).max(160.0);
        let (sw, sh) = self.preview.as_ref().and_then(|p| p.source_size()).unwrap_or((16.0, 9.0));
        let scale = (avail / sw).min(max_h / sh);
        let size = vec2(sw * scale, sh * scale);
        let (outer, _) = ui.allocate_exact_size(vec2(avail, size.y), Sense::hover());
        let rect = Rect::from_center_size(outer.center(), size);
        ui.painter().rect_filled(outer, CornerRadius::same(8), Color32::from_rgb(0x0b, 0x0c, 0x10));
        let minimized = platform::is_minimized(target.id);
        if self.preview.is_some() && !minimized {
            let ppp = ui.ctx().pixels_per_point();
            self.preview_rect = Some([
                (rect.min.x * ppp) as i32,
                (rect.min.y * ppp) as i32,
                (rect.max.x * ppp) as i32,
                (rect.max.y * ppp) as i32,
            ]);
        } else {
            ui.painter().text(
                outer.center(),
                Align2::CENTER_CENTER,
                if minimized { "Window is minimized: restore it to preview and record" } else { "No preview" },
                FontId::proportional(15.0),
                MUTED,
            );
        }
        ui.add_space(12.0);

        // Controls.
        ui.horizontal(|ui| {
            let active = self.recording_active();
            let stopping = self.rec.as_ref().is_some_and(|r| r.stop_requested.is_some());
            if !active {
                let b = egui::Button::new(RichText::new("Start recording").size(17.0).strong().color(Color32::WHITE))
                    .fill(REC)
                    .min_size(vec2(210.0, 44.0))
                    .corner_radius(CornerRadius::same(8));
                if ui.add_enabled(matches!(self.consent, Consent::Ok), b).clicked() {
                    self.start(target.clone());
                }
            } else {
                let label = if stopping { "Stopping… (click to skip upload wait)" } else { "Stop" };
                let b = egui::Button::new(RichText::new(label).size(17.0).strong().color(Color32::WHITE))
                    .fill(Color32::from_rgb(0x3a, 0x3f, 0x4b))
                    .min_size(vec2(210.0, 44.0))
                    .corner_radius(CornerRadius::same(8));
                if ui.add(b).clicked() {
                    self.stop();
                }
                let paused = self.rec.as_ref().and_then(|r| r.state.lock().unwrap().status.as_ref().map(|s| s["state"] == "paused")).unwrap_or(false);
                let pb = egui::Button::new(RichText::new(if paused { "Resume" } else { "Pause" }).size(15.0))
                    .min_size(vec2(120.0, 44.0))
                    .corner_radius(CornerRadius::same(8));
                if ui.add_enabled(!stopping, pb).clicked() {
                    if let Some(r) = &mut self.rec {
                        r.send("pause");
                    }
                }
            }
            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(format!("Hotkey: {}", hotkeys::combo_label(&self.settings.start_stop))).color(MUTED));
                ui.label(
                    RichText::new(if self.settings.upload { "Uploads to Garage after each segment" } else { "Upload off: segments stay local" })
                        .color(MUTED),
                );
            });
        });
        ui.add_space(10.0);

        if let Some(r) = &self.rec {
            let st = r.state.lock().unwrap();
            if let Some(s) = &st.status {
                stats_row(ui, s);
            } else {
                ui.label(RichText::new("Starting the recorder… (opening the encoder)").color(MUTED));
            }
            drop(st);
            log_view(ui, r);
        } else if let Some((msg, err)) = &self.last_result {
            let c = if *err { REC } else { GREEN };
            egui::Frame::new().fill(CARD).corner_radius(CornerRadius::same(8)).inner_margin(12).stroke(Stroke::new(1.0, BORDER)).show(ui, |ui| {
                ui.label(RichText::new(if *err { "Recorder error" } else { "Last recording" }).color(c).strong());
                ui.label(RichText::new(msg).color(TEXT));
            });
        }
    }

    fn hotkeys_tab(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Global hotkeys").size(22.0).strong());
        ui.label(
            RichText::new(
                "Work while a game is focused. Combine keyboard keys, mouse middle/side buttons and gamepad buttons. \
                 Read passively (Raw Input + gamepad polling), no hooks; the combo is also logged as input in the recording.",
            )
            .color(MUTED),
        );
        ui.add_space(12.0);
        for a in Action::ALL {
            egui::Frame::new().fill(CARD).corner_radius(CornerRadius::same(10)).inner_margin(14).stroke(Stroke::new(1.0, BORDER)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(a.label()).size(16.0).strong());
                        ui.label(
                            RichText::new(match a {
                                Action::StartStop => "Starts recording the foreground (or selected) window; press again to stop.",
                                Action::Pause => "Toggles pause while recording (Scroll Lock also works).",
                            })
                            .color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.capturing != Some(a) {
                            let clear = egui::Button::new(RichText::new("Clear").color(TEXT)).fill(Color32::from_rgb(0x2b, 0x31, 0x3d));
                            if ui.add(clear).clicked() {
                                self.settings.binding_mut(a).clear();
                                self.save_bindings();
                            }
                            if ui.add(egui::Button::new(RichText::new("Record binding").color(Color32::WHITE)).fill(ACCENT)).clicked() {
                                self.capturing = Some(a);
                                let mut s = self.engine.state.lock().unwrap();
                                s.capture = Some(hotkeys::Capture::default());
                            }
                            combo_chips(ui, self.settings.binding(a), true);
                        }
                    });
                });
                if self.capturing == Some(a) {
                    ui.add_space(10.0);
                    self.capture_ui(ui, a);
                }
            });
            ui.add_space(10.0);
        }

        ui.add_space(8.0);
        ui.label(RichText::new("Options").size(16.0).strong());
        ui.add_space(4.0);
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("The start hotkey records:");
            changed |= ui.radio_value(&mut self.settings.hotkey_target, HotkeyTarget::Foreground, "the foreground window").changed();
            changed |= ui.radio_value(&mut self.settings.hotkey_target, HotkeyTarget::Selected, "the window selected in Record").changed();
        });
        changed |= ui.checkbox(&mut self.settings.beep, "Beep on start / stop (you can't see this window in a fullscreen game)").changed();
        changed |= ui.checkbox(&mut self.settings.upload, "Upload segments while recording").changed();
        if changed {
            let _ = self.settings.save();
        }
        ui.add_space(10.0);
        let st = self.engine.state.lock().unwrap();
        ui.label(RichText::new(format!("Input sources: {}", if st.sources.is_empty() { "none".into() } else { st.sources.join(", ") })).color(MUTED));
        for e in &st.errors {
            ui.label(RichText::new(e).color(REC));
        }
        drop(st);
        ui.add_space(10.0);
        egui::CollapsingHeader::new("Advanced: binaries").show(ui, |ui| {
            let mut ch = false;
            ui.horizontal(|ui| {
                ui.label("gamecap.exe");
                ch |= ui.add(egui::TextEdit::singleline(&mut self.settings.gamecap_exe).hint_text(self.launcher.exe.display().to_string()).desired_width(460.0)).lost_focus();
            });
            ui.horizontal(|ui| {
                ui.label("FFmpeg DLL folder");
                let hint = self.launcher.dll_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_else(|| "next to gamecap.exe".into());
                ch |= ui.add(egui::TextEdit::singleline(&mut self.settings.ffmpeg_dir).hint_text(hint).desired_width(420.0)).lost_focus();
            });
            if ch {
                let _ = self.settings.save();
                self.launcher = Launcher::resolve(Some(&self.settings.gamecap_exe), Some(&self.settings.ffmpeg_dir));
                self.consent = query_consent(&self.launcher);
                self.sessions_root = None;
            }
            ui.label(RichText::new(format!("Settings file: {}", Settings::path().display())).color(MUTED));
        });
    }

    fn capture_ui(&mut self, ui: &mut Ui, a: Action) {
        let cap = self.engine.state.lock().unwrap().capture.clone().unwrap_or_default();
        egui::Frame::new().fill(Color32::from_rgb(0x17, 0x1f, 0x33)).corner_radius(CornerRadius::same(8)).inner_margin(12).stroke(Stroke::new(1.0, ACCENT)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            match &cap.result {
                None => {
                    ui.label(RichText::new("Listening… press the combo now").size(16.0).color(TEXT));
                    ui.label(RichText::new("Keys, mouse middle/side buttons or gamepad buttons, all at once. Release everything to finish.").color(MUTED));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if cap.pressed.is_empty() {
                            ui.label(RichText::new("waiting for input").italics().color(MUTED));
                        } else {
                            combo_chips(ui, &hotkeys::normalize(cap.pressed.iter().copied()), false);
                        }
                    });
                    ui.add_space(8.0);
                    if ui.button("Cancel").clicked() {
                        self.end_capture();
                    }
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                Some(combo) => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Captured:").size(16.0));
                        combo_chips(ui, combo, false);
                    });
                    let other = Action::ALL.iter().find(|o| **o != a && self.settings.binding(**o) == combo);
                    if let Some(o) = other {
                        ui.label(RichText::new(format!("Already used by \"{}\"; saving moves it here.", o.label())).color(AMBER));
                    }
                    if combo.len() == 1 && matches!(combo[0], Elem::Key(c) if !(0x3B..=0x44).contains(&c) && c != 0x57 && c != 0x58 && c != 0x46) {
                        ui.label(RichText::new("A single key without modifiers will also trigger during normal play.").color(AMBER));
                    }
                    let keys: Vec<u32> = combo.iter().filter_map(|e| if let Elem::Key(k) = e { Some(*k) } else { None }).collect();
                    if !combo.is_empty() && combo.iter().all(|e| matches!(e, Elem::Key(k) if hotkeys::is_modifier(*k))) {
                        ui.label(
                            RichText::new(
                                "Only modifiers were captured. If you pressed another key too, another app (Discord, OBS, …) \
                                 owns that shortcut and Windows never passes it on. Pick a different combo.",
                            )
                            .color(AMBER),
                        );
                    } else if keys.len() == combo.len() && platform::shortcut_taken(&keys) {
                        ui.label(RichText::new("Another app has registered this shortcut; it may not reach gamecap or your game.").color(AMBER));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new("Save").color(Color32::WHITE)).fill(ACCENT).min_size(vec2(90.0, 30.0))).clicked() {
                            let combo = combo.clone();
                            if let Some(o) = other.copied() {
                                self.settings.binding_mut(o).clear();
                            }
                            *self.settings.binding_mut(a) = combo;
                            self.save_bindings();
                            self.end_capture();
                            self.toast("Hotkey saved", false);
                        }
                        if ui.button("Try again").clicked() {
                            self.engine.state.lock().unwrap().capture = Some(hotkeys::Capture::default());
                        }
                        if ui.button("Cancel").clicked() {
                            self.end_capture();
                        }
                    });
                }
            }
        });
    }

    fn end_capture(&mut self) {
        self.capturing = None;
        self.engine.state.lock().unwrap().capture = None;
    }

    fn save_bindings(&mut self) {
        self.engine.state.lock().unwrap().bindings = self.settings.bindings();
        if let Err(e) = self.settings.save() {
            self.toast(format!("Saving settings: {e}"), true);
        }
    }

    fn recordings_tab(&mut self, ui: &mut Ui) {
        if self.sessions_at.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
            self.refresh_sessions();
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("Recordings").size(22.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("⟳ Refresh").clicked() {
                    self.refresh_sessions();
                    self.status_text = self.launcher.run(&["status"]).unwrap_or_else(|e| format!("{e:#}"));
                }
                if ui.add(egui::Button::new(RichText::new("Open viewer").color(Color32::WHITE)).fill(ACCENT)).clicked() {
                    open(&self.settings.viewer_url);
                }
                if ui.add(egui::TextEdit::singleline(&mut self.settings.viewer_url).desired_width(260.0)).lost_focus() {
                    let _ = self.settings.save();
                }
                ui.label(RichText::new("Viewer").color(MUTED));
            });
        });
        if let Some(root) = &self.sessions_root {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Local sessions in {}", root.display())).color(MUTED));
                if ui.small_button("Open folder").clicked() {
                    open(&root.display().to_string());
                }
            });
        }
        ui.add_space(8.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - 150.0).show(ui, |ui| {
            if self.sessions.is_empty() {
                ui.label(RichText::new("No local sessions yet.").color(MUTED));
            }
            egui::Grid::new("sessions").striped(true).num_columns(5).spacing(vec2(24.0, 8.0)).show(ui, |ui| {
                for h in ["Session", "Game", "Segments", "Size", ""] {
                    ui.label(RichText::new(h).color(MUTED));
                }
                ui.end_row();
                let viewer = self.settings.viewer_url.trim_end_matches('/').to_string();
                for s in &self.sessions {
                    // Verified uploads are deleted locally; the session lives on in Garage.
                    let uploaded = s.segments == 0 && s.partial == 0;
                    ui.label(RichText::new(pretty_session(&s.id)).monospace());
                    ui.label(if s.game.is_empty() { "–" } else { &s.game });
                    if uploaded {
                        ui.label(RichText::new("uploaded").color(GREEN));
                    } else {
                        ui.label(if s.partial > 0 { format!("{} (+{} partial)", s.segments, s.partial) } else { s.segments.to_string() });
                    }
                    ui.label(if uploaded { "–".into() } else { fmt_bytes(s.bytes) });
                    ui.horizontal(|ui| {
                        if !uploaded && ui.small_button("Folder").clicked() {
                            open(&s.path.display().to_string());
                        }
                        let link = match (&self.upload_user, uploaded) {
                            (_, false) => Some(format!("{viewer}/#s=local&id={}", s.id)),
                            (Some(u), true) => Some(format!("{viewer}/#s=garage&id={u}/{}", s.id)),
                            (None, true) => None,
                        };
                        if let Some(link) = link {
                            if ui.small_button("View").clicked() {
                                open(&link);
                            }
                        }
                    });
                    ui.end_row();
                }
            });
        });
        ui.add_space(8.0);
        egui::CollapsingHeader::new("Upload queue (gamecap status)").default_open(!self.status_text.is_empty()).show(ui, |ui| {
            if self.status_text.is_empty() && ui.button("Load").clicked() {
                self.status_text = self.launcher.run(&["status"]).unwrap_or_else(|e| format!("{e:#}"));
            }
            ui.label(RichText::new(&self.status_text).monospace().size(12.5));
        });
    }

    fn consent_modal(&mut self, ctx: &egui::Context) {
        let terms = match &self.consent {
            Consent::Needed(t) => t.clone(),
            Consent::Error(e) => {
                let e = e.clone();
                egui::Modal::new(egui::Id::new("gamecap-missing")).show(ctx, |ui| {
                    ui.set_width(560.0);
                    ui.label(RichText::new("Can't run gamecap").size(18.0).strong().color(REC));
                    ui.label(&e);
                    ui.label(RichText::new(format!("Looked for {}", self.launcher.exe.display())).color(MUTED));
                    ui.label(RichText::new("Set its path under Hotkeys → Advanced, or put gamecap.exe and the FFmpeg DLLs next to gamecap-gui.exe.").color(MUTED));
                    if ui.button("Retry").clicked() {
                        self.consent = query_consent(&self.launcher);
                    }
                    if ui.button("Continue anyway").clicked() {
                        self.consent = Consent::Ok;
                    }
                });
                return;
            }
            Consent::Ok => return,
        };
        egui::Modal::new(egui::Id::new("consent")).show(ctx, |ui| {
            ui.set_width(640.0);
            ui.label(RichText::new("Before you record").size(20.0).strong());
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                ui.label(RichText::new(&terms).monospace().size(12.5));
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new("I accept").color(Color32::WHITE)).fill(ACCENT).min_size(vec2(110.0, 32.0))).clicked() {
                    self.consent = match self.launcher.run(&["consent", "--accept"]) {
                        Ok(_) => Consent::Ok,
                        Err(e) => Consent::Error(format!("{e:#}")),
                    };
                }
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.my_hwnd.is_none() {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            self.my_hwnd = frame.window_handle().ok().and_then(|h| match h.as_raw() {
                RawWindowHandle::Win32(w) => Some(w.hwnd.get() as u64),
                _ => None,
            });
        }
        while let Ok(a) = self.actions.try_recv() {
            self.on_action(a);
        }
        self.poll_recording();
        if self.windows_at.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            self.refresh_windows();
        }
        // Keep status fresh while recording; idle otherwise (hotkeys wake us).
        ctx.request_repaint_after(if self.rec.is_some() { Duration::from_millis(250) } else { Duration::from_secs(2) });
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.preview_rect = None;
        egui::Panel::top("top").frame(egui::Frame::new().fill(PANEL).inner_margin(10)).show(ui, |ui| self.top_bar(ui));
        if self.tab == Tab::Record {
            egui::Panel::left("windows")
                .resizable(true)
                .default_size(330.0)
                .size_range(240.0..=520.0)
                .frame(egui::Frame::new().fill(PANEL).inner_margin(10))
                .show(ui, |ui| self.window_list(ui));
        }
        egui::CentralPanel::default_margins().frame(egui::Frame::new().fill(BG).inner_margin(18)).show(ui, |ui| match self.tab {
            Tab::Record => self.record_tab(ui),
            Tab::Hotkeys => {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.hotkeys_tab(ui));
            }
            Tab::Recordings => self.recordings_tab(ui),
        });
        let ctx = ui.ctx().clone();
        self.consent_modal(&ctx);
        let modal = !matches!(self.consent, Consent::Ok);
        if let Some(p) = &self.preview {
            p.show(if modal { None } else { self.preview_rect });
        }
    }
}

fn query_consent(l: &Launcher) -> Consent {
    match l.run_json(&["consent", "--json"]) {
        Ok(v) if v["accepted"].as_bool() == Some(true) => Consent::Ok,
        Ok(v) => Consent::Needed(v["terms"].as_str().unwrap_or_default().to_string()),
        Err(e) => Consent::Error(format!("{e:#}")),
    }
}

fn stats_row(ui: &mut Ui, s: &Value) {
    let q = &s["queue"];
    let cards = [
        ("Segment", s["segment"].as_u64().map(|n| n.to_string()).unwrap_or_else(|| "–".into())),
        ("Frames", s["frames"].to_string()),
        ("Dropped", format!("{:.2}%", s["dropped_pct"].as_f64().unwrap_or(0.0))),
        ("Repeated", format!("{:.1}%", s["repeated_pct"].as_f64().unwrap_or(0.0))),
        ("Inputs", s["inputs"].to_string()),
        ("Done", s["segments_done"].to_string()),
        (
            "Upload",
            if q.is_null() {
                "off".into()
            } else {
                let failed = q["failed"].as_u64().unwrap_or(0);
                let pending = q["pending"].as_u64().unwrap_or(0) + q["uploading"].as_u64().unwrap_or(0);
                format!("{} sent · {pending} queued{}", q["verified"], if failed > 0 { format!(" · {failed} failed") } else { String::new() })
            },
        ),
        ("Disk", fmt_bytes(s["disk_used"].as_u64().unwrap_or(0))),
    ];
    // Fixed-width cards in as many columns as fit.
    let gap = 8.0;
    let cols = ((ui.available_width() + gap) / (150.0 + gap)).floor().clamp(1.0, 4.0) as usize;
    let w = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
    for row in cards.chunks(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (k, v) in row {
                let (rect, _) = ui.allocate_exact_size(vec2(w, 54.0), Sense::hover());
                let p = ui.painter();
                p.rect(rect, CornerRadius::same(8), CARD, Stroke::new(1.0, BORDER), StrokeKind::Inside);
                p.text(rect.min + vec2(12.0, 8.0), Align2::LEFT_TOP, *k, FontId::proportional(12.0), MUTED);
                p.with_clip_rect(rect.shrink(4.0)).text(rect.min + vec2(12.0, 25.0), Align2::LEFT_TOP, v, FontId::proportional(16.5), TEXT);
            }
        });
        ui.add_space(gap - ui.spacing().item_spacing.y);
    }
    if let Some(r) = s["pause_reasons"].as_str().filter(|r| !r.is_empty()) {
        ui.label(RichText::new(format!("Paused: {r}")).color(AMBER));
    }
    if s["state"] == "waiting" {
        ui.label(RichText::new("Waiting for the first frame: the window must be visible (not minimized).").color(AMBER));
    }
    if let Some(e) = s["last_error"].as_str() {
        ui.label(RichText::new(format!("Last error: {e}")).color(REC));
    }
}

fn log_view(ui: &mut Ui, r: &Recording) {
    ui.add_space(6.0);
    egui::CollapsingHeader::new("Recorder log").show(ui, |ui| {
        let st = r.state.lock().unwrap();
        egui::ScrollArea::vertical().max_height(180.0).stick_to_bottom(true).show(ui, |ui| {
            for l in st.log.iter().filter(|l| !l.contains(" REC ") && !l.contains(" PAUSED ")) {
                ui.label(RichText::new(l).monospace().size(11.5).color(MUTED));
            }
        });
    });
    let _ = r.last_log();
}

fn combo_chips(ui: &mut Ui, combo: &[Elem], right_to_left: bool) {
    if combo.is_empty() {
        ui.label(RichText::new("not set").italics().color(MUTED));
        return;
    }
    let mut items: Vec<String> = combo.iter().map(|e| hotkeys::elem_name(*e)).collect();
    if right_to_left {
        items.reverse();
    }
    for (i, name) in items.iter().enumerate() {
        if i > 0 {
            ui.label(RichText::new("+").color(MUTED));
        }
        egui::Frame::new()
            .fill(Color32::from_rgb(0x2b, 0x31, 0x3d))
            .stroke(Stroke::new(1.0, Color32::from_rgb(0x44, 0x4b, 0x5a)))
            .corner_radius(CornerRadius::same(5))
            .inner_margin(egui::Margin::symmetric(8, 3))
            .show(ui, |ui| {
                ui.label(RichText::new(name).monospace().size(13.5).color(TEXT));
            });
    }
}

fn pill(ui: &mut Ui, text: &str, color: Color32) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(14.0), color);
    let size = galley.size() + vec2(24.0, 12.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect(rect, CornerRadius::same(255), color.gamma_multiply(0.14), Stroke::new(1.0, color.gamma_multiply(0.6)), StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
}

fn pretty_session(id: &str) -> String {
    // 20260930T031357Z-79e0 -> 2026-09-30 03:13:57Z  79e0
    if id.len() >= 16 && id.as_bytes()[8] == b'T' {
        format!("{}-{}-{} {}:{}:{}Z  {}", &id[0..4], &id[4..6], &id[6..8], &id[9..11], &id[11..13], &id[13..15], id.get(17..).unwrap_or(""))
    } else {
        id.to_string()
    }
}

fn fmt_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn open(target: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(target).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(target).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(target).spawn();
}

fn style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = Color32::from_rgb(0x0e, 0x10, 0x14);
    v.faint_bg_color = Color32::from_rgb(0x1a, 0x1d, 0x24);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.5);
    v.hyperlink_color = ACCENT;
    v.widgets.noninteractive.fg_stroke.color = TEXT;
    v.widgets.inactive.fg_stroke.color = TEXT;
    v.widgets.hovered.fg_stroke.color = Color32::WHITE;
    v.widgets.active.fg_stroke.color = Color32::WHITE;
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x26, 0x2a, 0x34);
    v.widgets.inactive.bg_fill = Color32::from_rgb(0x26, 0x2a, 0x34);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x30, 0x35, 0x42);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0x38, 0x3e, 0x4d);
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
        w.corner_radius = CornerRadius::same(6);
    }
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.all_styles_mut(|s| {
        use egui::TextStyle::*;
        s.text_styles.insert(Body, FontId::proportional(14.5));
        s.text_styles.insert(Button, FontId::proportional(14.5));
        s.text_styles.insert(Small, FontId::proportional(12.0));
        s.text_styles.insert(Monospace, FontId::monospace(13.0));
        s.text_styles.insert(Heading, FontId::proportional(22.0));
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 5.0);
    });
}
