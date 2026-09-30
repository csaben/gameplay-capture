//! Game blocklist / allowlist.
//!
//! Phase 1: a local `blocklist.toml` next to the config:
//!
//! ```toml
//! # When true, only games with status = "allowed" record (default-deny).
//! default_deny = false
//!
//! [[game]]
//! id = "VALORANT-Win64-Shipping.exe"  # exe name or bundle id, case-insensitive
//! status = "blocked"                  # "blocked" | "allowed"
//! publisher = "Riot Games, Inc."      # optional
//! reason = "kernel anti-cheat (Vanguard)"
//! ```
//!
//! Phase 2 (presigned target): `GET /config` also returns a server list,
//! cached in `blocklist.server.json`; it is merged with the local file
//! (either list's "blocked" wins, `default_deny` is OR-ed).
//!
//! Matching: `id` equals the game id (ASCII case-insensitive). If an entry
//! has a `publisher`, it must equal the window's publisher (case-insensitive)
//! when that is known. A *blocked* entry with a publisher still matches when
//! the publisher is unknown (conservative); an *allowed* entry with a
//! publisher requires a known, matching publisher.

use anyhow::{Context, Result};
use cap_types::GameIdentity;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Blocked,
    Allowed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub status: Status,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blocklist {
    #[serde(default)]
    pub default_deny: bool,
    #[serde(default, rename = "game")]
    pub games: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    /// Refuse, with a human-readable reason.
    Refused(String),
}

impl Decision {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allowed)
    }
}

impl Blocklist {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Server `GET /config` body -> list (`blocklist` / `allowlist` arrays of
    /// `{game_id, publisher, notes}` plus `default_deny`).
    pub fn from_server_config(v: &serde_json::Value) -> Self {
        let mut games = Vec::new();
        for (key, status) in [("blocklist", Status::Blocked), ("allowlist", Status::Allowed)] {
            for g in v[key].as_array().into_iter().flatten() {
                if let Some(id) = g["game_id"].as_str() {
                    games.push(Entry {
                        id: id.to_string(),
                        status,
                        publisher: g["publisher"].as_str().map(str::to_string),
                        reason: g["notes"].as_str().map(str::to_string),
                    });
                }
            }
        }
        Self { default_deny: v["default_deny"].as_bool().unwrap_or(false), games }
    }

    /// Union of two lists (blocked entries are checked first, so a block in
    /// either list wins).
    pub fn merged(mut self, other: Blocklist) -> Self {
        self.default_deny |= other.default_deny;
        self.games.extend(other.games);
        self
    }

    fn matches(e: &Entry, id: &GameIdentity) -> bool {
        if !e.id.eq_ignore_ascii_case(&id.game_id) {
            return false;
        }
        match (&e.publisher, &id.publisher) {
            (None, _) => true,
            (Some(want), Some(have)) => want.trim().eq_ignore_ascii_case(have.trim()),
            (Some(_), None) => e.status == Status::Blocked,
        }
    }

    pub fn check(&self, id: &GameIdentity) -> Decision {
        if id.game_id.is_empty() {
            return Decision::Refused("the window has no game identity (exe name / bundle id unknown)".into());
        }
        if let Some(e) = self.games.iter().find(|e| e.status == Status::Blocked && Self::matches(e, id)) {
            let why = e.reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default();
            return Decision::Refused(format!("{} is on the blocklist{why}", id.game_id));
        }
        if self.games.iter().any(|e| e.status == Status::Allowed && Self::matches(e, id)) {
            return Decision::Allowed;
        }
        if self.default_deny {
            return Decision::Refused(format!(
                "{} is not on the allowlist and default_deny = true (only allowed games record)",
                id.game_id
            ));
        }
        Decision::Allowed
    }

    /// Is `game_id` explicitly blocked? (Used while recording to pause when a
    /// blocked game comes to the foreground; publisher unknown.)
    pub fn is_blocked_id(&self, game_id: &str) -> bool {
        !game_id.is_empty()
            && self.games.iter().any(|e| {
                e.status == Status::Blocked && Self::matches(e, &GameIdentity { game_id: game_id.into(), publisher: None })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gid(id: &str, publisher: Option<&str>) -> GameIdentity {
        GameIdentity { game_id: id.into(), publisher: publisher.map(Into::into) }
    }

    const LIST: &str = r#"
default_deny = false
[[game]]
id = "valorant.exe"
status = "blocked"
reason = "kernel anti-cheat"
[[game]]
id = "eldenring.exe"
status = "allowed"
[[game]]
id = "signed.exe"
status = "allowed"
publisher = "Good Games"
[[game]]
id = "shady.exe"
status = "blocked"
publisher = "Shady Inc"
"#;

    #[test]
    fn decisions() {
        let b: Blocklist = toml::from_str(LIST).unwrap();
        let d = b.check(&gid("VALORANT.exe", None));
        assert!(matches!(&d, Decision::Refused(r) if r.contains("blocklist") && r.contains("kernel anti-cheat")), "{d:?}");
        assert!(b.check(&gid("eldenring.exe", None)).is_allowed());
        assert!(b.check(&gid("unknown.exe", None)).is_allowed(), "default allow");
        assert!(!b.check(&gid("shady.exe", None)).is_allowed(), "blocked + unknown publisher -> blocked");
        assert!(b.check(&gid("shady.exe", Some("Other"))).is_allowed(), "publisher mismatch");
        assert!(!b.check(&gid("", None)).is_allowed());
        assert!(b.is_blocked_id("valorant.EXE"));
        assert!(!b.is_blocked_id("eldenring.exe"));

        let deny = Blocklist { default_deny: true, ..b };
        assert!(deny.check(&gid("eldenring.exe", None)).is_allowed());
        assert!(!deny.check(&gid("unknown.exe", None)).is_allowed());
        assert!(!deny.check(&gid("signed.exe", None)).is_allowed(), "allowed+publisher needs a known publisher");
        assert!(deny.check(&gid("signed.exe", Some("good games"))).is_allowed());
    }

    #[test]
    fn server_merge() {
        let v = serde_json::json!({
            "blocklist": [{"game_id": "eldenring.exe", "publisher": null, "notes": "EULA"}],
            "allowlist": [{"game_id": "x.exe"}],
            "default_deny": true
        });
        let local: Blocklist = toml::from_str(LIST).unwrap();
        let m = local.merged(Blocklist::from_server_config(&v));
        assert!(m.default_deny);
        assert!(!m.check(&gid("eldenring.exe", None)).is_allowed(), "server block beats local allow");
        assert!(m.check(&gid("x.exe", None)).is_allowed());
    }
}
