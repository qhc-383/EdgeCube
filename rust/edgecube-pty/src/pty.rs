use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::RawFd;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use portable_pty::{CommandBuilder, MasterPty, PtyPair, PtySize, native_pty_system};

/// 单元格像素尺寸未知时保持 0，与 `ecpty.c:74` 一致。
pub const NO_PIXELS: u16 = 0;

/// 打开一对 PTY，尺寸立即写进 kernel（对应 `ecpty.c:74`）。
pub fn open_pty(rows: u16, cols: u16) -> anyhow::Result<PtyPair> {
    native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: NO_PIXELS,
        pixel_height: NO_PIXELS,
    })
}

/// 对应 `ecpty.c:118-121` 的 `clearenv()` + `putenv("TERM=...")`：
/// 子进程只拿到我们显式注入的变量，绝不继承 App 进程的环境污染。
pub fn clear_env_command(program: &str) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(program);
    cmd.env_clear();
    cmd.env("TERM", "xterm-256color");
    cmd
}

/// 一个已启动的 PTY 会话。
///
/// `master` 归父进程；`child` 见 [`Spawned::child`]。
pub struct Spawned {
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
}

/// 把命令丢进 PTY 的前台进程组（`portable-pty` 的 `pre_exec` 内已 `setsid()`
/// + `TIOCSCTTY`，与 `ecpty.c:44-141` 等价）。
///
/// **消费 `pair` 并立刻 drop 掉父进程的 slave fd**：这是刻意的接口形状。
/// 若父进程继续握着 slave，`read(master)` 永远等不到 EOF，读线程就无法在
/// 子进程退出后自行收线（`tests/smoke.rs` 第 2 例即为此而写）。
pub fn spawn(
    pair: PtyPair,
    program: &str,
    args: &[&str],
    cwd: Option<&str>,
) -> anyhow::Result<Spawned> {
    let mut cmd = clear_env_command(program);
    cmd.args(args.iter().copied());
    if let Some(dir) = cwd {
        cmd.cwd(dir);
    }
    let portable_pty::PtyPair { slave, master } = pair;
    let child = slave.spawn_command(cmd).context("spawn_command 失败")?;
    drop(slave);
    Ok(Spawned { master, child })
}

/// `as_raw_fd()`；`set_echo` 与后续自定义 ioctl 都靠它。
pub fn master_fd(master: &dyn MasterPty) -> anyhow::Result<RawFd> {
    master
        .as_raw_fd()
        .ok_or_else(|| anyhow!("master 未暴露 raw fd"))
}

/// 对应 `ecpty.c:249`：`tcgetattr` 摘/置 `ECHO` 后 `tcsetattr`。
///
/// 在 master fd 上做是可靠的：Linux/Android 的 pty ioctl 会把未处理的
/// termios 请求转发给 slave，`portable-pty` 自身（`UnixMasterWriter::drop`）
/// 也是这么读 `VEOF` 的。
pub fn set_echo(fd: RawFd, on: bool) -> anyhow::Result<()> {
    let mut t: libc::termios = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
    if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
        return Err(std::io::Error::last_os_error()).context("tcgetattr 失败");
    }
    if on {
        t.c_lflag |= libc::ECHO;
    } else {
        t.c_lflag &= !libc::ECHO;
    }
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } != 0 {
        return Err(std::io::Error::last_os_error()).context("tcsetattr 失败");
    }
    Ok(())
}

/// 读回当前 `ECHO` 状态（`set_echo` 的自检）。
pub fn echo_enabled(fd: RawFd) -> anyhow::Result<bool> {
    let mut t: libc::termios = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
    if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
        return Err(std::io::Error::last_os_error()).context("tcgetattr 失败");
    }
    Ok(t.c_lflag & libc::ECHO != 0)
}

/// 通知子进程窗口变化（`TIOCSWINSZ` → `SIGWINCH`）。
///
/// `pixel_width = cols * cell_w`、`pixel_height = rows * cell_h`，与
/// `ecpty.c:74` / `ecpty.c:218` 的算式一致；cell 尺寸未知时传
/// [`NO_PIXELS`]（kernel 侧 `ws_xpixel`/`ws_ypixel` 允许为 0）。
pub fn resize(
    master: &dyn MasterPty,
    rows: u16,
    cols: u16,
    cell_w: u16,
    cell_h: u16,
) -> anyhow::Result<()> {
    master.resize(PtySize {
        rows,
        cols,
        pixel_width: cols.saturating_mul(cell_w),
        pixel_height: rows.saturating_mul(cell_h),
    })
}

/// 杀掉整个进程组
pub fn kill_process_group(pgid: i32) -> anyhow::Result<()> {
    if pgid <= 0 {
        return Err(anyhow!("非法 pgid {pgid}"));
    }
    if unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) } == -1 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(e).context("killpg 失败");
    }
    Ok(())
}

