//! Process-name helpers (pure parts are tested on every OS).

/// Last path component, accepting both `/` and `\` separators.
pub fn exe_basename(path: &str) -> String {
    let p = path.trim_end_matches(['/', '\\']);
    p.rsplit(['/', '\\']).next().unwrap_or(p).to_string()
}

/// Is this the Wine/Proton loader rather than the game itself?
#[allow(dead_code)]
pub(crate) fn is_wine_loader(name: &str) -> bool {
    matches!(
        name,
        "wine" | "wine64" | "wine-preloader" | "wine64-preloader" | "wineserver"
    )
}

/// Windows exe name from a Wine process' `/proc/<pid>/cmdline` (argv[0] is
/// the Windows path, e.g. `Z:\games\Elden Ring\eldenring.exe`). If argv[0] is
/// itself the loader, look at argv[1].
#[allow(dead_code)]
pub(crate) fn wine_exe_from_cmdline(cmdline: &[u8]) -> Option<String> {
    for arg in cmdline.split(|&b| b == 0).take(2) {
        let s = String::from_utf8_lossy(arg);
        let base = exe_basename(&s);
        if base.is_empty() || is_wine_loader(&base) {
            continue;
        }
        return Some(base);
    }
    None
}

/// Linux: executable name of `pid` from `/proc` (reads the `exe` symlink /
/// `cmdline` / `comm`; never opens the process or its memory). Wine/Proton
/// games report their Windows exe name.
#[cfg(target_os = "linux")]
pub(crate) fn linux_exe_for_pid(pid: u32) -> Option<String> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| exe_basename(&p.to_string_lossy()))
        .map(|s| s.trim_end_matches(" (deleted)").to_string());
    let name = match exe {
        Some(e) => e,
        None => std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()?
            .trim()
            .to_string(),
    };
    if is_wine_loader(&name) || name.ends_with(".exe") {
        if let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) {
            if let Some(w) = wine_exe_from_cmdline(&cmd) {
                return Some(w);
            }
        }
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basenames() {
        assert_eq!(exe_basename("/usr/bin/firefox"), "firefox");
        assert_eq!(exe_basename(r"C:\Games\Foo\foo.exe"), "foo.exe");
        assert_eq!(exe_basename("plain"), "plain");
        assert_eq!(exe_basename("/opt/x/"), "x");
    }

    #[test]
    fn wine_cmdline() {
        let c = b"Z:\\home\\u\\Games\\Elden Ring\\Game\\eldenring.exe\0-flag\0";
        assert_eq!(wine_exe_from_cmdline(c).as_deref(), Some("eldenring.exe"));
        let c = b"/usr/bin/wine64-preloader\0C:\\x\\game.exe\0";
        assert_eq!(wine_exe_from_cmdline(c).as_deref(), Some("game.exe"));
        assert_eq!(wine_exe_from_cmdline(b"wine\0").as_deref(), None);
        assert!(is_wine_loader("wine64-preloader"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn own_process_name() {
        let me = linux_exe_for_pid(std::process::id()).unwrap();
        assert!(me.starts_with("cap_focus"), "{me}");
    }
}
