package com.venti1112.edgecube.server

import android.content.Context
import android.os.Handler
import android.os.Looper
import com.venti1112.edgecube.keepalive.KeepAlivePrefs
import com.venti1112.edgecube.pty.PtySession
import com.venti1112.edgecube.widget.WidgetUpdater
import io.flutter.plugin.common.EventChannel
import java.io.ByteArrayOutputStream
import java.io.File
import java.nio.charset.StandardCharsets

/**
 * 把服务端 JVM 作为独立子进程拉起，并管理其生命周期。应用级单例，不绑定 Activity——
 * 这样 Activity 因切后台被销毁重建时，进程与日志仍在。
 *
 * 关键点：
 *  - 通过 [PtySession]（Rust `portable-pty`）创建伪终端：服务端进程跑在真实 TTY 上
 *    （fork+ execvp liblaunch.so，由它 dlopen libjli.so 启动 JVM），故支持 Tab 补全、
 *    命令历史、JLine 控制台与原生 ANSI 着色。**fd / 流 / 读线程 / 输出回放全在 Rust 侧**。
 *  - 启动命令（java/php/proot 三种运行时）由 [ServerLauncher] 构造。
 *  - PTY 读出的字节有两个去向：① 作为 `term` 事件原样给 Flutter 的 xterm.dart 渲染；
 *    ② 在常驻 collector 订阅者的 [assembleLines] 里按行去 ANSI，作为 `log` 事件喂给
 *    既有的状态识别 / 玩家解析 / 崩溃检测逻辑。
 *  - 进程级隔离：服务端崩溃不影响应用；可独立 kill。
 *  - 启动时拉起前台 [ServerService] 保活，进程终局（[onExited]）时撤下。
 *  - 维护清洗日志行缓冲（`log` 回放用）；终端画面回放由 Rust 的输出历史负责。
 */
class ServerProcessManager private constructor(private val appContext: Context) {

    companion object {
        private const val MAX_LOG_LINES = 2000

        /** 单行未遇换行时的最大累积字节，超出强制断行，防止异常输出撑爆内存。 */
        private const val MAX_LINE_BYTES = 16 * 1024

        const val STATUS_PREPARING = "preparing"
        const val STATUS_STARTING = "starting"
        const val STATUS_RUNNING = "running"

        // —— PTY 初始窗口尺寸；界面布局完成后会通过 resize() 校正为真实值。 ——
        private const val DEFAULT_ROWS = 24
        private const val DEFAULT_COLS = 80
        private const val DEFAULT_CELL_W = 8
        private const val DEFAULT_CELL_H = 16

        /** 服务端初始化完成标志：匹配
         *  - 英文 "Done (Xs)!"（Velocity/Paper）
         *  - 中文 "启动完成 (Xs)"（Paper 中文）
         *  - 中文 "加载完成 (X 秒)！"（PocketMine-MP）
         *  - "Network interface started at <ip>:<port> [(... )] (<ms> ms)"（Allay，
         *    IPv4-only 与 IPv6 双栈两种变体的公共前缀与尾缀）。 */
        private val DONE_PATTERN =
            Regex("""Done\s*\([0-9.]+s\)!|启动完成\s*\([0-9.]+s\)|加载完成\s*\([0-9.]+\s*秒\)！?|Network interface started at .+\([0-9]+\s*ms\)""")

        /** 日志中的 ANSI 转义序列（CSI，如颜色码 \x1B[36m）；按行解析前剔除。 */
        private val ANSI_PATTERN = Regex("\\x1B\\[[0-?]*[ -/]*[@-~]")

        @Volatile
        private var instance: ServerProcessManager? = null

        fun getInstance(context: Context): ServerProcessManager =
            instance ?: synchronized(this) {
                instance ?: ServerProcessManager(context.applicationContext).also { instance = it }
            }
    }

    private val mainHandler = Handler(Looper.getMainLooper())

    // —— 清洗后的日志行缓冲（供复制/解析/回放）——
    private val logLock = Any()
    private val logBuffer = ArrayDeque<String>()

    // —— 按行组装 term 原始字节为清洗后的日志行 ——
    private val lineAssembler = ByteArrayOutputStream()

