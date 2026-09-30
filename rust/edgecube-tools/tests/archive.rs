//! M1 归档模块回归测试：格式分发、Zip Slip 防护、进度/计数语义、
//! ecpkg 前缀提取与异常消息 —— 与原 Kotlin 实现逐条对齐。
//!
//! 注：rar 无开源编码器，无法本地生成 fixture，rar 解压留真机验证。

use std::fs::{self, File};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use edgecube_tools::archive::{
    ArchError, canonicalize_like_java, compress_to_zip, extract, extract_prefixes, has_dir_prefix,
    read_entry, resolve_safe,
};

// ─── 测试基础设施 ───

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("edgecube-m1-{}-{}-{}", tag, std::process::id(), n));
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn read_to_string_checked(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn expect_arg(error: ArchError, message: &str) {
    match error {
        ArchError::Arg(actual) => assert_eq!(actual, message),
        other => panic!("expected Arg({message}), got {other:?}"),
    }
}

fn expect_security(error: ArchError, message: &str) {
    match error {
        ArchError::Security(actual) => assert_eq!(actual, message),
        other => panic!("expected Security({message}), got {other:?}"),
    }
}

/// 手工构造 zip（stored + UTF-8 标志）：完全控制条目名与 external attrs，
/// 用于生成 zip crate 写入器造不出的穿越名 / 符号链接条目。
#[derive(Clone)]
struct ZipSpec {
    name: &'static str,
    mode: u32,
    data: &'static [u8],
}

fn build_raw_zip(path: &Path, entries: &[ZipSpec]) {
    struct Written {
        name: String,
        mode: u32,
        crc: u32,
        size: u32,
        offset: u32,
    }

    let mut file = File::create(path).unwrap();
    let mut written = Vec::new();
    for entry in entries {
        let offset = file.stream_position().unwrap() as u32;
        let mut crc = flate2::Crc::new();
        crc.update(entry.data);
        let crc = crc.sum();
        let size = entry.data.len() as u32;

        file.write_all(&0x04034b50u32.to_le_bytes()).unwrap(); // local header sig
        file.write_all(&20u16.to_le_bytes()).unwrap(); // version needed
        file.write_all(&0x0800u16.to_le_bytes()).unwrap(); // UTF-8 flag
        file.write_all(&0u16.to_le_bytes()).unwrap(); // stored
        file.write_all(&0u16.to_le_bytes()).unwrap(); // mod time
        file.write_all(&0x21u16.to_le_bytes()).unwrap(); // mod date (1980-01-01)
        file.write_all(&crc.to_le_bytes()).unwrap();
        file.write_all(&size.to_le_bytes()).unwrap();
        file.write_all(&size.to_le_bytes()).unwrap();
        file.write_all(&(entry.name.len() as u16).to_le_bytes())
            .unwrap();
        file.write_all(&0u16.to_le_bytes()).unwrap(); // extra len
        file.write_all(entry.name.as_bytes()).unwrap();
        file.write_all(entry.data).unwrap();

        written.push(Written {
            name: entry.name.to_string(),
            mode: entry.mode,
            crc,
            size,
            offset,
        });
    }

    let cd_offset = file.stream_position().unwrap() as u32;
    for item in &written {
        file.write_all(&0x02014b50u32.to_le_bytes()).unwrap(); // central sig
        file.write_all(&0x031eu16.to_le_bytes()).unwrap(); // made by: unix 3.0
        file.write_all(&20u16.to_le_bytes()).unwrap(); // version needed
        file.write_all(&0x0800u16.to_le_bytes()).unwrap(); // UTF-8 flag
        file.write_all(&0u16.to_le_bytes()).unwrap(); // stored
        file.write_all(&0u16.to_le_bytes()).unwrap(); // time
        file.write_all(&0x21u16.to_le_bytes()).unwrap(); // date
        file.write_all(&item.crc.to_le_bytes()).unwrap();
        file.write_all(&item.size.to_le_bytes()).unwrap();
        file.write_all(&item.size.to_le_bytes()).unwrap();
        file.write_all(&(item.name.len() as u16).to_le_bytes())
            .unwrap();
        file.write_all(&0u16.to_le_bytes()).unwrap(); // extra
        file.write_all(&0u16.to_le_bytes()).unwrap(); // comment
        file.write_all(&0u16.to_le_bytes()).unwrap(); // disk
        file.write_all(&0u16.to_le_bytes()).unwrap(); // internal attrs
        file.write_all(&(item.mode << 16).to_le_bytes()).unwrap(); // external attrs
        file.write_all(&item.offset.to_le_bytes()).unwrap();
        file.write_all(item.name.as_bytes()).unwrap();
    }
    let cd_size = (file.stream_position().unwrap() as u32) - cd_offset;
    let count = written.len() as u16;

    file.write_all(&0x06054b50u32.to_le_bytes()).unwrap(); // EOCD
    file.write_all(&0u16.to_le_bytes()).unwrap(); // disk
    file.write_all(&0u16.to_le_bytes()).unwrap(); // cd disk
    file.write_all(&count.to_le_bytes()).unwrap();
    file.write_all(&count.to_le_bytes()).unwrap();
    file.write_all(&cd_size.to_le_bytes()).unwrap();
    file.write_all(&cd_offset.to_le_bytes()).unwrap();
    file.write_all(&0u16.to_le_bytes()).unwrap(); // comment len
}

/// 写入原始条目名（绕过 `Header::set_path` 校验，测试穿越名用）。
fn set_tar_name(header: &mut tar::Header, name: &str) {
    let bytes = name.as_bytes();
    assert!(bytes.len() < 100, "fixture name too long");
    let field = &mut header.as_old_mut().name;
    field[..bytes.len()].copy_from_slice(bytes);
    field[bytes.len()..].fill(0);
}

fn tar_file(builder: &mut tar::Builder<Vec<u8>>, name: &str, content: &[u8]) {
    let mut header = tar::Header::new_gnu();
    set_tar_name(&mut header, name);
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append(&header, content).unwrap();
}

fn tar_dir(builder: &mut tar::Builder<Vec<u8>>, name: &str) {
    let mut header = tar::Header::new_gnu();
    set_tar_name(&mut header, name);
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    header.set_cksum();
    builder.append(&header, std::io::empty()).unwrap();
}

fn tar_symlink(builder: &mut tar::Builder<Vec<u8>>, name: &str, target: &str) {
    let mut header = tar::Header::new_gnu();
    set_tar_name(&mut header, name);
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    header.set_link_name(target).unwrap();
    header.set_cksum();
    builder.append(&header, std::io::empty()).unwrap();
}

// ─── compress / extract 往返 ───

#[test]
fn compress_and_extract_zip_roundtrip() {
    let tmp = TempDir::new("roundtrip");
    let src = tmp.path().join("src");
    write_file(&src.join("a.txt"), "alpha");
    write_file(&src.join("sub").join("中文.txt"), "unicode content");
    fs::create_dir_all(src.join("empty")).unwrap();

    let archive = tmp.path().join("out.zip");
    let count = compress_to_zip(
        &[src.to_string_lossy().into_owned()],
        &archive.to_string_lossy(),
    )
    .unwrap();
    assert_eq!(count, 2, "空目录与子目录自身不计数");

    let dest = tmp.path().join("dest");
    let mut progress = Vec::new();
    let extracted = extract(
        &archive.to_string_lossy(),
        &dest.to_string_lossy(),
        &mut |current, total| progress.push((current, total)),
    )
    .unwrap();
    assert_eq!(extracted, 2);
    assert_eq!(
        read_to_string_checked(&dest.join("src").join("a.txt")),
        "alpha"
    );
    assert_eq!(
        read_to_string_checked(&dest.join("src").join("sub").join("中文.txt")),
        "unicode content"
    );
    assert!(dest.join("src").join("empty").is_dir());
    assert_eq!(progress.last(), Some(&(5, -1)), "zip 进度含目录：5 个条目");

    // 归档自身包含在源列表中 → canonical 相等被跳过。
    let self_count = compress_to_zip(
        &[archive.to_string_lossy().into_owned()],
        &archive.to_string_lossy(),
    )
    .unwrap();
    assert_eq!(self_count, 0);
}

#[test]
fn zip_slip_entries_skipped_without_counting() {
    let tmp = TempDir::new("zip-slip");
    let archive = tmp.path().join("slip.zip");
    build_raw_zip(
        &archive,
        &[
            ZipSpec {
                name: "safe.txt",
                mode: 0o100644,
                data: b"safe",
            },
            ZipSpec {
                name: "d/",
                mode: 0o040755,
                data: b"",
            },
            ZipSpec {
                name: "../evil.txt",
                mode: 0o100644,
                data: b"evil",
            },
            ZipSpec {
                name: "sub/ok.txt",
                mode: 0o100644,
                data: b"ok",
            },
        ],
    );

    let dest_root = tmp.path().join("dest");
    let dest = dest_root.join("inner");
    fs::create_dir_all(&dest).unwrap();
    let mut progress = Vec::new();
    let count = extract(
        &archive.to_string_lossy(),
        &dest.to_string_lossy(),
        &mut |current, total| progress.push((current, total)),
    )
    .unwrap();

    assert_eq!(count, 2, "被拒条目不计数（zip 怪癖）");
    assert_eq!(progress, vec![(1, -1), (2, -1), (3, -1)]);
    assert!(dest.join("safe.txt").is_file());
    assert!(dest.join("sub").join("ok.txt").is_file());
    assert!(!dest.join("evil.txt").exists());
    assert!(
        !dest_root.join("evil.txt").exists(),
        "穿越条目不得逃出 dest"
    );
}

#[test]
fn tar_rejected_entries_counted_and_symlink_becomes_empty_file() {
    let tmp = TempDir::new("tar-quirks");
    let mut builder = tar::Builder::new(Vec::new());
    tar_file(&mut builder, "safe.txt", b"safe");
    tar_dir(&mut builder, "dd");
    tar_file(&mut builder, "../evil.txt", b"evil");
    tar_symlink(&mut builder, "link", "safe.txt");
    let tar_bytes = builder.into_inner().unwrap();

    let archive = tmp.path().join("t.tar");
    fs::write(&archive, &tar_bytes).unwrap();

    let dest_root = tmp.path().join("dest");
    let dest = dest_root.join("inner");
    fs::create_dir_all(&dest).unwrap();
    let mut progress = Vec::new();
    let count = extract(
        &archive.to_string_lossy(),
        &dest.to_string_lossy(),
        &mut |current, total| progress.push((current, total)),
    )
    .unwrap();

    assert_eq!(count, 3, "tar 被拒条目也计数（怪癖）；目录不计数");
    assert_eq!(
        progress,
        vec![(1, -1), (1, -1), (2, -1), (3, -1)],
        "每条目恰好一次进度，目录报进度但 count 不变"
    );
    assert!(dest.join("safe.txt").is_file());
    assert!(dest.join("dd").is_dir());
    assert!(!dest_root.join("evil.txt").exists());
    let link = dest.join("link");
    assert!(link.is_file());
    assert_eq!(
        fs::metadata(&link).unwrap().len(),
        0,
        "符号链接条目写为空文件"
    );
}

#[test]
fn tar_compressed_variants_dispatch() {
    let tmp = TempDir::new("tar-variants");
    let mut builder = tar::Builder::new(Vec::new());
    tar_file(&mut builder, "payload.txt", b"variant payload");
    tar_dir(&mut builder, "folder");
    let tar_bytes = builder.into_inner().unwrap();

    fn gz(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }
    fn xz(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 6);
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }
    fn bz2(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(9));
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }
    fn zst(data: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(data, 0).unwrap()
    }
    fn lz4(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    let variants: Vec<(&str, Vec<u8>)> = vec![
        ("v.tar", tar_bytes.clone()),
        ("v.tar.gz", gz(&tar_bytes)),
        ("v.tgz", gz(&tar_bytes)),
        ("v.tar.xz", xz(&tar_bytes)),
        ("v.txz", xz(&tar_bytes)),
        ("v.tar.bz2", bz2(&tar_bytes)),
        ("v.tbz2", bz2(&tar_bytes)),
        ("v.tar.zst", zst(&tar_bytes)),
        ("v.tzst", zst(&tar_bytes)),
        ("v.tar.lz4", lz4(&tar_bytes)),
    ];

    for (index, (name, bytes)) in variants.iter().enumerate() {
        let archive = tmp.path().join(name);
        fs::write(&archive, bytes).unwrap();
        let dest = tmp.path().join(format!("dest-{index}"));
        let mut progress = Vec::new();
        let count = extract(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            &mut |current, total| progress.push((current, total)),
        )
        .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert_eq!(count, 1, "{name}: tar 目录不计数，仅 1 个文件");
        assert_eq!(
            read_to_string_checked(&dest.join("payload.txt")),
            "variant payload",
            "{name}"
        );
        assert!(dest.join("folder").is_dir(), "{name}");
    }
}

