//! 一轮进程的拉起，以及输入 / 输出 / 等待三条线程。

use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::session::{Phase, ProcessError, RunInfo, Session};

/// 自动重启前的等待，避免「秒退 → 秒起」把 CPU 打满。
const AUTO_RESTART_DELAY: Duration = Duration::from_secs(1);

/// 同一轮启动里自动重启的次数上限。
///
/// 没有上限的话，一条「跑完就退」的命令会变成死循环，用户只能看着 CPU 烧起来。
const MAX_AUTO_RESTARTS: u32 = 10;

/// 起一轮进程需要的全部输入。
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// 线程名与日志定位用（shell 用 label，服务端用 instanceId）。
    pub label: String,
    pub argv: Vec<String>,
    /// 父进程的 cwd（proot 模式下是 host 侧目录）。
    pub cwd: String,
    /// `KEY=VALUE` 形式，等价 `ecpty.c` 的 `clearenv()` + `putenv()` 循环。
    pub envp: Vec<String>,
    pub rows: u16,
    pub cols: u16,
    pub cell_w: u16,
    pub cell_h: u16,
    /// 拉起瞬间的阶段：shell 直接 `running`，服务端 `starting`。
    pub initial_phase: Phase,
    /// 进程正常退出后是否自动重启。
    pub auto_restart: bool,
}

impl Default for SpawnSpec {
    fn default() -> Self {
        Self {
            label: String::new(),
            argv: Vec::new(),
            cwd: String::new(),
            envp: Vec::new(),
            rows: 24,
            cols: 80,
            cell_w: 8,
            cell_h: 16,
            initial_phase: Phase::Running,
            auto_restart: false,
        }
    }
}

/// 送给输入线程的指令。
pub(crate) enum Input {
    /// 原样写进 PTY（用户敲的键、停止命令……）。
    Data(Vec<u8>),
    /// 调整 PTY 尺寸（`cols`, `rows`, 单元像素宽/高）。
    Resize(u16, u16, u16, u16),
}

/// 当前这一轮运行持有的句柄。
pub(crate) struct Run {
    pub input: Sender<Input>,
    pub killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// 与输出 / 等待线程共享。
    pub info: Arc<Mutex<RunInfo>>,
    /// 供 `set_echo` 取 raw fd；同时保证 fd 活得过这一轮。
    pub master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
}

/// 拉起一轮进程。
pub(crate) fn spawn_run(session: &Arc<Session>, spec: SpawnSpec) -> Result<RunInfo, ProcessError> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| ProcessError::Spawn(format!("创建 PTY 失败: {e}")))?;

    let cmd = build_command(&spec);
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| ProcessError::Spawn(format!("拉起进程失败: {e}")))?;
    drop(pair.slave);

    let pid = child.process_id();
    let killer = child.clone_killer();
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| ProcessError::Spawn(format!("取 PTY writer 失败: {e}")))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| ProcessError::Spawn(format!("取 PTY reader 失败: {e}")))?;
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));

    let info = Arc::new(Mutex::new(RunInfo {
        phase: spec.initial_phase,
        pid,
        started_at_ms: Some(now_ms()),
        ..RunInfo::default()
    }));

    let (input_tx, input_rx) = mpsc::channel::<Input>();

    // 先把这一轮挂上，再起线程：等待线程可能在任何一刻收尾，
    // 反过来的话它写的 `None` 会被我们随后写的 `Some` 覆盖掉。
    {
        let mut guard = session
            .run
            .lock()
            .map_err(|_| ProcessError::Spawn("会话锁损坏".into()))?;
        *guard = Some(Run {
            input: input_tx,
            killer: Mutex::new(killer),
            info: info.clone(),
            master: master.clone(),
        });
    }

    let spawn_thread = |name: String, f: Box<dyn FnOnce() + Send>| -> Result<(), ProcessError> {
        std::thread::Builder::new()
            .name(name)
            .spawn(f)
            .map(|_| ())
            .map_err(|e| ProcessError::Spawn(format!("起线程失败: {e}")))
    };

    let out_session = session.clone();
    let wait_session = session.clone();
    let wait_info = info.clone();

    let launched = spawn_thread(
        format!("ec-pty-in-{}", spec.label),
        Box::new(move || input_loop(writer, master, input_rx)),
    )
    .and_then(|()| {
        spawn_thread(
            format!("ec-pty-out-{}", spec.label),
            Box::new(move || output_loop(reader, out_session)),
        )
    })
    .and_then(|()| {
        spawn_thread(
            format!("ec-pty-wait-{}", spec.label),
            Box::new(move || wait_loop(child, wait_session, wait_info, spec)),
        )
    });

    if let Err(e) = launched {
        // 半死不活的会话比没有更糟：清掉，让用户能重试。
        if let Ok(mut run) = session.run.lock() {
            *run = None;
        }
        return Err(e);
    }

    let snapshot = info.lock().map(|i| i.clone()).unwrap_or_default();
    if let Ok(mut last) = session.last.lock() {
        *last = snapshot.clone();
    }
    session.publish_state(&snapshot);
    Ok(snapshot)
}

