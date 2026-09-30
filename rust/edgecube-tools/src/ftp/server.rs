//! FTP 服务生命周期：单实例 start / stop / is_running。

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use libunftp::ServerBuilder;
use libunftp::options::{ActivePassiveMode, Shutdown, SiteMd5};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use unftp_sbe_fs::{Filesystem, Meta};
use unftp_sbe_restrict::RestrictingVfs;

use super::auth::{FtpAuth, FtpUser, FtpUserProvider};
use crate::rt;

/// 控制连接空闲超时（秒），对齐 Apache FTPServer Listener 默认 idleTimeout=300。
const IDLE_SESSION_TIMEOUT_SECS: u64 = 300;

/// 运行中的服务句柄：关停信号 + 监听任务。
struct Handle {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

static STATE: Mutex<Option<Handle>> = Mutex::new(None);

fn lock_state() -> std::sync::MutexGuard<'static, Option<Handle>> {
    // 单测 panic 污染锁后继续用（服务状态自洽，见 stop 的幂等语义）。
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 启动 FTP 服务。
///
/// * `username` 为空 → 匿名模式，`password` 忽略；
/// * `root_dir` 不存在时自动创建（对齐原 `root.mkdirs()`）；
/// * 已运行时返回 `FTP 服务已在运行`；
/// * 端口被占用等绑定错误在此同步抛出（对齐 Apache `start()` 即时抛异常，
///   libunftp 的 `listen()` 在异步任务里绑定，失败无法回传到本调用，
///   故先同步预检绑定）。
pub fn start(
    root_dir: &str,
    port: i32,
    username: &str,
    password: &str,
    writable: bool,
    ipv6_enabled: bool,
) -> Result<(), String> {
    if !(0..=65535).contains(&port) {
        return Err(format!("端口超出范围：{port}"));
    }
    let port = port as u16;

    let mut state = lock_state();
    if let Some(handle) = state.as_ref() {
        if !handle.task.is_finished() {
            return Err("FTP 服务已在运行".to_string());
        }
        // 上一轮异常退出（如绑定竞态）的残留句柄，清理后允许重启。
        *state = None;
    }

    let root = PathBuf::from(root_dir);
    if let Err(e) = fs::create_dir_all(&root) {
        return Err(format!("无法创建根目录：{e}"));
    }

    let addr = if ipv6_enabled {
        format!("[::]:{port}")
    } else {
        format!("0.0.0.0:{port}")
    };

    drop(std::net::TcpListener::bind(&addr).map_err(|e| format!("无法绑定端口 {port}：{e}"))?);

    // 停止信号：watch 通道（Sender/Receiver 均 Sync，满足 shutdown_indicator
    // 的 Future + Send + Sync 约束）；stop() 置位或通道关闭都触发关停。
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let storage_root = root.clone();
    let builder = ServerBuilder::<_, FtpUser>::with_user_detail_provider(
        Box::new(move || {
            RestrictingVfs::<Filesystem, FtpUser, Meta>::new(
                Filesystem::new(storage_root.clone())
                    .expect("edgecube: 打开 FTP 根目录失败（启动时已创建，中途被删除？）"),
            )
        }),
        Arc::new(FtpUserProvider { writable }),
    )
    .authenticator(Arc::new(FtpAuth {
        username: username.to_string(),
        password: password.to_string(),
    }))
    .idle_session_timeout(IDLE_SESSION_TIMEOUT_SECS)
    .active_passive_mode(ActivePassiveMode::ActiveAndPassive)
    .sitemd5(SiteMd5::None)
    .shutdown_indicator(async move {
        let mut rx = shutdown_rx;
        let _ = rx.wait_for(|stop| *stop).await;
        Shutdown::new().grace_period(Duration::ZERO)
    });

    let server = builder
        .build()
        .map_err(|e| format!("FTP 服务构建失败：{e}"))?;

    let listen_addr = addr.clone();
    let task = rt::shared().spawn(async move {
        let _ = server.listen(listen_addr).await;
    });

    *state = Some(Handle {
        shutdown: shutdown_tx,
        task,
    });
    Ok(())
}

/// 停止 FTP 服务（幂等，永不抛异常；对齐原 `stop()`）。
///
/// 通知会话关停并等待监听任务退出（释放端口，便于立即重启），上限 5s；
/// 调用方须是非运行时线程。
pub fn stop() {
    let handle = lock_state().take();
    if let Some(handle) = handle {
        let _ = handle.shutdown.send(true);
        let _ = rt::shared()
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), handle.task).await });
    }
}

/// 服务是否在监听（对齐原 `isRunning`；`stop()` 返回后立即为 false）。
pub fn is_running() -> bool {
    lock_state()
        .as_ref()
        .is_some_and(|handle| !handle.task.is_finished())
}
