//! M3 SSH 模块集成测试：生命周期/启动校验、认证（密码 + 公钥拒绝）、
//! SFTP 往返 + jail + 只读、shell（TERM 优先级 / resize / 退出码）、
//! exec 拒绝、主机密钥指纹与线上密钥一致性。
//!
//! 客户端用 russh 直连；服务是进程级单例，各用例互斥锁串行、进入前
//! `stop()` 清残留。普通 `#[test]` + `rt::shared().block_on`（`stop()`
//! 内部也会 block_on，嵌进 tokio 运行时线程会 panic）。每个用例的异步
//! 主体统一 30s 超时，避免挂死。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use edgecube_tools::rt;
use edgecube_tools::ssh;
use russh::client::{self, Handle as ClientHandle};
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKeyOrCertificate, key::safe_rng};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{OpenFlags, StatusCode};

// ─── 测试基础设施 ───

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

const USER: &str = "alice";
const PASS: &str = "s3cret";

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("edgecube-m3-{}-{}-{}", tag, std::process::id(), n));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn str(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 取一个当前空闲的端口（先绑后放；启动时同步预检绑定兜底竞态）。
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_ready(port: u16) {
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("SSH 服务未在端口 {port} 就绪");
}

/// 在共享运行时上跑异步主体，统一 30s 超时（防止用例挂死）。
fn block<T>(fut: impl Future<Output = T>) -> T {
    rt::shared().block_on(async {
        match tokio::time::timeout(Duration::from_secs(30), fut).await {
            Ok(v) => v,
            Err(_) => panic!("SSH 测试超时（30s）"),
        }
    })
}

#[derive(Clone, Copy)]
struct Opts {
    writable: bool,
    sftp: bool,
    shell: bool,
    username: &'static str,
    password: &'static str,
    shell_argv: &'static [&'static str],
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            writable: true,
            sftp: true,
            shell: true,
            username: USER,
            password: PASS,
            shell_argv: &["/bin/sh", "-i"],
        }
    }
}

/// 组装与 Kotlin `SshServerManager.start` 同形的 JSON 载荷。
fn config_json(root: &TempDir, keydir: &TempDir, port: u16, opts: &Opts) -> String {
    serde_json::json!({
        "rootDir": root.str(),
        "port": port,
        "username": opts.username,
        "password": opts.password,
        "writable": opts.writable,
        "sftpEnabled": opts.sftp,
        "shellEnabled": opts.shell,
        "ipv6Enabled": false,
        "hostKeyPath": keydir.path().join("hostkey").to_str().unwrap(),
        "shellArgv": opts.shell_argv,
        "shellCwd": root.str(),
        "env": {
            "PATH": "/usr/local/bin:/usr/bin:/bin",
            "HOME": root.str(),
            "TERM": "xterm-256color",
        },
    })
    .to_string()
}

fn start(root: &TempDir, keydir: &TempDir, port: u16, opts: &Opts) -> Result<(), String> {
    let result = ssh::start(&config_json(root, keydir, port, opts));
    if result.is_ok() {
        wait_ready(port);
    }
    result
}

fn start_ok(root: &TempDir, keydir: &TempDir, port: u16, opts: &Opts) {
    start(root, keydir, port, opts).unwrap();
}

// ─── russh 客户端 ───

struct TestClient {
    /// 若指定，校验服务端公钥指纹须与之一致（主机密钥测试用）。
    expect_fingerprint: Option<String>,
}

impl client::Handler for TestClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fp = server_key
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        match &self.expect_fingerprint {
            Some(want) => Ok(&fp == want),
            None => Ok(true),
        }
    }
}

async fn connect(port: u16, expect_fingerprint: Option<String>) -> ClientHandle<TestClient> {
    let config = Arc::new(client::Config::default());
    client::connect(
        config,
        ("127.0.0.1", port),
        TestClient { expect_fingerprint },
    )
    .await
    .expect("SSH 连接失败")
}

async fn authed(port: u16) -> ClientHandle<TestClient> {
    let mut h = connect(port, None).await;
    let result = h
        .authenticate_password(USER, PASS)
        .await
        .expect("认证请求失败");
    assert!(result.success(), "正确凭据应通过认证");
    h
}

