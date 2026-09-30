//! Synthetic segment folders for tests (dummy bytes, correct manifest hashes).

use cap_types::{files, segment_dir_name, Manifest, SCHEMA_VERSION};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Write `<root>/<session_id>/seg_<idx>/` with dummy data files of the given
/// sizes (`video.mp4` gets `video_bytes`, the parquet files a few KB) and a
/// valid `manifest.json`. Written as `.partial` then renamed, like the recorder.
pub fn write_segment(root: &Path, session_id: &str, idx: u32, video_bytes: usize) -> std::io::Result<PathBuf> {
    let final_dir = root.join(session_id).join(segment_dir_name(idx));
    let partial = root.join(session_id).join(format!("{}{}", segment_dir_name(idx), files::PARTIAL_SUFFIX));
    std::fs::create_dir_all(&partial)?;
    let mut blake = BTreeMap::new();
    let mut sizes = BTreeMap::new();
    for (i, name) in files::DATA.iter().enumerate() {
        let len = if *name == files::VIDEO { video_bytes } else { 1024 * (i + 1) + idx as usize };
        let data: Vec<u8> = (0..len).map(|j| ((j * 31 + i * 7 + idx as usize) % 251) as u8).collect();
        std::fs::write(partial.join(name), &data)?;
        blake.insert(name.to_string(), blake3::hash(&data).to_hex().to_string());
        sizes.insert(name.to_string(), len as u64);
    }
    let m = Manifest {
        schema_version: SCHEMA_VERSION,
        session_id: session_id.into(),
        segment_idx: idx,
        client_version: "0.1.0".into(),
        os: "linux".into(),
        gpu: "test".into(),
        encoder: "hevc_nvenc".into(),
        encoder_params: BTreeMap::new(),
        width: 640,
        height: 360,
        rate_hz: 20,
        game_id: "testgame.exe".into(),
        t_start_ns: idx as i64 * 60_000_000_000,
        t_end_ns: (idx as i64 + 1) * 60_000_000_000,
        frame_count: 1200,
        dropped_frames: 0,
        latency_offset_ns: 0,
        blake3: blake,
        sizes,
    };
    std::fs::write(partial.join(files::MANIFEST), serde_json::to_vec_pretty(&m).unwrap())?;
    let _ = std::fs::remove_dir_all(&final_dir);
    std::fs::rename(&partial, &final_dir)?;
    Ok(final_dir)
}
