//! SFTP 文件系统后端：rootDir jail + 只读门禁 + 打开句柄表。

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, Metadata, Permissions};
use std::io::ErrorKind;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use russh_sftp::protocol::{
    Attrs, Data, File as SftpFile, FileAttributes, Handle as SftpHandle, Name, OpenFlags, Status,
    StatusCode,
};
use russh_sftp::server::{Handler, StatusReply};

/// 单次 `SSH_FXP_READ` 最多返回 64 KiB（协议允许任意值，取中庸）。
const READ_CAP: u32 = 64 * 1024;
/// 单次 `SSH_FXP_READDIR` 返回的条目批大小。
const READDIR_BATCH: usize = 128;
/// 符号链接展开上限（对齐 `ELOOP`）。
const MAX_SYMLINK_HOPS: usize = 40;

const READONLY_MSG: &str = "SFTP 处于只读模式，写操作已被拒绝";
const OUTSIDE_MSG: &str = "路径超出根目录";
const BAD_HANDLE_MSG: &str = "无效的句柄";

/// 一个 SFTP 子系统连接（`subsystem_request` 创建并移交给 `server::run`）。
pub(crate) struct Fs {
    /// 已 canonicalize 的根目录（jail 边界）。
    root: PathBuf,
    writable: bool,
    handles: HashMap<String, HandleKind>,
    next: u64,
}

enum HandleKind {
    /// 普通文件；`append` 时写入恒落在文件尾（SFTPv3 语义）。
    File { file: File, append: bool },
    /// 目录快照；`pos` 为 `READDIR` 游标。
    Dir {
        path: PathBuf,
        entries: Vec<SftpFile>,
        pos: usize,
    },
}

impl Fs {
    pub(crate) fn new(root: &Path, writable: bool) -> std::io::Result<Self> {
        let root = fs::canonicalize(root)?;
        Ok(Self {
            root,
            writable,
            handles: HashMap::new(),
            next: 0,
        })
    }

    fn alloc_handle(&mut self) -> String {
        let name = format!("h{}", self.next);
        self.next += 1;
        name
    }

    fn deny_write(&self) -> Result<(), StatusReply> {
        if self.writable {
            Ok(())
        } else {
            Err(denied(READONLY_MSG))
        }
    }

    /// 虚拟路径 → jail 内物理路径。
    fn resolve(&self, vpath: &str, follow: bool) -> Result<PathBuf, StatusReply> {
        let mut queue: VecDeque<String> = lex_clean(vpath).into();
        let mut cur = self.root.clone();
        let mut literal = false;
        let mut hops = 0usize;

        while let Some(comp) = queue.pop_front() {
            if comp == ".." {
                cur.pop();
                continue;
            }
            // 已进入字面尾段，或这是 `follow=false` 的最后一段 → 直接拼接。
            if literal || (queue.is_empty() && !follow) {
                cur.push(&comp);
                continue;
            }
            let next = cur.join(&comp);
            match fs::symlink_metadata(&next) {
                Ok(md) if md.file_type().is_symlink() => {
                    hops += 1;
                    if hops > MAX_SYMLINK_HOPS {
                        return Err(failure("符号链接层数过多"));
                    }
                    let target = fs::read_link(&next).map_err(io_err)?;
                    if target.is_absolute() {
                        cur = PathBuf::from("/");
                    }
                    let mut expanded: VecDeque<String> = target
                        .to_string_lossy()
                        .split('/')
                        .filter(|s| !s.is_empty() && *s != ".")
                        .map(String::from)
                        .collect();
                    expanded.append(&mut queue);
                    queue = expanded;
                }
                Ok(_) => cur = next,
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    literal = true;
                    cur = next;
                }
                Err(e) => return Err(io_err(e)),
            }
        }

        if !cur.starts_with(&self.root) {
            return Err(denied(OUTSIDE_MSG));
        }
        Ok(cur)
    }

    /// `opendir` 快照里 `..` 条目的属性（根目录的 `..` 指回根自身）。
    fn dotdot_attrs(&self, path: &Path) -> FileAttributes {
        let target = if path == self.root {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        fs::symlink_metadata(target)
            .map(|md| attrs_of(&md))
            .unwrap_or_default()
    }
}

impl Handler for Fs {
    type Error = StatusReply;

