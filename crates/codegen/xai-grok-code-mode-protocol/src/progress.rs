//! Bounded progress channel for nested tool invocations.
//!
//! A nested `tools.*` call may stream intermediate output (terminal chunks,
//! log lines, partial results). The delegate receives a
//! [`NestedToolProgressSink`] alongside the invocation and pushes chunks into
//! it; the runtime drains the paired [`NestedToolProgressReceiver`] and feeds
//! them to awaiting JavaScript. Pushing never blocks and never fails: when the
//! buffer is full the oldest queued chunk is dropped, so a slow consumer can
//! never grow memory without bound.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use tokio::sync::Notify;

/// Chunk buffer capacity per nested invocation. When full, the oldest queued
/// chunk is dropped before a new one is enqueued.
pub const NESTED_TOOL_PROGRESS_CAPACITY: usize = 64;

/// One incremental progress chunk for a nested tool invocation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NestedToolProgress {
    /// Human-readable text chunk. Empty when the chunk carries only payload.
    pub text: String,
    /// Optional structured payload (e.g. partial-result JSON).
    pub payload: Option<JsonValue>,
}

impl NestedToolProgress {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            payload: None,
        }
    }

    pub fn with_payload(text: impl Into<String>, payload: JsonValue) -> Self {
        Self {
            text: text.into(),
            payload: Some(payload),
        }
    }
}

struct Shared {
    queue: Mutex<VecDeque<NestedToolProgress>>,
    notify: Notify,
    closed: AtomicBool,
    dropped_chunks: AtomicU64,
    /// Live [`NestedToolProgressSink`] handles. Reaching zero closes the
    /// channel, so the consumer always terminates once the invocation ends.
    sink_handles: AtomicU64,
}

/// Producer half of the channel. Cloneable; pushing never blocks. Dropping
/// every clone closes the channel.
pub struct NestedToolProgressSink {
    shared: Arc<Shared>,
}

impl Clone for NestedToolProgressSink {
    fn clone(&self) -> Self {
        Self::new(Arc::clone(&self.shared))
    }
}

/// Consumer half of the channel. Dropping or closing it makes every future
/// sink push a no-op and ends `recv` with `None`.
pub struct NestedToolProgressReceiver {
    shared: Arc<Shared>,
}

impl NestedToolProgressSink {
    fn new(shared: Arc<Shared>) -> Self {
        shared.sink_handles.fetch_add(1, Ordering::Relaxed);
        Self { shared }
    }

    /// Enqueues one chunk, dropping the oldest queued chunk when the buffer
    /// is at capacity. No-op after the channel is closed.
    pub fn push(&self, progress: NestedToolProgress) {
        if self.shared.closed.load(Ordering::Acquire) {
            return;
        }
        if let Ok(mut queue) = self.shared.queue.lock() {
            if queue.len() >= NESTED_TOOL_PROGRESS_CAPACITY {
                queue.pop_front();
                self.shared
                    .dropped_chunks
                    .fetch_add(1, Ordering::Relaxed);
            }
            queue.push_back(progress);
        }
        self.shared.notify.notify_one();
    }

    /// Chunks dropped so far because the buffer was full.
    pub fn dropped_chunks(&self) -> u64 {
        self.shared.dropped_chunks.load(Ordering::Relaxed)
    }

    /// Whether the receiver side has gone away.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }
}

impl Drop for NestedToolProgressSink {
    fn drop(&mut self) {
        if self.shared.sink_handles.fetch_sub(1, Ordering::Release) == 1 {
            self.shared.closed.store(true, Ordering::Release);
            self.shared.notify.notify_waiters();
        }
    }
}

impl NestedToolProgressReceiver {
    /// Next queued chunk in FIFO order, waiting for new pushes. Returns
    /// `None` once the channel is closed and fully drained.
    pub async fn recv(&self) -> Option<NestedToolProgress> {
        loop {
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            if let Some(progress) = self.try_recv() {
                return Some(progress);
            }
            if self.shared.closed.load(Ordering::Acquire) {
                return None;
            }
            notified.await;
        }
    }

    /// Closes the channel. Queued chunks stay readable via [`Self::try_recv`]
    /// / [`Self::recv`]; later sink pushes are dropped.
    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.notify.notify_waiters();
    }

    /// Whether the channel was closed by receiver close/drop or by all
    /// producer handles being dropped.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Next queued chunk without waiting.
    pub fn try_recv(&self) -> Option<NestedToolProgress> {
        self.shared
            .queue
            .lock()
            .ok()
            .and_then(|mut queue| queue.pop_front())
    }
}

impl Drop for NestedToolProgressReceiver {
    fn drop(&mut self) {
        self.close();
    }
}

