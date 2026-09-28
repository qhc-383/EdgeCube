//! 批次 3 阶段 3A 的核心契约测试：回放原子性、历史裁剪、阶段机、停止/强杀。
//!
//! 与 `smoke.rs`（只验原语）不同，这里走的是上层 `Session` + `lifecycle`
//! 的真实调用路径 —— 也就是 Kotlin 之后会经由 JNI 调到的那条路。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use edgecube_pty::frame::{FrameSink, OutFrame, SharedSink};
use edgecube_pty::lifecycle::{self, stop_payload};
use edgecube_pty::run::SpawnSpec;
use edgecube_pty::session::{Phase, ProcessError, RunInfo, Session};

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 把收到的帧原样攒下来。**绝不阻塞** —— 这也是真实订阅者的契约。
#[derive(Default)]
struct Recorder {
    frames: Mutex<Vec<OutFrame>>,
}

impl Recorder {
    fn frames(&self) -> Vec<OutFrame> {
        self.frames.lock().unwrap().clone()
    }

    fn only_data(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        for f in self.frames() {
            if let OutFrame::Data(d) = f {
                buf.extend_from_slice(&d);
            }
        }
        buf
    }
}

impl FrameSink for Recorder {
    fn send(&self, frame: OutFrame) {
        self.frames.lock().unwrap().push(frame);
    }
}

