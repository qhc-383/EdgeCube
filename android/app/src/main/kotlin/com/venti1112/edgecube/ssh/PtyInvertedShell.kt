package com.venti1112.edgecube.ssh

import android.content.Context
import android.util.Log
import com.venti1112.edgecube.pty.FrameListener
import com.venti1112.edgecube.pty.PtyException
import com.venti1112.edgecube.pty.PtySession
import com.venti1112.edgecube.shell.ShellResolver
import org.apache.sshd.server.Environment
import org.apache.sshd.server.Signal
import org.apache.sshd.server.SignalListener
import org.apache.sshd.server.channel.ChannelSession
import org.apache.sshd.server.session.ServerSession
import org.apache.sshd.server.shell.InvertedShell
import org.json.JSONObject
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * 把 MINA SSHD 的 SSH 终端（shell 通道）桥接到 [PtySession]（Rust `portable-pty`）。
 *
 * 每个 SSH 客户端的每个 shell 通道都 [PtySession] 一个**独立**的伪终端（独立会话 +
 * 独立子进程），[ShellResolver] 负责解析系统 shell 与构造环境，因此多会话天然隔离
 * （不复用单例 `ShellProcessManager`，那是为单个 UI 终端设计的）。
 *
 * 原来这里用的是 `EcPty.createSubprocess`（`ecpty.c`）+ 自己开的 master fd +
 * `fdFromInt` 反射 hack；迁移后 **fd / 读线程 / `waitpid` 全在 Rust 侧**：
 * 输出经 `subscribe(withHistory = false)` 推进 [FrameQueueInputStream]，
 * 输入经 [PtySession.write] 直写，`waitFor` 由 Rust `wait_loop` 发的 state/exit 控制帧
 * 替代，窗口变化经 [PtySession.resize]（→ `SIGWINCH`）。
 *
 * ### 为什么 `withHistory = false` 且在 [PtySession.start] **之前**订阅
 *
 * - `true` 会把历史整段回放给客户端；SSH 每次开的都是全新会话，历史要么为空
 *   （没必要回放），要么是上一轮 shell 的残留（回放就是脏数据）。
 * - 先订阅再 `start`：shell 起来后第一行提示符也一定在订阅之后产生，不会漏开头。
 *   反过来会在「start → subscribe」的窗口里丢掉最早的输出。
 *
 * 实现 [InvertedShell] 的「反转流」语义（站在子进程视角命名）：
 *  - [getInputStream] 返回写端：SSH server 把客户端按键写进来 → 子进程 stdin；
 *  - [getOutputStream] 返回读端：SSH server 从这里读 → 子进程 stdout；
 *  - [getErrorStream] 返回恒空流：PTY 已把 stderr 合并进 master，无独立 stderr。
 *    注意**不能返回 null**——MINA SSHD 的 `InvertedShellWrapper.pumpStreams()` 会无条件
 *    调用 `shellErr.available()`，null 会在泵循环首轮触发 NPE → catch(Throwable) →
 *    destroy() → 会话立即断开（表现为登录后刚打出提示符就 Connection closed）。
 *
 * 回显不在此处理：由 PTY 的 tty 行规负责，这正是真实终端体验的来源。
 *
 * @param context 应用上下文（用于 PTY 环境构造）。
 * @param rootDir 终端初始工作目录（当前实例目录，与 SFTP 根目录一致）。
 */