#[test]
fn single_stream_formats_write_stripped_file_without_progress() {
    let tmp = TempDir::new("single");
    let payload = b"single stream payload";

    fn gz(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }
    fn xz(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 6);
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }
    fn bz2(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(9));
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }
    fn zst(data: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(data, 0).unwrap()
    }
    fn lz4(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    let variants: Vec<(&str, Vec<u8>, &str)> = vec![
        ("payload.dat.gz", gz(payload), "payload.dat"),
        ("payload.dat.xz", xz(payload), "payload.dat"),
        ("payload.dat.bz2", bz2(payload), "payload.dat"),
        ("payload.dat.zst", zst(payload), "payload.dat"),
        ("payload.dat.lz4", lz4(payload), "payload.dat"),
        ("PAYLOAD.DAT.GZ", gz(payload), "PAYLOAD.DAT"), // 输出名保留原大小写（对齐 stripExt）
    ];

    for (index, (name, bytes, out_name)) in variants.iter().enumerate() {
        let archive = tmp.path().join(name);
        fs::write(&archive, bytes).unwrap();
        let dest = tmp.path().join(format!("dest-{index}"));
        let mut progress = Vec::new();
        let count = extract(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            &mut |current, total| progress.push((current, total)),
        )
        .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert_eq!(count, 1, "{name}");
        assert!(progress.is_empty(), "{name}: 单流无进度回调");
        assert_eq!(
            read_to_string_checked(&dest.join(out_name)),
            "single stream payload",
            "{name}"
        );
    }
}

