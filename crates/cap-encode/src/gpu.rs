//! Best-effort GPU name for the segment manifest.

use std::process::Command;

pub fn gpu_name() -> String {
    if let Some(n) = nvidia_smi() {
        return n;
    }
    #[cfg(target_os = "linux")]
    if let Some(n) = linux_proc_nvidia().or_else(linux_drm) {
        return n;
    }
    #[cfg(windows)]
    if let Some(n) = crate::ffi::d3d11::adapter_name() {
        return n;
    }
    #[cfg(target_os = "macos")]
    if let Some(n) = run("sysctl", &["-n", "machdep.cpu.brand_string"]) {
        return format!("{n} (integrated GPU)");
    }
    "unknown".into()
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Distinct GPU names joined with ", " (e.g. two identical cards -> one name).
fn nvidia_smi() -> Option<String> {
    let s = run("nvidia-smi", &["--query-gpu=name", "--format=csv,noheader"])?;
    Some(dedup(s.lines().map(|l| l.trim().to_string())))
}

fn dedup(names: impl Iterator<Item = String>) -> String {
    let mut v: Vec<String> = Vec::new();
    for n in names.filter(|n| !n.is_empty()) {
        if !v.contains(&n) {
            v.push(n);
        }
    }
    v.join(", ")
}

#[cfg(target_os = "linux")]
fn linux_proc_nvidia() -> Option<String> {
    let dir = std::fs::read_dir("/proc/driver/nvidia/gpus").ok()?;
    let names = dir.filter_map(|e| {
        let info = std::fs::read_to_string(e.ok()?.path().join("information")).ok()?;
        info.lines().find_map(|l| l.strip_prefix("Model:").map(|m| m.trim().to_string()))
    });
    let s = dedup(names);
    (!s.is_empty()).then_some(s)
}

/// Vendor + PCI device id of DRM cards (AMD/Intel without extra tools).
#[cfg(target_os = "linux")]
fn linux_drm() -> Option<String> {
    let dir = std::fs::read_dir("/sys/class/drm").ok()?;
    let names = dir.filter_map(|e| {
        let e = e.ok()?;
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("card") || name.contains('-') {
            return None;
        }
        let dev = e.path().join("device");
        let vendor = std::fs::read_to_string(dev.join("vendor")).ok()?;
        let device = std::fs::read_to_string(dev.join("device")).ok()?;
        let v = match vendor.trim() {
            "0x10de" => "NVIDIA",
            "0x1002" => "AMD",
            "0x8086" => "Intel",
            other => other,
        };
        Some(format!("{v} GPU {}", device.trim()))
    });
    let s = dedup(names);
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    #[test]
    fn gpu_name_nonempty() {
        let n = super::gpu_name();
        eprintln!("gpu_name = {n}");
        assert!(!n.is_empty());
    }
}
