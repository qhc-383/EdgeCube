use std::env;

/// 为 Android 目标生成 pthread 空桩静态库（`OUT_DIR/libpthread.a`）。
///
/// `unrar_sys` 的 build 脚本在非 Windows 平台无条件输出 `cargo:rustc-link-lib=pthread`，
/// 而 Android bionic 没有独立的 libpthread。此处用 `cc` 编译一个空目标文件成静态库并由
/// `cc` 自动注册 `-L OUT_DIR`，使该 `-lpthread` 就地解析，链接不产生 DT_NEEDED。
/// 其余平台/未启用 archive 特性时无此需求，直接跳过。
fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    if env::var_os("CARGO_FEATURE_ARCHIVE").is_none() {
        return;
    }

    println!("cargo:rerun-if-changed=pthread_stub.c");
    println!("cargo:rerun-if-env-changed=ANDROID_NDK_HOME");
    println!("cargo:rerun-if-env-changed=ANDROID_NDK_ROOT");
    println!("cargo:rerun-if-env-changed=ANDROID_SDK_ROOT");
    println!("cargo:rerun-if-env-changed=ANDROID_HOME");

    cc::Build::new().file("pthread_stub.c").compile("pthread");
}