fn recorder() -> (Arc<Recorder>, SharedSink) {
    let rec = Arc::new(Recorder::default());
    let sink: SharedSink = rec.clone();
    (rec, sink)
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 反复读状态直到进程结束，或超时。
fn wait_finished(session: &Session) -> RunInfo {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let info = session.info();
        if !info.alive() {
            return info;
        }
        assert!(
            Instant::now() < deadline,
            "进程 10 秒内没有退出，状态仍为 {:?}",
            info.phase
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 起一个 `sh -c '<body>'`。
fn shell_spec(label: &str, body: &str) -> SpawnSpec {
    SpawnSpec {
        label: label.to_string(),
        argv: vec!["/bin/sh".into(), "-c".into(), body.into()],
        cwd: "/tmp".into(),
        envp: vec![
            "PATH=/usr/local/bin:/usr/bin:/bin".into(),
            "HOME=/tmp".into(),
            "TERM=xterm-256color".into(),
        ],
        initial_phase: Phase::Running,
        ..SpawnSpec::default()
    }
}

fn session(label: &str) -> Arc<Session> {
    Arc::new(Session::new(label))
}

// ---------------------------------------------------------------------------
// 回放 / 订阅
// ---------------------------------------------------------------------------

#[test]
fn subscribe_with_history_replays_before_it_listens() {
    let s = session("replay");
    // 先攒一些历史（不经 PTY，直接灌广播）
    s.notice("line one");
    s.notice("line two");

    let (rec, sink) = recorder();
    let id = s.subscribe(sink, true);

    let frames = rec.frames();
    assert_eq!(
        frames.first(),
        Some(&OutFrame::ReplayBegin),
        "必须以回放开始打头"
    );
    assert_eq!(
        frames.last(),
        Some(&OutFrame::ReplayEnd(s.state_json())),
        "必须以回放结束收尾，并带上当前状态"
    );
    let text = String::from_utf8_lossy(&rec.only_data()).into_owned();
    assert!(text.contains("line one"), "回放里没有历史：{text:?}");
    assert!(text.contains("line two"), "回放里没有历史：{text:?}");

    // 回放之后才是「活着的」帧
    s.notice("live after");
    assert!(
        String::from_utf8_lossy(&rec.only_data()).contains("live after"),
        "回放后应该继续收到实时帧"
    );

    s.unsubscribe(id);
    assert_eq!(s.subscriber_count(), 0);
    s.notice("after unsubscribe");
    assert!(!String::from_utf8_lossy(&rec.only_data()).contains("after unsubscribe"));
}

#[test]
fn subscribe_without_history_skips_replay_entirely() {
    let s = session("no-replay");
    s.notice("hidden history");

    let (rec, sink) = recorder();
    let _id = s.subscribe(sink, false);

    // 不带历史 = SSH 那条路径：回放边界与历史都不发，连状态也不发
    //（SSH 侧 `fromPty` 只要字节流，多一帧 JSON 会被当终端数据打出来）。
    assert!(
        rec.frames().is_empty(),
        "订阅时不该有任何帧：{:?}",
        rec.frames()
    );
    assert_eq!(s.subscriber_count(), 1, "但订阅者必须已经挂上");

    // 而且收到的是**实时**帧，历史那句不会追着来
    s.notice("live only");
    let text = String::from_utf8_lossy(&rec.only_data()).into_owned();
    assert!(text.contains("live only"));
    assert!(!text.contains("hidden history"), "不该回放历史：{text:?}");
    for f in rec.frames() {
        assert!(
            !matches!(f, OutFrame::ReplayBegin | OutFrame::ReplayEnd(_)),
            "不带历史时不该出现回放边界：{f:?}"
        );
    }
}

#[test]
fn history_is_capped_but_never_emptied() {
    let s = session("cap");
    let cap = edgecube_pty::session::DEFAULT_HISTORY_BYTES;
    assert!(cap > 0);

    // 单块就超过上限：只该留下这一块，绝不能裁成空
    s.notice(&"x".repeat(cap + 4096));
    assert_eq!(
        s.history_len(),
        cap + 4096 + 4,
        "单块超限不裁（至少留一块）"
    );

    // 再来一块 → 裁掉旧的，但必须留下新块
    s.notice(&"y".repeat(16));
    assert!(
        s.history_len() < cap,
        "裁剪后应低于上限，实际 {}",
        s.history_len()
    );
    assert!(s.history_len() > 0, "裁剪不能把历史清空");

    let snap = s.history_snapshot();
    assert!(String::from_utf8_lossy(&snap).contains("yyyy"));
}

#[test]
fn transient_control_frames_never_enter_history() {
    let s = session("transient");
    let (rec, sink) = recorder();
    let _id = s.subscribe(sink, true);

    s.notice("before exit");
    let before = s.history_len();

    // finish 走的就是这条：一次记忆状态 + 一次瞬时退出事件
    s.publish_state(&RunInfo {
        phase: Phase::Stopped,
        exit_code: Some(0),
        ..RunInfo::default()
    });
    s.publish_transient(r#"{"type":"exit","code":0}"#.into());

    assert_eq!(s.history_len(), before, "控制帧不得进历史");
    let frames = rec.frames();
    let controls: Vec<&str> = frames
        .iter()
        .filter_map(|f| match f {
            OutFrame::Control(j) => Some(j.as_str()),
            _ => None,
        })
        .collect();
    assert!(controls.iter().any(|j| j.contains("\"type\":\"state\"")));
    assert!(controls.iter().any(|j| j.contains("\"type\":\"exit\"")));

    // 记住的状态要能从新订阅者的 ReplayEnd 里拿到
    let (rec2, sink2) = recorder();
    let _id2 = s.subscribe(sink2, true);
    assert_eq!(
        rec2.frames().last(),
        Some(&OutFrame::ReplayEnd(s.state_json()))
    );
    assert!(s.state_json().contains("\"phase\":\"stopped\""));
}

#[test]
fn clear_history_keeps_the_subscriber() {
    let s = session("clear");
    let (rec, sink) = recorder();
    let _id = s.subscribe(sink, true);
    s.notice("gone soon");
    s.clear_history();
    assert_eq!(s.history_len(), 0);
    assert_eq!(s.subscriber_count(), 1, "清历史不能把订阅者也清掉");

    s.notice("still delivered");
    assert!(String::from_utf8_lossy(&rec.only_data()).contains("still delivered"));
}

// ---------------------------------------------------------------------------
// 启动 / 退出
// ---------------------------------------------------------------------------

#[test]
fn start_captures_output_and_finishes_cleanly() {
    let s = session("run");
    let (rec, sink) = recorder();
    let _id = s.subscribe(sink, true);

    let info = lifecycle::start(&s, shell_spec("run", "printf hello-from-pty")).unwrap();
    assert_eq!(info.phase, Phase::Running);
    assert!(info.pid.is_some());
    assert!(s.info().alive());

    let done = wait_finished(&s);
    assert_eq!(done.phase, Phase::Stopped, "退出码 0 应判为已停止");
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(done.pid, None);

    let out = String::from_utf8_lossy(&rec.only_data()).into_owned();
    assert!(out.contains("hello-from-pty"), "没抓到子进程输出：{out:?}");
    assert!(
        out.contains("进程已退出（退出码 0）"),
        "退出提示缺失：{out:?}"
    );

    // 退出事件是瞬时控制帧
    let has_exit = rec
        .frames()
        .iter()
        .any(|f| matches!(f, OutFrame::Control(j) if j.contains("\"type\":\"exit\"")));
    assert!(has_exit, "退出后应广播 exit 控制帧");
}

/// 输出线程读到头时必须发一帧 `output_eof`：Kotlin 的按行组装器靠它冲刷
/// 最后那段没换行的半行（退出状态帧由另一条线程发，拿它当冲刷点会丢行）。
#[test]
fn output_eof_is_emitted_when_the_pty_ends() {
    let s = session("eof");
    let (rec, sink) = recorder();
    let _id = s.subscribe(sink, true);

    // 不带换行的尾巴，正好看组装器能不能拿到
    lifecycle::start(&s, shell_spec("eof", "printf tail-without-newline")).unwrap();
    wait_finished(&s);

    assert!(
        wait_until(Duration::from_secs(3), || {
            rec.frames()
                .iter()
                .any(|f| matches!(f, OutFrame::Control(j) if j.contains("output_eof")))
        }),
        "没收到 output_eof 控制帧：{:?}",
        rec.frames()
    );
    let text = String::from_utf8_lossy(&s.history_snapshot()).into_owned();
    assert!(text.contains("tail-without-newline"));
}

#[test]
fn non_zero_exit_is_reported_as_crashed() {
    let s = session("crash");
    let (rec, sink) = recorder();
    let _id = s.subscribe(sink, true);

    lifecycle::start(&s, shell_spec("crash", "exit 3")).unwrap();
    let done = wait_finished(&s);

    assert_eq!(done.phase, Phase::Crashed, "非 0 退出应判为异常退出");
    assert_eq!(done.exit_code, Some(3));
    assert!(!done.stop_requested);
    assert!(String::from_utf8_lossy(&rec.only_data()).contains("进程异常退出（退出码 3）"));
}

#[test]
fn starting_twice_is_rejected_then_allowed_after_exit() {
    let s = session("twice");
    lifecycle::start(&s, shell_spec("twice", "sleep 30")).unwrap();

    match lifecycle::start(&s, shell_spec("twice2", "true")) {
        Err(ProcessError::AlreadyRunning { pid }) => assert!(pid.is_some()),
        other => panic!("应当拒绝重复启动，实际 {other:?}"),
    }

    // 杀掉后应当能重新启动
    lifecycle::kill(&s).unwrap();
    let done = wait_finished(&s);
    assert!(!done.alive());

    lifecycle::start(&s, shell_spec("twice3", "true")).unwrap();
    wait_finished(&s);
}

#[test]
fn write_and_stop_without_a_process_are_rejected() {
    let s = session("dead");
    assert!(matches!(
        lifecycle::stop(&s, "stop", "\n"),
        Err(ProcessError::NotRunning)
    ));
    assert!(matches!(lifecycle::kill(&s), Err(ProcessError::NotRunning)));
    assert!(matches!(
        s.write(b"hi".to_vec()),
        Err(ProcessError::NotRunning)
    ));
    assert!(matches!(
        s.resize(80, 24, 8, 16),
        Err(ProcessError::NotRunning)
    ));

    // 非空历史/状态仍然可用 —— 没进程不等于没会话
    assert_eq!(s.subscriber_count(), 0);
}

#[test]
fn a_completed_process_allows_another_start() {
    let s = session("restart");
    lifecycle::start(&s, shell_spec("r1", "true")).unwrap();
    wait_finished(&s);

    let info = lifecycle::start(&s, shell_spec("r2", "printf second")).unwrap();
    assert!(info.alive());
    let done = wait_finished(&s);
    assert_eq!(done.exit_code, Some(0));
    // 两轮都留在同一份历史里
    let out = String::from_utf8_lossy(&s.history_snapshot()).into_owned();
    assert!(out.contains("second"));
}

// ---------------------------------------------------------------------------
// 停止
// ---------------------------------------------------------------------------

#[test]
fn caret_stop_sends_the_control_byte_and_interrupts() {
    let s = session("caret");
    let info = lifecycle::start(&s, shell_spec("caret", "cat > /dev/null")).unwrap();
    assert!(info.alive());
    assert!(!info.stop_requested);

    // cat 是交互读，stdin 不给 EOF 就一直挂着 —— 只有 ^C 能把它弄死
    std::thread::sleep(Duration::from_millis(300));
    assert!(s.info().alive(), "启动后应当还在跑");

    lifecycle::stop(&s, "^C", "\n").unwrap();

    let done = wait_finished(&s);
    assert!(done.stop_requested, "优雅停止要标记 stop_requested");
    assert_eq!(done.phase, Phase::Stopped, "我们主动停的不算崩溃");
    // sh 被信号中断 → 128 + 2 (SIGINT) = 130
    assert_eq!(done.exit_code, Some(130), "预期 SIGINT 中断码 130");
}

#[test]
fn kill_terminates_a_stubborn_process_group() {
    let s = session("kill");
    // sleep 不理会我们写的任何「停止命令」，只能强杀
    let info = lifecycle::start(&s, shell_spec("kill", "sleep 120")).unwrap();
    let pid = info.pid.unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(s.info().alive());

    lifecycle::kill(&s).unwrap();
    let done = wait_finished(&s);

    assert_eq!(done.phase, Phase::Stopped, "强杀也是主动停");
    assert!(done.stop_requested);
    // SIGKILL → 128 + 9 = 137；且 shell 连同 sleep 一起被清掉
    assert_eq!(done.exit_code, Some(137), "预期 SIGKILL 码 137");

    // 进程组确实没了
    let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
    assert!(!alive, "pid {pid} 还活着，进程组没杀干净");
}

#[test]
fn force_kill_of_an_already_dead_process_still_reports() {
    let s = session("double-kill");
    lifecycle::start(&s, shell_spec("dk", "true")).unwrap();
    wait_finished(&s);
    // 进程已经没了 → 必须是 NotRunning，而不是「假装成功」
    assert!(matches!(lifecycle::kill(&s), Err(ProcessError::NotRunning)));
}

#[test]
fn a_stopped_session_can_be_killed_again_only_while_alive() {
    let s = session("notify");
    lifecycle::start(&s, shell_spec("n", "sleep 30")).unwrap();
    lifecycle::kill(&s).unwrap();
    wait_finished(&s);

    // notify_ready 在没有运行轮次时是安全的空操作
    lifecycle::notify_ready(&s);
    assert_eq!(s.info().phase, Phase::Stopped);
}

// ---------------------------------------------------------------------------
// 尺寸 / 回显
// ---------------------------------------------------------------------------

#[test]
fn resize_reaches_the_child_tty() {
    let s = session("resize");
    // 子进程把 stty 尺寸打出来，随后退出
    let spec = shell_spec("resize", "read x; stty size");
    lifecycle::start(&s, spec).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    s.resize(132, 43, 8, 16).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    s.write(b"q\n".to_vec()).unwrap();

    wait_finished(&s);
    let out = String::from_utf8_lossy(&s.history_snapshot()).into_owned();
    assert!(out.contains("43 132"), "resize 没生效，输出：{out:?}");
}

const BEACON: &str = "ECHOBEACON0123456789";

/// 回显默认开着：tty 会把敲进去的字回流一次，`printf` 再打一次 → 两处。
#[test]
fn echo_on_reflows_the_input_once() {
    let s = session("echo-on");
    lifecycle::start(
        &s,
        shell_spec("echo-on", "read line; printf 'got:%s' \"$line\""),
    )
    .unwrap();
    wait_until(Duration::from_secs(3), || s.info().pid.is_some());
    std::thread::sleep(Duration::from_millis(200));

    s.write(format!("{BEACON}\n").into_bytes()).unwrap();
    wait_finished(&s);

    let out = String::from_utf8_lossy(&s.history_snapshot()).into_owned();
    assert!(
        out.contains(&format!("got:{BEACON}")),
        "子进程没收到数据：{out:?}"
    );
    assert_eq!(
        out.matches(BEACON).count(),
        2,
        "回显开着应当出现两处（tty 回流 + printf）：{out:?}"
    );
}

/// 关掉回显后 tty 不再回流，只剩 `printf` 那一处。
#[test]
fn echo_off_suppresses_the_tty_reflow() {
    let s = session("echo-off");
    lifecycle::start(
        &s,
        shell_spec("echo-off", "read line; printf 'got:%s' \"$line\""),
    )
    .unwrap();
    wait_until(Duration::from_secs(3), || s.info().pid.is_some());
    std::thread::sleep(Duration::from_millis(200));

    s.set_echo(false).unwrap();
    s.write(format!("{BEACON}\n").into_bytes()).unwrap();
    wait_finished(&s);

    let out = String::from_utf8_lossy(&s.history_snapshot()).into_owned();
    assert!(
        out.contains(&format!("got:{BEACON}")),
        "子进程没收到数据：{out:?}"
    );
    assert_eq!(
        out.matches(BEACON).count(),
        1,
        "关掉回显后仍然回流了一次：{out:?}"
    );
}

// ---------------------------------------------------------------------------
// 停止命令字节化
// ---------------------------------------------------------------------------

#[test]
fn stop_payload_handles_caret_and_line_endings() {
    assert_eq!(stop_payload(Some("^C"), "\n"), Some(vec![0x03]));
    assert_eq!(stop_payload(Some("^c"), "\n"), Some(vec![0x03]));
    assert_eq!(stop_payload(Some("^@"), "\n"), Some(vec![0x00]));
    assert_eq!(stop_payload(Some("^?"), "\n"), Some(vec![0x7f]));
    assert_eq!(stop_payload(Some("stop"), "\n"), Some(b"stop\n".to_vec()));
    assert_eq!(
        stop_payload(Some("  end  "), "\r\n"),
        Some(b"end\r\n".to_vec())
    );
    // 空 / 全空白 → 只有强杀可用
    assert_eq!(stop_payload(Some(""), "\n"), None);
    assert_eq!(stop_payload(Some("   "), "\n"), None);
    assert_eq!(stop_payload(None, "\n"), None);
    // 只有整串就是一个记号才算 —— 否则普通命令会被误当控制字符
    assert_eq!(
        stop_payload(Some("say ^C"), "\n"),
        Some(b"say ^C\n".to_vec())
    );
    assert_eq!(stop_payload(Some("^X!"), "\n"), Some(b"^X!\n".to_vec()));
}

// ---------------------------------------------------------------------------
// 回放分片
// ---------------------------------------------------------------------------

#[test]
fn large_history_is_replayed_in_chunks() {
    let s = session("chunk");
    // 用 NOTICE 叠出超过 64 KB 的历史
    for _ in 0..8 {
        s.notice(&"y".repeat(12 * 1024));
    }
    assert!(s.history_len() > 64 * 1024);

    let (rec, sink) = recorder();
    let _id = s.subscribe(sink, true);

    let frames = rec.frames();
    let data_frames: Vec<&OutFrame> = frames
        .iter()
        .filter(|f| matches!(f, OutFrame::Data(_)))
        .collect();
    assert!(
        data_frames.len() >= 2,
        "大历史应切成多帧，实际 {} 帧",
        data_frames.len()
    );
    for f in &data_frames {
        if let OutFrame::Data(d) = f {
            assert!(d.len() <= 64 * 1024, "单帧超过 64 KB：{}", d.len());
        }
    }
    // 切片拼回去必须等于完整历史
    let replayed = rec.only_data();
    assert_eq!(replayed.len(), s.history_len(), "回放分片拼不回原样");
    assert!(String::from_utf8_lossy(&replayed).contains("yyyy"));
}

// ---------------------------------------------------------------------------
// 自动重启默认关
// ---------------------------------------------------------------------------

#[test]
fn auto_restart_is_off_by_default() {
    let s = session("auto");
    lifecycle::start(&s, shell_spec("auto", "exit 0")).unwrap();
    wait_finished(&s);
    assert_eq!(lifecycle::auto_restart_count(&s), 0, "默认不开自动重启");
    // 且退出后不会自己又起来
    std::thread::sleep(Duration::from_millis(300));
    assert!(!s.info().alive(), "不该自动重启");
}
