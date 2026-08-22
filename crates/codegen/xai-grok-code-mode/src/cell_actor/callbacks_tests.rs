use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use xai_grok_code_mode_protocol::NESTED_TOOL_PROGRESS_CAPACITY;
use xai_grok_code_mode_protocol::NestedToolProgress;
use xai_grok_code_mode_protocol::NestedToolProgressSink;
use xai_grok_code_mode_protocol::nested_tool_progress_channel;

use super::*;
use crate::cell_actor::CellState;
use crate::cell_actor::CompletionCommit;
use crate::runtime::RuntimeCommand;
use crate::session_runtime::CellEvent;
use crate::session_runtime::ToolKind;
use crate::session_runtime::ToolName;

struct PanickingCallbackHost;

impl CellHost for PanickingCallbackHost {
    async fn invoke_tool(
        &self,
        _invocation: CellToolCall,
        _cancellation_token: CancellationToken,
        _progress: NestedToolProgressSink,
    ) -> Result<JsonValue, String> {
        panic!("tool callback panic probe");
    }

    async fn notify(
        &self,
        _call_id: String,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> Result<(), String> {
        panic!("notification callback panic probe");
    }

    async fn commit_completion(
        &self,
        _stored_value_writes: HashMap<String, JsonValue>,
        _event: CellEvent,
        _pending_initial_yield_items: Option<Vec<crate::session_runtime::OutputItem>>,
        _cell_state: Arc<CellState>,
    ) -> CompletionCommit {
        panic!("unexpected completion commit");
    }

    async fn closed(&self) {}
}

#[tokio::test]
async fn tool_callback_panic_rejects_the_js_promise_and_reports_failure() {
    let mut tasks = JoinSet::new();
    let (runtime_tx, runtime_rx) = std_mpsc::channel();
    let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
    spawn_tool(
        &mut tasks,
        Arc::new(PanickingCallbackHost),
        CellToolCall {
            id: "tool-1".to_string(),
            name: ToolName {
                name: "panic".to_string(),
                namespace: None,
            },
            kind: ToolKind::Function,
            input: None,
        },
        runtime_tx,
        CancellationToken::new(),
        Some(Arc::new(move |reason| {
            let _ = failure_tx.send(reason);
        })),
    );

    tasks
        .join_next()
        .await
        .expect("tool callback task")
        .expect("tool callback wrapper");
    let command = runtime_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("tool error command");
    let RuntimeCommand::ToolError { id, error_text } = command else {
        panic!("expected a tool error command");
    };
    assert_eq!(id, "tool-1");
    assert_eq!(error_text, "code mode tool task panicked");
    assert_eq!(failure_rx.recv().await, Some(error_text));
}

#[tokio::test]
async fn notification_callback_panic_reports_failure() {
    let mut tasks = JoinSet::new();
    let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
    spawn_notification(
        &mut tasks,
        Arc::new(PanickingCallbackHost),
        "notify-1".to_string(),
        "hello".to_string(),
        CancellationToken::new(),
        Some(Arc::new(move |reason| {
            let _ = failure_tx.send(reason);
        })),
    );

    tasks
        .join_next()
        .await
        .expect("notification callback task")
        .expect("notification callback wrapper");
    let failure_reason = failure_rx.recv().await.expect("notification failure");
    assert_eq!(failure_reason, "code mode notification task panicked");
}

#[tokio::test]
async fn callback_wrapper_join_error_reports_failure() {
    let task_result = tokio::spawn(async {
        panic!("callback wrapper panic probe");
    })
    .await;
    let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
    let task_failure_handler: TaskFailureHandler = Arc::new(move |reason| {
        let _ = failure_tx.send(reason);
    });

    report_task_result(Some(task_result), "tool", Some(&task_failure_handler));

    let failure_reason = failure_rx.recv().await.expect("wrapper failure");
    assert!(failure_reason.contains("code mode tool task failed"));
}

#[tokio::test]
async fn progress_forwarder_keeps_backlogged_chunks_in_the_bounded_queue() {
    let (sink, receiver) = nested_tool_progress_channel();
    let (runtime_tx, runtime_rx) = std_mpsc::channel();
    let forwarder = spawn_progress_forwarder(
        "tool-1".to_string(),
        receiver,
        runtime_tx,
        CancellationToken::new(),
    );

    sink.push(NestedToolProgress::text("first"));
    let first_command = next_runtime_command(&runtime_rx).await;
    let RuntimeCommand::ToolProgress {
        progress,
        acknowledgement,
        ..
    } = first_command
    else {
        panic!("expected the first progress command");
    };
    assert_eq!(progress, NestedToolProgress::text("first"));

    for index in 0..=NESTED_TOOL_PROGRESS_CAPACITY {
        sink.push(NestedToolProgress::text(format!("chunk-{index}")));
    }

    assert_eq!(sink.dropped_chunks(), 1);
    assert!(matches!(
        runtime_rx.try_recv(),
        Err(std_mpsc::TryRecvError::Empty)
    ));
    sink.close();
    acknowledgement.send(()).unwrap();

    for index in 1..=NESTED_TOOL_PROGRESS_CAPACITY {
        let command = next_runtime_command(&runtime_rx).await;
        let RuntimeCommand::ToolProgress {
            progress,
            acknowledgement,
            ..
        } = command
        else {
            panic!("expected a queued progress command");
        };
        assert_eq!(progress, NestedToolProgress::text(format!("chunk-{index}")));
        acknowledgement.send(()).unwrap();
    }

    forwarder.await.unwrap();
}

#[tokio::test]
async fn cancellation_stops_a_progress_forwarder_waiting_for_v8() {
    let (sink, receiver) = nested_tool_progress_channel();
    let (runtime_tx, runtime_rx) = std_mpsc::channel();
    let cancellation_token = CancellationToken::new();
    let forwarder = spawn_progress_forwarder(
        "tool-1".to_string(),
        receiver,
        runtime_tx,
        cancellation_token.clone(),
    );
    sink.push(NestedToolProgress::text("first"));
    let command = next_runtime_command(&runtime_rx).await;
    let RuntimeCommand::ToolProgress {
        acknowledgement, ..
    } = command
    else {
        panic!("expected a progress command");
    };

    cancellation_token.cancel();

    tokio::time::timeout(Duration::from_secs(1), forwarder)
        .await
        .unwrap()
        .unwrap();
    assert!(acknowledgement.send(()).is_err());
    assert!(sink.is_closed());
}

async fn next_runtime_command(receiver: &std_mpsc::Receiver<RuntimeCommand>) -> RuntimeCommand {
    for _ in 0..10_000 {
        match receiver.try_recv() {
            Ok(command) => return command,
            Err(std_mpsc::TryRecvError::Empty) => tokio::task::yield_now().await,
            Err(std_mpsc::TryRecvError::Disconnected) => panic!("runtime channel disconnected"),
        }
    }
    panic!("timed out waiting for a runtime command");
}
