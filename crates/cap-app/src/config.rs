//! `config.toml`, the local paths derived from it, and the small JSON state
//! files that live next to it (`state.json`, `token.json`).
//!
//! Location: `--config <path>`, else `<platform config dir>/gamecap/config.toml`
//! (Linux `~/.config/gamecap`, Windows `%APPDATA%\gamecap`, macOS
//! `~/Library/Application Support/gamecap`). `blocklist.toml`, `state.json`
//! and `token.json` sit in the same directory. A missing config file means
//! "all defaults" (no upload target).

use anyhow::{bail, Context, Result};
use cap_types::CaptureSettings;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Scan code set 1 (the dataset's key code space, see cap-input).
pub mod scan {
    pub const ENTER: u32 = 0x1C;
    pub const ESCAPE: u32 = 0x01;
    pub const SCROLL_LOCK: u32 = 0x46;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Base directory for sessions, the upload queue DB and logs.
    /// Default: `<platform data dir>/gamecap`.
    pub data_dir: Option<PathBuf>,
    /// Where segments are written. Default `<data_dir>/sessions`.
    /// On Windows put this on a big, fast drive (e.g. `D:/gamecap/sessions`).
    pub sessions_root: Option<PathBuf>,
    /// Pause recording when local segments under `sessions_root` use this many
    /// GiB (default 20). Recording resumes below 90% of the cap.
    pub disk_cap_gb: Option<f64>,
    /// Alignment offset (ns) copied into every manifest. `gamecap calibrate`
    /// stores `-L` for a measured input-to-captured-frame latency `L`
    /// (pipeline window: `[capture_ns[k] + offset, capture_ns[k+1] + offset)`).
    pub latency_offset_ns: Option<i64>,
    /// Human-readable machine name for device login. Default: hostname.
    pub client_name: Option<String>,
    pub capture: CaptureConfig,
    pub encoder: EncoderSection,
    pub upload: Option<UploadSection>,
    pub hotkeys: HotkeyConfig,
    /// Per-game overrides keyed by game id (exe name / bundle id, case-insensitive).
    pub games: BTreeMap<String, GameConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub rate_hz: u32,
    pub segment_secs: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        let d = CaptureSettings::default();
        Self { width: d.width, height: d.height, rate_hz: d.rate_hz, segment_secs: d.segment_secs }
    }
}