    @Volatile private var runningInstanceId: String? = null
    @Volatile private var runningInstanceName: String? = null
    @Volatile private var runningRuntimeId: String? = null
    @Volatile private var currentStatus: String? = null
    /** 当前 UI 事件接收端；只用于发 `log` 事件（终端画面走 `PtySession` 自己的 sink）。 */
    @Volatile private var eventSink: EventChannel.EventSink? = null
    /** 在线玩家数；由 Dart 侧 ServerController 通过 widget 通道上报，供小组件渲染。 */
    @Volatile private var playerCount: Int = 0

    /** 本次启动是否为 proot 模式（顶层进程是 proot，实际服务端在其子进程中）。 */
    @Volatile private var runningIsProot: Boolean = false
    /** 本次启动解析到的 JVM 最大堆（-Xmx，MB）；未配置为 -1。 */
    @Volatile private var runningMaxHeapMb: Long = -1
    /** 命令尾换行符，默认 Linux 风格 "\n"；Windows 风格为 "\r\n"。 */
    @Volatile private var lineEnding: String = "\n"

    // —— 最近一次由界面上报的终端尺寸；新进程沿用，避免一闪一变。 ——
    @Volatile private var lastRows = DEFAULT_ROWS
    @Volatile private var lastCols = DEFAULT_COLS
    @Volatile private var lastCellW = DEFAULT_CELL_W
    @Volatile private var lastCellH = DEFAULT_CELL_H

    private val session = PtySession(
        label = "server",
        stateFields = {
            mapOf(
                "instanceId" to runningInstanceId,
                "instanceName" to runningInstanceName,
            )
        },
        onLiveBytes = { assembleLines(it) },
        onEof = { flushAssembler() },
        onExited = { code -> onProcessExited(code) },
    )

    // 在线玩家数 setter：Dart 侧每秒解析日志后调用；同步落入 prefs 快照。
    fun setPlayerCount(count: Int) {
        playerCount = count.coerceAtLeast(0)
        KeepAlivePrefs.putWidgetSnapshot(appContext, playerCount = playerCount)
        WidgetUpdater.requestUpdate(appContext)
    }

    /** 在线玩家数 getter（仅供 AAppWidgetProvider 渲染时读取)。 */
    fun getPlayerCount(): Int = playerCount

    val isRunning: Boolean get() = session.isRunning
    val activeInstanceId: String? get() = runningInstanceId

    /** 当前正在使用的运行时 id（如 "jre21"/"php8.2"）；未运行时返回 null。 */
    val activeRuntimeId: String? get() = runningRuntimeId

    /** 子进程 PID；未运行时返回 -1。 */
    val pid: Int get() = if (isRunning) session.pid else -1

    /** 当前启动是否为 proot 模式（顶层进程是 proot，服务端在其子进程中）。 */
    val isProotLaunch: Boolean get() = isRunning && runningIsProot

    /** 本次启动的 JVM 最大堆（-Xmx，MB）；未运行或未配置返回 -1。 */
    val maxHeapMb: Long get() = if (isRunning) runningMaxHeapMb else -1

    /**
     * 设置/解除事件接收端。设置时由 [PtySession] 回放终端历史与当前状态，
     * 这里再补一段清洗日志行的回放（`log` 事件，供日志页/崩溃报告用），
     * 使重建/被回收后重连的界面能恢复终端画面与正在运行的实例。
     */
    fun setEventSink(sink: EventChannel.EventSink?) {
        eventSink = sink
        if (sink != null) {
            val logSnapshot = synchronized(logLock) { logBuffer.toList() }
            mainHandler.post {
                for (line in logSnapshot) {
                    sink.success(mapOf("type" to "log", "line" to line))
                }
            }
        }
        session.setEventSink(sink)
    }