// ─── 路径安全 ───

#[test]
fn resolve_safe_rejects_escapes_and_normalizes() {
    let tmp = TempDir::new("resolve");
    let base = tmp.path().join("base");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, base.join("escape")).unwrap();

    let base_canon = canonicalize_like_java(&base);

    assert_eq!(
        resolve_safe(&base_canon, "../evil.txt"),
        None,
        "词法穿越必须拒绝"
    );
    assert_eq!(
        resolve_safe(&base_canon, "sub/../../evil.txt"),
        None,
        "多级词法穿越必须拒绝"
    );
    assert_eq!(
        resolve_safe(&base_canon, "escape/link.txt"),
        None,
        "经符号链接逃逸必须拒绝"
    );
    assert_eq!(
        resolve_safe(&base_canon, "escape/../ok.txt"),
        None,
        "符号链接 + `..` 按 realpath 语义拒绝（对齐 Java canonicalize）"
    );
    assert_eq!(
        resolve_safe(&base_canon, "a\\b.txt"),
        Some(base_canon.join("a").join("b.txt")),
        "反斜杠统一为正斜杠"
    );
    assert_eq!(
        resolve_safe(&base_canon, "/abs.txt"),
        Some(base_canon.join("abs.txt")),
        "绝对条目名被重根到 base 下（Java 字符串拼接怪癖）"
    );
    assert_eq!(
        resolve_safe(&base_canon, "sub/../ok.txt"),
        Some(base_canon.join("ok.txt"))
    );
}

