use jni::JNIEnv;
use jni::objects::{JObject, JString};
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jint};

use crate::jni_util::{jstr, throw_java};

/// `FtpServerManager.start(rootDir, port, username, password, writable, ipv6Enabled)`
///
/// 参数缺失抛 `IllegalArgumentException`；服务层错误抛
/// `IllegalStateException`（消息逐字透传，FtpChannel 均转为
/// `FTP_START_FAILED` + e.message）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_venti1112_edgecube_ftp_FtpBridge_nativeFtpStart<'local>(
    mut env: JNIEnv<'local>,
    _thiz: JObject<'local>,
    root_dir: JString<'local>,
    port: jint,
    username: JString<'local>,
    password: JString<'local>,
    writable: jboolean,
    ipv6_enabled: jboolean,
) {
    let (Some(root_dir), Some(username), Some(password)) = (
        jstr(&mut env, &root_dir),
        jstr(&mut env, &username),
        jstr(&mut env, &password),
    ) else {
        throw_java(
            &mut env,
            "java/lang/IllegalArgumentException",
            "缺少 rootDir/username/password",
        );
        return;
    };
    if let Err(message) = super::start(
        &root_dir,
        port,
        &username,
        &password,
        writable != JNI_FALSE,
        ipv6_enabled != JNI_FALSE,
    ) {
        throw_java(&mut env, "java/lang/IllegalStateException", &message);
    }
}

/// `FtpServerManager.stop()`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_venti1112_edgecube_ftp_FtpBridge_nativeFtpStop<'local>(
    _env: JNIEnv<'local>,
    _thiz: JObject<'local>,
) {
    super::stop();
}

/// `FtpServerManager.isRunning: Boolean`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_venti1112_edgecube_ftp_FtpBridge_nativeFtpIsRunning<'local>(
    _env: JNIEnv<'local>,
    _thiz: JObject<'local>,
) -> jboolean {
    if super::is_running() {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}
