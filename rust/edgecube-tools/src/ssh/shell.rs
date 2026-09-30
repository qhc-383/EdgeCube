//! shell 通道 → `edgecube-pty` 的桥：订阅、输入直通、输出泵送、窗口尺寸、退出码。

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use edgecube_pty::frame::{FrameSink, OutFrame};
use edgecube_pty::lifecycle;
use edgecube_pty::run::SpawnSpec;
use edgecube_pty::session::{Phase, Session};
use russh::ChannelId;
use russh::server::Handle as SshServerHandle;
use tokio::sync::Notify;
use tokio::time::Instant;

/// 单通道输出队列深度；满时丢最旧（对齐原 `ArrayBlockingQueue(16)`）。
const QUEUE_CAP: usize = 16;

/// 进程已退出后等待 `output_eof` / 尾部输出的窗口（一次性，不随数据重置）。
const EXIT_GRACE: Duration = Duration::from_millis(500);

const CELL_W: u16 = 8;
const CELL_H: u16 = 16;
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;

/// 一个 shell 通道需要的进程规格（Kotlin `SshServerManager.start` 组好传入）。
pub(crate) struct ShellTemplate {
    /// 完整 argv（`[cmd, arg…]`，如 `["/system/bin/sh", "-i"]`）。
    pub argv: Vec<String>,
    /// 初始 cwd（启动时已选好并校验过存在的目录）。
    pub cwd: String,
    /// 基础环境（Kotlin `ShellResolver.baseEnv`，含 TERM/PATH/HOME 等）。
    pub env: Vec<(String, String)>,
}

/// 泵与 sink 共享的控制状态（终局帧解析结果）。
#[derive(Clone, Copy)]
struct Ctrl {
    /// PTY 输出到头（`output_eof` 控制帧）。
    eof: bool,
    /// 进程已退出（`exit` / 终局 `state` 帧，先到者置位）。
    exited: bool,
    /// 退出码；未知（被信号击杀且无码）按 255。
    code: i32,
}

impl Default for Ctrl {
    fn default() -> Self {
        Self {
            eof: false,
            exited: false,
            code: 255,
        }
    }
}

/// 输出队列 + 控制状态；实现 [`FrameSink`]（必须非阻塞，跑在持
/// broadcast 锁的输出线程上），由泵任务异步消费。
struct Shared {
    queue: Mutex<VecDeque<Vec<u8>>>,
    q_notify: Notify,
    ctrl: Mutex<Ctrl>,
    ctrl_notify: Notify,
}

impl Shared {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            q_notify: Notify::new(),
            ctrl: Mutex::new(Ctrl::default()),
            ctrl_notify: Notify::new(),
        }
    }

    fn snapshot(&self) -> Ctrl {
        *self.ctrl.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 只认三类帧：`output_eof` / `exit` / 终局 `state`（对齐原
    /// `handleControl`）；全部只写锁内字段 + 通知，无 I/O。
    fn on_control(&self, json: &str) {
        if json.contains("\"type\":\"output_eof\"") {
            self.ctrl.lock().unwrap_or_else(PoisonError::into_inner).eof = true;
            self.q_notify.notify_one();
            self.ctrl_notify.notify_one();
            return;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
            return;
        };
        let terminal = match v.get("type").and_then(serde_json::Value::as_str) {
            Some("exit") => v
                .get("code")
                .and_then(serde_json::Value::as_i64)
                .map(|c| c as i32),
            Some("state") => {
                let phase = v.get("phase").and_then(serde_json::Value::as_str);
                if matches!(phase, Some("stopped" | "crashed")) {
                    v.get("exitCode")
                        .and_then(serde_json::Value::as_i64)
                        .map(|c| c as i32)
                } else {
                    None
                }
            }
            _ => None,
        };
        // 上面的 match 对非终局帧返回 None —— 但 exit 帧本身也可能是
        // `code: null`。用 type 区分「非终局（忽略）」与「终局但无码（255）」。
        let is_terminal = match v.get("type").and_then(serde_json::Value::as_str) {
            Some("exit") => true,
            Some("state") => matches!(
                v.get("phase").and_then(serde_json::Value::as_str),
                Some("stopped" | "crashed")
            ),
            _ => false,
        };
        if !is_terminal {
            return;
        }
        let mut ctrl = self.ctrl.lock().unwrap_or_else(PoisonError::into_inner);
        if !ctrl.exited {
            ctrl.exited = true;
            ctrl.code = terminal.unwrap_or(255);
        }
        drop(ctrl);
        // 泵可能停在队列等待上，两类通知都发。
        self.q_notify.notify_one();
        self.ctrl_notify.notify_one();
    }
}

impl FrameSink for Shared {
    fn send(&self, frame: OutFrame) {
        match frame {
            OutFrame::Data(bytes) => {
                {
                    let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
                    if q.len() >= QUEUE_CAP {
                        q.pop_front();
                    }
                    q.push_back(bytes);
                }
                self.q_notify.notify_one();
            }
            OutFrame::Control(json) => self.on_control(&json),
            // withHistory=false：回放帧不会出现；真出现也不该当实时数据打给客户端。
            OutFrame::ReplayBegin | OutFrame::ReplayEnd(_) => {}
        }
    }
}

/// 一个已拉起的 shell：PTY 会话 + 订阅句柄 + 共享状态。
///
/// 由会话槽位持有；槽位移除（`channel_close` / 会话结束）即 Drop →
/// 整组 SIGKILL + 退订，保证断开连接不会留下孤儿 PTY。
pub(crate) struct ShellHandle {
    pty: Arc<Session>,
    sub_id: u64,
    _shared: Arc<Shared>,
}

