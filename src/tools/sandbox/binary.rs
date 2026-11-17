//! Layer 5: binary read fail-fast.
//!
//! Inspects file headers to detect binary content before plain text reads.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::SandboxError;

/// Layer 5: binary read fail-fast. Inspect the first 4096 bytes with
/// `content_inspector::inspect`: binary content (null bytes / non-text BOM)
/// yields `SandboxError::BinaryFile`, carrying the descriptive mime/extension
/// from `infer::get` when a known signature matches, or `application/octet-stream`
/// otherwise. Text content passes through.
pub fn check_binary(path: &Path) -> Result<PathBuf, SandboxError> {
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0u8; 4096];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    if content_inspector::inspect(&buf).is_binary() {
        let (detected_type, reason) = match infer::get(&buf) {
            Some(kind) => (
                format!("{} ({})", kind.mime_type(), kind.extension()),
                format!("magic bytes identify it as {}", kind.mime_type()),
            ),
            None => (
                "application/octet-stream (binary)".to_string(),
                "null bytes detected in file header".to_string(),
            ),
        };
        return Err(SandboxError::BinaryFile {
            file: path.to_path_buf(),
            detected_type,
            reason,
        });
    }
    Ok(path.to_path_buf())
}
