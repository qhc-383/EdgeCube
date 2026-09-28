//! 批次 2 冒烟：六项判据必须全绿才能进入批次 3。
//!
//! 这些测试在**主机**上跑（`cargo test`），Android 侧靠
//! `cargo ndk -t ... build --release` 编译通过来验证；两侧共用同一份代码。

use std::time::Duration;

use edgecube_pty::pty::*;

const TIMEOUT: Duration = Duration::from_secs(10);

/// 用绝对路径，绕开 `env_clear()` 后 PATH 为空对「外部命令」的依赖。
fn stty_path() -> &'static str {
    for p in ["/usr/bin/stty", "/bin/stty"] {
        if std::path::Path::new(p).exists() {
            return p;
        }
    }
    panic!("主机上找不到 stty");
}

/// 1. `openpty`：拿到合法 master fd、内核记下的窗口尺寸、子端 tty 名。
#[test]
fn openpty_gives_geometry_fd_and_tty_name() {
    let pair = open_pty(30, 100).expect("openpty");
    let fd = master_fd(&*pair.master).expect("as_raw_fd");
    assert!(fd >= 0, "master fd 应为合法 fd，实际 {fd}");

    let size = pair.master.get_size().expect("get_size");
    assert_eq!(
        (size.rows, size.cols),
        (30, 100),
        "内核记录的尺寸应与 openpty 时一致"
    );

    let name = pair.master.tty_name().expect("tty_name");
    assert!(
        name.to_string_lossy().starts_with("/dev/pts/"),
        "子端 tty 应是 /dev/pts/*，实际 {name:?}"
    );
}

/// 2. `spawn_command` + 读输出 + `env_clear()` + `cwd` + 子进程退出后 EOF。
///
/// 对应 `ecpty.c:118-121`：子进程绝不继承 App 进程的环境污染，工作目录由
/// 我们显式指定。`read_to_end` 能在 10s 内返回，即证明父进程没把 slave 握死。
///
/// 验证清空不能用 `PATH`：`env -i /bin/sh` 下 bash 也会自己补一份默认 `PATH`
/// （`echo ${PATH:-EMPTY}` 照样有值），那是 shell 的行为而非我们的泄漏。
#[test]
fn spawn_emits_output_with_cleared_env_and_explicit_cwd() {
    assert!(
        std::env::var_os("HOME").is_some(),
        "测试前提：父进程必须持有 HOME，否则 env_clear 的断言会空过"
    );

    let pair = open_pty(24, 80).expect("openpty");
    let Spawned { master, mut child } = spawn(
        pair,
        "/bin/sh",
        &["-c", "echo H=${HOME:-EMPTY}; echo T=$TERM; pwd"],
        Some("/tmp"),
    )
    .expect("spawn");

    let out = read_all_with_timeout(
        master.try_clone_reader().expect("try_clone_reader"),
        TIMEOUT,
    )
    .expect("读取子进程输出");
    let status = child.wait().expect("wait");

    assert!(
        status.success(),
        "子进程应正常退出，实际 {status:?} / 输出 {out:?}"
    );
    assert!(
        out.contains("H=EMPTY"),
        "env_clear 未生效，父进程 HOME 泄漏进子进程，输出：{out:?}"
    );
    assert!(
        out.contains("T=xterm-256color"),
        "TERM 未注入，输出：{out:?}"
    );
    assert!(out.contains("/tmp"), "cwd 未生效，输出：{out:?}");
}

/// 3. `resize`（`TIOCSWINSZ` → `SIGWINCH`）真的传到子进程的 tty。
///
/// 对应 `ecpty.c:74` / `ecpty.c:218` 的 `pixel_width = cols * cell_w` 算式。
#[test]
fn resize_propagates_to_child_tty() {
    let pair = open_pty(24, 80).expect("openpty");
    let Spawned { master, mut child } = spawn(
        pair,
        "/bin/sh",
        &[
            "-c",
            &format!("{} size; read x; {} size", stty_path(), stty_path()),
        ],
        None,
    )
    .expect("spawn");

    let rx = spawn_line_reader(master.try_clone_reader().expect("try_clone_reader"));
    recv_until(&rx, "24 80", TIMEOUT).expect("首次 stty 应报 24 80");

    resize(&*master, 60, 120, NO_PIXELS, NO_PIXELS).expect("resize");

    let _writer = write_input(&*master, b"\n").expect("write_input");
    let second = recv_until(&rx, "60 120", TIMEOUT).expect("resize 后 stty 应报 60 120");

    let status = child.wait().expect("wait");
    assert!(
        status.success(),
        "子进程应正常退出，实际 {status:?} / 累积 {second:?}"
    );
}

/// 4. `set_echo` 翻转 termios 的 `ECHO` 位（`ecpty.c:249` 的等价实现）。
///
/// SSH 反向 shell 要靠它隐藏输入回显，是必须保留的能力。
#[test]
fn set_echo_toggles_termios_lflag() {
    let pair = open_pty(24, 80).expect("openpty");
    let fd = master_fd(&*pair.master).expect("as_raw_fd");

    set_echo(fd, false).expect("set_echo(false)");
    assert!(!echo_enabled(fd).expect("echo_enabled"), "ECHO 应已关闭");

    set_echo(fd, true).expect("set_echo(true)");
    assert!(echo_enabled(fd).expect("echo_enabled"), "ECHO 应已重新打开");
}

/// 5. `kill_process_group` 连子进程一起收干净。
#[test]
fn kill_process_group_reaps_child_and_its_offspring() {
    let pair = open_pty(24, 80).expect("openpty");
    let Spawned { master, mut child } =
        spawn(pair, "/bin/sh", &["-c", "/bin/sleep 30"], None).expect("spawn");

    let pgid = foreground_process_group(&*master).expect("process_group_leader");
    assert_eq!(
        pgid as u32,
        child.process_id().expect("process_id"),
        "前台进程组应即子进程 pid"
    );

    let started = std::time::Instant::now();
    kill_process_group(pgid).expect("kill_process_group");
    let status = child.wait().expect("wait");
    let elapsed = started.elapsed();

    assert!(
        !status.success(),
        "被 SIGKILL 的进程不应报成功，实际 {status:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "收尸应立即完成（原 sleep 30），实际耗时 {elapsed:?}"
    );
    assert!(status.signal().is_some(), "应是被信号杀死而非自行退出");
}

/// 6. `kill_process_group` 对已消失的组必须幂等（stop 会被重复调用）。
#[test]
fn kill_process_group_is_idempotent() {
    let pair = open_pty(24, 80).expect("openpty");
    // 保持 master 存活，确保子进程是「自行退出」而非被 SIGHUP 打断。
    let Spawned {
        master: _master,
        mut child,
    } = spawn(pair, "/bin/sh", &["-c", "exit 3"], None).expect("spawn");
    let status = child.wait().expect("wait");
    assert!(!status.success());

    kill_process_group(0x7fff_ffff).expect("ESRCH 应被当作成功");
}

/// 7. 整链自检探针：与 `edgecube_pty_smoke_probe` 导出符号跑同一条路径，
///    保证「链接得上」的那份代码在主机上确实「跑得通」。
#[test]
fn smoke_probe_runs_end_to_end() {
    let notes = smoke_probe().expect("smoke_probe 应全通过");
    assert!(notes.contains("spawn=ok"), "{notes}");
    assert!(notes.contains("resize=ok"), "{notes}");
}
