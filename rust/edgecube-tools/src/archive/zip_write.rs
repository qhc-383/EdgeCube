//! `compressToZip`：UTF-8 条目名、deflate level 9、目录条目先于子项、
//! 子项按小写名稳定排序

use std::fs::{self, File};
use std::path::Path;

use zip::write::SimpleFileOptions;

use super::ArchError;
use super::safe::canonicalize_like_java;

/// 把 `source_paths` 中的文件/目录压缩为 zip；返回写入的文件数
/// （空目录不计、源不存在跳过、源为归档自身跳过）。
pub fn compress_to_zip(source_paths: &[String], archive_path: &str) -> Result<i32, ArchError> {
    if source_paths.is_empty() {
        return Err(ArchError::Arg("没有可压缩的文件".to_string()));
    }
    let archive = Path::new(archive_path);
    if let Some(parent) = archive.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = fs::create_dir_all(parent);
    }
    let archive_canon = canonicalize_like_java(archive);
    let file = File::create(archive).map_err(ArchError::io)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(9));

    let mut count = 0i32;
    for path in source_paths {
        let source = Path::new(path);
        if !source.exists() {
            continue;
        }
        let entry_name = source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        count += add_entry(&mut zip, source, &entry_name, &archive_canon, options)?;
    }
    zip.finish().map_err(ArchError::io)?;
    Ok(count)
}

fn add_entry(
    zip: &mut zip::ZipWriter<File>,
    source: &Path,
    entry_name: &str,
    archive_canon: &Path,
    options: SimpleFileOptions,
) -> Result<i32, ArchError> {
    if canonicalize_like_java(source) == archive_canon {
        return Ok(0);
    }
    let normalized = entry_name.replace('\\', "/");
    if source.is_dir() {
        let dir_name = format!("{}/", normalized.trim_end_matches('/'));
        zip.add_directory(&dir_name, options)
            .map_err(ArchError::io)?;
        let mut children: Vec<_> = fs::read_dir(source)
            .map_err(ArchError::io)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        children.sort_by_key(|path| {
            path.file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        });
        let mut count = 0;
        for child in children {
            let child_name = child
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            count += add_entry(
                zip,
                &child,
                &format!("{dir_name}{child_name}"),
                archive_canon,
                options,
            )?;
        }
        return Ok(count);
    }

    zip.start_file(&normalized, options)
        .map_err(ArchError::io)?;
    let mut input = File::open(source).map_err(ArchError::io)?;
    std::io::copy(&mut input, zip).map_err(ArchError::io)?;
    Ok(1)
}