/// 等通道请求回 Success；收到 Failure / 连接断开即 panic。
async fn expect_ok(ch: &mut russh::Channel<client::Msg>) {
    loop {
        match ch.wait().await {
            Some(russh::ChannelMsg::Success) => return,
            Some(russh::ChannelMsg::Failure) => panic!("通道请求被服务端拒绝"),
            Some(_) => {}
            None => panic!("等待通道回复时连接关闭"),
        }
    }
}

/// 等通道请求回 Failure（exec 拒绝测试用）。
async fn expect_failure(ch: &mut russh::Channel<client::Msg>) {
    loop {
        match ch.wait().await {
            Some(russh::ChannelMsg::Failure) => return,
            Some(russh::ChannelMsg::Success) => panic!("exec 应被拒绝，却收到了 Success"),
            Some(_) => {}
            None => panic!("等待通道回复时连接关闭"),
        }
    }
}

/// 收集通道输出直到出现 `needle`；通道提前关闭则带已收内容 panic。
async fn recv_until(ch: &mut russh::Channel<client::Msg>, needle: &str) -> String {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match ch.wait().await {
            Some(russh::ChannelMsg::Data { data }) => {
                buf.extend_from_slice(&data);
                let text = String::from_utf8_lossy(&buf).into_owned();
                if text.contains(needle) {
                    return text;
                }
            }
            Some(russh::ChannelMsg::Close) | None => {
                panic!("通道提前关闭，已收：\n{}", String::from_utf8_lossy(&buf));
            }
            _ => {}
        }
    }
}

/// 打开一个已认证连接上的 SFTP 会话（Handle 须由调用方保活）。
async fn open_sftp(h: &mut ClientHandle<TestClient>) -> SftpSession {
    let mut ch = h.channel_open_session().await.unwrap();
    ch.request_subsystem(true, "sftp").await.unwrap();
    expect_ok(&mut ch).await;
    SftpSession::new(ch.into_stream()).await.unwrap()
}

