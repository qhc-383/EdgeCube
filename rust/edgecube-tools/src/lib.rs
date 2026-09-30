//! EdgeCube tools：解压 / FTP / SSH 等附加功能的 Android JNI 库。

#[cfg(feature = "archive")]
pub mod archive;

#[cfg(feature = "ftp")]
pub mod ftp;

#[cfg(feature = "ssh")]
pub mod ssh;

pub mod jni_util;

/// ftp/ssh 共用 tokio 运行时（单例）。
#[cfg(any(feature = "ftp", feature = "ssh"))]
pub mod rt;
