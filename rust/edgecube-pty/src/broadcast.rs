//! 输出历史 + 订阅者扇出。

use std::collections::{HashMap, VecDeque};

use crate::frame::{OutFrame, SharedSink};

/// 回放时单帧分片上限。
pub const REPLAY_CHUNK_BYTES: usize = 64 * 1024;

/// 输出历史 + 订阅者。
pub(crate) struct Broadcast {
    /// 每个元素是一次 PTY 读取的原始输出块。
    chunks: VecDeque<Vec<u8>>,
    total: usize,
    max: usize,
    subscribers: HashMap<u64, SharedSink>,
    /// 最近一次状态载荷；回放结束时原样带回，保证新连上的界面立刻拿到状态。
    state_json: String,
}

impl Broadcast {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            total: 0,
            max,
            subscribers: HashMap::new(),
            state_json: String::from("{}"),
        }
    }

    /// 追加一块输出：先裁历史（至少留一块），再扇给所有订阅者。
    pub(crate) fn append(&mut self, data: Vec<u8>) {
        self.total += data.len();
        self.chunks.push_back(data.clone());
        while self.total > self.max && self.chunks.len() > 1 {
            if let Some(old) = self.chunks.pop_front() {
                self.total -= old.len();
            }
        }
        self.fan_out(OutFrame::Data(data));
    }

    /// 广播控制消息；`remember` 为真时同时存为回放结束时带回的状态。
    pub(crate) fn control(&mut self, json: String, remember: bool) {
        if remember {
            self.state_json = json.clone();
        }
        self.fan_out(OutFrame::Control(json));
    }

    fn fan_out(&mut self, frame: OutFrame) {
        for sink in self.subscribers.values() {
            sink.send(frame.clone());
        }
    }

    /// 注册订阅者。`with_history` 为真时在同一把锁内完成回放。
    pub(crate) fn subscribe(&mut self, id: u64, sink: SharedSink, with_history: bool) {
        if with_history {
            sink.send(OutFrame::ReplayBegin);
            for chunk in &self.chunks {
                for part in chunk.chunks(REPLAY_CHUNK_BYTES) {
                    sink.send(OutFrame::Data(part.to_vec()));
                }
            }
            sink.send(OutFrame::ReplayEnd(self.state_json.clone()));
        }
        self.subscribers.insert(id, sink);
    }

    pub(crate) fn unsubscribe(&mut self, id: u64) {
        self.subscribers.remove(&id);
    }

    pub(crate) fn clear_history(&mut self) {
        self.chunks.clear();
        self.total = 0;
    }

    /// 把历史拼成一份连续快照（测试/自省用）。
    pub(crate) fn snapshot(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.total);
        for chunk in &self.chunks {
            buf.extend_from_slice(chunk);
        }
        buf
    }

    pub(crate) fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    pub(crate) fn history_len(&self) -> usize {
        self.total
    }

    pub(crate) fn state_json(&self) -> &str {
        &self.state_json
    }
}
