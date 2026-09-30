//! M2 FTP 模块集成测试：生命周期、认证、只读、根目录 jail、curl 传输往返。
//!
//! 依赖宿主机 `curl`（计划指定的验证手段）；服务是进程级单例，
//! 各用例用互斥锁串行执行，进入前先 `stop()` 清残留。

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use edgecube_tools::ftp;

// ─── 测试基础设施 ───

static LOCK: Mutex<()> = Mutex::new(());
static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("edgecube-m2-{}-{}-{}", tag, std::process::id(), n));
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn str(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 取一个当前空闲的端口（先绑后放，服务启动时同步预检绑定）。
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 等待服务可连接（start() 返回时监听任务可能尚未完成绑定）。
fn wait_ready(port: u16) {
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok()
            || TcpStream::connect(("::1", port)).is_ok()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("FTP 服务未在端口 {port} 就绪");
}

/// 跑 curl；`--noproxy *` 排除环境代理变量干扰。
fn curl(args: &[&str]) -> (bool, String) {
    let mut full = vec!["--noproxy", "*"];
    full.extend_from_slice(args);
    let out = Command::new("curl")
        .args(&full)
        .output()
        .expect("宿主机需安装 curl");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn curl_ok(args: &[&str]) -> String {
    let (ok, text) = curl(args);
    assert!(ok, "curl 应成功，args={args:?}\n{text}");
    text
}

fn write_file(path: &Path, content: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap()
}

fn start_anon(root: &TempDir, port: u16, writable: bool) {
    ftp::start(root.str(), port as i32, "", "", writable, false).unwrap();
    wait_ready(port);
}

// ─── 生命周期 ───

#[test]
fn lifecycle_start_stop_restart() {
    let _g = lock();
    ftp::stop();
    let root = TempDir::new("lifecycle");
    let port = free_port();

    assert!(!ftp::is_running());
    start_anon(&root, port, true);
    assert!(ftp::is_running());

    // 已运行时再 start → 消息对齐原 IllegalStateException
    let err = ftp::start(root.str(), port as i32, "", "", true, false).unwrap_err();
    assert_eq!(err, "FTP 服务已在运行");

    ftp::stop();
    assert!(!ftp::is_running());
    ftp::stop(); // 幂等

    // stop 后端口立即可重启（Controller 的配置变更 restart 依赖此语义）
    start_anon(&root, port, true);
    assert!(ftp::is_running());
    ftp::stop();
    assert!(!ftp::is_running());
}

#[test]
fn bind_conflict_reports_error() {
    let _g = lock();
    ftp::stop();
    let blocker = TcpListener::bind("0.0.0.0:0").unwrap();
    let port = blocker.local_addr().unwrap().port();
    let root = TempDir::new("bindconflict");

    let err = ftp::start(root.str(), port as i32, "", "", true, false).unwrap_err();
    assert!(err.contains(&format!("无法绑定端口 {port}")), "err = {err}");
    drop(blocker);
}

// ─── 匿名可写：curl 传输往返（PASV/EPSV、REST、MKD、jail） ───

#[test]
fn anonymous_writable_roundtrip_via_curl() {
    let _g = lock();
    ftp::stop();
    let root = TempDir::new("anon");
    let port = free_port();

    // jail 目标：根目录之外的诱饵文件（.. 不得逃出）
    let secret_name = format!("secret-{}.txt", std::process::id());
    let secret_path = root.path().parent().unwrap().join(&secret_name);
    write_file(&secret_path, b"TOP-SECRET-MUST-NOT-ESCAPE");

    start_anon(&root, port, true);
    let base = format!("ftp://127.0.0.1:{port}");
    let auth = "anonymous:probe@example.com";
    let upload: Vec<u8> = (0u8..=255).cycle().take(64 * 1024).collect();
    let src = std::env::temp_dir().join(format!("edgecube-m2-upload-{}.bin", std::process::id()));
    write_file(&src, &upload);

    // STOR（上传）
    curl_ok(&[
        "--user",
        auth,
        "-T",
        src.to_str().unwrap(),
        &format!("{base}/marker.txt"),
    ]);
    assert_eq!(fs::read(root.path().join("marker.txt")).unwrap(), upload);

    // LIST
    let out = curl_ok(&["--user", auth, "-s", "--list-only", &format!("{base}/")]);
    assert!(out.contains("marker.txt"), "LIST 输出: {out}");

    // RETR（下载）
    let dst = std::env::temp_dir().join(format!("edgecube-m2-download-{}.bin", std::process::id()));
    curl_ok(&[
        "--user",
        auth,
        "-o",
        dst.to_str().unwrap(),
        &format!("{base}/marker.txt"),
    ]);
    assert_eq!(fs::read(&dst).unwrap(), upload);

    // MKD + 子目录内上传
    curl_ok(&[
        "--user",
        auth,
        "--quote",
        "MKD sub",
        "-s",
        "--list-only",
        &format!("{base}/"),
    ]);
    assert!(root.path().join("sub").is_dir());
    curl_ok(&[
        "--user",
        auth,
        "-T",
        "/dev/null",
        &format!("{base}/sub/inner.txt"),
    ]);
    assert!(root.path().join("sub/inner.txt").exists());

    // REST 断点续传：完整下载 → 截断一半 → curl -C - 补齐 → 内容一致
    curl_ok(&[
        "--user",
        auth,
        "-T",
        src.to_str().unwrap(),
        &format!("{base}/big.bin"),
    ]);
    let resume = dst.with_extension("resume");
    curl_ok(&[
        "--user",
        auth,
        "-o",
        resume.to_str().unwrap(),
        &format!("{base}/big.bin"),
    ]);
    let full_len = fs::metadata(&resume).unwrap().len();
    fs::File::options()
        .write(true)
        .open(&resume)
        .unwrap()
        .set_len(full_len / 2)
        .unwrap();
    curl_ok(&[
        "--user",
        auth,
        "-C",
        "-",
        "-o",
        resume.to_str().unwrap(),
        &format!("{base}/big.bin"),
    ]);
    assert_eq!(fs::read(&resume).unwrap(), upload);

    // 根目录 jail：`..` 不得逃出（RETR 越界必须失败且拿不到内容）
    let (ok, text) = curl(&[
        "--user",
        auth,
        "--quote",
        &format!("RETR ../{secret_name}"),
        "-s",
        &base,
    ]);
    assert!(!ok, "越界 RETR 应失败：{text}");
    let secret_on_wire = String::from_utf8_lossy(&fs::read(&secret_path).unwrap()).to_string();
    assert!(
        !text.contains(&secret_on_wire),
        "越界读取泄漏了根目录外内容"
    );

    // CWD .. 后列表仍只应看到根内条目
    let out = curl(&[
        "--user",
        auth,
        "--quote",
        "CWD ..",
        "-s",
        "--list-only",
        &format!("{base}/"),
    ]);
    let text = out.1;
    assert!(!text.contains(&secret_name), "CWD .. 逃出根目录：{text}");

    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
    let _ = fs::remove_file(&resume);
    let _ = fs::remove_file(&secret_path);
    ftp::stop();
}

// ─── 具名认证 ───

#[test]
fn named_user_auth_rejects_bad_credentials() {
    let _g = lock();
    ftp::stop();
    let root = TempDir::new("named");
    let port = free_port();
    write_file(&root.path().join("hello.txt"), b"hi");

    ftp::start(root.str(), port as i32, "alice", "s3cret", true, false).unwrap();
    wait_ready(port);
    let base = format!("ftp://127.0.0.1:{port}");

    // 正确凭据
    curl_ok(&[
        "--user",
        "alice:s3cret",
        "-s",
        "--list-only",
        &format!("{base}/"),
    ]);
    // 密码错误
    let (ok, text) = curl(&[
        "--user",
        "alice:wrong",
        "-s",
        "--list-only",
        &format!("{base}/"),
    ]);
    assert!(!ok, "错误密码应被拒绝：{text}");
    // 匿名登录被拒（具名模式下 anonymous 不是注册用户）
    let (ok, text) = curl(&[
        "--user",
        "anonymous:x@example.com",
        "-s",
        "--list-only",
        &format!("{base}/"),
    ]);
    assert!(!ok, "匿名登录应被拒绝：{text}");

    ftp::stop();
}

// ─── 只读模式 ───

#[test]
fn readonly_denies_all_writes() {
    let _g = lock();
    ftp::stop();
    let root = TempDir::new("readonly");
    let port = free_port();
    write_file(&root.path().join("ro.txt"), b"read-only-content");

    start_anon(&root, port, false);
    let base = format!("ftp://127.0.0.1:{port}");
    let auth = "anonymous:probe@example.com";

    // 读操作可用
    let dst = std::env::temp_dir().join(format!("edgecube-m2-ro-dst-{}.txt", std::process::id()));
    curl_ok(&[
        "--user",
        auth,
        "-o",
        dst.to_str().unwrap(),
        &format!("{base}/ro.txt"),
    ]);
    assert_eq!(fs::read(&dst).unwrap(), b"read-only-content");
    let _ = fs::remove_file(&dst);

    // STOR 被拒
    let src = std::env::temp_dir().join(format!("edgecube-m2-ro-src-{}.txt", std::process::id()));
    write_file(&src, b"nope");
    let (ok, _) = curl(&[
        "--user",
        auth,
        "-T",
        src.to_str().unwrap(),
        &format!("{base}/upload.txt"),
    ]);
    assert!(!ok, "只读模式 STOR 应失败");
    assert!(!root.path().join("upload.txt").exists());
    let _ = fs::remove_file(&src);

    // MKD 被拒（不看 curl 退出码，看副作用）
    let _ = curl(&[
        "--user",
        auth,
        "--quote",
        "MKD denied",
        "-s",
        &format!("{base}/"),
    ]);
    assert!(!root.path().join("denied").exists());

    // DELE 被拒
    let _ = curl(&[
        "--user",
        auth,
        "--quote",
        "DELE ro.txt",
        "-s",
        &format!("{base}/"),
    ]);
    assert!(
        root.path().join("ro.txt").exists(),
        "只读模式 DELE 不应生效"
    );

    // RNFR/RNTO 被拒
    let _ = curl(&[
        "--user",
        auth,
        "--quote",
        "RNFR ro.txt",
        "--quote",
        "RNTO moved.txt",
        "-s",
        &format!("{base}/"),
    ]);
    assert!(
        root.path().join("ro.txt").exists(),
        "只读模式重命名不应生效"
    );
    assert!(!root.path().join("moved.txt").exists());

    ftp::stop();
}

// ─── 双栈（ipv6Enabled=true） ───

#[test]
fn dual_stack_ipv4_and_ipv6_roundtrip() {
    let _g = lock();
    ftp::stop();
    let root = TempDir::new("dual");
    let port = free_port();

    ftp::start(root.str(), port as i32, "", "", true, true).unwrap();
    wait_ready(port);
    let auth = "anonymous:probe@example.com";
    let payload = b"dual-stack-payload";

    // v4 客户端（127.0.0.1 → 双栈监听的 v4-mapped 连接）
    let src4 = std::env::temp_dir().join(format!("edgecube-m2-v4-{}.txt", std::process::id()));
    write_file(&src4, payload);
    curl_ok(&[
        "--user",
        auth,
        "-T",
        src4.to_str().unwrap(),
        &format!("ftp://127.0.0.1:{port}/v4.txt"),
    ]);
    let dst4 = src4.with_extension("v4dst");
    curl_ok(&[
        "--user",
        auth,
        "-o",
        dst4.to_str().unwrap(),
        &format!("ftp://127.0.0.1:{port}/v4.txt"),
    ]);
    assert_eq!(fs::read(&dst4).unwrap(), payload);

    // v6 客户端（EPSV 是 v6 下唯一被动模式）
    let dst6 = src4.with_extension("v6dst");
    curl_ok(&[
        "--user",
        auth,
        "-T",
        src4.to_str().unwrap(),
        &format!("ftp://[::1]:{port}/v6.txt"),
    ]);
    curl_ok(&[
        "--user",
        auth,
        "-o",
        dst6.to_str().unwrap(),
        &format!("ftp://[::1]:{port}/v6.txt"),
    ]);
    assert_eq!(fs::read(&dst6).unwrap(), payload);

    assert!(root.path().join("v4.txt").exists());
    assert!(root.path().join("v6.txt").exists());

    let _ = fs::remove_file(&src4);
    let _ = fs::remove_file(&dst4);
    let _ = fs::remove_file(&dst6);
    ftp::stop();
}

// ─── 被动模式行为锁定（双栈限制，见 mod.rs 已知偏差） ───

/// 单条控制连接上执行一条命令，返回回复文本（空串 = 连接被断开）。
fn ftp_command(port: u16, cmd: &str) -> String {
    use std::io::{Read, Write};
    fn read_reply(c: &mut TcpStream) -> String {
        let mut b = [0u8; 2048];
        let n = c.read(&mut b).unwrap_or(0);
        String::from_utf8_lossy(&b[..n]).trim_end().to_string()
    }
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    read_reply(&mut c); // greeting
    c.write_all(b"USER anonymous\r\n").unwrap();
    read_reply(&mut c);
    c.write_all(b"PASS probe@example.com\r\n").unwrap();
    read_reply(&mut c);
    c.write_all(format!("{cmd}\r\n").as_bytes()).unwrap();
    read_reply(&mut c)
}

/// 被动模式行为：
/// - 纯 v4 监听（默认）：PASV 与 EPSV 均可用（对齐 Apache）；
/// - 双栈监听：EPSV 正常；PASV 被 libunftp 断开（上游对 V6 本地地址
///   不回 227/500 而是掉线，见 `ftp/mod.rs` 已知偏差——v4 客户端在
///   ipv6Enabled=true 下需支持 EPSV，curl/现代客户端默认即 EPSV）。
#[test]
fn passive_mode_behavior_pasv_epsv() {
    let _g = lock();

    // 纯 v4：PASV 227 + EPSV 229
    ftp::stop();
    let root = TempDir::new("pasv-v4");
    let port = free_port();
    ftp::start(root.str(), port as i32, "", "", true, false).unwrap();
    wait_ready(port);
    assert!(
        ftp_command(port, "PASV").starts_with("227 "),
        "v4 PASV: {}",
        ftp_command(port, "PASV")
    );
    assert!(
        ftp_command(port, "EPSV").starts_with("229 "),
        "v4 EPSV: {}",
        ftp_command(port, "EPSV")
    );
    ftp::stop();

    // 双栈：EPSV 可用，PASV 连接被断开（空回复）
    ftp::stop();
    let root = TempDir::new("pasv-dual");
    let port = free_port();
    ftp::start(root.str(), port as i32, "", "", true, true).unwrap();
    wait_ready(port);
    assert!(
        ftp_command(port, "EPSV").starts_with("229 "),
        "dual EPSV: {}",
        ftp_command(port, "EPSV")
    );
    assert_eq!(
        ftp_command(port, "PASV"),
        "",
        "dual PASV 应断开（libunftp 已知限制）"
    );
    ftp::stop();
}