/// Creates a bounded (drop-oldest) progress channel for one nested invocation.
pub fn nested_tool_progress_channel() -> (NestedToolProgressSink, NestedToolProgressReceiver) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::new()),
        notify: Notify::new(),
        closed: AtomicBool::new(false),
        dropped_chunks: AtomicU64::new(0),
        sink_handles: AtomicU64::new(0),
    });
    (
        NestedToolProgressSink::new(Arc::clone(&shared)),
        NestedToolProgressReceiver { shared },
    )
}

#[cfg(test)]
mod tests {
    use super::NESTED_TOOL_PROGRESS_CAPACITY;
    use super::NestedToolProgress;
    use super::nested_tool_progress_channel;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn progress_serializes_roundtrip() {
        let chunk = NestedToolProgress::with_payload("out", json!({"n": 1}));
        let serialized = serde_json::to_string(&chunk).unwrap();
        assert_eq!(serialized, r#"{"text":"out","payload":{"n":1}}"#);
        assert_eq!(
            serde_json::from_str::<NestedToolProgress>(&serialized).unwrap(),
            chunk
        );
    }

    #[test]
    fn text_constructor_leaves_payload_absent() {
        assert_eq!(NestedToolProgress::text("hi").payload, None);
    }

    #[test]
    fn chunks_are_delivered_in_fifo_order() {
        let (sink, receiver) = nested_tool_progress_channel();
        sink.push(NestedToolProgress::text("a"));
        sink.push(NestedToolProgress::text("b"));
        assert_eq!(receiver.try_recv(), Some(NestedToolProgress::text("a")));
        assert_eq!(receiver.try_recv(), Some(NestedToolProgress::text("b")));
        assert_eq!(receiver.try_recv(), None);
    }

    #[test]
    fn full_buffer_drops_oldest_chunk() {
        let (sink, receiver) = nested_tool_progress_channel();
        for index in 0..=NESTED_TOOL_PROGRESS_CAPACITY {
            sink.push(NestedToolProgress::text(format!("chunk-{index}")));
        }
        // The oldest chunk (`chunk-0`) made room for the newest one.
        assert_eq!(receiver.try_recv(), Some(NestedToolProgress::text("chunk-1")));
        assert_eq!(sink.dropped_chunks(), 1);
        for index in 2..=NESTED_TOOL_PROGRESS_CAPACITY {
            assert_eq!(
                receiver.try_recv(),
                Some(NestedToolProgress::text(format!("chunk-{index}")))
            );
        }
        assert_eq!(receiver.try_recv(), None);
    }

    #[tokio::test]
    async fn recv_waits_for_pushes_then_drains_in_order() {
        let (sink, receiver) = nested_tool_progress_channel();
        sink.push(NestedToolProgress::with_payload("a", json!({"i": 0})));
        assert_eq!(
            receiver.recv().await,
            Some(NestedToolProgress::with_payload("a", json!({"i": 0})))
        );
        let pusher = tokio::spawn({
            let sink = sink.clone();
            async move {
                tokio::task::yield_now().await;
                sink.push(NestedToolProgress::text("b"));
            }
        });
        assert_eq!(receiver.recv().await, Some(NestedToolProgress::text("b")));
        assert!(pusher.await.is_ok());
    }

    #[tokio::test]
    async fn close_ends_recv_and_disables_push() {
        let (sink, receiver) = nested_tool_progress_channel();
        sink.push(NestedToolProgress::text("queued"));
        receiver.close();
        assert!(sink.is_closed());
        // Already-queued chunks stay readable after close.
        assert_eq!(receiver.recv().await, Some(NestedToolProgress::text("queued")));
        assert_eq!(receiver.recv().await, None);
        sink.push(NestedToolProgress::text("late"));
        assert_eq!(receiver.try_recv(), None);
    }

    #[tokio::test]
    async fn dropping_receiver_closes_the_channel() {
        let (sink, receiver) = nested_tool_progress_channel();
        drop(receiver);
        assert!(sink.is_closed());
        sink.push(NestedToolProgress::text("ignored"));
        assert_eq!(sink.dropped_chunks(), 0);
    }

    #[tokio::test]
    async fn dropping_every_sink_clone_closes_the_channel() {
        let (sink, receiver) = nested_tool_progress_channel();
        let sink_clone = sink.clone();
        assert!(!receiver.is_closed());
        drop(sink);
        assert!(
            !sink_clone.is_closed(),
            "a live clone keeps the channel open"
        );
        drop(sink_clone);
        assert!(receiver.is_closed());
        // After the last producer handle is gone, recv must end.
        assert_eq!(receiver.recv().await, None);
    }
}
