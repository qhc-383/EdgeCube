use jni::JNIEnv;
use jni::objects::{JObject, JString};
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean};

use crate::jni_util::{jstr, throw_java};

/// `SshServerManager.start(...)` 的 JSON 载荷入口。
///
/// 载荷由 Kotlin `SshServerManager.start` 组装（rootDir/port/凭据/开关/
/// hostKeyPath/shellArgv/shellCwd/env），缺失字段与服务层错误均抛
/// `IllegalStateException`。
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_venti1112_edgecube_ssh_SshBridge_nativeSshStart<'local>(
    mut env: JNIEnv<'local>,
    _thiz: JObject<'local>,
    config_json: JString<'local>,
) {
    let Some(json) = jstr(&mut env, &config_json) else {
        throw_java(
            &mut env,
            "java/lang/IllegalArgumentException",
            "缺少 configJson",
        );
        return;
    };
    if let Err(message) = super::start(&json) {
        throw_java(&mut env, "java/lang/IllegalStateException", &message);
    }
}

/// `SshServerManager.stop()`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_venti1112_edgecube_ssh_SshBridge_nativeSshStop<'local>(
    _env: JNIEnv<'local>,
    _thiz: JObject<'local>,
) {
    super::stop();
}

/// `SshServerManager.isRunning: Boolean`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_venti1112_edgecube_ssh_SshBridge_nativeSshIsRunning<'local>(
    _env: JNIEnv<'local>,
    _thiz: JObject<'local>,
) -> jboolean {
    if super::is_running() {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

/// `SshServerManager.hostKeyFingerprint(hostKeyPath): String?`
///
/// 密钥文件不存在时先生成再算指纹；任何失败返回 null。
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_venti1112_edgecube_ssh_SshBridge_nativeSshHostKeyFingerprint<
    'local,
>(
    mut env: JNIEnv<'local>,
    _thiz: JObject<'local>,
    host_key_path: JString<'local>,
) -> JString<'local> {
    let Some(path) = jstr(&mut env, &host_key_path) else {
        return JObject::null().into();
    };
    match super::keys::fingerprint(std::path::Path::new(&path)) {
        Ok(fp) => env
            .new_string(fp)
            .unwrap_or_else(|_| JObject::null().into()),
        Err(_) => JObject::null().into(),
    }
}
