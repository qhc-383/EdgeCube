package com.venti1112.edgecube.pty

import org.json.JSONObject

/**
 * PTY 桥的 native 面；实现见 `rust/edgecube-pty/src/bridge.rs`（`libedgecube_pty.so`）。
 *
 * Rust 侧**不抛异常**：所有可能失败的调用返回一个 JSON envelope
 * `{"ok":true,…}` 或 `{"ok":false,"code":"pty_*","message":…}`。
 * 这里负责把 envelope 翻成 [PtyException]（`code` 即 MethodChannel 的 errorCode），
 * 或者把成功载荷解出来。
 */
object PtyBridge {

    init {
        System.loadLibrary("edgecube_pty")
    }

    // ── 会话 ─────────────────────────────────────────────────────────────
    external fun nativeCreate(label: String): Long
    external fun nativeDestroy(handle: Long)

    // ── 生命周期 ─────────────────────────────────────────────────────────
    external fun nativeStart(
        handle: Long,
        argv: Array<String>,
        envp: Array<String>,
        cwd: String,
        rows: Int,
        cols: Int,
        cellW: Int,
        cellH: Int,
        initialPhase: String,
        autoRestart: Boolean,
    ): String

    external fun nativeStop(handle: Long, stopCommand: String, lineEnding: String): String
    external fun nativeKill(handle: Long): String

    /** 前端匹配到就绪标记后调：`starting` → `running`。Rust 自己不读那个标记。 */
    external fun nativeNotifyReady(handle: Long)

    /** 静默收掉进程（不改阶段、不发通知）。 */
    external fun nativeShutdownQuiet(handle: Long)

    // ── I/O ──────────────────────────────────────────────────────────────
    /** `0` = 成功，`1` = 没有在跑的进程。 */
    external fun nativeWrite(handle: Long, bytes: ByteArray): Int

    /** `0` = 成功，`1` = 没有在跑的进程。 */
    external fun nativeResize(handle: Long, rows: Int, cols: Int, cellW: Int, cellH: Int): Int

    /** `0` = 成功，`1` = 没有在跑的进程。 */
    external fun nativeSetEcho(handle: Long, echo: Boolean): Int

    // ── 订阅 ─────────────────────────────────────────────────────────────
    /** 返回 sub id（0 = 句柄/监听器无效）。 */
    external fun nativeSubscribe(handle: Long, listener: FrameListener, withHistory: Boolean): Long
    external fun nativeUnsubscribe(handle: Long, subId: Long)

    // ── 其它 ─────────────────────────────────────────────────────────────
    external fun nativeClearHistory(handle: Long)
    external fun nativeNotice(handle: Long, text: String)

    /** 当前状态 JSON（`{"phase":…,"pid":…,…}`）；句柄无效时是 `"{}"`。 */
    external fun nativeInfo(handle: Long): String
}

/**
 * Rust → Kotlin 的帧回调。**由 `broadcast.rs` 在持锁状态下同步调用**，
 * 所以实现里只能做「不阻塞、不取任何 Kotlin 锁」的事（典型：`mainHandler.post`）。
 *
 * 一旦在实现里去取别的锁，就会与「Rust 锁 → Kotlin 锁」的既有顺序构成
 * AB-BA 死锁。这条约束写在 `rust/edgecube-pty/src/frame.rs`。
 */
interface FrameListener {
    companion object {
        const val KIND_REPLAY_BEGIN = 0
        const val KIND_DATA = 1
        const val KIND_REPLAY_END = 2
        const val KIND_CONTROL = 3
    }

    /**
     * @param kind   [KIND_REPLAY_BEGIN] / [KIND_DATA] / [KIND_REPLAY_END] / [KIND_CONTROL]
     * @param bytes  仅 [KIND_DATA] 非空（原始终端字节，回放时是历史分片 ≤ 64 KB）
     * @param json   仅 [KIND_REPLAY_END]（状态载荷）与 [KIND_CONTROL]（`state`/`exit`）非空
     */
    fun onFrame(kind: Int, bytes: ByteArray?, json: String?)
}

/** PTY 操作失败；`code` 是 Rust 侧的稳定错误码，直接当 MethodChannel 的 errorCode。 */
class PtyException(val code: String, message: String) : IllegalStateException(message)

/**
 * 把 native 返回的 envelope 解开：
 * `{"ok":true,…}` → 内层 `JSONObject`；否则抛 [PtyException]。
 */
internal fun String.ptyUnwrap(): JSONObject {
    val obj = JSONObject(this)
    if (obj.optBoolean("ok", false)) return obj
    throw PtyException(
        obj.optString("code", "pty_error"),
        obj.optString("message", "PTY 操作失败"),
    )
}