/// 拼出交给系统 shell 的命令。
fn build_command(spec: &SpawnSpec) -> CommandBuilder {
    let argv: Vec<OsString> = spec.argv.iter().map(OsString::from).collect();
    let mut cmd = CommandBuilder::from_argv(argv);
    cmd.env_clear();
    let mut has_term = false;
    for entry in &spec.envp {
        if let Some((k, v)) = entry.split_once('=') {
            if k == "TERM" {
                has_term = true;
            }
            cmd.env(k, v);
        }
    }
    if !has_term {
        cmd.env("TERM", "xterm-256color");
    }
    if !spec.cwd.is_empty() {
        cmd.cwd(&spec.cwd);
    }
    cmd
}

/// 输入线程：收字节 / resize，写进 PTY。
fn input_loop(
    mut writer: Box<dyn Write + Send>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    rx: Receiver<Input>,
) {
    while let Ok(msg) = rx.recv() {
        match msg {
            Input::Data(data) => {
                if writer.write_all(&data).is_err() || writer.flush().is_err() {
                    break;
                }
            }
            Input::Resize(cols, rows, cell_w, cell_h) => {
                if let Ok(m) = master.lock() {
                    let _ = m.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: cols.saturating_mul(cell_w),
                        pixel_height: rows.saturating_mul(cell_h),
                    });
                }
            }
        }
    }
}

/// 输出线程：阻塞读 PTY，进历史并扇给订阅者。
fn output_loop(mut reader: Box<dyn Read + Send>, session: Arc<Session>) {
    // 本线程要把帧投给 Kotlin，attach 一次、随线程结束 detach。
    // 主机上跑测试没有 JVM，退化成空守卫。
    let _jni = crate::bridge::JniThreadGuard::acquire();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let data = buf[..n].to_vec();
                if let Ok(mut state) = session.broadcast.lock() {
                    state.append(data);
                }
            }
            // PTY 主设备在子进程退出后 read 常返回 EIO，作正常结束处理。
            Err(_) => break,
        }
    }

    // 输出到头了。Kotlin 的按行组装器必须在这里冲刷最后一段没有换行的半行 ——
    // 退出状态帧由 `wait_loop` 发，那条线程不知道输出线程读完了没有，
    // 拿它当冲刷点会丢最后一行。
    session.publish_transient(r#"{"type":"output_eof"}"#.to_string());
}

