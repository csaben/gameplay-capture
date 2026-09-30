//! Small helpers: file locks, Ctrl-C, formatting, session ids.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Exclusive advisory lock on `path` (created if missing). `Ok(None)` if
/// another process holds it. Released when the `File` is dropped / the process exits.
pub fn try_lock(path: &Path) -> Result<Option<File>> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let f = OpenOptions::new().create(true).truncate(false).write(true).open(path).with_context(|| format!("opening {}", path.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Ctrl-C / SIGTERM counter. The third press exits immediately.
#[derive(Clone)]
pub struct CtrlC(Arc<AtomicU32>);

impl CtrlC {
    pub fn install() -> Result<Self> {
        let n = Arc::new(AtomicU32::new(0));
        let n2 = n.clone();
        ctrlc::set_handler(move || {
            let c = n2.fetch_add(1, Ordering::SeqCst) + 1;
            match c {
                1 => eprintln!("\nstopping... (Ctrl-C again to skip the upload drain, 3x to exit now)"),
                2 => eprintln!("\nskipping upload drain..."),
                _ => std::process::exit(130),
            }
        })
        .context("installing Ctrl-C handler")?;
        Ok(Self(n))
    }
    pub fn count(&self) -> u32 {
        self.0.load(Ordering::SeqCst)
    }
    pub fn triggered(&self) -> bool {
        self.count() > 0
    }
    /// Programmatic stop (tray Quit, --duration).
    pub fn trigger(&self) {
        self.0.fetch_max(1, Ordering::SeqCst);
    }
    /// Programmatic second press: stop and skip the upload drain.
    pub fn trigger_again(&self) {
        self.0.fetch_max(2, Ordering::SeqCst);
    }
}

pub fn fmt_bytes(n: u64) -> String {
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

/// `20260929T213000Z-3fa2`: sortable, S3-key safe, unique enough per machine.
pub fn new_session_id() -> String {
    let ts = humantime::format_rfc3339_seconds(SystemTime::now()).to_string();
    let compact: String = ts.chars().filter(|c| *c != '-' && *c != ':').collect();
    format!("{compact}-{:04x}", rand::random::<u16>())
}

pub fn stderr_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_key_safe() {
        let s = new_session_id();
        assert_eq!(s.len(), "20260929T213000Z-3fa2".len(), "{s}");
        assert!(s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "{s}");
    }

    #[test]
    fn locks_are_exclusive() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.lock");
        let a = try_lock(&p).unwrap();
        assert!(a.is_some());
        // Same process, separate open file description: flock semantics block it.
        assert!(try_lock(&p).unwrap().is_none());
        drop(a);
        assert!(try_lock(&p).unwrap().is_some());
    }

    #[test]
    fn bytes() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
    }
}