#[test]
fn canonicalize_resolves_existing_symlink_prefix_lexical_tail() {
    let tmp = TempDir::new("canonical");
    let base = tmp.path().join("base");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(base.join("real")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, base.join("real").join("link")).unwrap();

    let resolved = canonicalize_like_java(&base.join("real").join("link").join("x.txt"));
    assert_eq!(
        resolved,
        outside.join("x.txt"),
        "已存在符号链接前缀须 realpath 化"
    );

    let lexical = canonicalize_like_java(&base.join("nope").join("..").join("real"));
    assert_eq!(lexical, base.join("real"), "未知后缀按词法处理");
}

// ─── 错误消息（逐字对齐） ───

#[test]
fn error_messages_match_original() {
    let tmp = TempDir::new("errors");
    let missing = tmp.path().join("nope.tar");
    let dest = tmp.path().join("dest");
    let error = extract(
        &missing.to_string_lossy(),
        &dest.to_string_lossy(),
        &mut |_, _| {},
    )
    .unwrap_err();
    expect_arg(
        error,
        &format!("归档文件不存在：{}", missing.to_string_lossy()),
    );

    let unknown = tmp.path().join("data.bin");
    fs::write(&unknown, b"xx").unwrap();
    let error = extract(
        &unknown.to_string_lossy(),
        &dest.to_string_lossy(),
        &mut |_, _| {},
    )
    .unwrap_err();
    expect_arg(error, "不支持的归档格式：data.bin");

    let error = compress_to_zip(&[], "/tmp/unused.zip").unwrap_err();
    expect_arg(error, "没有可压缩的文件");
}

