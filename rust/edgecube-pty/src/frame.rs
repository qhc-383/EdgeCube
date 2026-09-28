//! 控制台帧与订阅者接口。


use std::sync::Arc;

/// 发给订阅者的一帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutFrame {
    /// 回放开始；Kotlin 转成 `historyBegin`（Dart 清屏）。
    ReplayBegin,
    /// 一段原始终端字节。回放时是历史分片，实时时是 PTY 读到的原始块。
    Data(Vec<u8>),
    /// 回放结束，携带当前状态 JSON（[`crate::session::RunInfo`] 的 state 载荷）；
    /// Kotlin 转成 `{type: state}` + `{type: historyEnd}` 两条 —— 状态复用 Dart 现有的
    /// state 解析，提示符重绘/尺寸补发由紧跟其后的 `historyEnd` 触发。
    ReplayEnd(String),
    /// 实时控制消息（JSON）：`{"type":"state",…}` / `{"type":"exit",…}`。
    /// **不进历史** —— 那是瞬时状态，不是输出。
    Control(String),
}

/// 一端订阅者。实现方**必须是非阻塞的**。

pub trait FrameSink: Send + Sync {
    fn send(&self, frame: OutFrame);
}

impl<F> FrameSink for F
where
    F: Fn(OutFrame) + Send + Sync,
{
    fn send(&self, frame: OutFrame) {
        self(frame);
    }
}

/// 被会话持有、可跨线程共享的订阅者句柄。
pub type SharedSink = Arc<dyn FrameSink>;
