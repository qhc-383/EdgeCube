//! tar 及 tar.* 压缩流包装解压

use std::fs::File;
use std::io::Read;
use std::path::Path;

use super::safe::{canonicalize_like_java, resolve_safe};
use super::{ArchError, Progress};

/// 顺序读取 tar（`reader` 可为裸 tar 或任一压缩流解码器）。
pub fn extract_tar<R: Read>(reader: R, dest: &Path, progress: Progress) -> Result<i32, ArchError> {
    let base = canonicalize_like_java(dest);
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().map_err(ArchError::io)?;
    let mut count = 0i32;
    for entry in entries {
        let mut entry = entry.map_err(ArchError::io)?;
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        if entry.header().entry_type().is_dir() {
            if let Some(target) = resolve_safe(&base, &name) {
                let _ = std::fs::create_dir_all(&target);
            }
        } else {
            let Some(target) = resolve_safe(&base, &name) else {
                count += 1;
                progress(count, -1);
                continue;
            };
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut out = File::create(&target).map_err(ArchError::io)?;
            std::io::copy(&mut entry, &mut out).map_err(ArchError::io)?;
            count += 1;
        }
        progress(count, -1);
    }
    Ok(count)
}