class PtyInvertedShell(
    private val context: Context,
    private val rootDir: String,
) : InvertedShell {

    private companion object {
        const val TAG = "PtyInvertedShell"
        const val DEFAULT_ROWS = 24
        const val DEFAULT_COLS = 80
        const val CELL_W = 8
        const val CELL_H = 16

        /** 单块输出的读取粒度（对齐原实现的 4096）。 */
        const val CHUNK = 4096

        /** 队列深度；满时按最旧→最新的顺序补位，绝不无限堆积。 */
        const val QUEUE_DEPTH = 16
    }

    private var session: PtySession? = null
    private var subId: Long = 0
    private var output: FrameQueueInputStream? = null
    private var input: PtyOutputStream? = null

    @Volatile private var alive = false
    @Volatile private var exitCode = -1

    private var environment: Environment? = null
    private var winchListener: SignalListener? = null
    private var serverSession: ServerSession? = null
    private var channelSession: ChannelSession? = null

    /**
     * shell → 客户端的字节队列，由 [FrameListener] 在 Rust **输出线程**上喂入，
     * 由 MINA 的泵线程读出。
     *
     * MINA SSHD 的 `InvertedShellWrapper.pumpStreams()` 在**单线程**中按顺序轮询三个方向：
     * ```
     * for (;;) {
     *     pumpStream(in, shellIn, buf)      // 客户端 → shell（写 PTY）
     *     pumpStream(shellOut, out, buf)     // shell → 客户端（读本队列）
     *     pumpStream(shellErr, err, buf)     // stderr（PTY 下恒空，跳过）
     *     if (!alive && all drained) exit
     *     sleep(...)
     * }
     * ```
     * `pumpStream` 先调 `available()`，所以 `available()`/`read()` **绝不能阻塞在
     * 某个系统调用上**，否则泵线程回不到 client→shell 方向，用户按键就没了。
     * 这里全部只跟队列打交道（`read()` 最多 poll 50ms）。
     *
     * `onFrame` 跑在持有 Rust broadcast 锁的输出线程上，[offer] 因此**有上限地等**
     * （最多 50ms 就转去丢最旧一块）：无限阻塞会让 `destroy()` 里的 `unsubscribe`
     * 永久卡在锁上 → 主线程 ANR。
     */
    private inner class FrameQueueInputStream : InputStream() {
        private val queue = ArrayBlockingQueue<ByteArray>(QUEUE_DEPTH)

        /** 哨兵：输出读到头（Rust `output_eof`）时放入，read() 见到即返回 EOF。 */
        private val eofSentinel = ByteArray(0)

        @Volatile private var eof = false

        /**
         * 泵线程是否已经消费了 EOF 哨兵。
         *
         * 与 [eof] 不同：[eof] 表示生产端已收工，队列里可能还有没消费的数据；
         * [pumpSawEof] 表示泵线程已经取走了哨兵，此后 `available()` 才允许返回 0，
         * 让 MINA SSHD 泵退出循环。
         */
        @Volatile private var pumpSawEof = false

        /** 上一次 poll 出的数组未消费完的剩余部分（避免放回队列破坏顺序）。 */
        private var leftover: ByteArray? = null
        private var leftoverOff = 0

        /** 喂一段输出。持有 broadcast 锁的输出线程调用，因此有界等待 + 丢最旧。 */
        fun offer(data: ByteArray) {
            if (queue.offer(data, 50, TimeUnit.MILLISECONDS)) return
            // 消费端跟不上：丢最旧一块腾位（极端情况丢数据，但不阻塞读线程）。
            queue.poll()
            queue.offer(data)
        }

        /** 输出读到头：置 EOF 并放哨兵（幂等）。 */
        fun signalEof() {
            if (eof) return
            eof = true
            while (!queue.offer(eofSentinel, 50, TimeUnit.MILLISECONDS)) {
                queue.poll()
            }
        }

        override fun available(): Int {
            if (pumpSawEof) return 0
            leftover?.let { return it.size - leftoverOff }
            val first = queue.peek()
            if (first != null) {
                if (first === eofSentinel) {
                    pumpSawEof = true
                    return 0
                }
                return first.size
            }
            // 队列空但哨兵可能还在路上：返回 1 迫使泵线程调 read()，
            // read() 内部 poll 带短超时，既不阻塞泵线程也不让它过早退出。
            return if (!eof) 1 else if (pumpSawEof) 0 else 1
        }

        override fun read(): Int {
            val buf = ByteArray(1)
            val n = read(buf, 0, 1)
            return if (n == -1) -1 else buf[0].toInt() and 0xFF
        }

        override fun read(b: ByteArray, off: Int, len: Int): Int {
            if (len == 0) return 0
            leftover?.let { lo ->
                val avail = lo.size - leftoverOff
                val n = minOf(len, avail)
                System.arraycopy(lo, leftoverOff, b, off, n)
                leftoverOff += n
                if (leftoverOff >= lo.size) {
                    leftover = null
                    leftoverOff = 0
                }
                return n
            }
            if (pumpSawEof) return -1

            val data = queue.poll(50, TimeUnit.MILLISECONDS)
                ?: return if (pumpSawEof) -1 else 0
            if (data === eofSentinel) {
                pumpSawEof = true
                return -1
            }
            val n = minOf(len, data.size)
            System.arraycopy(data, 0, b, off, n)
            if (n < data.size) {
                leftover = data
                leftoverOff = n
            }
            return n
        }

        override fun read(b: ByteArray): Int = read(b, 0, b.size)

        override fun close() {
            // 由 destroy() 统一收尾；这里只停读，避免 MINA 关流时误伤会话。
            eof = true
        }
    }

    /**
     * 客户端 → shell 的写端。
     *
     * [close] 必须是 **no-op**：MINA 在检测到 SSH 通道 EOF 时会调 `shellIn.close()`，
     * 真去关会话就把还没收尾的 PTY 掐了；生命周期完全由 [destroy] 管。
     */
    private inner class PtyOutputStream : OutputStream() {
        override fun write(b: Int) = write(byteArrayOf(b.toByte()))

        override fun write(b: ByteArray, off: Int, len: Int) {
            if (len <= 0) return
            val s = session ?: return
            // Session::write 走无界通道，永不阻塞；进程没了就静默丢弃。
            s.write(b.copyOfRange(off, off + len))
        }

        override fun close() {
            // 见类注释：由 destroy() 统一管理。
        }
    }

    /** shell → 客户端的字节，由本会话自己的订阅者喂入。 */
    private val frameListener = object : FrameListener {
        override fun onFrame(kind: Int, bytes: ByteArray?, json: String?) {
            when (kind) {
                FrameListener.KIND_DATA -> if (bytes != null) output?.offer(bytes)
                FrameListener.KIND_CONTROL -> if (json != null) handleControl(json)
                // withHistory=false：回放帧不会出现，真出现也不该当实时数据打给客户端。
                else -> Unit
            }
        }
    }

    /**
     * 只认两类控制帧：
     *  - `output_eof`：PTY 读到头 → 放 EOF 哨兵，让 MINA 的泵退出循环；
     *  - `state` / `exit`：进程终局 → 置 `alive = false` 并记下退出码。
     *
     * 两者都只写 volatile 字段 / 入队，**绝不取任何锁**（这段跑在持有
     * broadcast 锁的输出线程上）。
     */
    private fun handleControl(json: String) {
        if (json.contains("\"type\":\"output_eof\"")) {
            output?.signalEof()
            return
        }
        val obj = try {
            JSONObject(json)
        } catch (_: Throwable) {
            return
        }
        when (obj.optString("type")) {
            "exit" -> {
                if (!obj.isNull("code")) exitCode = obj.optInt("code")
                alive = false
            }
            "state" -> when (obj.optString("phase")) {
                "stopped", "crashed" -> {
                    if (!obj.isNull("exitCode")) exitCode = obj.optInt("exitCode")
                    alive = false
                }
            }
        }
    }

    override fun start(channel: ChannelSession, env: Environment) {
        environment = env
        channelSession = channel

        // ── 1. 解析 shell 与 cwd ──
        val spec = ShellResolver.resolveInteractive()
        // 初始工作目录用当前实例目录；无效则回退到默认目录（外部存储根或私有目录）。
        var cwd = rootDir.takeIf { File(it).isDirectory } ?: ShellResolver.defaultCwd(context)

        Log.i(TAG, "Starting SSH shell: cmd=${spec.cmd}, argv=${spec.argvPrefix}, cwd=$cwd")

        // ── 2. 预检查：shell 二进制是否可执行 ──
        if (!File(spec.cmd).canExecute()) {
            val msg = "EdgeCube: shell binary not found or not executable: ${spec.cmd}"
            Log.e(TAG, msg)
            throw IOException(msg)
        }

        // ── 3. 预检查：cwd 是否有效，无效则回退 ──
        if (!File(cwd).isDirectory) {
            Log.w(TAG, "cwd not a directory: $cwd, falling back to default")
            cwd = ShellResolver.defaultCwd(context)
        }

        // ── 4. 构造环境变量 ──
        val envMap = ShellResolver.baseEnv(context, cwd)
        // 优先采用客户端请求的 TERM，以获得正确的终端能力（颜色/全屏程序）。
        env.env[Environment.ENV_TERM]?.takeIf { it.isNotBlank() }?.let { envMap["TERM"] = it }
        val envp = envMap.map { "${it.key}=${it.value}" }.toTypedArray()

        val rows = env.env[Environment.ENV_LINES]?.toIntOrNull()?.takeIf { it > 0 } ?: DEFAULT_ROWS
        val cols = env.env[Environment.ENV_COLUMNS]?.toIntOrNull()?.takeIf { it > 0 } ?: DEFAULT_COLS

        val argv = ArrayList<String>()
        argv.add(spec.cmd)
        argv.addAll(spec.argvPrefix)

        // ── 5. 建会话、先订阅、再拉起（见类注释为什么顺序不能反）──
        val outputQueue = FrameQueueInputStream()
        output = outputQueue
        val inputStream = PtyOutputStream()
        input = inputStream

        val sess = PtySession("ssh")
        session = sess
        subId = sess.subscribe(frameListener, withHistory = false)
        if (subId == 0L) {
            sess.destroy()
            session = null
            output = null
            input = null
            throw IOException("订阅 PTY 输出失败")
        }

        // 先置 alive 再 start：start() 期间 Rust 就会广播 Running/Stopped 状态帧，
        // 万一同毫秒内进程就退出了，后置的 `alive = true` 会把终局状态盖掉 →
        // MINA 的泵永远等不到 `!isAlive()`，客户端再也收不到 exit。
        alive = true
        try {
            sess.start(
                argv = argv.toTypedArray(),
                envp = envp,
                cwd = cwd,
                rows = rows,
                cols = cols,
                cellW = CELL_W,
                cellH = CELL_H,
                initialPhase = "running",
            )
        } catch (e: PtyException) {
            Log.e(TAG, "PTY start failed: ${e.code} ${e.message}")
            failStart()
            throw IOException("创建 PTY 子进程失败: ${e.message}", e)
        } catch (e: Exception) {
            Log.e(TAG, "PTY start threw: ${e.message}", e)
            failStart()
            throw IOException("创建 PTY 子进程失败: ${e.message}", e)
        }

        alive = true
        Log.i(TAG, "SSH shell ready: pid=${sess.pid}, shell=${spec.cmd}")

        // 客户端窗口尺寸变化（SSH WINDOW_CHANGE → Signal.WINCH）时同步 PTY 窗口，
        // 子进程随之收到 SIGWINCH 重排。回调里从 Environment 重新读取列/行。
        val listener = SignalListener { _, _ ->
            val e = environment ?: return@SignalListener
            val r = e.env[Environment.ENV_LINES]?.toIntOrNull()?.takeIf { it > 0 }
                ?: return@SignalListener
            val c = e.env[Environment.ENV_COLUMNS]?.toIntOrNull()?.takeIf { it > 0 }
                ?: return@SignalListener
            try {
                session?.resize(r, c, CELL_W, CELL_H)
            } catch (_: Exception) {
            }
        }
        winchListener = listener
        env.addSignalListener(listener, Signal.WINCH)
    }

    /** 拉起失败后的回滚：退订 + 销毁会话，别把半截会话留着。 */
    private fun failStart() {
        val sess = session
        session = null
        if (sess != null && subId != 0L) {
            try {
                sess.unsubscribe(subId)
            } catch (_: Exception) {
            }
        }
        subId = 0
        try {
            sess?.destroy()
        } catch (_: Exception) {
        }
        output = null
        input = null
        alive = false
    }

    override fun getInputStream(): OutputStream =
        input ?: throw IllegalStateException("shell 尚未启动")

    override fun getOutputStream(): InputStream =
        output ?: throw IllegalStateException("shell 尚未启动")

    /**
     * 恒空的 stderr 流。PTY 行规已把子进程 stderr 合并进 master（dup2 到同一从设备），
     * 不存在独立错误流；`available()` 恒 0 使 MINA 的泵循环直接跳过 stderr 方向。
     *
     * 不能返回 null：`InvertedShellWrapper.pumpStreams()` 每轮都会无条件调用
     * `shellErr.available()`（泵和退出判定各一次，均无 null 检查），null 会在首轮
     * 触发 NPE → catch(Throwable) → destroy()，表现为客户端刚看到提示符就被断开。
     */
    private val emptyErrorStream = object : InputStream() {
        override fun available(): Int = 0
        override fun read(): Int = -1
        override fun read(b: ByteArray, off: Int, len: Int): Int = if (len == 0) 0 else -1
    }

    override fun getErrorStream(): InputStream = emptyErrorStream

    override fun isAlive(): Boolean = alive

    override fun exitValue(): Int = exitCode

    // 来自 ServerSessionAware / 通道感知接口：保存并回传当前 SSH 会话与通道。
    override fun setSession(session: ServerSession) {
        serverSession = session
    }

    override fun getServerSession(): ServerSession? = serverSession

    override fun getServerChannelSession(): ChannelSession? = channelSession

    override fun destroy(channel: ChannelSession) {
        Log.w(TAG, "destroy() called: alive=$alive, exitCode=$exitCode")
        winchListener?.let { l -> environment?.removeSignalListener(l) }
        winchListener = null

        val sess = session
        session = null
        if (sess != null) {
            // 整组 SIGKILL —— 等价原实现 destroy() 里那句 kill(pid, SIGTERM)：
            // 收掉的都是本通道自己那一个 shell，不会波及别的会话。
            try {
                sess.kill()
            } catch (_: Exception) {
            }
            if (subId != 0L) {
                try {
                    sess.unsubscribe(subId)
                } catch (_: Exception) {
                }
            }
            subId = 0
            try {
                sess.destroy()
            } catch (_: Exception) {
            }
        }
        output?.close()
        output = null
        input = null
        alive = false
    }
}
