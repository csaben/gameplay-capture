//! First-run consent screen. The accepted version and time are stored in
//! `state.json`; `record` refuses to start without a current consent.

use crate::config::{update_state, State};
use anyhow::{bail, Result};
use std::io::{BufRead, Write};
use std::path::Path;

/// Bump when the terms change; everyone has to accept again.
pub const CONSENT_VERSION: &str = "1";

pub const TERMS: &str = "\
gamecap records gameplay for building machine-learning datasets.

What is recorded, and only while you run `gamecap record`:
  * Video of the ONE game window you choose, downscaled (default 640x360 @ 20 Hz).
    Never the desktop, other windows, your webcam or microphone.
  * Keyboard (as scan codes, never text), mouse and gamepad inputs, only while
    that game window is focused. Alt-tab stops input logging immediately.
  * Which app is in the foreground (its exe name / bundle id) and timing data.
  * Machine info in each segment: OS, GPU and encoder names, client version.

Controls:
  * One-key pause (Scroll Lock by default) and an optional chat pause.
  * Games on the blocklist refuse to record.

Use of the data:
  * Segments are uploaded to the project's storage and may be used, including
    commercially, to train models on gameplay and inputs.
  * You can request deletion of all your raw data at any time
    (`gamecap delete-my-data`); it is also removed from future dataset builds.

Anti-cheat: gamecap only uses OS capture and passive input APIs and never touches
the game process, but no tool can guarantee a given anti-cheat will not flag it.
Recording is at your own risk; test on an alt account first.";

pub fn has_consent(state: &State) -> bool {
    state.consent_version.as_deref() == Some(CONSENT_VERSION)
}

/// Pure check of the typed answer.
pub fn is_yes(answer: &str) -> bool {
    answer.trim().eq_ignore_ascii_case("yes")
}

/// Show the terms and ask for `yes` on `input`. Stores consent on success.
pub fn prompt(state_path: &Path, input: &mut dyn BufRead, out: &mut dyn Write) -> Result<State> {
    writeln!(out, "\n==== gamecap terms (version {CONSENT_VERSION}) ====\n\n{TERMS}\n")?;
    write!(out, "Type `yes` to accept and continue, anything else to cancel: ")?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    if !is_yes(&line) {
        bail!("consent not given: recording refused (run again and type `yes` to accept the terms)");
    }
    let st = accept(state_path)?;
    writeln!(out, "Consent recorded (version {CONSENT_VERSION}, {}).", st.consent_accepted_at.as_deref().unwrap_or(""))?;
    Ok(st)
}

/// Record acceptance of the current terms.
pub fn accept(state_path: &Path) -> Result<State> {
    let now = humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string();
    update_state(state_path, |s| {
        s.consent_version = Some(CONSENT_VERSION.into());
        s.consent_accepted_at = Some(now.clone());
    })
}

/// Ensure consent, prompting on stdin if needed.
pub fn ensure(state_path: &Path, state: State) -> Result<State> {
    if has_consent(&state) {
        return Ok(state);
    }
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    prompt(state_path, &mut lock, &mut std::io::stderr())
}

pub fn revoke(state_path: &Path) -> Result<()> {
    update_state(state_path, |s| {
        s.consent_version = None;
        s.consent_accepted_at = None;
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_json;

    #[test]
    fn prompt_accepts_only_yes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        let mut out = Vec::new();
        assert!(prompt(&p, &mut "y\n".as_bytes(), &mut out).is_err());
        assert!(prompt(&p, &mut "".as_bytes(), &mut out).is_err());
        assert!(!has_consent(&load_json(&p).unwrap()));
        let st = prompt(&p, &mut " YES \n".as_bytes(), &mut out).unwrap();
        assert!(has_consent(&st));
        let st2: State = load_json(&p).unwrap();
        assert!(has_consent(&st2) && st2.consent_accepted_at.is_some());
        revoke(&p).unwrap();
        assert!(!has_consent(&load_json(&p).unwrap()));
    }
}
