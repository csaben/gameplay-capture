//! Reading and verifying a finished segment folder.

use crate::{Error, Result};
use cap_types::{files, Manifest};
use std::path::{Path, PathBuf};

/// One file of a segment, verified against the manifest.
#[derive(Debug, Clone)]
pub struct SegmentFile {
    /// File name inside the segment folder (e.g. `video.mp4`).
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    /// blake3 hex digest.
    pub blake3: String,
}

/// A segment folder whose data files matched `manifest.blake3` / `manifest.sizes`.
#[derive(Debug, Clone)]
pub struct VerifiedSegment {
    pub dir: PathBuf,
    pub manifest: Manifest,
    /// Data files in upload order (the order of `files::DATA`, then any extra
    /// files the manifest lists, sorted by name).
    pub data_files: Vec<SegmentFile>,
    /// `manifest.json` itself; always uploaded last.
    pub manifest_file: SegmentFile,
}

impl VerifiedSegment {
    /// Data files followed by the manifest: the order objects must be written in.
    pub fn upload_order(&self) -> impl Iterator<Item = &SegmentFile> {
        self.data_files.iter().chain(std::iter::once(&self.manifest_file))
    }
}

/// True if `dir` looks like a finished segment folder (has `manifest.json`
/// and is not a `.partial` folder).
pub fn is_finished_segment(dir: &Path) -> bool {
    let partial = dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.ends_with(files::PARTIAL_SUFFIX))
        .unwrap_or(true);
    !partial && dir.join(files::MANIFEST).is_file()
}

pub fn read_manifest(dir: &Path) -> Result<Manifest> {
    let p = dir.join(files::MANIFEST);
    let bytes = std::fs::read(&p).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Permanent(format!("{} missing", p.display()))
        } else {
            Error::Io(e)
        }
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|e| Error::Permanent(format!("invalid manifest {}: {e}", p.display())))
}

/// Hash a file with blake3 (streaming, blocking).
pub fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut f = std::fs::File::open(path)?;
    let mut h = blake3::Hasher::new();
    h.update_reader(&mut f)?;
    let size = f.metadata()?.len();
    Ok((h.finalize().to_hex().to_string(), size))
}

/// Blocking: re-hash every data file and compare against the manifest.
/// Mismatches and missing files are permanent errors (the segment is corrupt).
pub fn verify_segment(dir: &Path) -> Result<VerifiedSegment> {
    let manifest = read_manifest(dir)?;
    if manifest.blake3.is_empty() {
        return Err(Error::Permanent("manifest lists no files".into()));
    }
    let mut names: Vec<String> = files::DATA
        .iter()
        .filter(|n| manifest.blake3.contains_key(**n))
        .map(|s| s.to_string())
        .collect();
    let mut extra: Vec<String> = manifest
        .blake3
        .keys()
        .filter(|k| !files::DATA.contains(&k.as_str()))
        .cloned()
        .collect();
    extra.sort();
    names.extend(extra);

    let mut data_files = Vec::with_capacity(names.len());
    for name in names {
        if name == files::MANIFEST || name.contains('/') || name.contains('\\') || name.starts_with('.') {
            return Err(Error::Permanent(format!("manifest lists invalid file name {name:?}")));
        }
        let path = dir.join(&name);
        let (hash, size) = match hash_file(&path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::Permanent(format!("{name} listed in manifest but missing")))
            }
            Err(e) => return Err(Error::Io(e)),
        };
        let want = &manifest.blake3[&name];
        if !hash.eq_ignore_ascii_case(want) {
            return Err(Error::Permanent(format!("blake3 mismatch for {name}: manifest {want}, file {hash}")));
        }
        if let Some(&want_size) = manifest.sizes.get(&name) {
            if want_size != size {
                return Err(Error::Permanent(format!("size mismatch for {name}: manifest {want_size}, file {size}")));
            }
        }
        data_files.push(SegmentFile { name, path, size, blake3: hash });
    }
    let mpath = dir.join(files::MANIFEST);
    let (mhash, msize) = hash_file(&mpath)?;
    Ok(VerifiedSegment {
        dir: dir.to_path_buf(),
        manifest,
        data_files,
        manifest_file: SegmentFile { name: files::MANIFEST.into(), path: mpath, size: msize, blake3: mhash },
    })
}

/// Object key for one file: `raw/<user_id>/<session_id>/seg_<nnnnnn>/<file>`.
///
/// `seg_<n>` uses the same zero-padded name as the local folder
/// (`cap_types::segment_dir_name`).
pub fn object_key(user_id: &str, session_id: &str, segment_idx: u32, file: &str) -> String {
    format!("{}/{file}", segment_prefix(user_id, session_id, segment_idx))
}

/// `raw/<user_id>/<session_id>/seg_<nnnnnn>`
pub fn segment_prefix(user_id: &str, session_id: &str, segment_idx: u32) -> String {
    format!("raw/{user_id}/{session_id}/{}", cap_types::segment_dir_name(segment_idx))
}

/// IDs used as object key components: `[A-Za-z0-9._-]{1,128}`, not starting with `.`.
pub fn valid_key_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}