/// 新建文件并写满（SFTPv3：新文件须 CREATE 打开，裸 WRITE 会 NoSuchFile）。
async fn sftp_create(s: &SftpSession, path: &str, data: &[u8]) {
    use tokio::io::AsyncWriteExt;
    let mut f = s
        .open_with_flags(
            path,
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .unwrap();
    f.write_all(data).await.unwrap();
    f.close().await.unwrap();
}

fn assert_status(err: SftpError, want: StatusCode, ctx: &str) {
    match err {
        SftpError::Status(status) => assert_eq!(
            status.status_code, want,
            "{ctx}：状态不符（消息：{}）",
            status.error_message
        ),
        other => panic!("{ctx}：期望 Status({want:?})，得到 {other:?}"),
    }
}

// ─── 生命周期 / 启动校验 ───

#[test]
fn lifecycle_start_stop_restart() {
    let _g = lock();
    ssh::stop();
    let root = TempDir::new("lifecycle");
    let keydir = TempDir::new("lifecycle-key");
    let port = free_port();

    assert!(!ssh::is_running());
    start_ok(&root, &keydir, port, &Opts::default());
    assert!(ssh::is_running());

    // 已运行时再 start → 消息对齐原 IllegalStateException
    let err = start(&root, &keydir, port, &Opts::default()).unwrap_err();
    assert_eq!(err, "SSH 服务已在运行");

    ssh::stop();
    assert!(!ssh::is_running());
    ssh::stop(); // 幂等

    // stop 后端口立即可重启（页面配置变更 restart 依赖此语义）
    start_ok(&root, &keydir, port, &Opts::default());
    assert!(ssh::is_running());
    ssh::stop();
    assert!(!ssh::is_running());
}

#[test]
fn start_validation_matches_original_messages() {
    let _g = lock();
    ssh::stop();
    let root = TempDir::new("validate");
    let keydir = TempDir::new("validate-key");
    let port = free_port();
    let base = |extra: serde_json::Value| {
        let mut v = serde_json::json!({
            "rootDir": root.str(),
            "port": port,
            "username": USER,
            "password": PASS,
            "writable": true,
            "sftpEnabled": true,
            "shellEnabled": true,
            "ipv6Enabled": false,
            "hostKeyPath": keydir.path().join("hostkey").to_str().unwrap(),
            "shellArgv": ["/bin/sh", "-i"],
            "shellCwd": root.str(),
            "env": {"TERM": "xterm-256color"},
        });
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v.to_string()
    };

    // SFTP 与 shell 均关闭
    let err = ssh::start(&base(serde_json::json!({
        "sftpEnabled": false, "shellEnabled": false
    })))
    .unwrap_err();
    assert_eq!(err, "SFTP 与 SSH 终端至少需启用其一");

    // 空用户名 / 空密码
    let err = ssh::start(&base(serde_json::json!({"username": "  "}))).unwrap_err();
    assert_eq!(err, "SSH 服务要求设置用户名与密码");
    let err = ssh::start(&base(serde_json::json!({"password": ""}))).unwrap_err();
    assert_eq!(err, "SSH 服务要求设置用户名与密码");

    // 端口越界
    let err = ssh::start(&base(serde_json::json!({"port": 70_000}))).unwrap_err();
    assert_eq!(err, "端口超出范围：70000");

    // 启用 shell 却没有命令
    let err = ssh::start(&base(serde_json::json!({"shellArgv": []}))).unwrap_err();
    assert_eq!(err, "缺少 shell 命令");

    // 必填字段缺失
    let mut v: serde_json::Value = serde_json::from_str(&base(serde_json::json!({}))).unwrap();
    v.as_object_mut().unwrap().remove("rootDir");
    let err = ssh::start(&v.to_string()).unwrap_err();
    assert_eq!(err, "缺少 rootDir");

    // 载荷不是合法 JSON
    let err = ssh::start("{nope").unwrap_err();
    assert!(err.starts_with("参数解析失败"), "err = {err}");

    assert!(!ssh::is_running());
}

// ─── 认证 ───

#[test]
fn password_auth_rejects_bad_credentials() {
    let _g = lock();
    ssh::stop();
    let root = TempDir::new("auth");
    let keydir = TempDir::new("auth-key");
    let port = free_port();
    start_ok(&root, &keydir, port, &Opts::default());

    block(async {
        // 密码错误 → 拒绝（服务器回调精确比对，客户端拿到 Failure）
        let mut h = connect(port, None).await;
        let result = h
            .authenticate_password(USER, "wrong")
            .await
            .expect("认证请求失败");
        assert!(!result.success(), "错误密码应被拒绝");

        // 用户名不存在 → 同样拒绝
        let mut h = connect(port, None).await;
        let result = h
            .authenticate_password("nobody", PASS)
            .await
            .expect("认证请求失败");
        assert!(!result.success(), "未知用户应被拒绝");

        // 正确凭据 → 通过
        let mut h = connect(port, None).await;
        let result = h
            .authenticate_password(USER, PASS)
            .await
            .expect("认证请求失败");
        assert!(result.success());
    });

    ssh::stop();
}

#[test]
fn publickey_auth_is_rejected() {
    let _g = lock();
    ssh::stop();
    let root = TempDir::new("pubkey");
    let keydir = TempDir::new("pubkey-key");
    let port = free_port();
    start_ok(&root, &keydir, port, &Opts::default());

    block(async {
        // 对齐原 PasswordAuthenticator-only：russh 默认 auth_publickey 拒绝，
        // 即便用户名密码都对，公钥方式也不可登录。
        let mut h = connect(port, None).await;
        let key = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
        let with_hash = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), None);
        let result = h
            .authenticate_publickey(USER, with_hash)
            .await
            .expect("公钥认证请求失败");
        assert!(!result.success(), "公钥认证应被拒绝");
    });

    ssh::stop();
}

// ─── 主机密钥 ───

#[test]
fn host_key_fingerprint_matches_wire_key() {
    let _g = lock();
    ssh::stop();
    let root = TempDir::new("fp");
    let keydir = TempDir::new("fp-key");
    let port = free_port();
    let key_path = keydir.path().join("hostkey");

    // 查询即生成（对齐原 hostKeyFingerprint 的惰性生成）
    let fp1 = ssh::host_key_fingerprint(&key_path).unwrap();
    assert!(fp1.starts_with("SHA256:"), "fp = {fp1}");
    let fp2 = ssh::host_key_fingerprint(&key_path).unwrap();
    assert_eq!(fp1, fp2, "同一密钥文件的指纹应稳定");

    start_ok(&root, &keydir, port, &Opts::default());

    block(async {
        // 线上握手用的公钥必须与磁盘指纹一致（check_server_key 精确比对）
        let _h = connect(port, Some(fp1.clone())).await;
        // 不一致的指纹必须导致连接被客户端拒绝
        let bogus = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let result = client::connect(
            Arc::new(client::Config::default()),
            ("127.0.0.1", port),
            TestClient {
                expect_fingerprint: Some(bogus.to_string()),
            },
        )
        .await;
        assert!(result.is_err(), "指纹不符应导致密钥校验失败");
    });

    assert_eq!(ssh::host_key_fingerprint(&key_path).unwrap(), fp1);
    ssh::stop();
}

