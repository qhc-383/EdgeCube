//! 解压模块：扩展名分发 + 各格式实现 + JNI 边界。

mod jni;
mod rar_extract;
mod safe;
mod sevenz_extract;
mod single_stream;
mod tar_extract;
mod zip_extract;
mod zip_write;

pub use safe::{canonicalize_joined, canonicalize_like_java, resolve_safe};
pub use zip_extract::{extract_prefixes, has_dir_prefix, read_entry};
pub use zip_write::compress_to_zip;

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

#[derive(Debug)]
pub enum ArchError {
    Arg(String),
    Security(String),
    Io(String),
}

impl ArchError {
    /// 包装 I/O / 解码错误为 [`ArchError::Io`]。
    pub fn io(error: impl std::fmt::Display) -> Self {
        Self::Io(error.to_string())
    }
}

/// 进度回调 `(current, total)`；普通解压恒以 `total = -1` 报告。
pub type Progress<'a> = &'a mut dyn FnMut(i32, i32);


pub fn extract(archive_path: &str, dest_dir: &str, progress: Progress) -> Result<i32, ArchError> {
    let archive = Path::new(archive_path);
    if !archive.is_file() {
        return Err(ArchError::Arg(format!("归档文件不存在：{archive_path}")));
    }
    let dest = Path::new(dest_dir);
    if !dest.is_dir() {
        let _ = std::fs::create_dir_all(dest);
    }
    let name = archive
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lower = name.to_lowercase();

    if lower.ends_with(".zip") {
        zip_extract::extract_zip(archive, dest, progress)
    } else if lower.ends_with(".tar") {
        let file = File::open(archive).map_err(ArchError::io)?;
        tar_extract::extract_tar(file, dest, progress)
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        let file = File::open(archive).map_err(ArchError::io)?;
        tar_extract::extract_tar(flate2::read::GzDecoder::new(file), dest, progress)
    } else if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
        let file = File::open(archive).map_err(ArchError::io)?;
        tar_extract::extract_tar(xz2::read::XzDecoder::new(file), dest, progress)
    } else if lower.ends_with(".tar.bz2") || lower.ends_with(".tbz2") {
        let file = File::open(archive).map_err(ArchError::io)?;
        tar_extract::extract_tar(bzip2::read::BzDecoder::new(file), dest, progress)
    } else if lower.ends_with(".tar.zst") || lower.ends_with(".tzst") {
        let file = File::open(archive).map_err(ArchError::io)?;
        let decoder =
            ruzstd::decoding::StreamingDecoder::new(BufReader::new(file)).map_err(ArchError::io)?;
        tar_extract::extract_tar(decoder, dest, progress)
    } else if lower.ends_with(".tar.lz4") {
        let file = File::open(archive).map_err(ArchError::io)?;
        let decoder = lz4_flex::frame::FrameDecoder::new(BufReader::new(file));
        tar_extract::extract_tar(decoder, dest, progress)
    } else if lower.ends_with(".7z") {
        sevenz_extract::extract_7z(archive, dest, progress)
    } else if lower.ends_with(".rar") {
        rar_extract::extract_rar(archive, dest, progress)
    } else if lower.ends_with(".xz") {
        let file = File::open(archive).map_err(ArchError::io)?;
        let out_name = strip_ext(&name, ".xz");
        single_stream::extract_single_stream(xz2::read::XzDecoder::new(file), dest, &out_name)
    } else if lower.ends_with(".gz") {
        let file = File::open(archive).map_err(ArchError::io)?;
        let out_name = strip_ext(&name, ".gz");
        single_stream::extract_single_stream(flate2::read::GzDecoder::new(file), dest, &out_name)
    } else if lower.ends_with(".bz2") {
        let file = File::open(archive).map_err(ArchError::io)?;
        let out_name = strip_ext(&name, ".bz2");
        single_stream::extract_single_stream(bzip2::read::BzDecoder::new(file), dest, &out_name)
    } else if lower.ends_with(".zst") {
        let file = File::open(archive).map_err(ArchError::io)?;
        let decoder =
            ruzstd::decoding::StreamingDecoder::new(BufReader::new(file)).map_err(ArchError::io)?;
        let out_name = strip_ext(&name, ".zst");
        single_stream::extract_single_stream(decoder, dest, &out_name)
    } else if lower.ends_with(".lz4") {
        let file = File::open(archive).map_err(ArchError::io)?;
        let decoder = lz4_flex::frame::FrameDecoder::new(BufReader::new(file));
        let out_name = strip_ext(&name, ".lz4");
        single_stream::extract_single_stream(decoder, dest, &out_name)
    } else {
        Err(ArchError::Arg(format!("不支持的归档格式：{name}")))
    }
}

/// 去掉文件名末尾的 `ext`（大小写不敏感），用于单文件压缩流的输出名。
fn strip_ext(name: &str, ext: &str) -> String {
    if name.to_lowercase().ends_with(ext) {
        name[..name.len() - ext.len()].to_string()
    } else {
        name.to_string()
    }
}
