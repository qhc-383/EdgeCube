//! 主机密钥：加载 / 首次生成 OpenSSH ed25519 私钥（0600）并给出 SHA-256 指纹。
//!
//! 替代原 `SimpleGeneratorHostKeyProvider`（RSA-2048 `hostkey.ser`，Java
//! 序列化格式）：新格式为 OpenSSH PEM，旧文件不再读取——老客户端二次连接
//! 必然提示 host key changed（计划内行为，见 PLAN M3 执行记录）。
//!
//! 查询指纹与服务启动共用同一路径（`filesDir/ssh/hostkey`），文件不存在时
//! 先生成再算指纹（对齐原 `hostKeyFingerprint()` 的惰性生成语义）。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, HashAlg, PrivateKey, key::safe_rng, load_secret_key};

/// 加载主机密钥；文件不存在则生成 ed25519 并以 0600 落盘（对齐原
/// SimpleGeneratorHostKeyProvider 的首次生成语义）。
pub(crate) fn load_or_generate(path: &Path) -> Result<PrivateKey, String> {
    if path.exists() {
        return load_secret_key(path, None).map_err(|e| format!("读取主机密钥失败：{e}"));
    }

    let mut rng = safe_rng();
    let key = PrivateKey::random(&mut rng, Algorithm::Ed25519)
        .map_err(|e| format!("生成主机密钥失败：{e}"))?;
    let pem = key
        .to_openssh(LineEnding::LF)
        .map_err(|e| format!("编码主机密钥失败：{e}"))?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建主机密钥目录失败：{e}"))?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("写入主机密钥失败：{e}"))?;
    file.write_all(pem.as_bytes())
        .map_err(|e| format!("写入主机密钥失败：{e}"))?;
    Ok(key)
}

/// 主机密钥指纹（OpenSSH 形式 `SHA256:…`），供页面展示首次连接核对。
pub fn fingerprint(path: &Path) -> Result<String, String> {
    let key = load_or_generate(path)?;
    Ok(key.fingerprint(HashAlg::Sha256).to_string())
}
