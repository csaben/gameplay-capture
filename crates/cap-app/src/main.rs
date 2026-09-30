//! `gamecap`: record (frame, action) gameplay data and upload it.
//! See README.md in this crate for setup, config reference and commands.

mod blocklist;
// The measurement math is always built (and unit-tested); the window needs `calibrate`.
#[cfg_attr(not(feature = "calibrate"), allow(dead_code))]
mod calibrate;
mod config;
mod consent;
mod login;
mod pause;
mod record;
mod synth;
#[cfg(all(feature = "tray", any(windows, target_os = "macos")))]
mod tray;
mod upload;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::{Config, Paths};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "gamecap", version, about = "Gameplay frame + action capture")]
struct Cli {
    /// Config file (default: <platform config dir>/gamecap/config.toml).
    #[arg(long, global = true, env = "GAMECAP_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List capturable windows (native id, pid, game id, title).
    Windows,
    /// Record a game window (or a synthetic source) into segments and upload them.
    Record(RecordArgs),
    /// Run only the upload queue: recover partials, queue finished segments, drain.
    Upload {
        /// Keep running and upload new segments as they appear.
        #[arg(long)]
        follow: bool,
        /// Give up after this many seconds.
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Upload queue stats, local disk usage, consent and target.
    Status {
        /// List every queued segment.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Move permanently failed segments back to pending.
    RetryFailed,
    /// Measure input-to-captured-frame latency and store it as latency_offset_ns.
    Calibrate {
        /// Number of key presses to measure.
        #[arg(long, default_value_t = 20)]
        trials: usize,
        /// Only print the result; don't write the config.
        #[arg(long)]
        no_save: bool,
    },
    /// Sign in to the ingest API (device-code flow) and store the token.
    Login {
        #[arg(long)]
        api_base: Option<String>,
    },
    /// Ask the server to delete all raw data uploaded by your account.
    DeleteMyData {
        #[arg(long)]
        api_base: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Show the terms, or revoke consent.
    Consent {
        #[arg(long)]
        revoke: bool,
    },
    /// Print the resolved paths (config, data, sessions, queue, logs).
    Paths,
}

#[derive(clap::Args)]
struct RecordArgs {
    /// Game to record: game id (exe / bundle id), window title substring, or native window id.
    #[arg(long, required_unless_present = "synthetic")]
    game: Option<String>,
    /// Synthetic frames + scripted inputs + fake focus (no display needed).
    #[arg(long)]
    synthetic: bool,
    /// Game id reported by the synthetic source (the blocklist applies to it).
    #[arg(long, default_value = "synthetic.exe")]
    synthetic_game_id: String,
    /// Synthetic source size, WxH.
    #[arg(long, default_value = "1280x720", value_parser = parse_size)]
    synthetic_size: (u32, u32),
    /// Synthetic source frame rate.
    #[arg(long, default_value_t = 30)]
    synthetic_fps: u32,
    /// Synthetic focus: alt-tab away for the last 20% of every N seconds.
    #[arg(long)]
    synthetic_alt_tab: Option<u64>,
    /// Game id of the app that takes focus on a synthetic alt-tab.
    #[arg(long, default_value = "firefox")]
    synthetic_foreground: String,
    /// Fake encoder + muxer (no GPU; video.mp4 is not a real MP4). Testing only.
    #[arg(long)]
    fake_encoder: bool,
    /// Don't upload; keep segments on disk.
    #[arg(long)]
    no_upload: bool,
    /// Stop after this many seconds.
    #[arg(long)]
    duration: Option<u64>,
    /// Seconds to keep uploading after stopping (default: [upload] drain_secs or 30).
    #[arg(long)]
    drain_secs: Option<u64>,
    /// Show a tray icon (builds with feature `tray`, Windows/macOS).
    #[arg(long)]
    tray: bool,
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once(['x', 'X']).ok_or("expected WxH")?;
    let w: u32 = w.parse().map_err(|_| "bad width")?;
    let h: u32 = h.parse().map_err(|_| "bad height")?;
    if w < 16 || h < 16 {
        return Err("too small".into());
    }
    Ok((w, h))
}

fn init_logging(paths: &Paths) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr = fmt::layer().with_writer(std::io::stderr).with_target(false).with_filter(filter());
    let file = std::fs::create_dir_all(&paths.log_dir).ok().and_then(|_| {
        tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("gamecap")
            .filename_suffix("log")
            .max_log_files(14)
            .build(&paths.log_dir)
            .ok()
    });
    match file {
        Some(appender) => {
            let (nb, guard) = tracing_appender::non_blocking(appender);
            let file_layer = fmt::layer().with_writer(nb).with_ansi(false).with_filter(filter());
            tracing_subscriber::registry().with(stderr).with(file_layer).init();
            Some(guard)
        }
        None => {
            tracing_subscriber::registry().with(stderr).init();
            None
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let config_file = cli.config.clone().unwrap_or_else(config::default_config_file);
    let cfg = Config::load(&config_file)?;
    let paths = Paths::resolve(&config_file, &cfg);
    let _log_guard = init_logging(&paths);
    tracing::debug!(config = %config_file.display(), "starting");

    match cli.cmd {
        Cmd::Windows => record::cmd_windows(),
        Cmd::Record(a) => {
            let ctrlc = util::CtrlC::install()?;
            let opts = record::RecordOpts {
                game: a.game,
                synthetic: a.synthetic,
                synthetic_game_id: a.synthetic_game_id,
                synthetic_size: a.synthetic_size,
                synthetic_fps: a.synthetic_fps,
                synthetic_alt_tab_secs: a.synthetic_alt_tab,
                synthetic_foreground: a.synthetic_foreground,
                fake_encoder: a.fake_encoder,
                no_upload: a.no_upload,
                duration: a.duration.map(Duration::from_secs),
                drain_secs: a.drain_secs,
                tray: a.tray,
            };
            record::cmd_record(&cfg, &paths, opts, ctrlc)
        }
        Cmd::Upload { follow, timeout } => {
            let ctrlc = util::CtrlC::install()?;
            upload::cmd_upload(&cfg, &paths, upload::UploadOpts { follow, timeout: timeout.map(Duration::from_secs) }, &ctrlc)
        }
        Cmd::Status { verbose } => upload::cmd_status(&cfg, &paths, verbose),
        Cmd::RetryFailed => upload::cmd_retry_failed(&cfg, &paths),
        Cmd::Calibrate { trials, no_save } => {
            #[cfg(feature = "calibrate")]
            {
                calibrate::cmd_calibrate(&paths, trials.max(1), !no_save)
            }
            #[cfg(not(feature = "calibrate"))]
            {
                let _ = (trials, no_save);
                anyhow::bail!("this build has no calibration window (rebuild with feature `calibrate`)")
            }
        }
        Cmd::Login { api_base } => login::cmd_login(&cfg, &paths, api_base.as_deref()),
        Cmd::DeleteMyData { api_base, yes } => login::cmd_delete_my_data(&cfg, &paths, api_base.as_deref(), yes),
        Cmd::Consent { revoke } => {
            if revoke {
                consent::revoke(&paths.state)?;
                println!("consent revoked; `gamecap record` will ask again");
            } else {
                let st: config::State = config::load_json(&paths.state)?;
                println!("{}\n", consent::TERMS);
                match (st.consent_version, st.consent_accepted_at) {
                    (Some(v), Some(t)) => println!("accepted version {v} at {t} (current version {})", consent::CONSENT_VERSION),
                    _ => println!("not accepted yet; `gamecap record` asks on first run"),
                }
            }
            Ok(())
        }
        Cmd::Paths => {
            println!("config:        {}", paths.config_file.display());
            println!("config dir:    {}", paths.config_dir.display());
            println!("blocklist:     {}", paths.blocklist.display());
            println!("state:         {}", paths.state.display());
            println!("token:         {}", paths.token.display());
            println!("data dir:      {}", paths.data_dir.display());
            println!("sessions_root: {}", paths.sessions_root.display());
            println!("queue db:      {}", paths.queue_db.display());
            println!("logs:          {}", paths.log_dir.display());
            Ok(())
        }
    }
}