// ─── ecpkg（RuntimeInstaller.importPackage 对应） ───

fn ecpkg_zip(path: &Path, extra: &[ZipSpec]) {
    let mut entries = vec![
        ZipSpec {
            name: "edgecube-package.json",
            mode: 0o100644,
            data: br#"{"id":"demo"}"#,
        },
        ZipSpec {
            name: "universal/bin/u.txt",
            mode: 0o100644,
            data: b"U",
        },
        ZipSpec {
            name: "universal/lib/common.txt",
            mode: 0o100644,
            data: b"universal-common",
        },
        ZipSpec {
            name: "arch-arm64/lib/a64.txt",
            mode: 0o100644,
            data: b"A64",
        },
        ZipSpec {
            name: "arch-arm64/lib/common.txt",
            mode: 0o100644,
            data: b"arch-common",
        },
    ];
    entries.extend_from_slice(extra);
    build_raw_zip(path, &entries);
}

#[test]
fn ecpkg_prefix_extract_overrides_and_symlink() {
    let tmp = TempDir::new("ecpkg");
    let archive = tmp.path().join("pkg.ecpkg");
    ecpkg_zip(
        &archive,
        &[ZipSpec {
            name: "universal/sub/link.txt",
            mode: 0o120777,
            data: b"../common.txt",
        }],
    );

    let dest = tmp.path().join("tmp-dir");
    fs::create_dir_all(&dest).unwrap();
    let (processed, total) =
        extract_prefixes(&archive, &dest, Some("universal"), "arch-arm64").unwrap();

    assert_eq!(total, 6, "total = 全部条目数");
    assert_eq!(
        processed, 5,
        "universal 3 条 + arch 2 条（根清单不在前缀内）"
    );

    assert_eq!(read_to_string_checked(&dest.join("bin").join("u.txt")), "U");
    assert_eq!(
        read_to_string_checked(&dest.join("lib").join("a64.txt")),
        "A64"
    );
    assert_eq!(
        read_to_string_checked(&dest.join("lib").join("common.txt")),
        "arch-common",
        "arch 前缀覆盖同名文件"
    );
    let link = dest.join("sub").join("link.txt");
    let link_meta = fs::symlink_metadata(&link).unwrap();
    assert!(link_meta.file_type().is_symlink());
    assert_eq!(
        fs::read_link(&link).unwrap(),
        Path::new("../common.txt"),
        "链接内容按原文创建"
    );

    // 目录前缀存在性检查。
    assert!(has_dir_prefix(&archive, "universal").unwrap());
    assert!(!has_dir_prefix(&archive, "nope").unwrap());

    // 清单条目读取。
    assert_eq!(
        read_entry(&archive, "edgecube-package.json").unwrap(),
        r#"{"id":"demo"}"#
    );
    let error = read_entry(&archive, "missing.json").unwrap_err();
    expect_arg(error, "ZIP 中缺少 missing.json");
}

