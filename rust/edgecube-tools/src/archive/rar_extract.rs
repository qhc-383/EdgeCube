//! rar 解压

use std::path::Path;

use super::safe::{canonicalize_like_java, resolve_safe};
use super::{ArchError, Progress};

/// 解压 rar 归档；拒绝条目计数并报进度（与 tar 分支一致）。
pub fn extract_rar(archive: &Path, dest: &Path, progress: Progress) -> Result<i32, ArchError> {
    let base = canonicalize_like_java(dest);
    let mut handle = unrar::Archive::new(archive)
        .open_for_processing()
        .map_err(ArchError::io)?;
    let mut count = 0i32;
    loop {
        let Some(header) = handle.read_header().map_err(ArchError::io)? else {
            break;
        };
        let name = header.entry().filename.to_string_lossy().into_owned();
        if header.entry().is_directory() {
            if let Some(target) = resolve_safe(&base, &name) {
                let _ = std::fs::create_dir_all(&target);
            }
            progress(count, -1);
            handle = header.skip().map_err(ArchError::io)?;
            continue;
        }
        match resolve_safe(&base, &name) {
            None => {
                count += 1;
                progress(count, -1);
                handle = header.skip().map_err(ArchError::io)?;
            }
            Some(target) => {
                if let Some(parent) = target.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                handle = header.extract_to(&target).map_err(ArchError::io)?;
                count += 1;
                progress(count, -1);
            }
        }
    }
    Ok(count)
}