/// 等待线程：等子进程退出，收尾，必要时自动重启。
fn wait_loop(
    child: Box<dyn Child + Send + Sync>,
    session: Arc<Session>,
    info: Arc<Mutex<RunInfo>>,
    spec: SpawnSpec,
) {
    // `finish` / 自动重启的 notice 都要投给 Kotlin，同样需要 attach。
    let _jni = crate::bridge::JniThreadGuard::acquire();
    let pid = child.process_id();
    let code = pid.and_then(wait_status);

    let (stop_requested, phase_before) = info
        .lock()
        .map(|i| (i.stop_requested, i.phase))
        .unwrap_or((false, Phase::Stopped));

    // 只有还停在过渡态/运行态时才由我们收尾；已经被收过（比如强制停完又手动起了
    // 一轮）就别改人家的状态了。
    if !matches!(phase_before, Phase::Stopped | Phase::Crashed) {
        // 退出码 0 或者是我们主动停的 → 已停止；否则算异常退出
        let phase = if stop_requested || code == Some(0) {
            Phase::Stopped
        } else {
            Phase::Crashed
        };
        finish(&session, &info, phase, code);
    }
    drop(child);

    // 自己正常退出（不是我们让它停的、退出码 0）且开了自动重启
    if code == Some(0) && !stop_requested && spec.auto_restart {
        let done = session.auto_restarts.fetch_add(1, Ordering::Relaxed) + 1;
        if done > MAX_AUTO_RESTARTS {
            session.notice(&format!(
                "已自动重启 {MAX_AUTO_RESTARTS} 次仍立即退出，停止重试"
            ));
            return;
        }
        // 只有「当前挂的还是刚退出的那一轮」才重启，避免和手动启动抢
        let stale = session
            .run
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|r| !Arc::ptr_eq(&r.info, &info)))
            .unwrap_or(true);
        if stale {
            return;
        }
        session.notice("进程已正常退出，正在自动重启…");
        std::thread::sleep(AUTO_RESTART_DELAY);
        let next = SpawnSpec {
            auto_restart: true,
            ..spec
        };
        if let Err(e) = spawn_run(&session, next) {
            session.notice(&format!("自动重启失败：{e}"));
        }
    }
}

/// 阻塞等一个 pid 退出，返回退出码。
///
/// * 正常退出 → `WEXITSTATUS`
/// * 被信号杀死 → `128 + signal`（`ecpty` 路径原本由 `128 - raw` 算出同一个值）
/// * 收尸失败 → `None`
fn wait_status(pid: u32) -> Option<i32> {
    let mut status: libc::c_int = 0;
    loop {
        let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
        if r == -1 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return None;
        }
        break;
    }
    if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        Some(128 + libc::WTERMSIG(status))
    } else {
        None
    }
}

/// 收尾：落阶段 + 广播状态/退出事件 + 在控制台留一句。
fn finish(session: &Arc<Session>, info: &Arc<Mutex<RunInfo>>, phase: Phase, code: Option<i32>) {
    let snapshot = {
        let Ok(mut i) = info.lock() else { return };
        i.phase = phase;
        i.exit_code = code;
        i.stopped_at_ms = Some(now_ms());
        i.pid = None;
        i.clone()
    };
    if let Ok(mut last) = session.last.lock() {
        *last = snapshot.clone();
    }
    // 先清运行轮次再广播：回放订阅者因此看到的是「已结束」的状态。
    if let Ok(mut run) = session.run.lock() {
        *run = None;
    }

    let code_text = code.map(|c| c.to_string()).unwrap_or_else(|| "未知".into());
    match phase {
        Phase::Crashed => session.notice(&format!("进程异常退出（退出码 {code_text}）")),
        _ => session.notice(&format!("进程已退出（退出码 {code_text}）")),
    }

    session.publish_state(&snapshot);
    session.publish_transient(
        serde_json::json!({
            "type": "exit",
            "code": code,
            "phase": phase.as_str(),
        })
        .to_string(),
    );
}

/// 改阶段并广播（只作用于当前这一轮）。
pub(crate) fn transition(session: &Arc<Session>, phase: Phase, stop_requested: bool) {
    let running = session
        .run
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|r| r.info.clone()));
    let Some(info) = running else { return };
    let snapshot = {
        let Ok(mut i) = info.lock() else { return };
        if i.phase == phase && !stop_requested {
            return;
        }
        i.phase = phase;
        if stop_requested {
            i.stop_requested = true;
        }
        i.clone()
    };
    if let Ok(mut last) = session.last.lock() {
        *last = snapshot.clone();
    }
    session.publish_state(&snapshot);
}

/// 当前 Unix 毫秒。
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
