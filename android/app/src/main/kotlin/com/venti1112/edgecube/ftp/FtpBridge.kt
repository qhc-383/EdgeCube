package com.venti1112.edgecube.ftp

/**
 * ftp 功能 JNI 桥
 */
internal object FtpBridge {

    init {
        System.loadLibrary("edgecube_tools")
    }
    external fun nativeFtpStart(
        rootDir: String,
        port: Int,
        username: String,
        password: String,
        writable: Boolean,
        ipv6Enabled: Boolean,
    )

    external fun nativeFtpStop()

    external fun nativeFtpIsRunning(): Boolean
}
