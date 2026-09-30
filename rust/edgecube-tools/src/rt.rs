//! ftp/ssh 共用的 tokio 运行时

use std::sync::OnceLock;

use tokio::runtime::Runtime;

static RT: OnceLock<Runtime> = OnceLock::new();

/// 懒初始化共享运行时（线程名 `edgecube-rt`）。
///
/// 只能在非运行时线程上 `block_on`。
pub fn shared() -> &'static Runtime {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("edgecube-rt")
            .enable_all()
            .build()
            .expect("edgecube: 创建 tokio 运行时失败")
    })
}