impl CaptureConfig {
    pub fn settings(&self) -> CaptureSettings {
        CaptureSettings { width: self.width, height: self.height, rate_hz: self.rate_hz, segment_secs: self.segment_secs }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EncoderSection {
    /// "hevc" (default) or "av1".
    pub codec: String,
    pub qp: u32,
    pub lossless: bool,
    /// Keyframe interval in frames (default: rate_hz = 1 s).
    pub gop: Option<u32>,
    /// Force one FFmpeg encoder (e.g. "hevc_nvenc"; "libx265" for testing only).
    pub force_encoder: Option<String>,
}

impl Default for EncoderSection {
    fn default() -> Self {
        Self { codec: "hevc".into(), qp: 19, lossless: false, gop: None, force_encoder: None }
    }
}

impl EncoderSection {
    pub fn encoder_config(&self, s: &CaptureSettings) -> Result<cap_encode::EncoderConfig> {
        let mut c = cap_encode::EncoderConfig::new(s.width, s.height, s.rate_hz);
        c.codec = match self.codec.to_ascii_lowercase().as_str() {
            "hevc" | "h265" => cap_encode::Codec::Hevc,
            "av1" => cap_encode::Codec::Av1,
            other => bail!("encoder.codec: unknown codec {other:?} (hevc | av1)"),
        };
        c.qp = self.qp;
        c.lossless = self.lossless;
        c.gop = self.gop.unwrap_or(s.rate_hz).max(1);
        c.force_encoder = self.force_encoder.clone().filter(|s| !s.is_empty());
        Ok(c)
    }
}

/// `[upload]`. Accepts exactly the snippet `deploy/garage/add-client.sh` prints:
///
/// ```toml
/// [upload]
/// target = "s3"
/// user_id = "gaming-pc"
/// endpoint = "http://100.x.y.z:3900"
/// region = "garage"
/// bucket = "gameplay"
/// access_key = "GK..."
/// secret_key = "..."
/// allow_http = true
/// ```
///
/// or, for Phase 2, `target = "presigned"` + `api_base = "https://..."` (the
/// bearer token comes from `gamecap login` and is stored in `token.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UploadSection {
    /// "s3" | "presigned" | "none".
    pub target: String,
    /// `raw/<user_id>/...` (S3 target). Default: client_name / hostname.
    pub user_id: Option<String>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub bucket: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub allow_http: bool,
    pub api_base: Option<String>,
    /// Seconds `record` keeps uploading after Ctrl-C before exiting (default 30).
    pub drain_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum UploadTargetCfg {
    S3 { endpoint: String, region: String, bucket: String, access_key: String, secret_key: String, allow_http: bool },
    Presigned { api_base: String },
}

impl UploadSection {
    /// `None` for `target = "none"`.
    pub fn target(&self) -> Result<Option<UploadTargetCfg>> {
        let need = |v: &Option<String>, k: &str| -> Result<String> {
            match v {
                Some(s) if !s.is_empty() => Ok(s.clone()),
                _ => bail!("[upload] target = {:?} needs `{k}`", self.target),
            }
        };
        Ok(match self.target.to_ascii_lowercase().as_str() {
            "s3" => Some(UploadTargetCfg::S3 {
                endpoint: need(&self.endpoint, "endpoint")?,
                region: self.region.clone().unwrap_or_else(|| "garage".into()),
                bucket: need(&self.bucket, "bucket")?,
                access_key: need(&self.access_key, "access_key")?,
                secret_key: need(&self.secret_key, "secret_key")?,
                allow_http: self.allow_http,
            }),
            "presigned" => Some(UploadTargetCfg::Presigned { api_base: need(&self.api_base, "api_base")? }),
            "none" | "" => None,
            other => bail!("[upload] target: unknown {other:?} (s3 | presigned | none)"),
        })
    }
}

/// `[hotkeys]`: observed passively from the recorder's own input stream
/// (evdev / Raw Input / IOHIDManager), never through global hooks. Codes are
/// scan code set 1 (`0x46` = Scroll Lock, `0x1C` = Enter, `0x01` = Escape;
/// extended keys `0xE0xx`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyConfig {
    /// One-key pause toggle (works whether or not the game is focused). 0 = off.
    pub pause_key: u32,
    /// Chat pause default for all games (off by default; enable per game).
    pub chat_pause: bool,
    /// Key that opens chat (starts a chat pause while the game is focused).
    pub chat_key: u32,
    /// Keys that close chat (end the chat pause).
    pub chat_close_keys: Vec<u32>,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self { pause_key: scan::SCROLL_LOCK, chat_pause: false, chat_key: scan::ENTER, chat_close_keys: vec![scan::ENTER, scan::ESCAPE] }
    }
}

/// `[games."<game_id>"]` per-game overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GameConfig {
    pub chat_pause: Option<bool>,
    pub chat_key: Option<u32>,
    pub chat_close_keys: Option<Vec<u32>>,
}

/// Effective chat-pause rule for one game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRule {
    pub open_key: u32,
    pub close_keys: Vec<u32>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::parse(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        let c: Config = toml::from_str(s)?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        let c = &self.capture;
        if c.width == 0 || c.height == 0 || !c.width.is_multiple_of(2) || !c.height.is_multiple_of(2) {
            bail!("capture.width/height must be even and > 0");
        }
        if c.rate_hz == 0 || c.segment_secs == 0 {
            bail!("capture.rate_hz and capture.segment_secs must be > 0");
        }
        if let Some(u) = &self.upload {
            u.target()?;
        }
        if self.disk_cap_gb.is_some_and(|g| g <= 0.0 || !g.is_finite()) {
            bail!("disk_cap_gb must be > 0");
        }
        Ok(())
    }

    pub fn disk_cap_bytes(&self) -> u64 {
        match self.disk_cap_gb {
            Some(gb) => (gb * 1024.0 * 1024.0 * 1024.0) as u64,
            None => cap_upload::DEFAULT_DISK_CAP_BYTES,
        }
    }

    pub fn client_name(&self) -> String {
        self.client_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(hostname)
    }

    /// `user_id` for S3 object keys: `[upload] user_id`, else the client name
    /// (sanitised to `[A-Za-z0-9._-]`).
    pub fn user_id(&self) -> String {
        let raw = self.upload.as_ref().and_then(|u| u.user_id.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| self.client_name());
        sanitize_key(&raw)
    }

    pub fn latency_offset_ns(&self) -> i64 {
        self.latency_offset_ns.unwrap_or(0)
    }

    /// Chat-pause rule for `game_id`, or `None` if chat pause is off for it.
    pub fn chat_rule(&self, game_id: &str) -> Option<ChatRule> {
        let g = self.games.iter().find(|(k, _)| k.eq_ignore_ascii_case(game_id)).map(|(_, v)| v);
        let on = g.and_then(|g| g.chat_pause).unwrap_or(self.hotkeys.chat_pause);
        if !on {
            return None;
        }
        let open_key = g.and_then(|g| g.chat_key).unwrap_or(self.hotkeys.chat_key);
        let close_keys = g.and_then(|g| g.chat_close_keys.clone()).unwrap_or_else(|| self.hotkeys.chat_close_keys.clone());
        (open_key != 0).then_some(ChatRule { open_key, close_keys })
    }
}