// ─── SFTP 往返 ───

#[test]
fn sftp_roundtrip_write_read_list_rename_delete() {
    let _g = lock();
    ssh::stop();
    let root = TempDir::new("sftp");
    let keydir = TempDir::new("sftp-key");
    let port = free_port();
    start_ok(&root, &keydir, port, &Opts::default());

    block(async {
        let mut h = authed(port).await;
        let sftp = open_sftp(&mut h).await;

        // 新建 + 写入 → 落盘
        let payload: Vec<u8> = (0u8..=255).cycle().take(70 * 1024).collect();
        sftp_create(&sftp, "/hello.txt", &payload).await;
        assert_eq!(
            std::fs::read(root.path().join("hello.txt")).unwrap(),
            payload,
            "SFTP 写入应与磁盘一致（含 64KiB 分片读路径）"
        );

        // 读回 → 字节一致
        let back = sftp.read("/hello.txt").await.unwrap();
        assert_eq!(back, payload);

        // 建目录 + 列目录
        sftp.create_dir("/sub").await.unwrap();
        let mut names: Vec<String> = Vec::new();
        let rd = sftp.read_dir("/").await.unwrap();
        for entry in rd {
            names.push(entry.file_name());
        }
        assert!(
            names.contains(&"hello.txt".to_string()),
            "names = {names:?}"
        );
        assert!(names.contains(&"sub".to_string()), "names = {names:?}");

        // 重命名（跨目录）
        sftp.rename("/hello.txt", "/sub/moved.bin").await.unwrap();
        assert!(!root.path().join("hello.txt").exists());
        assert_eq!(
            std::fs::read(root.path().join("sub/moved.bin")).unwrap(),
            payload
        );

        // 属性查询
        let md = sftp.metadata("/sub/moved.bin").await.unwrap();
        assert_eq!(md.size, Some(payload.len() as u64));

        // 删除
        sftp.remove_file("/sub/moved.bin").await.unwrap();
        assert!(!root.path().join("sub/moved.bin").exists());
        sftp.remove_dir("/sub").await.unwrap();
        assert!(!root.path().join("sub").exists());
    });

    ssh::stop();
}

// ─── SFTP jail：`..` 词法夹回根、符号链接逃逸被拒 ───

#[test]
fn sftp_jail_blocks_dotdot_and_symlink_escape() {
    let _g = lock();
    ssh::stop();
    let root = TempDir::new("jail");
    let keydir = TempDir::new("jail-key");
    let port = free_port();
    let secret_name = format!("secret-{}.txt", std::process::id());
    let secret_path = root.path().parent().unwrap().join(&secret_name);
    std::fs::write(&secret_path, b"TOP-SECRET-MUST-NOT-ESCAPE").unwrap();
    start_ok(&root, &keydir, port, &Opts::default());

    block(async {
        let mut h = authed(port).await;
        let sftp = open_sftp(&mut h).await;

        // `..` 被词法夹回根内 → 指向根内不存在的文件
        let err = sftp.read(format!("/../{secret_name}")).await.unwrap_err();
        assert_status(err, StatusCode::NoSuchFile, "`/../secret` 应被夹回根内");

        // 相对符号链接指向根外：stat（跟随）被拒
        sftp.symlink("/esc-rel", format!("../{secret_name}"))
            .await
            .unwrap();
        let err = sftp.metadata("/esc-rel").await.unwrap_err();
        assert_status(err, StatusCode::PermissionDenied, "相对链接逃逸 stat");

        // 绝对符号链接指向根外：同样被拒
        sftp.symlink("/esc-abs", secret_path.to_str().unwrap())
            .await
            .unwrap();
        let err = sftp.metadata("/esc-abs").await.unwrap_err();
        assert_status(err, StatusCode::PermissionDenied, "绝对链接逃逸 stat");

        // 读逃逸链接 → 拒绝（拿不到内容）
        let err = sftp.read("/esc-rel").await.unwrap_err();
        assert_status(err, StatusCode::PermissionDenied, "相对链接逃逸 read");

        // lstat / readlink 对链接本身可用（它就在 jail 内）
        let md = sftp.symlink_metadata("/esc-rel").await.unwrap();
        assert!(md.file_type().is_symlink(), "lstat 应看到链接本身");
        let target = sftp.read_link("/esc-rel").await.unwrap();
        assert_eq!(target, format!("../{secret_name}"));

        // realpath 词法绝对化（不跟随链接）
        let canonical = sftp.canonicalize("/../anything/./deep").await.unwrap();
        assert_eq!(canonical, "/anything/deep");
    });

    std::fs::remove_file(&secret_path).ok();
    ssh::stop();
}

