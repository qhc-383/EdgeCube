package com.venti1112.edgecube.ssh

import android.content.Context
import com.venti1112.edgecube.shell.ShellResolver
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

/**
 * SSH 服务器管理器
 */
object SshServerManager {

    /** SSH 服务是否正在运行。 */
    val isRunning: Boolean
        get() = SshBridge.nativeSshIsRunning()

    /**
     * 启动 SSH 服务。
     *
     * @param context 应用上下文（用于主机密钥路径与 shell 环境构造）。
     * @param rootDir SFTP 根目录与 SSH 终端初始工作目录（客户端 SFTP 只能访问此目录内）。
     * @param port 监听端口（默认建议 2222；<1024 在非 root Android 无法绑定）。
     * @param username 登录用户名（不可为空）。
     * @param password 登录密码（不可为空）。
     * @param writable 是否允许 SFTP 写入（上传/删除/重命名）；仅作用于 SFTP，不限制 SSH 终端。
     * @param sftpEnabled 是否启用 SFTP 文件访问。
     * @param shellEnabled 是否启用 SSH 终端。
     * @param ipv6Enabled 是否启用 IPv6（双栈）监听；关闭时仅监听 IPv4。
     */
    @Synchronized
    fun start(
        context: Context,
        rootDir: String,
        port: Int,
        username: String,
        password: String,
        writable: Boolean,
        sftpEnabled: Boolean,
        shellEnabled: Boolean,
        ipv6Enabled: Boolean,
    ) {
        // 校验保留在此（IllegalArgumentException 语义与原实现一致）；
        // 其余运行期错误（已在运行、绑定失败、shell 校验…）由 Rust 抛出并逐字透传。
        require(sftpEnabled || shellEnabled) { "SFTP 与 SSH 终端至少需启用其一" }
        require(username.isNotBlank() && password.isNotBlank()) { "SSH 服务要求设置用户名与密码" }

        val appContext = context.applicationContext
        val root = File(rootDir)
        if (!root.isDirectory) root.mkdirs()

        // 终端 shell 与初始工作目录：当前实例目录无效则回退到默认目录。
        val spec = ShellResolver.resolveInteractive()
        val cwd = rootDir.takeIf { File(it).isDirectory } ?: ShellResolver.defaultCwd(appContext)

        val env = JSONObject()
        for ((k, v) in ShellResolver.baseEnv(appContext, cwd)) {
            env.put(k, v)
        }
        val argv = JSONArray()
        argv.put(spec.cmd)
        for (arg in spec.argvPrefix) argv.put(arg)

        val payload = JSONObject().apply {
            put("rootDir", rootDir)
            put("port", port)
            put("username", username)
            put("password", password)
            put("writable", writable)
            put("sftpEnabled", sftpEnabled)
            put("shellEnabled", shellEnabled)
            put("ipv6Enabled", ipv6Enabled)
            put("hostKeyPath", File(appContext.filesDir, "ssh/hostkey").absolutePath)
            put("shellArgv", argv)
            put("shellCwd", cwd)
            put("env", env)
        }
        SshBridge.nativeSshStart(payload.toString())
    }

    /** 停止 SSH 服务。 */
    @Synchronized
    fun stop() {
        SshBridge.nativeSshStop()
    }

    /**
     * 返回 SSH 主机密钥的 SHA-256 指纹（OpenSSH 形式 `SHA256:...`），供页面展示以便首次连接核对。
     * 若主机密钥尚不存在会先生成并落盘（与服务启动时使用的是同一密钥）。失败时返回 null。
     */
    fun hostKeyFingerprint(context: Context): String? {
        return try {
            val keyFile = File(context.applicationContext.filesDir, "ssh/hostkey")
            SshBridge.nativeSshHostKeyFingerprint(keyFile.absolutePath)
        } catch (_: Exception) {
            null
        }
    }
}
