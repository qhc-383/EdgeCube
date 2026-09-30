//! 7z 解压

use std::fs::File;
use std::path::Path;

use super::safe::{canonicalize_like_java, resolve_safe};
use super::{ArchError, Progress};

/// 解压 7z 归档；拒绝条目计数并报进度（与 tar 分支一致）。
pub fn extract_7z(archive: &Path, dest: &Path, progress: Progress) -> Result<i32, ArchError> {
    let base = canonicalize_like_java(dest);
    let mut reader = sevenz_rust2::ArchiveReader::open(archive, sevenz_rust2::Password::empty())
        .map_err(ArchError::io)?;
    let mut count = 0i32;
    let mut failure: Option<ArchError> = None;

    let walked = reader.for_each_entries(|entry, data| {
        if failure.is_some() {
            return Ok(false);
        }
        if entry.is_directory {
            if let Some(target) = resolve_safe(&base, &entry.name) {
                let _ = std::fs::create_dir_all(&target);
            }
            progress(count, -1);
            return Ok(true);
        }
        let Some(target) = resolve_safe(&base, &entry.name) else {
            count += 1;
            progress(count, -1);
            return Ok(true);
        };
        let written = (|| -> std::io::Result<()> {
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut out = File::create(&target)?;
            std::io::copy(data, &mut out)?;
            Ok(())
        })();
        if let Err(error) = written {
            failure = Some(ArchError::io(error));
            return Ok(false);
        }
        count += 1;
        progress(count, -1);
        Ok(true)
    });

    if let Some(error) = failure {
        return Err(error);
    }
    walked.map_err(ArchError::io)?;
    Ok(count)
}
