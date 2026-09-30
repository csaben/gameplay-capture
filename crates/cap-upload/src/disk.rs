//! Disk-cap helpers for the recorder.

use std::path::Path;

/// Default local disk cap for unuploaded segments: 20 GB.
pub const DEFAULT_DISK_CAP_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// Total size in bytes of all regular files under `root` (recursive, does not
/// follow symlinks). A missing `root` counts as 0.
pub fn disk_usage(root: &Path) -> std::io::Result<u64> {
    let meta = match std::fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    if meta.is_file() {
        return Ok(meta.len());
    }
    if !meta.is_dir() {
        return Ok(0);
    }
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            // Folder deleted by the uploader while we walk: fine.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                if let Ok(m) = entry.metadata() {
                    total += m.len();
                }
            }
        }
    }
    Ok(total)
}

/// True if the recorder should pause: usage under `root` is at or above `cap_bytes`.
pub fn over_cap(root: &Path, cap_bytes: u64) -> std::io::Result<bool> {
    Ok(disk_usage(root)? >= cap_bytes)
}
