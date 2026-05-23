use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::models::{FileFingerprint, FileMetadataParts};

pub(crate) fn file_metadata_parts(path: &Path) -> Result<FileMetadataParts> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to stat session file {}", path.display()))?;
    let modified_unix_nanos = metadata
        .modified()
        .ok()
        .and_then(system_time_unix_nanos)
        .unwrap_or_default();

    Ok(FileMetadataParts {
        size: metadata.len(),
        modified_unix_nanos,
    })
}

pub(crate) fn hash_file_fingerprint(
    path: &Path,
    relative_path: &str,
    metadata: FileMetadataParts,
) -> Result<FileFingerprint> {
    let content = fs::read(path)
        .with_context(|| format!("failed to hash session file {}", path.display()))?;
    let content_hash = hash_hex(&content);
    Ok(fingerprint_from_hash(relative_path, metadata, content_hash))
}

pub(crate) fn fingerprint_from_hash(
    relative_path: &str,
    metadata: FileMetadataParts,
    content_hash: String,
) -> FileFingerprint {
    let leaf_hash = hash_text(&format!(
        "file\0{}\0{}\0{}\0{}",
        relative_path, metadata.size, metadata.modified_unix_nanos, content_hash
    ));
    FileFingerprint {
        size: metadata.size,
        modified_unix_nanos: metadata.modified_unix_nanos,
        content_hash,
        leaf_hash,
    }
}
pub(crate) fn relative_path_string(root: &Path, path: &Path) -> Result<String> {
    if let Ok(relative) = path.strip_prefix(root) {
        return Ok(path_components_string(relative));
    }

    let canonical_root = root.canonicalize().ok();
    let canonical_path = canonical_equivalent_path(path);
    if let (Some(root), Some(path)) = (canonical_root.as_ref(), canonical_path.as_ref()) {
        if let Ok(relative) = path.strip_prefix(root) {
            return Ok(path_components_string(relative));
        }
    }

    bail!("{} is not under {}", path.display(), root.display())
}

pub(crate) fn canonical_equivalent_path(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return path.canonicalize().ok();
    }

    let parent = path.parent()?.canonicalize().ok()?;
    let file_name = path.file_name()?;
    Some(parent.join(file_name))
}

pub(crate) fn path_components_string(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::from_reader(file).with_context(|| format!("failed to parse {}", path.display()))
}

pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    write_bytes_atomic(path, &bytes)
}

pub(crate) fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "{}tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!("{ext}."))
            .unwrap_or_default()
    ));
    {
        let mut file =
            File::create(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", tmp.display()))?;
    }
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            path.display(),
            tmp.display()
        )
    })?;
    Ok(())
}

pub(crate) fn hash_text(text: &str) -> String {
    hash_hex(text.as_bytes())
}

pub(crate) fn hash_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_bytes(&digest)
}

pub(crate) fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{:02x}", *byte));
    }
    out
}

pub(crate) fn system_time_unix_nanos(time: SystemTime) -> Option<u64> {
    let duration = time.duration_since(UNIX_EPOCH).ok()?;
    duration
        .as_secs()
        .checked_mul(1_000_000_000)?
        .checked_add(u64::from(duration.subsec_nanos()))
}

pub(crate) fn unix_seconds_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}
