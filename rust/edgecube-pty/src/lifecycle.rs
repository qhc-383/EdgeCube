//! 会话生命周期：启动、优雅停止、强制停止、静默收尾。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::pty;
use crate::run::{SpawnSpec, spawn_run, transition};
use crate::session::{Phase, ProcessError, RunInfo, Session};

/// 起一轮进程。
pub fn start(session: &Arc<Session>, spec: SpawnSpec) -> Result<RunInfo, ProcessError> {
    let existing = session.info();
    if existing.alive() {
        return Err(ProcessError::AlreadyRunning { pid: existing.pid });
    }
    session.reset_auto_restarts();
    match spawn_run(session, spec) {
        Ok(info) => Ok(info),
        Err(e) => {
            session.notice(&format!("[EdgeCube] 启动失败：{e}"));
            Err(e)
        }
    }
}

/// 停止：把停止命令写进 PTY。
pub fn stop(
    session: &Arc<Session>,
    stop_command: &str,
    line_ending: &str,
) -> Result<(), ProcessError> {
    let info = session.info();
    if !info.alive() {
        return Err(ProcessError::NotRunning);
    }
    let payload =
        stop_payload(Some(stop_command), line_ending).ok_or(ProcessError::NoStopCommand)?;

    // 先落「停止中」再写命令：反过来可能命令刚发出进程就没了，
    // 那个窗口里状态还是「运行中」，前端会闪一下。
    transition(session, Phase::Stopping, true);
    session.write(payload)
}

/// 强制停止：直接杀。
///
/// Unix 上额外给整个进程组补一刀 SIGKILL —— PTY 的子进程被 `setsid()` 提成了
/// 会话首进程（pgid == pid），所以 `sh -c` 里再 fork 出来的子孙也能一起清掉。
/// 这一发是原来缺的（缺陷 A8：shell 只杀 pid、非 proot 走 SIGTERM）。
pub fn kill(session: &Arc<Session>) -> Result<(), ProcessError> {
    let info = session.info();
    if !info.alive() {
        return Err(ProcessError::NotRunning);
    }
    transition(session, Phase::Stopping, true);

    // 守卫要活到 killer 用完（借出去会被当成悬垂）
    let run_guard = session
        .run
        .lock()
        .map_err(|_| ProcessError::Kill("会话锁损坏".into()))?;
    let Some(run) = run_guard.as_ref() else {
        return Err(ProcessError::NotRunning);
    };

    // 1) 先整组 SIGKILL。
    //
    //    **顺序很要紧**：`portable-pty` 的 `clone_killer()` 在 unix 上给的是
    //    `ProcessSignaller`，它发的是 **SIGHUP** 而不是 SIGKILL。SIGHUP 排在
    //    前面的话，退出码会变成 129 而不是 137，而且它只打一个 pid，
    //    `sh -c` fork 出来的兄弟进程照样活下来（缺陷 A8）。
    let mut result = match info.pid {
        Some(pid) => {
            pty::kill_process_group(pid as i32).map_err(|e| ProcessError::Kill(e.to_string()))
        }
        None => Err(ProcessError::Kill("没有 pid 可杀".into())),
    };

    // 2) 组没打中（pgid 失效、进程不带 setsid 之类）才退回 killer 补一刀。
    if result.is_err() {
        let mut killer = run
            .killer
            .lock()
            .map_err(|_| ProcessError::Kill("杀进程句柄损坏".into()))?;
        let fallback = killer.kill().map_err(|e| ProcessError::Kill(e.to_string()));
        drop(killer);
        if fallback.is_ok() {
            result = Ok(());
        }
    }

    drop(run_guard);
    result
}

/// 静默收掉一个会话：不等事件、不发通知，杀掉就走。
///
/// 调用方接着要丢掉这个 `Session` —— 会话连着 PTY 一起被丢掉，
/// PTY 一关子进程就没有控制终端了，内核会给它的前台进程组补一发 SIGHUP。
pub fn shutdown_quiet(session: &Session) {
    let Ok(guard) = session.run.lock() else {
        return;
    };
    let Some(run) = guard.as_ref() else { return };
    if let Ok(mut killer) = run.killer.lock() {
        let _ = killer.kill();
    }
    let pid = run.info.lock().ok().and_then(|i| i.pid);
    if let Some(pid) = pid {
        let _ = pty::kill_process_group(pid as i32);
    }
    // `guard`（会话 run 锁）在此处自然释放；不让状态机改阶段，只收进程。
}

/// 前端匹配到「服务端就绪」标记后调用：`starting` → `running`。
///
/// Rust 不读输出流里那个标记（`DONE_PATTERN` 在 Kotlin 的行组装里），
/// 所以状态的推进必须由 Kotlin 侧显式点一下。shell 起来就是 `running`，
/// 没有 `starting` 阶段，调它也不会出错（`transition` 见阶段没变就跳过）。
pub fn notify_ready(session: &Arc<Session>) {
    transition(session, Phase::Running, false);
}

// ---------------------------------------------------------------------------
// 停止命令的字节化
// ---------------------------------------------------------------------------

/// 认 `^@`–`^_`（`^C` 就是 0x03）与 `^?`（DEL），大小写都收。
/// **必须整个值就是一个记号**才算 —— 否则 `say ^C` 这种带 caret 的普通命令
/// 会被误当成控制字符，那种误判极难排查。
fn caret_byte(text: &str) -> Option<u8> {
    let mut chars = text.chars();
    if chars.next()? != '^' {
        return None;
    }
    let head = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let head = head.to_ascii_uppercase();
    match head {
        // `@`(0x40)–`_`(0x5f) 与 `^@`–`^_` 正好差 0x40，清掉第 6 位即可
        '@'..='_' => Some((head as u8) & 0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// 把「停止命令」翻译成要写进 PTY 的字节。
///
/// * 整个值是 caret 记号（如 `^C`）→ 只写那个控制字节。终端驱动会把 `^C`
///   变成发给前台进程组的 SIGINT —— 没有控制台命令的服务端（`python main.py`
///   之类）靠这个优雅退出。
/// * 其它 → 当成一条控制台命令，末尾补 `line_ending` 再写。
/// * 两者都拿不到（命令为空）→ `None`，此时只有「强制停止」可用。
pub fn stop_payload(stop_command: Option<&str>, line_ending: &str) -> Option<Vec<u8>> {
    let text = stop_command.map(str::trim).filter(|t| !t.is_empty())?;
    if let Some(byte) = caret_byte(text) {
        return Some(vec![byte]);
    }
    let mut payload = text.as_bytes().to_vec();
    if line_ending.is_empty() {
        payload.push(b'\n');
    } else {
        payload.extend_from_slice(line_ending.as_bytes());
    }
    Some(payload)
}

/// 自动重启计数（测试/自省用）。
pub fn auto_restart_count(session: &Session) -> u32 {
    session.auto_restarts.load(Ordering::Relaxed)
}
