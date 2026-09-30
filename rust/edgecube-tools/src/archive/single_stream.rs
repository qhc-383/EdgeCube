//! 单文件压缩流（xz / gz / bz2 / zst / lz4）解压

use std::fs::File;
use std::io::Read;
use std::path::Path;

use super::ArchError;

/// 解压为 `dest/outName` 单个文件。
pub fn extract_single_stream<R: Read>(
    mut input: R,
    dest: &Path,
    out_name: &str,
) -> Result<i32, ArchError> {
    let target = dest.join(out_name);
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut out = File::create(&target).map_err(ArchError::io)?;
    std::io::copy(&mut input, &mut out).map_err(ArchError::io)?;
    Ok(1)
}