// ─── 只读模式 ───

#[test]
fn readonly_denies_sftp_writes_but_not_shell() {
    let _g = lock();
    ssh::stop();
    let root = TempDir::new("readonly");
    let keydir = TempDir::new("readonly-key");
    let port = free_port();
    std::fs::write(root.path().join("ro.txt"), b"read-only-content").unwrap();
    let opts = Opts {
        writable: false,
        ..Opts::default()
    };
    start_ok(&root, &keydir, port, &opts);

    block(async {
        let mut h = authed(port).await;
        let sftp = open_sftp(&mut h).await;

        // 读可用
        assert_eq!(sftp.read("/ro.txt").await.unwrap(), b"read-only-content");

        // 写类操作逐一被拒，且无副作用
        let err = sftp_create_err(&sftp, "/upload.txt", b"nope").await;
        assert_status(err, StatusCode::PermissionDenied, "只读 open(CREATE|WRITE)");
        assert!(!root.path().join("upload.txt").exists());

        let err = sftp.write("/ro.txt", b"clobber").await.unwrap_err();
        assert_status(err, StatusCode::PermissionDenied, "只读 open(WRITE)");
        assert_eq!(
            std::fs::read(root.path().join("ro.txt")).unwrap(),
            b"read-only-content"
        );

        let err = sftp.create_dir("/denied").await.unwrap_err();
        assert_status(err, StatusCode::PermissionDenied, "只读 mkdir");
        assert!(!root.path().join("denied").exists());

        let err = sftp.remove_file("/ro.txt").await.unwrap_err();
        assert_status(err, StatusCode::PermissionDenied, "只读 remove");
        assert!(root.path().join("ro.txt").exists());

        let err = sftp.rename("/ro.txt", "/moved.txt").await.unwrap_err();
        assert_status(err, StatusCode::PermissionDenied, "只读 rename");
        assert!(root.path().join("ro.txt").exists());

        let err = sftp.symlink("/ln", "/ro.txt").await.unwrap_err();
        assert_status(err, StatusCode::PermissionDenied, "只读 symlink");

        // 只读仅作用于 SFTP：shell 仍可写（对齐原 ReadOnlySftpEventListener 边界）
        let mut sh = h.channel_open_session().await.unwrap();
        sh.request_pty(true, "linux", 80, 24, 0, 0, &[])
            .await
            .unwrap();
        sh.request_shell(true).await.unwrap();
        expect_ok(&mut sh).await;
        let marker = format!("touch {}/shell-wrote.txt\n", root.str());
        sh.data_bytes(marker).await.unwrap();
        recv_until(&mut sh, "shell-wrote.txt").await;
        let mut waited = 0;
        while !root.path().join("shell-wrote.txt").exists() && waited < 100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waited += 1;
        }
        assert!(
            root.path().join("shell-wrote.txt").exists(),
            "只读模式不应限制 SSH 终端"
        );
    });

    ssh::stop();
}

