//! Runs `gamecap` as a child process: one-shot JSON queries (`windows`,
//! `consent`, `paths`) and the long-running `record --control-stdin --status-json`.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Where the `gamecap` binary and its FFmpeg DLLs are.
#[derive(Clone, Debug)]
pub struct Launcher {
    pub exe: PathBuf,
    /// Prepended to the child's PATH (FFmpeg DLLs) when they aren't next to the exe.
    pub dll_dir: Option<PathBuf>,
}

impl Launcher {
    pub fn resolve(exe_override: Option<&str>, ffmpeg_override: Option<&str>) -> Self {
        let here = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
        let exe = exe_override
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| here.join(if cfg!(windows) { "gamecap.exe" } else { "gamecap" }));
        let has_dlls = |d: &Path| d.join("avcodec-61.dll").exists();
        let exe_dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
        let dll_dir = if has_dlls(&exe_dir) {
            None
        } else {
            ffmpeg_override
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("FFMPEG_DIR").map(|d| PathBuf::from(d).join("bin")))
                .filter(|d| has_dlls(d))
        };
        Self { exe, dll_dir }
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.exe);
        if let Some(d) = &self.dll_dir {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut parts = vec![d.clone()];
            parts.extend(std::env::split_paths(&path));
            if let Ok(p) = std::env::join_paths(parts) {
                c.env("PATH", p);
            }
        }
        #[cfg(windows)]
        c.creation_flags(CREATE_NO_WINDOW);
        c
    }

    /// Run `gamecap <args>` and return stdout (error with stderr on failure).
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = self
            .command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", self.exe.display()))?;
        if !out.status.success() {
            let err = strip_ansi(&String::from_utf8_lossy(&out.stderr));
            bail!("gamecap {}: {}", args.join(" "), err.trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn run_json(&self, args: &[&str]) -> Result<Value> {
        let s = self.run(args)?;
        serde_json::from_str(s.trim()).context("parsing gamecap JSON")
    }

    /// `gamecap paths` -> sessions_root.
    pub fn sessions_root(&self) -> Result<PathBuf> {
        let s = self.run(&["paths"])?;
        s.lines()
            .find_map(|l| l.strip_prefix("sessions_root:").map(|v| PathBuf::from(v.trim())))
            .context("no sessions_root in `gamecap paths`")
    }

    pub fn start_record(&self, native_id: u64, no_upload: bool, repaint: impl Fn() + Send + Sync + 'static) -> Result<Recording> {
        let id = format!("{native_id:#x}");
        let mut args = vec!["record", "--game", &id, "--control-stdin", "--status-json"];
        if no_upload {
            args.push("--no-upload");
        }
        let mut child = self
            .command()
            .args(&args)
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", self.exe.display()))?;
        let state = Arc::new(Mutex::new(RecState::default()));
        let repaint = Arc::new(repaint);
        let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        {
            let st = state.clone();
            let rp = repaint.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(|l| l.ok()) {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        let mut s = st.lock().unwrap();
                        match v["event"].as_str() {
                            Some("started") => s.started = Some(v),
                            Some("status") => s.status = Some(v),
                            Some("stopped") => s.stopped = Some(v),
                            _ => {}
                        }
                    }
                    rp();
                }
            });
        }
        {
            let st = state.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(|l| l.ok()) {
                    let line = strip_ansi(&line);
                    if line.trim().is_empty() {
                        continue;
                    }
                    let mut s = st.lock().unwrap();
                    if s.log.len() >= 400 {
                        s.log.pop_front();
                    }
                    s.log.push_back(line);
                    drop(s);
                    repaint();
                }
            });
        }
        let stdin = child.stdin.take();
        Ok(Recording { child, stdin, state, started_at: Instant::now(), stop_requested: None, exit: None })
    }
}

#[derive(Default)]
pub struct RecState {
    pub started: Option<Value>,
    pub status: Option<Value>,
    pub stopped: Option<Value>,
    pub log: VecDeque<String>,
}

pub struct Recording {
    child: Child,
    stdin: Option<ChildStdin>,
    pub state: Arc<Mutex<RecState>>,
    pub started_at: Instant,
    pub stop_requested: Option<Instant>,
    /// Exit code once the process is gone.
    pub exit: Option<Option<i32>>,
}

impl Recording {
    pub fn send(&mut self, cmd: &str) {
        if let Some(i) = &mut self.stdin {
            if writeln!(i, "{cmd}").and_then(|_| i.flush()).is_err() {
                self.stdin = None;
            }
        }
    }

    pub fn stop(&mut self) {
        if self.stop_requested.is_none() {
            self.stop_requested = Some(Instant::now());
            self.send("stop");
        } else {
            self.send("stop-now");
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// Poll for exit; returns true once the process has exited.
    pub fn poll(&mut self) -> bool {
        if self.exit.is_none() {
            if let Ok(Some(st)) = self.child.try_wait() {
                self.exit = Some(st.code());
            }
        }
        self.exit.is_some()
    }

    pub fn last_log(&self) -> Option<String> {
        self.state.lock().unwrap().log.back().cloned()
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        // Closing stdin (EOF) makes the recorder stop cleanly on its own.
        self.stdin = None;
    }
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_ansi() {
        assert_eq!(super::strip_ansi("\x1b[2m2026\x1b[0m \x1b[32m INFO\x1b[0m hi"), "2026  INFO hi");
    }
}
