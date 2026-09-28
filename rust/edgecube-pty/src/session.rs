//! PTY 会话：状态、历史、订阅者与进程句柄。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::broadcast::Broadcast;
use crate::frame::SharedSink;
use crate::run::{Input, Run};

pub const DEFAULT_HISTORY_BYTES: usize = 256 * 1024;

/// 进程所处阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Stopped,
    Preparing,
    Starting,
    Running,
    Stopping,
    Crashed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Preparing => "preparing",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Crashed => "crashed",
        }
    }

    /// 算不算「进程还活着」（含过渡态）。
    pub fn alive(self) -> bool {
        !matches!(self, Self::Stopped | Self::Crashed)
    }
}

/// 一轮运行的快照。
#[derive(Debug, Clone)]
pub struct RunInfo {
    pub phase: Phase,
    /// 进程 id；没在跑时为 `None`。
    pub pid: Option<u32>,
    /// 上一次退出的退出码。
    pub exit_code: Option<i32>,
    /// 本轮启动时刻（Unix 毫秒）。
    pub started_at_ms: Option<u64>,
    /// 本轮结束时刻。
    pub stopped_at_ms: Option<u64>,
    /// 是我们主动要求停的（用来区分「正常停止」与「异常退出」）。
    pub stop_requested: bool,
}

impl Default for RunInfo {
    fn default() -> Self {
        Self {
            phase: Phase::Stopped,
            pid: None,
            exit_code: None,
            started_at_ms: None,
            stopped_at_ms: None,
            stop_requested: false,
        }
    }
}

impl RunInfo {
    pub fn alive(&self) -> bool {
        self.phase.alive()
    }

    /// `state` 载荷的结构化形式。
    ///
    /// 故意**不带** `label` / `instanceId` / `instanceName` —— 那些是业务字段，
    /// 由 Kotlin 自己持有并在转成 Dart 事件时合并。Rust 只负责进程事实。
    ///
    /// 用 `json!` 手工拼而不是 `#[derive(Serialize)]`：为一个结构体引 `serde`
    /// 的 derive 宏不值当，而这几个字段本来就是给跨语言边界看的。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "state",
            "phase": self.phase.as_str(),
            "pid": self.pid,
            "exitCode": self.exit_code,
            "stopRequested": self.stop_requested,
            "startedAtMs": self.started_at_ms,
            "stoppedAtMs": self.stopped_at_ms,
        })
    }

    /// `state` 载荷：实时广播与回放结束时都用这一个形状。
    pub fn state_json(&self) -> String {
        self.to_json().to_string()
    }
}

/// 进程操作失败的原因。
///
/// 做成枚举而不是字符串，是为了让上层拿到**稳定错误码** —— 前端按码分支，
/// 不靠匹配 message。
#[derive(Debug)]
pub enum ProcessError {
    /// 已经在跑了。
    AlreadyRunning { pid: Option<u32> },
    /// 没在跑。
    NotRunning,
    /// 拉起失败（PTY、spawn、起线程……）。
    Spawn(String),
    /// 配置里没有可用的停止命令。
    NoStopCommand,
    /// 杀进程失败。
    Kill(String),
}

impl ProcessError {
    /// 稳定的错误码，JNI 侧原样抛给 Kotlin / Dart。
    pub fn code(&self) -> &'static str {
        match self {
            Self::AlreadyRunning { .. } => "pty_already_running",
            Self::NotRunning => "pty_not_running",
            Self::Spawn(_) => "pty_start_failed",
            Self::NoStopCommand => "pty_stop_command_missing",
            Self::Kill(_) => "pty_kill_failed",
        }
    }
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning { pid } => match pid {
                Some(pid) => write!(f, "已在运行（pid {pid}）"),
                None => write!(f, "已在运行"),
            },
            Self::NotRunning => write!(f, "没有在运行的进程"),
            Self::Spawn(detail) => write!(f, "{detail}"),
            Self::NoStopCommand => write!(f, "没有可用的停止命令，请改用强制停止"),
            Self::Kill(detail) => write!(f, "强制停止失败：{detail}"),
        }
    }
}

impl std::error::Error for ProcessError {}

/// 一个 PTY 会话。
pub struct Session {
    /// 仅用于线程命名与日志定位。
    pub label: String,
    pub(crate) broadcast: Mutex<Broadcast>,
    /// 当前这一轮；`None` = 没在跑。
    pub(crate) run: Mutex<Option<Run>>,
    /// 最近一次运行的信息（`run` 为 `None` 时就靠它）。
    pub(crate) last: Mutex<RunInfo>,
    next_sub: AtomicU64,
    /// 本轮启动以来的自动重启次数。
    pub(crate) auto_restarts: AtomicU32,
}