    fn unimplemented(&self) -> Self::Error {
        StatusReply::new(StatusCode::OpUnsupported)
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<SftpHandle, Self::Error> {
        let writeish = OpenFlags::WRITE
            | OpenFlags::APPEND
            | OpenFlags::CREATE
            | OpenFlags::TRUNCATE
            | OpenFlags::EXCLUDE;
        if !self.writable && pflags.intersects(writeish) {
            return Err(denied(READONLY_MSG));
        }
        if !pflags.intersects(OpenFlags::READ | OpenFlags::WRITE) {
            return Err(failure("打开模式缺少读写标志"));
        }
        let path = self.resolve(&filename, true)?;
        let append = pflags.contains(OpenFlags::APPEND);
        let file = fs::OpenOptions::from(pflags).open(&path).map_err(io_err)?;
        let handle = self.alloc_handle();
        self.handles
            .insert(handle.clone(), HandleKind::File { file, append });
        Ok(SftpHandle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        if self.handles.remove(&handle).is_some() {
            Ok(ok(id))
        } else {
            Err(failure(BAD_HANDLE_MSG))
        }
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let Some(HandleKind::File { file, .. }) = self.handles.get(&handle) else {
            return Err(failure(BAD_HANDLE_MSG));
        };
        let mut buf = vec![0u8; len.min(READ_CAP) as usize];
        let n = file.read_at(&mut buf, offset).map_err(io_err)?;
        if n == 0 {
            // SFTPv3 约定：读到尽头回 SSH_FX_EOF 而非空数据。
            return Err(StatusCode::Eof.into());
        }
        buf.truncate(n);
        Ok(Data { id, data: buf })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        self.deny_write()?;
        match self.handles.get_mut(&handle) {
            Some(HandleKind::File { file, append }) => {
                let at = if *append {
                    file.metadata().map_err(io_err)?.len()
                } else {
                    offset
                };
                file.write_all_at(&data, at).map_err(io_err)?;
                Ok(ok(id))
            }
            Some(HandleKind::Dir { .. }) => Err(failure("路径是目录")),
            None => Err(failure(BAD_HANDLE_MSG)),
        }
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let p = self.resolve(&path, false)?;
        let md = fs::symlink_metadata(p).map_err(io_err)?;
        Ok(Attrs {
            id,
            attrs: attrs_of(&md),
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let md = match self.handles.get(&handle) {
            Some(HandleKind::File { file, .. }) => file.metadata().map_err(io_err)?,
            Some(HandleKind::Dir { path, .. }) => fs::metadata(path).map_err(io_err)?,
            None => return Err(failure(BAD_HANDLE_MSG)),
        };
        Ok(Attrs {
            id,
            attrs: attrs_of(&md),
        })
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.deny_write()?;
        let p = self.resolve(&path, true)?;
        apply_path_attrs(&p, &attrs)?;
        Ok(ok(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        handle: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.deny_write()?;
        match self.handles.get(&handle) {
            Some(HandleKind::File { file, .. }) => {
                if let Some(size) = attrs.size {
                    file.set_len(size).map_err(io_err)?;
                }
                if let Some(mode) = attrs.permissions {
                    file.set_permissions(Permissions::from_mode(mode & 0o7777))
                        .map_err(io_err)?;
                }
            }
            Some(HandleKind::Dir { path, .. }) => {
                let path = path.clone();
                apply_path_attrs(&path, &attrs)?;
            }
            None => return Err(failure(BAD_HANDLE_MSG)),
        }
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<SftpHandle, Self::Error> {
        let p = self.resolve(&path, true)?;
        let md = fs::metadata(&p).map_err(io_err)?;
        if !md.is_dir() {
            return Err(failure("路径不是目录"));
        }
        let mut entries = vec![
            SftpFile::new(".", attrs_of(&md)),
            SftpFile::new("..", self.dotdot_attrs(&p)),
        ];
        for entry in fs::read_dir(&p).map_err(io_err)? {
            let Ok(entry) = entry else { continue };
            let Ok(emd) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            entries.push(SftpFile::new(name, attrs_of(&emd)));
        }
        let handle = self.alloc_handle();
        self.handles.insert(
            handle.clone(),
            HandleKind::Dir {
                path: p,
                entries,
                pos: 0,
            },
        );
        Ok(SftpHandle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let Some(HandleKind::Dir { entries, pos, .. }) = self.handles.get_mut(&handle) else {
            return Err(failure(BAD_HANDLE_MSG));
        };
        if *pos >= entries.len() {
            return Err(StatusCode::Eof.into());
        }
        let end = (*pos + READDIR_BATCH).min(entries.len());
        let files = entries[*pos..end].to_vec();
        *pos = end;
        Ok(Name { id, files })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        self.deny_write()?;
        let p = self.resolve(&filename, false)?;
        fs::remove_file(p).map_err(io_err)?;
        Ok(ok(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.deny_write()?;
        let p = self.resolve(&path, false)?;
        fs::create_dir(&p).map_err(io_err)?;
        if let Some(mode) = attrs.permissions {
            fs::set_permissions(&p, Permissions::from_mode(mode & 0o7777)).map_err(io_err)?;
        }
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        self.deny_write()?;
        // 不跟随最后一段：rmdir 一个符号链接应失败（内核 ENOTDIR 语义）。
        let p = self.resolve(&path, false)?;
        fs::remove_dir(p).map_err(io_err)?;
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        // 词法绝对化（`..` 已夹回根），不做符号链接解析——客户端多用于取
        // 显示用的绝对路径，词法结果恒在 jail 内。
        let comps = lex_clean(&path);
        let abs = if comps.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", comps.join("/"))
        };
        Ok(Name {
            id,
            files: vec![SftpFile::dummy(abs)],
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let p = self.resolve(&path, true)?;
        let md = fs::metadata(p).map_err(io_err)?;
        Ok(Attrs {
            id,
            attrs: attrs_of(&md),
        })
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        self.deny_write()?;
        // 两侧都不跟随最后一段：rename 移动的可以是符号链接本身。
        let from = self.resolve(&oldpath, false)?;
        let to = self.resolve(&newpath, false)?;
        fs::rename(from, to).map_err(io_err)?;
        Ok(ok(id))
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let p = self.resolve(&path, false)?;
        let target = fs::read_link(p).map_err(io_err)?;
        Ok(Name {
            id,
            files: vec![SftpFile::dummy(target.to_string_lossy().into_owned())],
        })
    }

    async fn symlink(
        &mut self,
        id: u32,
        linkpath: String,
        targetpath: String,
    ) -> Result<Status, Self::Error> {
        self.deny_write()?;
        // 链接本体必须在 jail 内；目标按客户端给的字面量存储（可指向 jail
        // 外——后续解析由 resolve 收尾的 starts_with 校验拦下）。
        let link = self.resolve(&linkpath, false)?;
        std::os::unix::fs::symlink(&targetpath, link).map_err(io_err)?;
        Ok(ok(id))
    }
}

/// 词法清洗：丢弃空段与 `.`，`..` 弹栈但不出根（对齐原词法 jail）。
fn lex_clean(vpath: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in vpath.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other.to_string()),
        }
    }
    out
}

/// `Metadata` → 协议属性；把类型位还原成内核原始 mode（`From<&Metadata>`
/// 会额外 OR 上 REG 位，符号链接等类型会失真）。
fn attrs_of(md: &Metadata) -> FileAttributes {
    let mut attrs = FileAttributes::from(md);
    attrs.permissions = Some(md.mode());
    attrs
}

/// `setstat` 的路径版应用：size（目录跳过）+ permissions（`0o7777` 掩码）。
fn apply_path_attrs(path: &Path, attrs: &FileAttributes) -> Result<(), StatusReply> {
    if let Some(size) = attrs.size {
        if fs::metadata(path).map_err(io_err)?.is_dir() {
            // 内核对目录 setsize 报 EISDIR；协议也未要求，忽略。
        } else {
            let file = fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(io_err)?;
            file.set_len(size).map_err(io_err)?;
        }
    }
    if let Some(mode) = attrs.permissions {
        fs::set_permissions(path, Permissions::from_mode(mode & 0o7777)).map_err(io_err)?;
    }
    Ok(())
}

fn io_err(e: std::io::Error) -> StatusReply {
    let status = match e.kind() {
        ErrorKind::NotFound => StatusCode::NoSuchFile,
        ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    };
    StatusReply::new(status).with_message(e.to_string())
}

fn denied(msg: &str) -> StatusReply {
    StatusReply::new(StatusCode::PermissionDenied).with_message(msg)
}

fn failure(msg: &str) -> StatusReply {
    StatusReply::new(StatusCode::Failure).with_message(msg)
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: String::new(),
    }
}