    /**
     * 启动服务端。含首次运行时（JRE / PHP）解压，耗时，必须在后台线程调用。
     *
     * [runtime] 为 "java"（默认）/ "php" / "proot"，具体命令由 [ServerLauncher.build]
     * 构造，进程拉起由 Rust 侧 `nativeStart` 完成，stdio 改为 PTY 从设备，
     * 因此进程认为自己连着真实终端。
     *
     * @throws com.venti1112.edgecube.pty.PtyException 拉起失败（`pty_*` 稳定码）
     */
    @Synchronized
    fun start(
        instanceId: String,
        instanceName: String,
        workingDir: String,
        runtimeId: String,
        runtime: String,
        runtimeArgs: List<String>,
        programArgs: List<String>,
        directExecute: Boolean = false,
        lineEnding: String = "\n",
    ) {
        if (isRunning) throw IllegalStateException("已有服务端正在运行，请先停止")

        // 重置上一次运行的 proot 标记（防止误读旧值）
        runningIsProot = false
        // 存储命令尾换行符。
        this.lineEnding = lineEnding
        // 解析 -Xmx（如 -Xmx2048M / -Xmx2G），供服务端内存占用率计算。
        runningMaxHeapMb = ServerLauncher.parseXmxMb(runtimeArgs)

        val work = File(workingDir)
        if (!work.isDirectory) throw IllegalStateException("工作目录不存在：$workingDir")

        // 清理上一次运行残留（server.lock、PMMP phar 缓存）。
        ServerLauncher.prepareWorkingDir(appContext, work, runtime)

        val spec = ServerLauncher.build(
            appContext, workingDir, runtimeId, runtime,
            runtimeArgs, programArgs, directExecute, ::emitNotice,
        )
        runningIsProot = spec.isProot

        // 新进程开始，重置按行组装器，避免上次的残留半行拼接进来。
        synchronized(lineAssembler) { lineAssembler.reset() }

        // 上下文要在 `session.start` **之前**就位：它尾部会广播一次 state，
        // 那条事件的 `stateFields()` 得读到这一轮的 instanceId/Name。
        runningInstanceId = instanceId
        runningInstanceName = instanceName
        runningRuntimeId = runtimeId
        currentStatus = STATUS_STARTING

        // 缓存最近一次启动参数，供桌面小组件「启动」按钮异步拉起。
        // 用 ␟ 分隔在 SharedPreferences 中存储字符串数组（NUL 在 prefs 受限）。
        KeepAlivePrefs.putLastStartArgs(
            appContext,
            instanceId = instanceId,
            instanceName = instanceName,
            workingDir = workingDir,
            runtimeId = runtimeId,
            runtime = runtime,
            runtimeArgs = runtimeArgs,
            programArgs = programArgs,
            compatMode = false, // Dart 侧负责；这里是兜底，无需 compat 信息
            directExecute = directExecute,
            lineEnding = lineEnding,
        )

        playerCount = 0

        val started = session.start(
            argv = spec.argv.toTypedArray(),
            envp = spec.envp(),
            cwd = workingDir,
            rows = lastRows,
            cols = lastCols,
            cellW = lastCellW,
            cellH = lastCellH,
            initialPhase = STATUS_STARTING,
        )

        KeepAlivePrefs.putWidgetSnapshot(
            appContext,
            status = STATUS_STARTING,
            instanceName = instanceName,
            playerCount = 0,
            // 记录服务端 PID：App 进程被杀后重启时凭它校验服务端是否还在运行，
            // 避免小组件停留在「运行中」而实际进程早已消亡。
            pid = started.optJSONObject("info")?.optInt("pid", -1) ?: -1,
        )
        WidgetUpdater.requestUpdate(appContext)

        // 拉起前台 Service 保活。
        ServerService.start(appContext, instanceName)
    }

    /** 向服务端 PTY 写入原始按键字节（来自 xterm 终端的直接输入）。 */
    fun writeInput(bytes: ByteArray) {
        session.write(bytes)
    }

    /** 向服务端 PTY 写入一行命令（自动补 lineEnding 换行）。供程序化发送使用。 */
    fun sendCommand(line: String) {
        session.write((line + lineEnding).toByteArray(StandardCharsets.UTF_8))
    }

    /** 界面终端尺寸变化时调用，同步 PTY 窗口大小，连接的程序会据此重排。 */
    fun resize(rows: Int, cols: Int, cellWidth: Int, cellHeight: Int) {
        if (rows <= 0 || cols <= 0) return
        lastRows = rows
        lastCols = cols
        if (cellWidth > 0) lastCellW = cellWidth
        if (cellHeight > 0) lastCellH = cellHeight
        session.resize(rows, cols, lastCellW, lastCellH)
    }

