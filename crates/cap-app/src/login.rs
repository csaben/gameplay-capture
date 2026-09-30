//! Phase 2 account commands: `gamecap login` (OAuth device-code flow against
//! ingest-api) and `gamecap delete-my-data`.

use crate::config::{save_json, Config, Paths, UploadTargetCfg};
use crate::upload::{fresh_token, runtime, token_from_response};
use anyhow::{bail, Context, Result};
use cap_upload::api::ApiClient;
use std::io::{BufRead, Write};
use std::time::Duration;

fn api_base(cfg: &Config, override_base: Option<&str>) -> Result<String> {
    if let Some(b) = override_base {
        return Ok(b.to_string());
    }
    match cfg.upload.as_ref().map(|u| u.target()).transpose()?.flatten() {
        Some(UploadTargetCfg::Presigned { api_base }) => Ok(api_base),
        _ => bail!("no API configured: set [upload] target = \"presigned\" and api_base, or pass --api-base"),
    }
}

pub fn cmd_login(cfg: &Config, paths: &Paths, override_base: Option<&str>) -> Result<()> {
    let base = api_base(cfg, override_base)?;
    let rt = runtime()?;
    let name = cfg.client_name();
    let tok = rt.block_on(async {
        let api = ApiClient::new(&base)?;
        api.device_login(&name, |c| {
            eprintln!("\nTo sign in, open {}", c.verification_uri_complete.as_deref().unwrap_or(&c.verification_uri));
            eprintln!("and enter the code:  {}\n", c.user_code);
            eprintln!("waiting for approval (expires in {} s)...", c.expires_in);
        })
        .await
    })
    .context("device login")?;
    let stored = token_from_response(&base, &tok);
    save_json(&paths.token, &stored, true)?;
    eprintln!("logged in as {} (device {}); token saved to {}", stored.user_id, stored.device_id, paths.token.display());
    Ok(())
}

/// Pure confirmation check (the user must type the word `delete`).
pub fn confirmed(answer: &str) -> bool {
    answer.trim() == "delete"
}

pub fn cmd_delete_my_data(cfg: &Config, paths: &Paths, override_base: Option<&str>, yes: bool) -> Result<()> {
    let base = api_base(cfg, override_base)?;
    if !yes {
        eprint!(
            "This asks the server to delete ALL raw data uploaded by your account and to drop it from\n\
             future dataset shards. It cannot be undone. Local, not yet uploaded segments are not touched.\n\
             Type `delete` to confirm: "
        );
        std::io::stderr().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if !confirmed(&line) {
            bail!("cancelled");
        }
    }
    let rt = runtime()?;
    let resp = rt.block_on(async {
        let tok = fresh_token(paths, &base, Duration::from_secs(60)).await?;
        let api = ApiClient::new(&base)?;
        api.delete_my_data(&tok.access_token).await.map_err(anyhow::Error::from)
    })?;
    println!("deletion {} queued: {} segment(s), status {}", resp.deletion_id, resp.segments, resp.status);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn confirm_word() {
        assert!(super::confirmed("delete\n"));
        assert!(!super::confirmed("yes"));
        assert!(!super::confirmed("Delete"));
    }
}
