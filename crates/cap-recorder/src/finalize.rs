//! Segment close: tables -> hashes -> manifest (last, fsynced) -> drop the
//! journal -> rename `.partial` away -> fsync the session dir.

use crate::journal::JOURNAL_FILE;
use crate::tables;
use cap_types::{files, FocusRecord, FrameRecord, InputEvent, Manifest};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct SegmentTables {
    pub frames: Vec<FrameRecord>,
    pub inputs: Vec<InputEvent>,
    pub focus: Vec<FocusRecord>,
}

/// `seg_000042.partial` -> `seg_000042`.
pub fn final_dir_for(partial: &Path) -> Option<PathBuf> {
    let name = partial.file_name()?.to_str()?;
    let base = name.strip_suffix(files::PARTIAL_SUFFIX)?;
    Some(partial.with_file_name(base))
}

pub fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((h.finalize().to_hex().to_string(), n))
}

pub fn fsync_dir(dir: &Path) {
    // Directory fsync is a no-op / unsupported on Windows; best effort elsewhere.
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Writes the three tables into `partial`, then the manifest (with hashes and
/// sizes of all data files), then renames the folder. Returns the final path.
pub fn finalize_segment(partial: &Path, mut manifest: Manifest, t: &SegmentTables) -> Result<PathBuf, String> {
    tables::write_frames(&partial.join(files::FRAMES), &t.frames)?;
    tables::write_inputs(&partial.join(files::INPUTS), &t.inputs)?;
    tables::write_focus(&partial.join(files::FOCUS), &t.focus)?;
    manifest.frame_count = t.frames.len() as u32;
    // The muxer closed video.mp4; make sure it is on disk before hashing.
    let video = partial.join(files::VIDEO);
    if let Ok(f) = OpenOptions::new().write(true).open(&video) {
        let _ = f.sync_all();
    }
    manifest.blake3.clear();
    manifest.sizes.clear();
    for name in files::DATA {
        let (h, n) = hash_file(&partial.join(name)).map_err(|e| format!("hash {name}: {e}"))?;
        manifest.blake3.insert(name.to_string(), h);
        manifest.sizes.insert(name.to_string(), n);
    }
    write_manifest(partial, &manifest)?;
    publish_partial(partial)
}

pub fn write_manifest(dir: &Path, m: &Manifest) -> Result<(), String> {
    let path = dir.join(files::MANIFEST);
    let tmp = dir.join("manifest.json.tmp");
    let json = serde_json::to_vec_pretty(m).map_err(|e| e.to_string())?;
    let mut f = File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(&json).and_then(|_| f.sync_all()).map_err(|e| format!("manifest: {e}"))?;
    drop(f);
    fs::rename(&tmp, &path).map_err(|e| format!("manifest rename: {e}"))?;
    fsync_dir(dir);
    Ok(())
}

/// Second half of close, also used by recovery when the manifest already
/// exists: remove the journal, rename `.partial` -> final, fsync the parent.
pub fn publish_partial(partial: &Path) -> Result<PathBuf, String> {
    let _ = fs::remove_file(partial.join(JOURNAL_FILE));
    let fin = final_dir_for(partial).ok_or_else(|| format!("not a partial dir: {}", partial.display()))?;
    if fin.exists() {
        return Err(format!("{} already exists", fin.display()));
    }
    fs::rename(partial, &fin).map_err(|e| format!("rename {}: {e}", partial.display()))?;
    if let Some(parent) = fin.parent() {
        fsync_dir(parent);
    }
    Ok(fin)
}

/// `std::env::consts::OS` plus version, best effort (e.g. `linux 6.8.0-45-generic (Ubuntu 24.04.1 LTS)`).
pub fn os_string() -> String {
    let os = std::env::consts::OS;
    #[cfg(target_os = "linux")]
    {
        let kernel = fs::read_to_string("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string()).unwrap_or_default();
        let pretty = fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
            })
            .unwrap_or_default();
        let mut s = os.to_string();
        if !kernel.is_empty() {
            s.push(' ');
            s.push_str(&kernel);
        }
        if !pretty.is_empty() {
            s.push_str(&format!(" ({pretty})"));
        }
        s
    }
    #[cfg(target_os = "macos")]
    {
        let v = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if v.is_empty() { os.to_string() } else { format!("{os} {v}") }
    }
    #[cfg(windows)]
    {
        // `ver` output looks like "Microsoft Windows [Version 10.0.22631.4317]".
        let v = std::process::Command::new("cmd")
            .args(["/C", "ver"])
            .output()
            .ok()
            .and_then(|o| {
                let s = String::from_utf8_lossy(&o.stdout).to_string();
                s.split("Version ").nth(1).map(|v| v.trim().trim_end_matches(']').to_string())
            })
            .unwrap_or_default();
        if v.is_empty() { os.to_string() } else { format!("{os} {v}") }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        os.to_string()
    }
}