pub fn sanitize_key(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect::<String>()
        .trim_start_matches('.')
        .chars()
        .take(64)
        .collect();
    if out.is_empty() {
        "default".into()
    } else {
        out
    }
}

pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: valid buffer and length.
        if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
            let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(s) = std::str::from_utf8(&buf[..n]) {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "gamecap-client".into())
}

/// Every local path the app uses.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_file: PathBuf,
    pub config_dir: PathBuf,
    pub blocklist: PathBuf,
    /// Cached server blocklist (`GET /config`, presigned target).
    pub blocklist_cache: PathBuf,
    pub state: PathBuf,
    pub token: PathBuf,
    pub data_dir: PathBuf,
    pub sessions_root: PathBuf,
    pub queue_db: PathBuf,
    pub log_dir: PathBuf,
    /// Held (OS file lock) by a running `record`.
    pub record_lock: PathBuf,
    /// Held by whichever process runs the upload worker.
    pub upload_lock: PathBuf,
}

pub fn default_config_file() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("gamecap").join("config.toml")
}

pub fn default_data_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("gamecap")
}

impl Paths {
    pub fn resolve(config_file: &Path, cfg: &Config) -> Self {
        let config_dir = config_file.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        let data_dir = cfg.data_dir.clone().unwrap_or_else(default_data_dir);
        let sessions_root = cfg.sessions_root.clone().unwrap_or_else(|| data_dir.join("sessions"));
        Self {
            config_file: config_file.to_path_buf(),
            blocklist: config_dir.join("blocklist.toml"),
            blocklist_cache: config_dir.join("blocklist.server.json"),
            state: config_dir.join("state.json"),
            token: config_dir.join("token.json"),
            config_dir,
            queue_db: data_dir.join("upload-queue.sqlite3"),
            log_dir: data_dir.join("logs"),
            record_lock: data_dir.join("record.lock"),
            upload_lock: data_dir.join("upload.lock"),
            sessions_root,
            data_dir,
        }
    }
}

/// Persistent client state (`state.json`, next to the config).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub consent_version: Option<String>,
    /// RFC 3339 UTC.
    pub consent_accepted_at: Option<String>,
    /// XDG portal restore token (Wayland), single-use: always the latest.
    pub wayland_restore_token: Option<String>,
}

/// Bearer token for the presigned target (`token.json`, mode 0600).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub api_base: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub access_expires_at: u64,
    pub refresh_expires_at: u64,
    pub user_id: String,
    pub device_id: String,
}

pub fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn load_token(path: &Path) -> Result<Option<StoredToken>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write via temp file + rename; `private` restricts to the owner (unix 0600).
pub fn save_json<T: Serialize>(path: &Path, v: &T, private: bool) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    let _ = private;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn update_state(path: &Path, f: impl FnOnce(&mut State)) -> Result<State> {
    let mut s: State = load_json(path)?;
    f(&mut s);
    save_json(path, &s, true)?;
    Ok(s)
}

