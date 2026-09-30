//! SSH 服务生命周期：JSON 参数解析 + 单实例 start / stop / is_running。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use russh::server::{
    self, Auth, ChannelOpenHandle, Config as RusshConfig, Msg, RunningServerHandle, Server as _,
    Session,
};
use russh::{Channel, ChannelId};
use serde_json::Value;

use super::{SshConfig, keys, sftp, shell};
use crate::rt;

pub(crate) struct StartArgs {
    pub root: String,
    pub port: i64,
    pub username: String,
    pub password: String,
    pub writable: bool,
    pub sftp_enabled: bool,
    pub shell_enabled: bool,
    pub ipv6_enabled: bool,
    pub host_key_path: String,
    pub shell_argv: Vec<String>,
    pub shell_cwd: String,
    pub env: Vec<(String, String)>,
}

fn parse_args(json: &str) -> Result<StartArgs, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("参数解析失败：{e}"))?;
    let text = |key: &str| -> Result<String, String> {
        v.get(key)
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| format!("缺少 {key}"))
    };
    let flag = |key: &str| v.get(key).and_then(Value::as_bool).unwrap_or(false);
    let port = v.get("port").and_then(Value::as_i64).ok_or("缺少 port")?;
    let shell_argv = v
        .get("shellArgv")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let env = v
        .get("env")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Ok(StartArgs {
        root: text("rootDir")?,
        port,
        username: text("username")?,
        password: text("password")?,
        writable: flag("writable"),
        sftp_enabled: flag("sftpEnabled"),
        shell_enabled: flag("shellEnabled"),
        ipv6_enabled: flag("ipv6Enabled"),
        host_key_path: text("hostKeyPath")?,
        shell_argv,
        shell_cwd: text("shellCwd")?,
        env,
    })
}

/// 解析 `start` 的 JSON 载荷并启动 SSH 服务。
pub fn start(config_json: &str) -> Result<(), String> {
    let args = parse_args(config_json)?;
    server_start(&args)
}

/// 运行中的服务句柄：关停广播 + accept 循环任务。
struct Handle {
    shutdown: RunningServerHandle,
    task: tokio::task::JoinHandle<()>,
}

static STATE: Mutex<Option<Handle>> = Mutex::new(None);

fn lock_state() -> std::sync::MutexGuard<'static, Option<Handle>> {
    // 单测 panic 污染锁后继续用（服务状态自洽，见 stop 的幂等语义）。
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn server_start(args: &StartArgs) -> Result<(), String> {
    let mut state = lock_state();
    if let Some(handle) = state.as_ref() {
        if !handle.task.is_finished() {
            return Err("SSH 服务已在运行".to_string());
        }
        // 上一轮异常退出的残留句柄，清理后允许重启。
        *state = None;
    }

    // 校验顺序对齐原 `SshServerManager.start`（require 在配置之前）。
    if !(args.sftp_enabled || args.shell_enabled) {
        return Err("SFTP 与 SSH 终端至少需启用其一".to_string());
    }
    if args.username.trim().is_empty() || args.password.trim().is_empty() {
        return Err("SSH 服务要求设置用户名与密码".to_string());
    }
    if !(0..=65535).contains(&args.port) {
        return Err(format!("端口超出范围：{}", args.port));
    }
    let port = args.port as u16;
    if args.shell_enabled && args.shell_argv.iter().all(|s| s.is_empty()) {
        return Err("缺少 shell 命令".to_string());
    }

    let root = std::path::PathBuf::from(&args.root);
    if let Err(e) = std::fs::create_dir_all(&root) {
        return Err(format!("无法创建根目录：{e}"));
    }

    // 换新 ed25519 主机密钥：存在则读取（OpenSSH PEM），否则生成并落盘。
    let host_key = keys::load_or_generate(Path::new(&args.host_key_path))?;

    // ipv6Enabled 为真绑 `[::]:port`（Android 内核 bindv6only=0 → 双栈），
    // 否则 `0.0.0.0:port` —— 与原实现及 FTP 监听器一致。
    let addr = if args.ipv6_enabled {
        format!("[::]:{port}")
    } else {
        format!("0.0.0.0:{port}")
    };
    let std_listener =
        std::net::TcpListener::bind(&addr).map_err(|e| format!("无法绑定端口 {port}：{e}"))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| format!("无法绑定端口 {port}：{e}"))?;
    // from_std 要求已进入 reactor（JNI 调用线程与测试线程均无运行时上下文）。
    let listener = {
        let _guard = rt::shared().enter();
        tokio::net::TcpListener::from_std(std_listener)
            .map_err(|e| format!("无法绑定端口 {port}：{e}"))?
    };

    let raw_cfg = RusshConfig {
        keys: vec![host_key],
        // 对齐原 IDLE_TIMEOUT/NIO2_READ_TIMEOUT/AUTH_TIMEOUT 全 0：
        // 不做空闲超时，避免长时间不操作的终端被服务端掐断。
        inactivity_timeout: None,
        // 常量时间的认证拒绝；首个 none 探测立即返回（客户端探测所必需）。
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..RusshConfig::default()
    };

    let cfg = Arc::new(SshConfig {
        root,
        username: args.username.clone(),
        password: args.password.clone(),
        writable: args.writable,
        sftp_enabled: args.sftp_enabled,
        shell_enabled: args.shell_enabled,
        shell: shell::ShellTemplate {
            argv: args.shell_argv.clone(),
            cwd: args.shell_cwd.clone(),
            env: args.env.clone(),
        },
    });

    // accept 循环跑在共享运行时上；handle 经同步通道在任务首次轮询时送出，
    // 保证 `stop()` 拿到的一定与本轮服务对应。
    let (tx, rx) = std::sync::mpsc::sync_channel::<RunningServerHandle>(1);
    let task = rt::shared().spawn(async move {
        let mut sshd = Sshd { cfg };
        let running = sshd.run_on_socket(Arc::new(raw_cfg), &listener);
        let _ = tx.send(running.handle());
        let _ = running.await;
    });
    let shutdown = rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "SSH 服务启动超时".to_string())?;

    *state = Some(Handle { shutdown, task });
    Ok(())
}