/// `SftpSession::write` 的新建文件版（失败时返回错误而非 panic）。
async fn sftp_create_err(s: &SftpSession, path: &str, data: &[u8]) -> SftpError {
    use tokio::io::AsyncWriteExt;
    let mut f = match s
        .open_with_flags(
            path,
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
    {
        Ok(f) => f,
        Err(e) => return e,
    };
    if let Err(e) = f.write_all(data).await {
        return SftpError::UnexpectedBehavior(e.to_string());
    }
    match f.close().await {
        Ok(()) => panic!("只读模式下 open 应在第一步就被拒"),
        Err(e) => SftpError::UnexpectedBehavior(e.to_string()),
    }
}

// ─── shell 终端 ───

#[test]
fn shell_echo_term_priority_and_resize() {
    let _g = lock();
    ssh::stop();
    let root = TempDir::new("shell");
    let keydir = TempDir::new("shell-key");
    let port = free_port();
    start_ok(&root, &keydir, port, &Opts::default());

    block(async {
        let h = authed(port).await;

        // 通道 1：只给 pty-request 的 TERM → 应生效（优先于模板默认）
        let mut ch = h.channel_open_session().await.unwrap();
        ch.request_pty(true, "linux", 80, 24, 0, 0, &[])
            .await
            .unwrap();
        expect_ok(&mut ch).await;
        ch.request_shell(true).await.unwrap();
        expect_ok(&mut ch).await;
        ch.data_bytes("echo T_$TERM\n").await.unwrap();
        let out = recv_until(&mut ch, "T_linux").await;
        assert!(out.contains("T_linux"), "pty-request TERM 应生效：{out}");

        // 窗口 resize → SIGWINCH → stty size 反映新尺寸（行=40 列=120）
        ch.window_change(120, 40, 0, 0).await.unwrap();
        ch.data_bytes("stty size\n").await.unwrap();
        let out = recv_until(&mut ch, "40 120").await;
        assert!(out.contains("40 120"), "resize 后尺寸应为 40x120：{out}");

        // 普通输出往返
        ch.data_bytes("echo MARK_SHELL_OK\n").await.unwrap();
        let out = recv_until(&mut ch, "MARK_SHELL_OK").await;
        assert!(out.contains("MARK_SHELL_OK"), "{out}");

        // 通道 2：env-request 的 TERM 优先于 pty-request
        let mut ch2 = h.channel_open_session().await.unwrap();
        ch2.request_pty(true, "linux", 80, 24, 0, 0, &[])
            .await
            .unwrap();
        expect_ok(&mut ch2).await;
        ch2.set_env(true, "TERM", "env-wins").await.unwrap();
        expect_ok(&mut ch2).await;
        ch2.request_shell(true).await.unwrap();
        expect_ok(&mut ch2).await;
        ch2.data_bytes("echo T_$TERM\n").await.unwrap();
        let out = recv_until(&mut ch2, "T_env-wins").await;
        assert!(out.contains("T_env-wins"), "env-request TERM 应优先：{out}");
        // 关掉通道 2，避免影响退出码断言
        ch2.close().await.unwrap();
    });

    ssh::stop();
}

#[test]
fn shell_exit_reports_exit_status_then_closes() {
    let _g = lock();
    ssh::stop();
    let root = TempDir::new("exit");
    let keydir = TempDir::new("exit-key");
    let port = free_port();
    start_ok(&root, &keydir, port, &Opts::default());

    block(async {
        let h = authed(port).await;
        let mut ch = h.channel_open_session().await.unwrap();
        ch.request_pty(true, "linux", 80, 24, 0, 0, &[])
            .await
            .unwrap();
        expect_ok(&mut ch).await;
        ch.request_shell(true).await.unwrap();
        expect_ok(&mut ch).await;
        ch.data_bytes("echo READY\n").await.unwrap();
        recv_until(&mut ch, "READY").await;

        // 退出码 7：exit-status → EOF → close（对齐原 PumpTask 收尾顺序）
        ch.data_bytes("exit 7\n").await.unwrap();
        let mut status = None;
        loop {
            match ch.wait().await {
                Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                    status = Some(exit_status);
                }
                Some(russh::ChannelMsg::Close) | None => break,
                _ => {}
            }
        }
        assert_eq!(status, Some(7), "exit-status 应为 7");
    });

    ssh::stop();
}

// ─── exec 拒绝 ───

#[test]
fn exec_request_is_rejected() {
    let _g = lock();
    ssh::stop();
    let root = TempDir::new("exec");
    let keydir = TempDir::new("exec-key");
    let port = free_port();
    start_ok(&root, &keydir, port, &Opts::default());

    block(async {
        let h = authed(port).await;
        let mut ch = h.channel_open_session().await.unwrap();
        // 对齐原 commandFactory = null：exec 一律 channel_failure
        ch.exec(true, b"echo should-not-run").await.unwrap();
        expect_failure(&mut ch).await;
        ch.close().await.unwrap();
    });

    ssh::stop();
}