/// Set one top-level integer key in the config file, keeping comments and
/// formatting (used by `calibrate`).
#[cfg_attr(not(any(test, feature = "calibrate")), allow(dead_code))]
pub fn set_config_int(path: &Path, key: &str, value: i64) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().with_context(|| format!("parsing {}", path.display()))?;
    doc[key] = toml_edit::value(value);
    let out = doc.to_string();
    Config::parse(&out).context("config would become invalid")?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly what `deploy/garage/add-client.sh` prints, comments included.
    const GARAGE_SNIPPET: &str = r#"
# --- paste into the client's config.toml ---
[upload]
target = "s3"
user_id = "gaming-pc"
endpoint = "http://100.64.0.7:3900"
region = "garage"
bucket = "gameplay"
access_key = "GK0123456789abcdef01234567"
secret_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
allow_http = true   # plain HTTP inside the tailnet (WireGuard-encrypted)
# ---
"#;

    #[test]
    fn accepts_garage_snippet() {
        let c = Config::parse(GARAGE_SNIPPET).unwrap();
        assert_eq!(c.user_id(), "gaming-pc");
        match c.upload.clone().unwrap().target().unwrap().unwrap() {
            UploadTargetCfg::S3 { endpoint, region, bucket, access_key, allow_http, .. } => {
                assert_eq!(endpoint, "http://100.64.0.7:3900");
                assert_eq!(region, "garage");
                assert_eq!(bucket, "gameplay");
                assert!(access_key.starts_with("GK"));
                assert!(allow_http);
            }
            other => panic!("{other:?}"),
        }
        // Defaults.
        assert_eq!(c.capture.settings(), CaptureSettings::default());
        assert_eq!(c.disk_cap_bytes(), cap_upload::DEFAULT_DISK_CAP_BYTES);
        assert_eq!(c.hotkeys.pause_key, 0x46);
    }

    #[test]
    fn full_config_and_chat_rules() {
        let c = Config::parse(
            r#"
sessions_root = "D:/gamecap/sessions"
disk_cap_gb = 0.5
latency_offset_ns = 41000000
client_name = "desk pc"
[capture]
segment_secs = 20
[encoder]
codec = "av1"
qp = 22
[hotkeys]
pause_key = 0x46
[games."EldenRing.exe"]
chat_pause = true
[games."cs2.exe"]
chat_pause = true
chat_key = 0x15
chat_close_keys = [0x1C]
"#,
        )
        .unwrap();
        assert_eq!(c.capture.segment_secs, 20);
        assert_eq!(c.capture.width, 640);
        assert_eq!(c.disk_cap_bytes(), 512 * 1024 * 1024);
        assert_eq!(c.user_id(), "desk-pc");
        assert!(c.upload.is_none());
        assert_eq!(c.chat_rule("eldenring.exe"), Some(ChatRule { open_key: 0x1C, close_keys: vec![0x1C, 0x01] }));
        assert_eq!(c.chat_rule("cs2.exe"), Some(ChatRule { open_key: 0x15, close_keys: vec![0x1C] }));
        assert_eq!(c.chat_rule("other.exe"), None, "chat pause defaults to off");
        let e = c.encoder.encoder_config(&c.capture.settings()).unwrap();
        assert_eq!((e.codec, e.qp, e.gop), (cap_encode::Codec::Av1, 22, 20));
    }

    #[test]
    fn rejects_bad_config() {
        assert!(Config::parse("[upload]\ntarget = \"s3\"\n").is_err(), "s3 without endpoint");
        assert!(Config::parse("[upload]\ntarget = \"ftp\"\n").is_err());
        assert!(Config::parse("[capture]\nwidth = 641\n").is_err());
        assert!(Config::parse("typo_key = 1\n").is_err());
        assert!(Config::parse("[upload]\ntarget = \"presigned\"\napi_base = \"https://x\"\n").is_ok());
    }

    #[test]
    fn set_int_preserves_rest() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, format!("# my comment\n{GARAGE_SNIPPET}")).unwrap();
        set_config_int(&p, "latency_offset_ns", 33_000_000).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("# my comment") && text.contains("allow_http = true"));
        let c = Config::parse(&text).unwrap();
        assert_eq!(c.latency_offset_ns(), 33_000_000);
        assert!(c.upload.is_some());
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_key("my pc/1"), "my-pc-1");
        assert_eq!(sanitize_key(".."), "default");
    }
}