/// 停止 SSH 服务（幂等，永不抛异常；对齐原 `stop()`）。
///
/// 通知 accept 循环与所有连接关停并等待任务退出（释放端口），上限 5s；
/// 调用方须是非运行时线程。
pub fn stop() {
    let handle = lock_state().take();
    if let Some(handle) = handle {
        handle.shutdown.shutdown("SSH 服务已停止".into());
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

// ─── russh 服务与会话处理 ───

/// 每次 `new_client` 一份；持有共享配置。
struct Sshd {
    cfg: Arc<SshConfig>,
}

impl server::Server for Sshd {
    type Handler = SshSession;

    fn new_client(&mut self, _peer_addr: Option<SocketAddr>) -> Self::Handler {
        SshSession {
            cfg: self.cfg.clone(),
            slots: HashMap::new(),
        }
    }

    fn handle_session_error(&mut self, error: <Self::Handler as server::Handler>::Error) {
        eprintln!("EdgeCube ssh: 会话异常结束：{error}");
    }
}

/// pty-req 的参数；shell 启动时取用（TERM 见 `env` 优先级）。
struct PtyReq {
    term: String,
    cols: u32,
    rows: u32,
}

enum ChannelKind {
    /// 已开通道但尚未 shell/subsystem；持有 Channel 供 sftp 取用。
    Pending,
    Shell(shell::ShellHandle),
    /// Channel 已移交 `into_stream`，输入输出由 sftp 任务消费。
    Sftp,
}

struct ChannelSlot {
    /// 仅 Pending 时持有；shell 启动即丢（见 `shell_request` 注释）。
    channel: Option<Channel<Msg>>,
    kind: ChannelKind,
    pty: Option<PtyReq>,
    env: HashMap<String, String>,
}

struct SshSession {
    cfg: Arc<SshConfig>,
    slots: HashMap<ChannelId, ChannelSlot>,
}

impl server::Handler for SshSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        // 强制用户名 + 密码精确比对（不允许匿名）；russh 自带常量时间拒绝。
        Ok(
            if user == self.cfg.username && password == self.cfg.password {
                Auth::Accept
            } else {
                Auth::reject()
            },
        )
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.slots.insert(
            channel.id(),
            ChannelSlot {
                channel: Some(channel),
                kind: ChannelKind::Pending,
                pty: None,
                env: HashMap::new(),
            },
        );
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.slots.get_mut(&channel) {
            Some(slot) => {
                slot.pty = Some(PtyReq {
                    term: term.to_string(),
                    cols: col_width,
                    rows: row_height,
                });
                let _ = session.channel_success(channel);
            }
            None => {
                let _ = session.channel_failure(channel);
            }
        }
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        variable_name: &str,
        variable_value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.slots.get_mut(&channel) {
            Some(slot) => {
                // 对齐原实现：shell 只消费 TERM（其余 env-request 忽略）；
                // env-request 的 TERM 优先于 pty-req 的 TERM。
                if variable_name == "TERM" && !variable_value.is_empty() {
                    slot.env
                        .insert("TERM".to_string(), variable_value.to_string());
                }
                let _ = session.channel_success(channel);
            }
            None => {
                let _ = session.channel_failure(channel);
            }
        }
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.cfg.shell_enabled {
            let _ = session.channel_failure(channel);
            return Ok(());
        }
        let Some(slot) = self.slots.get_mut(&channel) else {
            let _ = session.channel_failure(channel);
            return Ok(());
        };
        if !matches!(slot.kind, ChannelKind::Pending) {
            let _ = session.channel_failure(channel);
            return Ok(());
        }

        // TERM 优先级：env-request > pty-req > 模板默认（baseEnv 的
        // xterm-256color）；尺寸取 pty-req（0/缺省 → 24x80）。
        let (term, rows, cols) = {
            let pty = slot.pty.as_ref();
            let term = slot
                .env
                .get("TERM")
                .cloned()
                .or_else(|| pty.and_then(|p| (!p.term.is_empty()).then(|| p.term.clone())))
                .unwrap_or_default();
            let rows = pty.map(|p| p.rows).filter(|&r| r > 0).unwrap_or(24);
            let cols = pty.map(|p| p.cols).filter(|&c| c > 0).unwrap_or(80);
            (term, rows, cols)
        };

        let handle = session.handle();
        match shell::spawn(&self.cfg.shell, channel, &term, rows, cols, handle) {
            Ok(shell) => {
                // 丢弃 Channel（读半部的 receiver）：shell 输入由 `data`
                // 回调直通，无人消费 receiver 会让会话循环在约 100 条
                // 数据包后阻塞在 mpsc 上。SFTP 通道则必须保留（消费在
                // `into_stream` 里），故只在 shell 路径丢弃。
                slot.kind = ChannelKind::Shell(shell);
                slot.channel = None;
                let _ = session.channel_success(channel);
            }
            Err(e) => {
                eprintln!("EdgeCube ssh: shell 启动失败：{e}");
                let _ = session.channel_failure(channel);
            }
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // 对齐原 `commandFactory = null`：拒绝一切 exec（无错误文本）。
        let _ = session.channel_failure(channel);
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" || !self.cfg.sftp_enabled {
            let _ = session.channel_failure(channel);
            return Ok(());
        }
        let Some(slot) = self.slots.get_mut(&channel) else {
            let _ = session.channel_failure(channel);
            return Ok(());
        };
        if !matches!(slot.kind, ChannelKind::Pending) {
            let _ = session.channel_failure(channel);
            return Ok(());
        }
        let Some(stream_channel) = slot.channel.take() else {
            let _ = session.channel_failure(channel);
            return Ok(());
        };
        let fs = match sftp::Fs::new(&self.cfg.root, self.cfg.writable) {
            Ok(fs) => fs,
            Err(e) => {
                eprintln!("EdgeCube ssh: SFTP 根目录不可用：{e}");
                let _ = session.channel_failure(channel);
                return Ok(());
            }
        };
        slot.kind = ChannelKind::Sftp;
        let _ = session.channel_success(channel);
        // `run` 内部自建 tokio 任务，此调用立即返回：sftp 与 shell 可在
        // 同一连接并存，会话循环不被文件 I/O 阻塞。
        russh_sftp::server::run(stream_channel.into_stream(), fs).await;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // 客户端按键 → PTY stdin；SFTP 数据由 `into_stream` 消费（本回调
        // 对 SFTP 通道仅重复投递到已取出的 receiver，无副作用）。
        if let Some(slot) = self.slots.get(&channel)
            && let ChannelKind::Shell(shell) = &slot.kind
        {
            shell.write(data);
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(slot) = self.slots.get(&channel)
            && let ChannelKind::Shell(shell) = &slot.kind
        {
            shell.resize(col_width, row_height);
        }
        let _ = session.channel_success(channel);
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // 客户端收尾：关通道 → 对端回 CHANNEL_CLOSE → `channel_close`
        // 移除槽位 → ShellHandle::drop 退订并 SIGKILL 本通道 PTY。
        let _ = session.close(channel);
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // 槽位移除触发 ChannelKind::Shell 的 Drop（杀 PTY + 退订）；
        // Pending/Sftp 通道随槽位丢弃即可（sftp 任务随 stream EOF 自行退出）。
        self.slots.remove(&channel);
        Ok(())
    }
}