impl ShellHandle {
    /// 客户端按键 → PTY stdin；进程已死则静默丢弃（对齐原 `PtyOutputStream`）。
    pub(crate) fn write(&self, data: &[u8]) {
        let _ = self.pty.write(data.to_vec());
    }

    /// SSH `window-change` → PTY 尺寸（→ `SIGWINCH` 重排）；0 值忽略。
    pub(crate) fn resize(&self, cols: u32, rows: u32) {
        if cols == 0 || rows == 0 {
            return;
        }
        let c = cols.min(u16::MAX as u32) as u16;
        let r = rows.min(u16::MAX as u32) as u16;
        let _ = self.pty.resize(c, r, CELL_W, CELL_H);
    }
}

impl Drop for ShellHandle {
    fn drop(&mut self) {
        // 对齐原 destroy()：先整组 SIGKILL（收掉本通道自己的 shell 及其
        // 子孙），再退订；Session 随 Arc 释放。
        let _ = lifecycle::kill(&self.pty);
        self.pty.unsubscribe(self.sub_id);
    }
}

/// 启动一个 shell 通道的 PTY。
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    template: &ShellTemplate,
    channel: ChannelId,
    term: &str,
    rows: u32,
    cols: u32,
    ssh: SshServerHandle,
) -> Result<ShellHandle, String> {
    let argv0 = template
        .argv
        .first()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "缺少 shell 命令".to_string())?;
    if !is_executable(Path::new(argv0)) {
        return Err(format!(
            "EdgeCube: shell binary not found or not executable: {argv0}"
        ));
    }

    let mut cwd = template.cwd.clone();
    if !Path::new(&cwd).is_dir() {
        // 启动时 Kotlin 已选好存在的目录；跑一半被删的边角情况退到根目录。
        cwd = '/'.to_string();
    }

    // 模板环境 + 客户端 TERM 覆盖（env-request 已在 server 层择优）。
    let mut env = template.env.clone();
    if !term.is_empty() {
        if let Some(slot) = env.iter_mut().find(|(k, _)| k == "TERM") {
            slot.1 = term.to_string();
        } else {
            env.push(("TERM".to_string(), term.to_string()));
        }
    }
    let envp = env.into_iter().map(|(k, v)| format!("{k}={v}")).collect();

    let spec = SpawnSpec {
        label: "ssh".into(),
        argv: template.argv.clone(),
        cwd,
        envp,
        rows: clamp_dim(rows, DEFAULT_ROWS),
        cols: clamp_dim(cols, DEFAULT_COLS),
        cell_w: CELL_W,
        cell_h: CELL_H,
        // 对齐原 initialPhase = "running"：shell 起来就是运行中，无 starting。
        initial_phase: Phase::Running,
        // 对齐原 start 未传 autoRestart（默认 false）：进程退出即收尾，
        // 不在 SSH 通道里复活 shell。
        auto_restart: false,
    };

    let pty = Arc::new(Session::new("ssh"));
    let shared = Arc::new(Shared::new());
    // 先订阅（withHistory=false，见模块注释），泵随后、start 最后 ——
    // 任何输出都晚于订阅，首行提示符不丢。
    let sub_id = pty.subscribe(shared.clone(), false);
    if sub_id == 0 {
        return Err("订阅 PTY 输出失败".to_string());
    }
    let pump_shared = shared.clone();
    let pump_task = tokio::spawn(pump(channel, ssh, pump_shared));
    if let Err(e) = lifecycle::start(&pty, spec) {
        pump_task.abort();
        pty.unsubscribe(sub_id);
        return Err(format!("创建 PTY 子进程失败: {e}"));
    }

    Ok(ShellHandle {
        pty,
        sub_id,
        _shared: shared,
    })
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn clamp_dim(v: u32, fallback: u16) -> u16 {
    if v == 0 {
        fallback
    } else {
        v.min(u16::MAX as u32) as u16
    }
}

/// 输出泵：把队列里的 PTY 输出送进 SSH 通道，直到进程退出且输出到头，
/// 随后按序发 `exit-status` → EOF → close（之后由 `channel_close` 收尸）。
async fn pump(channel: ChannelId, ssh: SshServerHandle, shared: Arc<Shared>) {
    // 收尾窗口的截止点：首次观察到「已退出且队列空」时固定，不随后续数据顺延。
    let mut exit_deadline: Option<Instant> = None;
    loop {
        let chunk = {
            let mut q = shared.queue.lock().unwrap_or_else(PoisonError::into_inner);
            q.pop_front()
        };
        match chunk {
            Some(bytes) => {
                if ssh.data(channel, bytes).await.is_err() {
                    return; // 会话已断；PTY 收尾交由槽位 Drop
                }
            }
            None => {
                let ctrl = shared.snapshot();
                if ctrl.exited && ctrl.eof {
                    break;
                }
                if ctrl.exited {
                    let deadline =
                        *exit_deadline.get_or_insert_with(|| Instant::now() + EXIT_GRACE);
                    match tokio::time::timeout_at(deadline, shared.q_notify.notified()).await {
                        // output_eof / 尾部数据到了 → 回到循环继续排空。
                        Ok(()) => continue,
                        // 窗口耗尽（典型：后台进程占着 PTY 不放）→ 强制收尾。
                        Err(_) => break,
                    }
                }
                shared.q_notify.notified().await;
            }
        }
    }

    let ctrl = shared.snapshot();
    let code = if ctrl.exited && (0..=0x7FFF_FFFF).contains(&ctrl.code) {
        ctrl.code as u32
    } else {
        255
    };
    let _ = ssh.exit_status_request(channel, code).await;
    let _ = ssh.eof(channel).await;
    let _ = ssh.close(channel).await;
}
