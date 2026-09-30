//! JNI 共用辅助：异常投递与 Java 侧数据读取。

use jni::JNIEnv;
use jni::objects::{JObjectArray, JString};

/// 按类名抛出 Java 异常
pub fn throw_java(env: &mut JNIEnv, class: &str, msg: &str) {
    let _ = env.throw_new(class, msg);
}

/// 读取 `java.lang.String`；null / 解码失败返回 `None`。
pub fn jstr(env: &mut JNIEnv, s: &JString) -> Option<String> {
    env.get_string(s).ok().map(String::from)
}

/// 读取 `String[]`；任一元素失败整体返回 `None`。
pub fn jstr_vec(env: &mut JNIEnv, arr: &JObjectArray) -> Option<Vec<String>> {
    let n = env.get_array_length(arr).ok()?;
    let mut out = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n {
        let item = env.get_object_array_element(arr, i).ok()?;
        out.push(jstr(env, &JString::from(item))?);
    }
    Some(out)
}

/// 回调 Kotlin 后清理可能残留的 pending exception（防御性）。
pub fn clear_exception(env: &mut JNIEnv) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}