impl Session {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            broadcast: Mutex::new(Broadcast::new(DEFAULT_HISTORY_BYTES)),
            run: Mutex::new(None),
            last: Mutex::new(RunInfo::default()),
            next_sub: AtomicU64::new(1),
            auto_restarts: AtomicU32::new(0),
        }
    }

    /// 手动起一轮进程时清零自动重启计数。
    pub(crate) fn reset_auto_restarts(&self) {
        self.auto_restarts.store(0, Ordering::Relaxed);
    }

    /// 当前状态：在跑看这一轮的，没在跑看上一次的。
    pub fn info(&self) -> RunInfo {
        let running = self
            .run
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|r| r.info.clone()));
        match running {
            Some(info) => info.lock().map(|i| i.clone()).unwrap_or_default(),
            None => self.last.lock().map(|i| i.clone()).unwrap_or_default(),
        }
    }

    pub(crate) fn input_sender(&self) -> Option<std::sync::mpsc::Sender<Input>> {
        self.run.lock().ok()?.as_ref().map(|r| r.input.clone())
    }

    /// 往 PTY 里写原始字节（用户敲的键、停止命令……）。
    pub fn write(&self, data: Vec<u8>) -> Result<(), ProcessError> {
        let tx = self.input_sender().ok_or(ProcessError::NotRunning)?;
        // std 的无界通道，send 不阻塞
        tx.send(Input::Data(data))
            .map_err(|_| ProcessError::NotRunning)
    }

    /// 调整 PTY 尺寸（异步交给输入线程执行，故**永不阻塞调用方** —— 修缺陷 A6）。
    ///
    /// `pixel_width = cols * cell_w`，与 `ecpty.c:74/218` 同算法。
    pub fn resize(
        &self,
        cols: u16,
        rows: u16,
        cell_w: u16,
        cell_h: u16,
    ) -> Result<(), ProcessError> {
        let tx = self.input_sender().ok_or(ProcessError::NotRunning)?;
        tx.send(Input::Resize(cols, rows, cell_w, cell_h))
            .map_err(|_| ProcessError::NotRunning)
    }

    /// 开关 PTY 回显（命令行编辑模式关、原始终端模式开）。
    ///
    /// 直接作用在 master fd 上，与 `ecpty.c:249` 同一条路径；master 的生命周期由
    /// `Run` 里的 `Arc` 保证 —— 只要这一轮还在，fd 就不会被关。
    pub fn set_echo(&self, on: bool) -> Result<(), ProcessError> {
        let guard = self.run.lock().map_err(|_| ProcessError::NotRunning)?;
        let run = guard.as_ref().ok_or(ProcessError::NotRunning)?;
        let master = run.master.lock().map_err(|_| ProcessError::NotRunning)?;
        let fd = master
            .as_raw_fd()
            .ok_or_else(|| ProcessError::Spawn("master 未暴露 raw fd".into()))?;
        crate::pty::set_echo(fd, on).map_err(|e| ProcessError::Spawn(format!("设置回显失败：{e}")))
    }

    /// 订阅输出。
    pub fn subscribe(&self, sink: SharedSink, with_history: bool) -> u64 {
        let id = self.next_sub.fetch_add(1, Ordering::Relaxed);
        let mut state = self.broadcast.lock().unwrap_or_else(|e| e.into_inner());
        state.subscribe(id, sink, with_history);
        id
    }

    /// 退订。PTY 继续跑 —— 断开连接不等于停进程。
    pub fn unsubscribe(&self, sub_id: u64) {
        if let Ok(mut state) = self.broadcast.lock() {
            state.unsubscribe(sub_id);
        }
    }

    /// 清掉输出历史（界面的「清屏」）。
    pub fn clear_history(&self) {
        if let Ok(mut state) = self.broadcast.lock() {
            state.clear_history();
        }
    }

    /// 往控制台里写一条提示。
    pub fn notice(&self, text: &str) {
        let mut bytes = Vec::with_capacity(text.len() + 4);
        bytes.extend_from_slice(b"\r\n");
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(b"\r\n");
        if let Ok(mut state) = self.broadcast.lock() {
            state.append(bytes);
        }
    }

    /// 广播一条状态载荷并记住它（回放结束时原样带回）。
    ///
    /// pub 是给集成测试用的；生产路径只从 `run::finish` / `run::transition` 调。
    pub fn publish_state(&self, info: &RunInfo) {
        let json = info.state_json();
        if let Ok(mut state) = self.broadcast.lock() {
            state.control(json, true);
        }
    }

    /// 广播一条瞬时控制消息（不记忆）。
    ///
    /// 同上，pub 供集成测试用。
    pub fn publish_transient(&self, json: String) {
        if let Ok(mut state) = self.broadcast.lock() {
            state.control(json, false);
        }
    }

    pub fn subscriber_count(&self) -> usize {
        self.broadcast
            .lock()
            .map(|s| s.subscriber_count())
            .unwrap_or(0)
    }

    /// 当前历史字节数（测试/自省用）。
    pub fn history_len(&self) -> usize {
        self.broadcast.lock().map(|s| s.history_len()).unwrap_or(0)
    }

    /// 当前历史快照（测试用）。
    pub fn history_snapshot(&self) -> Vec<u8> {
        self.broadcast
            .lock()
            .map(|s| s.snapshot())
            .unwrap_or_default()
    }

    /// 当前状态载荷（测试用）。
    pub fn state_json(&self) -> String {
        self.broadcast
            .lock()
            .map(|s| s.state_json().to_string())
            .unwrap_or_else(|_| "{}".into())
    }
}
