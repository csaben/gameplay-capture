//! Orchestration: fixed-rate ticker, bounded channels, segmenter, drop accounting.
//!
//! Entry points for the app:
//! - [`recover_partials`] once at startup (finalizes segments left by a crash),
//! - [`Recorder::start`] with a [`RecorderConfig`] and the platform [`Sources`],
//! - [`Recorder::stats`] for the CLI / tray, [`Recorder::set_paused`] (or the
//!   shared `RecorderConfig::paused` flag) for chat pause / disk cap / blocklist,
//! - [`Recorder::stop`] for a clean shutdown.
//!
//! See `recorder.rs` for the threading model and the exact segment/time rules.

pub mod backend;
pub mod finalize;
pub mod gate;
pub mod journal;
mod recorder;
pub mod recover;
pub mod tables;
pub mod testkit;

pub use backend::{FfmpegEncoder, FfmpegSinkFactory, FrameEncoder, SegmentSink, SinkFactory};
pub use gate::{HeldState, InputGate};
pub use recorder::{
    JitterStats, Recorder, RecorderConfig, RecorderError, RecorderStats, Result, SegmentCallback, Sources, QUEUE_DEPTH,
};
pub use recover::{recover_partials, RecoveryReport};
