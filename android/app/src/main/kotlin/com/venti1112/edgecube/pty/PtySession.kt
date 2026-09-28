package com.venti1112.edgecube.pty

import android.os.Handler
import android.os.Looper
import io.flutter.plugin.common.EventChannel
import java.util.concurrent.atomic.AtomicInteger
import org.json.JSONObject

class PtySession(
    private val label: String,
    private val stateFields: () -> Map<String, Any?> = { emptyMap() },
    /** 实时（非回放）字节到达 —— 行组装 / `DONE_PATTERN` 在这里跑。 */
    private val onLiveBytes: (ByteArray) -> Unit = {},
    /** 输出读到头 —— 行组装器在这里冲刷最后一段没有换行的半行。 */
    private val onEof: () -> Unit = {},
    private val onExited: (exitCode: Int?) -> Unit = {},
) {
    companion object {
        private const val MAX_IN_FLIGHT = 64
    }

    private val mainHandler = Handler(Looper.getMainLooper())
    private val inFlight = AtomicInteger(0)

    @Volatile private var eventSink: EventChannel.EventSink? = null
    private var uiSubId: Long = 0L
    private val collectorSubId: Long

    val handle: Long = PtyBridge.nativeCreate(label)

    /** 当前状态；句柄失效时 `phase` 为空串。 */
    val info: JSONObject get() = JSONObject(PtyBridge.nativeInfo(handle))

    /** 进程是否还活着（含过渡态）。 */
    val isRunning: Boolean
        get() = when (info.optString("phase")) {
            "stopped", "crashed", "" -> false
            else -> true
        }

    /** 子进程 pid；没在跑返回 -1。 */
    val pid: Int get() = if (info.isNull("pid")) -1 else info.optInt("pid", -1)

    // ──────────────────────────────────────────────────────────────────────
    // 订阅
    // ──────────────────────────────────────────────────────────────────────

    /**
     * 设置/解除 UI 事件接收端。挂上时**重新订阅以触发回放**，使重建/被回收后
     * 重连的界面恢复画面 —— 回放全部在 Rust 的 broadcast 锁内同步完成，
     * 因此「历史」与「回放之后的实时帧」绝不会交错（缺陷 A1 的根因）。
     *
     * 解除时退订 UI，但 **collector 留着**。
     */
    @Synchronized
    fun setEventSink(sink: EventChannel.EventSink?) {
        if (uiSubId != 0L) {
            PtyBridge.nativeUnsubscribe(handle, uiSubId)
            uiSubId = 0L
        }
        eventSink = sink
        if (sink == null) return
        uiSubId = PtyBridge.nativeSubscribe(handle, uiListener, true)
    }

    /**
     * 自带监听器的订阅（SSH 反向 shell 用）。
     *
     * @param withHistory 通常传 `false`：SSH 没有终端 UI 要恢复，回放一帧 JSON
     *   会被当终端数据打给客户端。
     * @return sub id（0 = 失败），用完记得 [unsubscribe]
     */
    fun subscribe(listener: FrameListener, withHistory: Boolean): Long =
        PtyBridge.nativeSubscribe(handle, listener, withHistory)

    fun unsubscribe(subId: Long) = PtyBridge.nativeUnsubscribe(handle, subId)

    // ──────────────────────────────────────────────────────────────────────
    // 生命周期 / I/O 转发
    // ──────────────────────────────────────────────────────────────────────

    fun start(
        argv: Array<String>,
        envp: Array<String>,
        cwd: String,
        rows: Int,
        cols: Int,
        cellW: Int,
        cellH: Int,
        initialPhase: String,
        autoRestart: Boolean = false,
    ): JSONObject = PtyBridge.nativeStart(
        handle, argv, envp, cwd, rows, cols, cellW, cellH, initialPhase, autoRestart,
    ).ptyUnwrap()

    /** 优雅停止；空 [stopCommand] 抛 [PtyException]（`pty_stop_command_missing`）。 */
    fun stop(stopCommand: String, lineEnding: String): JSONObject =
        PtyBridge.nativeStop(handle, stopCommand, lineEnding).ptyUnwrap()

    /** 强制停止（整组 SIGKILL）。 */
    fun kill(): JSONObject = PtyBridge.nativeKill(handle).ptyUnwrap()

    /**
     * 前端匹配到就绪标记后调：`starting` → `running`。
     *
     * **调用方必须先 post 到别的线程**，绝不能在 `onFrame` 里同步调
     * （见类注释的死锁说明）。
     */
    fun notifyReady() = PtyBridge.nativeNotifyReady(handle)

    /** 静默收掉进程（不改阶段、不发通知）。 */
    fun shutdownQuiet() = PtyBridge.nativeShutdownQuiet(handle)

    /** @return 是否真的写进去了 */
    fun write(bytes: ByteArray): Boolean = PtyBridge.nativeWrite(handle, bytes) == 0

    fun resize(rows: Int, cols: Int, cellW: Int, cellH: Int): Boolean =
        PtyBridge.nativeResize(handle, rows, cols, cellW, cellH) == 0

    fun setEcho(echo: Boolean): Boolean = PtyBridge.nativeSetEcho(handle, echo) == 0

    fun clearHistory() = PtyBridge.nativeClearHistory(handle)

    fun notice(text: String) = PtyBridge.nativeNotice(handle, text)

    fun destroy() {
        setEventSink(null)
        PtyBridge.nativeUnsubscribe(handle, collectorSubId)
        PtyBridge.nativeDestroy(handle)
    }

    // ──────────────────────────────────────────────────────────────────────
    // 内部
    // ──────────────────────────────────────────────────────────────────────

    /** 常驻订阅者：只做行组装与 EOF 冲刷，**从不往 UI 发东西**。 */
    private val collector = object : FrameListener {
        override fun onFrame(kind: Int, bytes: ByteArray?, json: String?) {
            when (kind) {
                FrameListener.KIND_DATA -> if (bytes != null) {
                    try {
                        onLiveBytes(bytes)
                    } catch (_: Throwable) {
                        // 组装炸了不能连带搞死 PTY 读线程
                    }
                }

                FrameListener.KIND_CONTROL ->
                    if (json != null) {
                        if (json.contains("\"type\":\"output_eof\"")) {
                            try {
                                onEof()
                            } catch (_: Throwable) {
                            }
                        } else {
                            maybeExited(json)
                        }
                    }
            }
        }
    }

    /**
     * `state` 载荷里 `phase` 是 `stopped` / `crashed` 就是终局，把收尾搬回主线程。
     *
     * 放在 collector 而不是 UI 订阅者里：**没有界面时也得撤 Service、清快照**
     */
    private fun maybeExited(json: String) {
        if (!json.contains("\"phase\"")) return
        val o = try {
            JSONObject(json)
        } catch (_: Throwable) {
            return
        }
        when (o.optString("phase")) {
            "stopped", "crashed" -> {
                val code = if (o.isNull("exitCode")) null else o.optInt("exitCode")
                mainHandler.post {
                    try {
                        onExited(code)
                    } catch (_: Throwable) {
                    }
                }
            }
        }
    }

    /** UI 订阅者：把帧搬到主线程。 */
    private val uiListener = object : FrameListener {
        override fun onFrame(kind: Int, bytes: ByteArray?, json: String?) {
            when (kind) {
                FrameListener.KIND_REPLAY_BEGIN ->
                    post(mapOf("type" to "historyBegin"), droppable = false)

                FrameListener.KIND_DATA -> {
                    val data = bytes ?: return
                    post(mapOf("type" to "term", "bytes" to data), droppable = true)
                }

                FrameListener.KIND_REPLAY_END -> {
                    // 两条：先状态（复用 Dart 现有的 state 解析），再回放收尾。
                    // 两条都非可丢，且都在同一把 broadcast 锁里同步 post，
                    // 主线程队列里顺序即此顺序，不会与随后的实时帧穿插。
                    post(buildState(json, forReplay = true), droppable = false)
                    post(mapOf("type" to "historyEnd"), droppable = false)
                }

                FrameListener.KIND_CONTROL -> handleControl(json)
            }
        }
    }

    init {
        // collector 永不退订：行组装必须与「有没有界面」无关。
        collectorSubId = PtyBridge.nativeSubscribe(handle, collector, false)
    }

    private fun handleControl(json: String?) {
        if (json.isNullOrEmpty()) return
        val type = try {
            JSONObject(json).optString("type")
        } catch (_: Throwable) {
            ""
        }
        if (type == "state") post(buildState(json), droppable = false)
        // "exit" / "output_eof"：前者被 state 覆盖、后者只给 collector
    }

    private fun post(event: Map<String, Any?>, droppable: Boolean) {
        val sink = eventSink ?: return
        if (droppable && inFlight.get() >= MAX_IN_FLIGHT) return
        inFlight.incrementAndGet()
        mainHandler.post {
            try {
                sink.success(event)
            } catch (_: Throwable) {
                // 引擎被回收时 EventChannel 会抛 —— 丢这一条，别让读线程挂掉
            } finally {
                inFlight.decrementAndGet()
            }
        }
    }


    private fun buildState(stateJson: String?, forReplay: Boolean = false): Map<String, Any?> {
        val o = try {
            JSONObject(stateJson ?: "{}")
        } catch (_: Throwable) {
            JSONObject()
        }
        val phase = o.optString("phase", "")
        val exitCode = if (o.isNull("exitCode")) null else o.optInt("exitCode")
        val status: String? = when (phase) {
            "preparing", "starting", "running" -> phase
            "stopping" -> "running"
            else -> null // stopped / crashed / 未知
        }
        val map = HashMap<String, Any?>()
        map["type"] = "state"
        map["status"] = status // null 表示已退出
        if (exitCode != null && !forReplay) map["exitCode"] = exitCode
        map.putAll(stateFields())
        return map
    }
}
