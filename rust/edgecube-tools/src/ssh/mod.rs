//! SSH 模块：russh 服务端 + SFTP + PTY shell 接 edgecube-pty

use std::path::PathBuf;

mod jni;
mod keys;
mod server;
mod sftp;
mod shell;

pub use keys::fingerprint as host_key_fingerprint;
pub use server::{is_running, start, stop};

/// 一次 SSH 服务启动的全部配置（`start` 解析 JSON 后物化，各连接共享）。
pub(crate) struct SshConfig {
    /// SFTP 根目录（jail 边界）；shell 初始 cwd 已在 Kotlin 侧算好，不在这里推导。
    pub root: PathBuf,
    pub username: String,
    pub password: String,
    /// SFTP 只读开关；不限制 SSH 终端。
    pub writable: bool,
    pub sftp_enabled: bool,
    pub shell_enabled: bool,
    pub shell: shell::ShellTemplate,
}
