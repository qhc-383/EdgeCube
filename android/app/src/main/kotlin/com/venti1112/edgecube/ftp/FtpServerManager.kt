package com.venti1112.edgecube.ftp

/**
 * FTP 服务器管理器
 */
object FtpServerManager {

    /** FTP 服务是否正在运行。 */
    val isRunning: Boolean
        get() = FtpBridge.nativeFtpIsRunning()

    /**
     * 启动 FTP 服务。
     *
     * @param rootDir FTP 根目录（客户端只能访问此目录内的文件）。
     * @param port 监听端口。
     * @param username 登录用户名（空则启用匿名访问）。
     * @param password 登录密码（匿名访问时忽略）。
     * @param writable 是否允许写入（上传/删除/重命名）。
     * @param ipv6Enabled 是否启用 IPv6（双栈）监听；关闭时仅监听 IPv4。
     */
    @Synchronized
    fun start(rootDir: String, port: Int, username: String, password: String, writable: Boolean, ipv6Enabled: Boolean) {
        FtpBridge.nativeFtpStart(rootDir, port, username, password, writable, ipv6Enabled)
    }

    /** 停止 FTP 服务。 */
    @Synchronized
    fun stop() {
        FtpBridge.nativeFtpStop()
    }
}