/// 取 PTY 的前台进程组 id（即应被整体杀掉的那组）。
pub fn foreground_process_group(master: &dyn MasterPty) -> anyhow::Result<i32> {
    master
        .process_group_leader()
        .ok_or_else(|| anyhow!("master 未返回前台进程组"))
}

/// 起一个后台线程逐行读 PTY 输出，避免读取阻塞调用方。
pub fn spawn_line_reader(reader: Box<dyn Read + Send>) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut br = BufReader::new(reader);
        loop {
            let mut buf = Vec::new();
            match br.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

/// 累积读到包含 `needle` 为止；超时/流断都带上下文报错，方便定位。
pub fn recv_until(
    rx: &mpsc::Receiver<String>,
    needle: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    let deadline = Instant::now() + timeout;
    let mut acc = String::new();
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(chunk) => {
                acc.push_str(&chunk);
                if acc.contains(needle) {
                    return Ok(acc);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(anyhow!("读取超时：已累积 {acc:?}，未找到 {needle:?}"));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow!("输出流已关闭：已累积 {acc:?}，未找到 {needle:?}"));
            }
        }
    }
}

/// 阻塞读到子进程关闭输出为止（子进程退出 / writer drop 时 EOF）。
pub fn read_all_with_timeout(
    mut reader: Box<dyn Read + Send>,
    timeout: Duration,
) -> anyhow::Result<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    match rx.recv_timeout(timeout) {
        Ok(buf) => Ok(String::from_utf8_lossy(&buf).into_owned()),
        Err(_) => Err(anyhow!("读取输出超时（{timeout:?}）")),
    }
}

/// 写入 PTY 输入端（等价于用户在终端敲键盘）。
///
/// 返回的 writer 必须由调用方持有：`portable-pty` 规定 writer 只能
/// `take_writer()` 一次，且它被 drop 时会给子进程补 `\n` + `VEOF`（EOF）。
pub fn write_input(master: &dyn MasterPty, data: &[u8]) -> anyhow::Result<Box<dyn Write + Send>> {
    let mut w = master
        .take_writer()
        .context("take_writer 失败（可能已取过）")?;
    w.write_all(data).context("写 PTY 输入失败")?;
    w.flush().context("flush PTY 输入失败")?;
    Ok(w)
}

/// 一次跑完全部原语的自检探针，主机与真机共用。
///
/// 除了是回归手段，它还有个结构性作用：只要存在一个从 `#[no_mangle]`
/// 导出符号可达的调用链，LTO 就不会把 PTY 原语当死代码删掉，
/// `openpty` / `killpg` / `tcsetattr` 才会真正进入 Android 链接器的解析范围。
/// （批次 2 第一次交叉编译时，产物里 `openpty` 引用数为 0、armeabi 的 .so
/// 只有 2.7 KB —— 那次「编译通过」其实什么都没验证到。）
///
/// 返回逐步执行的说明；任一步失败即 `Err`。
pub fn smoke_probe() -> anyhow::Result<String> {
    let mut notes = String::new();

    let pair = open_pty(24, 80)?;
    notes.push_str(&format!("tty={:?}\n", pair.master.tty_name()));
    let fd = master_fd(&*pair.master)?;

    set_echo(fd, false)?;
    if echo_enabled(fd)? {
        return Err(anyhow!("set_echo(false) 后 ECHO 仍为开"));
    }
    set_echo(fd, true)?;
    if !echo_enabled(fd)? {
        return Err(anyhow!("set_echo(true) 后 ECHO 仍为关"));
    }
    notes.push_str("echo=ok\n");

    // 主机是 /bin/sh，Android 是 /system/bin/sh。
    let sh = if std::path::Path::new("/system/bin/sh").exists() {
        "/system/bin/sh"
    } else {
        "/bin/sh"
    };
    let Spawned { master, mut child } = spawn(pair, sh, &["-c", "echo probe-ok"], None)?;
    let out = read_all_with_timeout(master.try_clone_reader()?, TIMEOUT)?;
    if !out.contains("probe-ok") {
        return Err(anyhow!("子进程输出未包含 probe-ok，实际 {out:?}"));
    }
    if !child.wait()?.success() {
        return Err(anyhow!("探针子进程未正常退出"));
    }
    notes.push_str("spawn=ok\n");

    resize(&*master, 40, 100, NO_PIXELS, NO_PIXELS)?;
    let size = master.get_size()?;
    if (size.rows, size.cols) != (40, 100) {
        return Err(anyhow!(
            "resize 后尺寸不符，实际 {}x{}",
            size.cols,
            size.rows
        ));
    }
    notes.push_str("resize=ok\n");

    // 幂等性：对一个必然不存在的组，ESRCH 必须当成功。
    kill_process_group(0x7fff_ffff)?;
    notes.push_str("killpg=ok\n");

    Ok(notes)
}

/// 探针超时（比测试里的宽松，真机 IO 可能慢）。
const TIMEOUT: Duration = Duration::from_secs(15);
