//! Encoder candidates and per-family option sets. Pure logic, no FFI, so it is
//! unit-testable on every platform.

use crate::{Codec, EncoderConfig};

/// How frames reach an encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwKind {
    /// CUDA hw frames (NVENC on Linux).
    Cuda,
    /// VAAPI surfaces (hevc_vaapi / av1_vaapi).
    Vaapi,
    /// D3D11 textures (NVENC / AMF / QSV on Windows).
    D3d11,
    /// CVPixelBuffers (VideoToolbox).
    VideoToolbox,
    /// Plain CPU frames (libx265, libsvtav1, ...). Only when forced.
    Software,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Nvenc,
    Amf,
    Qsv,
    Vaapi,
    VideoToolbox,
    X265,
    OtherSoftware,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub family: Family,
    pub hw: HwKind,
}

/// Suffixes that identify hardware encoders in FFmpeg's naming scheme.
const HW_SUFFIXES: &[&str] = &["_nvenc", "_amf", "_qsv", "_vaapi", "_videotoolbox", "_mf", "_v4l2m2m", "_vulkan", "_d3d12va"];

pub fn is_hardware_name(name: &str) -> bool {
    HW_SUFFIXES.iter().any(|s| name.ends_with(s))
}

pub fn family_of(name: &str) -> Family {
    if name.ends_with("_nvenc") {
        Family::Nvenc
    } else if name.ends_with("_amf") {
        Family::Amf
    } else if name.ends_with("_qsv") {
        Family::Qsv
    } else if name.ends_with("_vaapi") {
        Family::Vaapi
    } else if name.ends_with("_videotoolbox") {
        Family::VideoToolbox
    } else if name == "libx265" {
        Family::X265
    } else {
        Family::OtherSoftware
    }
}

/// Frame transport used for an encoder family on the current platform.
pub fn hw_kind_of(family: Family) -> HwKind {
    match family {
        Family::Nvenc => {
            if cfg!(windows) {
                HwKind::D3d11
            } else {
                HwKind::Cuda
            }
        }
        Family::Amf | Family::Qsv => {
            if cfg!(windows) {
                HwKind::D3d11
            } else {
                // AMF/QSV on Linux are not in the probe order; if forced they
                // would need VAAPI/QSV-derived frames. Use the VAAPI transport
                // (qsv accepts VAAPI surfaces as input since FFmpeg 6.0).
                HwKind::Vaapi
            }
        }
        Family::Vaapi => HwKind::Vaapi,
        Family::VideoToolbox => HwKind::VideoToolbox,
        Family::X265 | Family::OtherSoftware => HwKind::Software,
    }
}

fn candidate(name: &str) -> Candidate {
    let family = family_of(name);
    Candidate { name: name.to_string(), family, hw: hw_kind_of(family) }
}

/// Platform probe order (spec: Windows nvenc, amf, qsv; Linux nvenc, vaapi;
/// macOS videotoolbox). Software is never in this list.
pub fn platform_order(codec: Codec) -> Vec<&'static str> {
    let hevc: &[&str] = if cfg!(windows) {
        &["hevc_nvenc", "hevc_amf", "hevc_qsv"]
    } else if cfg!(target_os = "macos") {
        &["hevc_videotoolbox"]
    } else {
        &["hevc_nvenc", "hevc_vaapi"]
    };
    let av1: &[&str] = if cfg!(windows) {
        &["av1_nvenc", "av1_amf", "av1_qsv"]
    } else if cfg!(target_os = "macos") {
        // No AV1 hardware encoder on Apple silicon (as of M4).
        &[]
    } else {
        &["av1_nvenc", "av1_vaapi"]
    };
    match codec {
        Codec::Hevc => hevc.to_vec(),
        Codec::Av1 => av1.to_vec(),
    }
}

/// Candidates to try for `cfg`. A forced encoder is the only candidate.
pub fn candidates(cfg: &EncoderConfig) -> Vec<Candidate> {
    match &cfg.force_encoder {
        Some(name) => vec![candidate(name)],
        None => platform_order(cfg.codec).into_iter().map(candidate).collect(),
    }
}

/// Codec-context level settings that are not AVOptions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CtxSettings {
    /// Set AV_CODEC_FLAG_QSCALE and `global_quality = qscale * FF_QP2LAMBDA`.
    pub qscale: Option<i32>,
    /// Force i/b quant factors to 1.0/offset 0 so QSV's CQP uses `qscale` as-is.
    pub unit_quant_factors: bool,
}

/// Encoder AVOptions for a family plus the manifest `encoder_params` record.
#[derive(Debug, Clone, Default)]
pub struct OptionSet {
    pub opts: Vec<(String, String)>,
    pub ctx: CtxSettings,
    /// Refuse this candidate (e.g. lossless requested but unsupported).
    pub unsupported: Option<String>,
}