#[test]
fn ecpkg_illegal_relative_path_throws_security() {
    let tmp = TempDir::new("ecpkg-illegal");
    let archive = tmp.path().join("pkg.ecpkg");
    ecpkg_zip(
        &archive,
        &[ZipSpec {
            name: "universal/../evil.txt",
            mode: 0o100644,
            data: b"x",
        }],
    );

    let dest = tmp.path().join("tmp-dir");
    fs::create_dir_all(&dest).unwrap();
    let error = extract_prefixes(&archive, &dest, Some("universal"), "arch-arm64").unwrap_err();
    expect_security(error, "非法路径：universal/../evil.txt");
    assert!(!dest.join("evil.txt").exists());
}

#[test]
fn ecpkg_symlink_escape_throws_security() {
    let tmp = TempDir::new("ecpkg-escape");
    let archive = tmp.path().join("pkg.ecpkg");
    ecpkg_zip(
        &archive,
        &[ZipSpec {
            name: "arch-arm64/bad/link2.txt",
            mode: 0o120777,
            data: b"../../../../../../etc/passwd",
        }],
    );

    let dest = tmp.path().join("tmp-dir");
    fs::create_dir_all(&dest).unwrap();
    let error = extract_prefixes(&archive, &dest, None, "arch-arm64").unwrap_err();
    expect_security(
        error,
        "符号链接目标逃逸：arch-arm64/bad/link2.txt -> ../../../../../../etc/passwd",
    );
}

// ─── 7z ───

#[test]
fn sevenz_extract_counts_like_tar() {
    let tmp = TempDir::new("7z");
    let archive = tmp.path().join("a.7z");
    {
        let mut writer = sevenz_rust2::ArchiveWriter::create(&archive).unwrap();
        writer
            .push_archive_entry(
                sevenz_rust2::ArchiveEntry::new_file("safe.txt"),
                Some(&b"sevenz content"[..]),
            )
            .unwrap();
        writer
            .push_archive_entry(
                sevenz_rust2::ArchiveEntry::new_directory("dd"),
                None as Option<&[u8]>,
            )
            .unwrap();
        writer
            .push_archive_entry(
                sevenz_rust2::ArchiveEntry::new_file("../evil7.txt"),
                Some(&b"evil"[..]),
            )
            .unwrap();
        writer.finish().unwrap();
    }

    let dest_root = tmp.path().join("dest");
    let dest = dest_root.join("inner");
    fs::create_dir_all(&dest).unwrap();
    let mut progress = Vec::new();
    let count = extract(
        &archive.to_string_lossy(),
        &dest.to_string_lossy(),
        &mut |current, total| progress.push((current, total)),
    )
    .unwrap();

    assert_eq!(count, 2, "7z 被拒条目也计数（怪癖）；目录不计数");
    assert_eq!(
        progress,
        vec![(1, -1), (2, -1), (2, -1)],
        "sevenz-rust2 先按块解码有数据条目、空条目（目录）居后，count 序列仍单调"
    );
    assert_eq!(
        read_to_string_checked(&dest.join("safe.txt")),
        "sevenz content"
    );
    assert!(dest.join("dd").is_dir());
    assert!(!dest_root.join("evil7.txt").exists());
}
