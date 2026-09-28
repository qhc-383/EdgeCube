package com.venti1112.edgecube.shell

import android.content.Context
import com.venti1112.edgecube.proot.ProotCommandBuilder
import com.venti1112.edgecube.pty.PtyException
import com.venti1112.edgecube.pty.PtySession
import io.flutter.plugin.common.EventChannel
import java.io.File

/**
 * 交互式 shell 的进程管理器（应用级单例）。
 *
 * 与 [com.venti1112.edgecube.server.ServerProcessManager] 一样，fd / 流 / 读线程 /
 * 输出回放全部由 [PtySession] + Rust 侧接管，这里只剩业务编排：
 * 选 shell（系统 sh / proot rootfs）、拼 argv 与环境、把按键写进 PTY、
 * 以及把终端尺寸/回显转发过去。
 *
 * 与服务端相比少了运行时解压、DONE 状态机、玩家解析与前台 Service —— shell 在
 * 真实 TTY 上自带行编辑/历史/补全，界面只需把原始字节交给 xterm 渲染。
 */
class ShellProcessManager private constructor(private val appContext: Context) {

    companion object {
        private const val DEFAULT_ROWS = 24
        private const val DEFAULT_COLS = 80
        private const val DEFAULT_CELL_W = 8
        private const val DEFAULT_CELL_H = 16

        @Volatile
        private var instance: ShellProcessManager? = null

        fun getInstance(context: Context): ShellProcessManager =
            instance ?: synchronized(this) {
                instance ?: ShellProcessManager(context.applicationContext).also { instance = it }
            }
    }

    @Volatile private var currentLabel: String? = null

    @Volatile private var lastRows = DEFAULT_ROWS
    @Volatile private var lastCols = DEFAULT_COLS
    @Volatile private var lastCellW = DEFAULT_CELL_W
    @Volatile private var lastCellH = DEFAULT_CELL_H

    private val session = PtySession(
        label = "shell",
        stateFields = { mapOf("label" to currentLabel) },
    )

    val isRunning: Boolean get() = session.isRunning

    /**
     * 设置/解除事件接收端。挂上时由 [PtySession] 回放输出历史与当前状态，
     * 使切走再回来的界面恢复画面。
     */
    fun setEventSink(sink: EventChannel.EventSink?) = session.setEventSink(sink)

    /**
     * 启动交互 shell。
     *
     * [shellId] 决定启动哪种 shell：
     *  - "system_sh" 或 null/空：系统自带的 /system/bin/sh（默认）
     *  - "proot:<rootfsId>"：进入指定 rootfs 的 proot 容器交互 shell
     *
     * [cwd] 为初始工作目录：
     *  - 系统 sh：空/无效则用外部存储根或私有目录（host 路径）
     *  - proot：空则用容器内 /root；非空时若是 host 路径会被忽略（proot 容器
     *    独立文件系统，host 路径无意义），需传容器内绝对路径如 /mnt/server
     *
     * @throws PtyException 起不来（`pty_*` 稳定码）
     */
    @Synchronized
    fun start(cwd: String?, shellId: String? = null) {
        // 上一个 shell 进程仍残留时（交互式 shell 会忽略 SIGTERM，导致状态未清理）
        // 先整组 SIGKILL 并等它退干净，再启动新 shell —— Rust 侧 `start` 对
        // 「还在跑」会直接报 `pty_already_running`，不先清掉就永远起不来。
        if (isRunning) {
            try {
                session.kill()
            } catch (_: Exception) {
            }
            for (i in 0 until 40) {
                if (!isRunning) break
                try {
                    Thread.sleep(50)
                } catch (_: Exception) {
                }
            }
        }
        if (isRunning) return  // 清理超时，放弃启动

        val isProot = shellId != null && shellId.startsWith("proot:")
        val argv: List<String>
        val env: Map<String, String>
        val label: String
        val ptyCwd: String

        if (isProot) {
            val rootfsId = shellId!!.removePrefix("proot:")
            // proot 的 cwd 是容器内路径，不能复用 host 的 workDir
            val guestCwd = cwd?.takeIf { it.isNotBlank() } ?: "/root"
            val prootCmd = ProotCommandBuilder.buildShellCommand(appContext, rootfsId, guestCwd)
            argv = prootCmd.argv
            env = prootCmd.env
            label = "proot: $rootfsId"
            // cwd 是 host 进程的 cwd；proot 通过 --cwd 设置容器内 cwd，
            // host cwd 用 app 私有目录即可（避免权限问题）。
            ptyCwd = appContext.filesDir.absolutePath
        } else {
            val workDir = cwd?.takeIf { File(it).isDirectory } ?: ShellResolver.defaultCwd(appContext)
            val spec = ShellResolver.resolveInteractive()
            env = ShellResolver.baseEnv(appContext, workDir)
            argv = listOf(spec.cmd) + spec.argvPrefix
            label = spec.label
            ptyCwd = workDir
        }

        // 先落 label：`session.start` 的尾部会广播一次 state，
        // 慢一步的话那条事件带的还是上一轮的 label。
        currentLabel = label
        session.start(
            argv = argv.toTypedArray(),
            envp = env.map { "${it.key}=${it.value}" }.toTypedArray(),
            cwd = ptyCwd,
            rows = lastRows,
            cols = lastCols,
            cellW = lastCellW,
            cellH = lastCellH,
            initialPhase = "running",
        )
        // 进历史 → 回放时也能看到；`session.notice` 自带 \r\n 换行
        session.notice("[EdgeCube] shell: $label")
    }

    /** 向 shell PTY 写入原始按键字节（来自 xterm 终端的直接输入）。 */
    fun writeInput(bytes: ByteArray) {
        try {
            session.write(bytes)
        } catch (_: Exception) {
        }
    }

    /** 向 shell PTY 写入一行命令（自动补换行）。 */
    fun sendCommand(line: String) {
        writeInput((line + "\n").toByteArray(Charsets.UTF_8))
    }

    /** 界面终端尺寸变化时同步 PTY 窗口大小。 */
    fun resize(rows: Int, cols: Int, cellWidth: Int, cellHeight: Int) {
        if (rows <= 0 || cols <= 0) return
        lastRows = rows
        lastCols = cols
        if (cellWidth > 0) lastCellW = cellWidth
        if (cellHeight > 0) lastCellH = cellHeight
        try {
            session.resize(rows, cols, lastCellW, lastCellH)
        } catch (_: Exception) {
        }
    }

    /** 开关 PTY 回显（交互 shell 一般保持开启，由 tty/shell 自身回显）。 */
    fun setEcho(echo: Boolean) {
        try {
            session.setEcho(echo)
        } catch (_: Exception) {
        }
    }

    /** 优雅退出：向 shell 发送 exit。 */
    fun stop() {
        try {
            session.stop("exit", "\n")
        } catch (_: Exception) {
            // 没在跑（NotRunning）/ 没有停止命令（NoStopCommand）都不必报
        }
    }

    /** 强制结束 shell 进程（整组 SIGKILL）。 */
    fun forceStop() {
        try {
            session.kill()
        } catch (_: Exception) {
        }
    }

    /** 清空输出历史（与界面清屏同步）。 */
    fun clearLog() {
        session.clearHistory()
    }
}