impl OptionSet {
    fn o(&mut self, k: &str, v: impl ToString) {
        self.opts.push((k.to_string(), v.to_string()));
    }
}

/// VideoToolbox has no constant-QP mode for HEVC; map QP to its 0..100
/// constant-quality scale (QP 0 -> 100, QP 19 -> ~62, QP 51 -> 1).
pub fn vt_quality_from_qp(qp: u32) -> i32 {
    (100 - (qp as i32 * 2)).clamp(1, 100)
}

pub fn options_for(c: &Candidate, cfg: &EncoderConfig) -> OptionSet {
    let mut s = OptionSet::default();
    let qp = cfg.qp;
    let gop = cfg.gop.max(1);
    match c.family {
        Family::Nvenc => {
            s.o("preset", "p4");
            if cfg.lossless {
                s.o("tune", "lossless");
            } else {
                s.o("tune", "hq");
                s.o("rc", "constqp");
                s.o("qp", qp);
            }
            // pict_type=I on a frame must produce an IDR, not just an intra frame.
            s.o("forced-idr", 1);
            // Output each packet as soon as its frame is encoded (20 Hz; latency
            // matters more than NVENC pipelining throughput here).
            s.o("delay", 0);
            s.o("zerolatency", 1);
            s.o("b_ref_mode", "disabled");
        }
        Family::Amf => {
            if cfg.lossless {
                s.unsupported = Some("AMF has no lossless mode".into());
            }
            s.o("usage", "transcoding");
            s.o("quality", "quality");
            s.o("rc", "cqp");
            s.o("qp_i", qp);
            s.o("qp_p", qp);
            // Repeat VPS/SPS/PPS only via extradata (global header).
            s.o("header_insertion_mode", "none");
        }
        Family::Qsv => {
            if cfg.lossless {
                s.unsupported = Some("QSV HEVC has no lossless mode".into());
            }
            s.ctx.qscale = Some(qp as i32);
            s.ctx.unit_quant_factors = true;
            s.o("forced_idr", 1);
            s.o("look_ahead", 0);
            s.o("preset", "medium");
        }
        Family::Vaapi => {
            if cfg.lossless {
                s.unsupported = Some("VAAPI encoders have no lossless mode".into());
            }
            s.o("rc_mode", "CQP");
            s.o("qp", qp);
            s.o("idr_interval", 0);
        }
        Family::VideoToolbox => {
            if cfg.lossless {
                s.unsupported = Some("VideoToolbox has no lossless mode".into());
            }
            s.ctx.qscale = Some(vt_quality_from_qp(qp));
            s.o("allow_sw", 0);
            s.o("realtime", 1);
            s.o("prio_speed", 0);
        }
        Family::X265 => {
            let rc = if cfg.lossless { "lossless=1".to_string() } else { format!("qp={qp}") };
            s.o("preset", "fast");
            // No lookahead: one packet out per frame in, like the hw encoders.
            s.o("tune", "zerolatency");
            s.o("forced-idr", 1);
            s.o(
                "x265-params",
                format!("{rc}:keyint={gop}:min-keyint={gop}:scenecut=0:bframes=0:repeat-headers=0:log-level=error"),
            );
        }
        Family::OtherSoftware => {
            // Best effort for e.g. libsvtav1 / libaom-av1 (tests only).
            s.o("qp", qp);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_is_only_candidate() {
        let mut cfg = EncoderConfig::new(640, 360, 20);
        cfg.force_encoder = Some("libx265".into());
        let c = candidates(&cfg);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].hw, HwKind::Software);
        assert!(!is_hardware_name("libx265"));
        assert!(is_hardware_name("hevc_nvenc"));
    }

    #[test]
    fn probe_order_has_no_software() {
        for codec in [Codec::Hevc, Codec::Av1] {
            for n in platform_order(codec) {
                assert!(is_hardware_name(n), "{n}");
            }
        }
        #[cfg(target_os = "linux")]
        assert_eq!(platform_order(Codec::Hevc), vec!["hevc_nvenc", "hevc_vaapi"]);
    }

    #[test]
    fn nvenc_opts() {
        let cfg = EncoderConfig::new(640, 360, 20);
        let s = options_for(&candidate("hevc_nvenc"), &cfg);
        assert!(s.opts.contains(&("rc".into(), "constqp".into())));
        assert!(s.opts.contains(&("qp".into(), "19".into())));
        let mut l = cfg.clone();
        l.lossless = true;
        let s = options_for(&candidate("hevc_nvenc"), &l);
        assert!(s.opts.contains(&("tune".into(), "lossless".into())));
        assert!(!s.opts.iter().any(|(k, _)| k == "qp"));
    }
}
