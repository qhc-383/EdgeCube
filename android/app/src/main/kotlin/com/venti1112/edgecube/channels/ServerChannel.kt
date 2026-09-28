package com.venti1112.edgecube.channels

import android.content.Context
import com.venti1112.edgecube.pty.PtyException
import com.venti1112.edgecube.server.RuntimeInstaller
import com.venti1112.edgecube.server.ServerProcessManager
import io.flutter.plugin.common.BinaryMessenger
import io.flutter.plugin.common.EventChannel
import io.flutter.plugin.common.MethodChannel

/**
 * 服务端通道：启动/停止/命令下发/终端 I/O，事件流回放见
 * [ServerProcessManager.setEventSink]。
 */
internal object ServerChannel {

    private const val CHANNEL = "com.venti1112.edgecube/server"
    private const val EVENT_CHANNEL = "com.venti1112.edgecube/server_events"

    fun register(messenger: BinaryMessenger, context: Context) {
        val serverManager = ServerProcessManager.getInstance(context)

        MethodChannel(messenger, CHANNEL).setMethodCallHandler { call, result ->
            when (call.method) {
                "availableJreIds" ->
                    result.success(RuntimeInstaller.availableJreIds(context))

                "availablePhpRuntimes" ->
                    result.success(RuntimeInstaller.availablePhpIds(context))

                "isRuntimeReady" -> {
                    val runtimeId = call.argument<String>("runtimeId")
                    if (runtimeId == null) {
                        result.error("BAD_ARGS", "缺少 runtimeId", null)
                    } else {
                        result.success(RuntimeInstaller.isInstalled(context, runtimeId))
                    }
                }

                "isRunning" -> result.success(serverManager.isRunning)

                "activeInstanceId" -> result.success(serverManager.activeInstanceId)

                "start" -> {
                    val instanceId = call.argument<String>("instanceId")
                    val workingDir = call.argument<String>("workingDir")
                    val runtimeId = call.argument<String>("runtimeId")
                    val runtime = call.argument<String>("runtime") ?: "java"
                    val runtimeArgs = call.argument<List<String>>("runtimeArgs") ?: emptyList()
                    val programArgs = call.argument<List<String>>("programArgs") ?: emptyList()
                    val directExecute = call.argument<Boolean>("directExecute") ?: false
                    val lineEnding = call.argument<String>("lineEnding") ?: "\n"
                    if (instanceId == null || workingDir == null || runtimeId == null) {
                        result.error("BAD_ARGS", "缺少 instanceId/workingDir/runtimeId", null)
                    } else {
                        val instanceName = call.argument<String>("instanceName") ?: instanceId
                        // 含解压，放后台线程；完成后回主线程返回结果。
                        // PtyException 的 pty_* 稳定码原样透传（已有服务端在跑 →
                        // pty_already_running 等），别被笼统的 START_FAILED 盖掉。
                        ChannelIo.runAsync(
                            result,
                            "START_FAILED",
                            { e -> (e as? PtyException)?.code ?: "START_FAILED" },
                        ) {
                            serverManager.start(
                                instanceId, instanceName, workingDir, runtimeId,
                                runtime, runtimeArgs, programArgs, directExecute, lineEnding,
                            )
                            true
                        }
                    }
                }

                "sendCommand" -> {
                    serverManager.sendCommand(call.argument<String>("line") ?: "")
                    result.success(null)
                }

                "writeInput" -> {
                    val bytes = call.argument<ByteArray>("bytes")
                    if (bytes != null) serverManager.writeInput(bytes)
                    result.success(null)
                }

                "resize" -> {
                    val cols = call.argument<Int>("cols") ?: 0
                    val rows = call.argument<Int>("rows") ?: 0
                    val cellWidth = call.argument<Int>("cellWidth") ?: 0
                    val cellHeight = call.argument<Int>("cellHeight") ?: 0
                    serverManager.resize(rows, cols, cellWidth, cellHeight)
                    result.success(null)
                }

                "setEcho" -> {
                    val echo = call.argument<Boolean>("echo") ?: true
                    serverManager.setEcho(echo)
                    result.success(null)
                }

                "stop" -> pty(result, "STOP_FAILED") { serverManager.stop() }

                "forceStop" -> pty(result, "FORCE_STOP_FAILED") {
                    serverManager.forceStop()
                }

                "clearLog" -> {
                    serverManager.clearLog()
                    result.success(null)
                }

                else -> result.notImplemented()
            }
        }

        EventChannel(messenger, EVENT_CHANNEL).setStreamHandler(
            object : EventChannel.StreamHandler {
                override fun onListen(arguments: Any?, events: EventChannel.EventSink?) {
                    serverManager.setEventSink(events)
                }

                override fun onCancel(arguments: Any?) {
                    serverManager.setEventSink(null)
                }
            },
        )
    }

    /**
     * 同步跑一段会抛 [PtyException] 的 PTY 调用，把稳定错误码透传回 Flutter。
     *
     * 不包的话 `IllegalStateException` 会被 Flutter 的 `IncomingMethodCallHandler`
     * 兜成笼统的 `code = "error"` —— Dart 侧就再也分不出
     * `pty_not_running` / `pty_stop_command_missing` 这些可分支的码。
     */
    private fun pty(result: MethodChannel.Result, fallback: String, block: () -> Unit) {
        try {
            block()
            result.success(null)
        } catch (e: PtyException) {
            result.error(e.code, e.message, null)
        } catch (e: RuntimeException) {
            result.error(fallback, e.message, null)
        }
    }
}
