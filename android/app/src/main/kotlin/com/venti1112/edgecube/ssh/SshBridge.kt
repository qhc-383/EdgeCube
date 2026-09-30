package com.venti1112.edgecube.ssh

/**
 * ssh 功能 JNI 桥
 */
internal object SshBridge {

    init {
        System.loadLibrary("edgecube_tools")
    }

    /**
     * 启动 SSH 服务；已在运行抛 `IllegalStateException("SSH 服务已在运行")`，
     * 端口占用抛 `IllegalStateException("无法绑定端口 <port>：…")`。
     *
     * [configJson] 载荷字段：rootDir/port/username/password/writable/
     * sftpEnabled/shellEnabled/ipv6Enabled/hostKeyPath/shellArgv/shellCwd/env。
     */
    external fun nativeSshStart(configJson: String)

    /** 停止 SSH 服务（幂等；阻塞至端口释放，最多 5s）。 */
    external fun nativeSshStop()

    /** SSH 服务是否正在运行。 */
    external fun nativeSshIsRunning(): Boolean

    /**
     * 返回 SSH 主机密钥的 SHA-256 指纹（OpenSSH 形式 `SHA256:…`）。
     * 密钥不存在时先生成再算指纹（与服务启动用同一文件）；失败返回 null。
     */
    external fun nativeSshHostKeyFingerprint(hostKeyPath: String): String?
}