    /** 开关 PTY 回显。命令行编辑模式关闭（App 自行回显），原始终端模式开启。 */
    fun setEcho(echo: Boolean) {
        session.setEcho(echo)
    }

    fun stop() {
        try {
            session.stop("stop", lineEnding)
        } catch (_: Exception) {
            // 没在跑（NotRunning）/ 没有停止命令（NoStopCommand）都不必报
        }
    }

    /**
     * 强制结束进程（整组 SIGKILL）。
     */
    fun forceStop() {
        try {
            session.kill()
        } catch (_: Exception) {
            emitNotice("[EdgeCube] 强制结束失败")
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // 桌面小组件专用：无 Flutter 引擎也能起停
    // ──────────────────────────────────────────────────────────────────────

    /**
     * 用 [KeepAlivePrefs] 缓存的最近一次启动参数重新拉起服务端。
     *
     * 供桌面小组件的「启动」按钮使用：这条路径不在 Flutter 引擎上下文，
     * 无法走 Dart 侧的 [ServerController.start]。本方法直接复用 [start] 的核心
     * 逻辑，参数来自用户最后一次在 App 内启动服务端时的缓存。
     *
     * 返回：
     * - `null` 表示已成功调用 [start]（实际启动结果仍异步通过 EventChannel
     *   返回，但 WidgetProvider 不会等待，只依赖后续的 [requestUpdate]）；
     * - 非空字符串表示失败原因（如未缓存过参数、目录不存在、已有服务端运行等）。
     */
    fun startFromWidgetSnapshot(): String? {
        if (isRunning) return "已有服务端正在运行"
        val args = KeepAlivePrefs.lastStartArgs(appContext)
        if (!args.isComplete) return "尚未启动过任何实例"
        val work = args.workingDir ?: return "工作目录缺失"
        if (!File(work).isDirectory) return "工作目录不存在：$work"
        val runtimeId = args.runtimeId ?: return "运行时 id 缺失"
        val instanceId = args.instanceId ?: return "实例 id 缺失"
        // 在后台线程同步执行；start 内部已 thread off。但 start 本身 @Synchronized
        // 含耗时操作（解压等），不能在主线程调用，因此这里需切换线程。
        Thread {
            try {
                start(
                    instanceId = instanceId,
                    instanceName = args.instanceName ?: instanceId,
                    workingDir = work,
                    runtimeId = runtimeId,
                    runtime = args.runtime,
                    runtimeArgs = args.runtimeArgs,
                    programArgs = args.programArgs,
                    directExecute = args.directExecute,
                    lineEnding = args.lineEnding,
                )
            } catch (e: Exception) {
                // 失败信息无处告知（无引擎），写入 prefs 状态以备下次 onUpdate 渲染。
                KeepAlivePrefs.putWidgetSnapshot(
                    appContext,
                    status = "stopped",
                    instanceName = args.instanceName ?: instanceId,
                )
                WidgetUpdater.requestUpdate(appContext)
            }
        }.start()
        return null
    }

    /** 桌面小组件调用的「停止」入口；与 [stop] 一致（发送 stop 命令）。 */
    fun stopFromWidget() {
        if (!isRunning) return
        stop()
    }

    /**
     * 由 [WidgetChannel] 转发 Dart 侧 ServerController 的公网地址（UPnP/STUN/DDNS）
     * 写入 prefs 快照，使小组件能在 onUpdate 时机读到最新的连接信息。
     */
    fun setPublicAddressForWidget(address: String?) {
        KeepAlivePrefs.putWidgetSnapshot(appContext, publicAddress = address)
        WidgetUpdater.requestUpdate(appContext)
    }

    /**
     * 由 [WidgetChannel] 转发 Dart 侧检测到的内网 IPv4 地址写入 prefs 快照；
     * 无公网地址时小组件用它兜底展示连接入口。
     */
    fun setLocalAddressForWidget(address: String?) {
        KeepAlivePrefs.putWidgetSnapshot(appContext, localAddress = address)
        WidgetUpdater.requestUpdate(appContext)
    }

    /** 由 [WidgetChannel] 转发 Dart 侧的 serverPort 写入 prefs 快照。 */
    fun setServerPortForWidget(port: Int) {
        KeepAlivePrefs.putWidgetSnapshot(appContext, serverPort = port)
        WidgetUpdater.requestUpdate(appContext)
    }

    /** 清空清洗日志行缓冲 + Rust 侧输出历史（与界面的清屏保持一致）。 */
    fun clearLog() {
        synchronized(logLock) { logBuffer.clear() }
        session.clearHistory()
    }

    // ──────────────────────────────────────────────────────────────────────
    // 内部：提示 / 按行组装 / 终局收尾
    // ──────────────────────────────────────────────────────────────────────

    /**
     * EdgeCube 自身的提示信息：这些不是 PTY 进程的输出，故须主动写进终端画面（term）
     * 才能被看到；同时进日志缓冲（log），让复制日志 / 崩溃报告也能包含它们。
     *
     * 两者现在由同一条路径达成：`session.notice` 进 Rust 历史 → 扇给 UI 订阅者
     * （`term`）与常驻 collector（`assembleLines` → `log`）。
     */
    private fun emitNotice(msg: String) {
        session.notice(msg)
    }

    /** 把原始字节按 '\n' 切成整行，逐行解码并交给 [emitLineFromAssembler]。 */
    private fun assembleLines(chunk: ByteArray) {
        synchronized(lineAssembler) {
            for (b in chunk) {
                if (b.toInt() == '\n'.code) {
                    emitLineFromAssembler()
                } else {
                    lineAssembler.write(b.toInt())
                    if (lineAssembler.size() >= MAX_LINE_BYTES) emitLineFromAssembler()
                }
            }
        }
    }

    /** 冲刷组装器中剩余的半行（输出 EOF 时由 `PtySession` 调）。 */
    private fun flushAssembler() {
        synchronized(lineAssembler) {
            if (lineAssembler.size() > 0) emitLineFromAssembler()
        }
    }

    /** 调用方须持有 lineAssembler 锁。 */
    private fun emitLineFromAssembler() {
        val raw = lineAssembler.toByteArray()
        lineAssembler.reset()
        // 解码 + 去 '\r' + 去 ANSI，得到用于解析/复制的纯文本行。
        val decoded = String(raw, StandardCharsets.UTF_8).replace("\r", "")
        val clean = ANSI_PATTERN.replace(decoded, "")
        emitLog(clean)
        if (currentStatus == STATUS_STARTING && DONE_PATTERN.containsMatchIn(clean)) {
            currentStatus = STATUS_RUNNING
            KeepAlivePrefs.putWidgetSnapshot(appContext, status = STATUS_RUNNING)
            WidgetUpdater.requestUpdate(appContext)
            // 推进 Rust 侧状态机（starting → running），由它广播 state 事件。
            // **必须 post**：本函数跑在持 Rust broadcast 锁的线程上，
            // 同步回调 Rust 就是同线程重入死锁。
            mainHandler.post {
                try {
                    session.notifyReady()
                } catch (_: Throwable) {
                }
            }
        }
    }

    /** 追加一条清洗后的日志行并下发 `log` 事件。 */
    private fun emitLog(line: String) {
        synchronized(logLock) {
            logBuffer.addLast(line)
            while (logBuffer.size > MAX_LOG_LINES) logBuffer.removeFirst()
        }
        val sink = eventSink ?: return
        mainHandler.post { sink.success(mapOf("type" to "log", "line" to line)) }
    }

    /** 进程走到终局：清实例上下文、撤前台 Service、刷小组件。 */
    private fun onProcessExited(exitCode: Int?) {
        runningInstanceId = null
        runningInstanceName = null
        runningRuntimeId = null
        runningIsProot = false
        currentStatus = null
        playerCount = 0
        KeepAlivePrefs.clearWidgetSnapshot(appContext)
        WidgetUpdater.requestUpdate(appContext)
        // 进程结束，撤下前台 Service。
        ServerService.stop(appContext)
    }
}
