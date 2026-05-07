//! Core agent runtime — the event-driven loop that drives LLM↔tool interactions.
//!
//! The runtime is generic over `LLMProvider` and `ToolExecutor`, making it
//! testable and extractable to a separate service later.

use std::sync::Arc;

use futures::{channel::mpsc, SinkExt};
use tokio::sync::watch;

use crate::config::RuntimeConfig;
use crate::error::RuntimeError;
use crate::events::{FinishReason, RuntimeEvent, StopReason};
use crate::messages::normalize::truncate_tool_results;
use crate::messages::{ConversationHistory, Message};
use crate::provider::{ChatRequest, ChatResponse, ChatStopReason, ChatStreamEvent, LLMProvider};
use crate::tools::{PermissionDecision, ToolCallResult, ToolExecutor};

/// The local agent runtime.
///
/// Drives the LLM→tool→result loop, yielding [`RuntimeEvent`]s
/// to the caller via a channel.
pub struct AgentRuntime<P: LLMProvider, T: ToolExecutor> {
    provider: Arc<P>,
    executor: Arc<T>,
    config: RuntimeConfig,
}

impl<P: LLMProvider, T: ToolExecutor> AgentRuntime<P, T> {
    pub fn new(provider: P, executor: T, config: RuntimeConfig) -> Self {
        Self {
            provider: Arc::new(provider),
            executor: Arc::new(executor),
            config,
        }
    }

    /// Run the agent loop for a single user message.
    ///
    /// Returns a receiver of `RuntimeEvent`s and a cancel handle.
    /// The loop runs in a spawned task and emits events as they occur.
    ///
    /// # Arguments
    /// * `model` - The model to use for this run
    /// * `initial_messages` - Pre-existing conversation history (from prior turns)
    /// * `user_input` - The new user message to process
    pub fn run(
        &self,
        model: String,
        initial_messages: Vec<Message>,
        user_input: String,
    ) -> (mpsc::Receiver<RuntimeEvent>, CancelHandle)
    where
        P: 'static,
        T: 'static,
    {
        let (tx, rx) = mpsc::channel(64);
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let config = self.config.clone();
        let provider = Arc::clone(&self.provider);
        let executor = Arc::clone(&self.executor);

        tokio::spawn(async move {
            let mut sink = ChannelEventSink { tx };
            let result = run_loop(
                provider,
                executor,
                config,
                model,
                initial_messages,
                user_input,
                Some(cancel_rx),
                &mut sink,
            )
            .await;

            if let Err(err) = result {
                if !matches!(err, RuntimeError::Cancelled) {
                    sink.send(RuntimeEvent::Finished {
                        reason: FinishReason::Error(err.to_string()),
                    })
                    .await;
                }
            }
        });

        (rx, CancelHandle { _tx: cancel_tx })
    }

    /// Run the agent loop to completion, collecting all events.
    ///
    /// This is the primary execution method. It drives the full
    /// LLM → tool_calls → execute → result → LLM cycle.
    pub async fn run_to_completion(
        &self,
        model: &str,
        initial_messages: Vec<Message>,
        user_input: &str,
    ) -> Result<(Vec<RuntimeEvent>, Vec<Message>), RuntimeError> {
        let mut sink = VecEventSink::default();
        let messages = run_loop(
            Arc::clone(&self.provider),
            Arc::clone(&self.executor),
            self.config.clone(),
            model.to_string(),
            initial_messages,
            user_input.to_string(),
            None,
            &mut sink,
        )
        .await?;

        Ok((sink.events, messages))
    }
}

#[async_trait::async_trait]
trait RuntimeEventSink {
    async fn send(&mut self, event: RuntimeEvent);
}

#[derive(Default)]
struct VecEventSink {
    events: Vec<RuntimeEvent>,
}

#[async_trait::async_trait]
impl RuntimeEventSink for VecEventSink {
    async fn send(&mut self, event: RuntimeEvent) {
        self.events.push(event);
    }
}

struct ChannelEventSink {
    tx: mpsc::Sender<RuntimeEvent>,
}

#[async_trait::async_trait]
impl RuntimeEventSink for ChannelEventSink {
    async fn send(&mut self, event: RuntimeEvent) {
        let _ = self.tx.send(event).await;
    }
}

async fn run_loop<P, T, S>(
    provider: Arc<P>,
    executor: Arc<T>,
    config: RuntimeConfig,
    model: String,
    initial_messages: Vec<Message>,
    user_input: String,
    mut cancel_rx: Option<watch::Receiver<bool>>,
    sink: &mut S,
) -> Result<Vec<Message>, RuntimeError>
where
    P: LLMProvider,
    T: ToolExecutor,
    S: RuntimeEventSink + Send,
{
    let mut history = if let Some(ref prompt) = config.system_prompt {
        ConversationHistory::with_system_prompt(prompt.clone())
    } else {
        ConversationHistory::new()
    };

    for msg in initial_messages {
        history.messages_mut().push(msg);
    }
    history.push_user(user_input);

    let tools = executor.available_tools();
    let mut turn: u32 = 0;

    loop {
        if is_cancelled(&cancel_rx) {
            sink.send(RuntimeEvent::Finished {
                reason: FinishReason::Cancelled,
            })
            .await;
            return Err(RuntimeError::Cancelled);
        }

        turn += 1;
        if turn > config.max_turns {
            sink.send(RuntimeEvent::Finished {
                reason: FinishReason::MaxTurns,
            })
            .await;
            return Ok(history.messages().to_vec());
        }

        sink.send(RuntimeEvent::TurnStarted { turn }).await;

        truncate_tool_results(&mut history, config.max_tool_result_chars);

        let request = ChatRequest {
            model: model.clone(),
            messages: history.messages().to_vec(),
            tools: tools.clone(),
        };

        let chat_result = match chat_with_cancel(
            Arc::clone(&provider),
            request,
            config.llm_timeout,
            &mut cancel_rx,
            sink,
        )
        .await?
        {
            Some(response) => response,
            None => {
                sink.send(RuntimeEvent::Finished {
                    reason: FinishReason::Cancelled,
                })
                .await;
                return Err(RuntimeError::Cancelled);
            }
        };
        let response = chat_result.response;

        if !response.text.is_empty() && !chat_result.streamed_text {
            sink.send(RuntimeEvent::TextCompleted {
                text: response.text.clone(),
            })
            .await;
        }

        if response.tool_calls.is_empty() {
            history.push_assistant(response.text, vec![]);
            sink.send(RuntimeEvent::TurnCompleted {
                reason: match response.stop_reason {
                    ChatStopReason::MaxTokens => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                },
            })
            .await;
            sink.send(RuntimeEvent::Finished {
                reason: FinishReason::Done,
            })
            .await;
            return Ok(history.messages().to_vec());
        }

        sink.send(RuntimeEvent::ToolCallsRequested {
            calls: response.tool_calls.clone(),
        })
        .await;
        sink.send(RuntimeEvent::TurnCompleted {
            reason: StopReason::ToolUse,
        })
        .await;

        history.push_assistant(response.text, response.tool_calls.clone());

        let mut should_stop = false;
        for call in &response.tool_calls {
            if is_cancelled(&cancel_rx) {
                sink.send(RuntimeEvent::Finished {
                    reason: FinishReason::Cancelled,
                })
                .await;
                return Err(RuntimeError::Cancelled);
            }

            let permission =
                match check_permission_with_cancel(Arc::clone(&executor), call, &mut cancel_rx)
                    .await
                {
                    Some(permission) => permission,
                    None => {
                        sink.send(RuntimeEvent::Finished {
                            reason: FinishReason::Cancelled,
                        })
                        .await;
                        return Err(RuntimeError::Cancelled);
                    }
                };

            match permission {
                PermissionDecision::Allow => {
                    sink.send(RuntimeEvent::ToolExecutionStarted {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                    })
                    .await;

                    let result = match execute_tool_with_cancel(
                        Arc::clone(&executor),
                        call,
                        config.tool_timeout,
                        &mut cancel_rx,
                    )
                    .await
                    {
                        Some(Ok(result)) => result,
                        Some(Err(e)) => {
                            ToolCallResult::error(format!("Tool execution failed: {}", e))
                        }
                        None => {
                            sink.send(RuntimeEvent::Finished {
                                reason: FinishReason::Cancelled,
                            })
                            .await;
                            return Err(RuntimeError::Cancelled);
                        }
                    };

                    sink.send(RuntimeEvent::ToolResult {
                        call_id: call.id.clone(),
                        result: result.clone(),
                    })
                    .await;
                    history.push_tool_result(&call.id, result);
                }
                PermissionDecision::Ask => {
                    sink.send(RuntimeEvent::PermissionRequired { call: call.clone() })
                        .await;

                    let error_result = ToolCallResult::error(
                        "Permission required — tool execution skipped in non-interactive mode",
                    );
                    history.push_tool_result(&call.id, error_result.clone());
                    sink.send(RuntimeEvent::ToolResult {
                        call_id: call.id.clone(),
                        result: error_result,
                    })
                    .await;

                    if config.stop_on_permission_denied {
                        should_stop = true;
                        break;
                    }
                }
                PermissionDecision::Deny { reason } => {
                    let error_result =
                        ToolCallResult::error(format!("Permission denied: {}", reason));
                    history.push_tool_result(&call.id, error_result.clone());
                    sink.send(RuntimeEvent::ToolResult {
                        call_id: call.id.clone(),
                        result: error_result,
                    })
                    .await;

                    if config.stop_on_permission_denied {
                        should_stop = true;
                        break;
                    }
                }
            }
        }

        if should_stop {
            sink.send(RuntimeEvent::Finished {
                reason: FinishReason::Done,
            })
            .await;
            return Ok(history.messages().to_vec());
        }
    }
}

struct ChatRunResult {
    response: ChatResponse,
    streamed_text: bool,
}

async fn chat_with_cancel<P>(
    provider: Arc<P>,
    request: ChatRequest,
    timeout: std::time::Duration,
    cancel_rx: &mut Option<watch::Receiver<bool>>,
    sink: &mut (impl RuntimeEventSink + Send),
) -> Result<Option<ChatRunResult>, RuntimeError>
where
    P: LLMProvider,
{
    if provider.capabilities().streaming {
        return chat_stream_with_cancel(provider, request, timeout, cancel_rx, sink).await;
    }

    let call = tokio::time::timeout(timeout, provider.chat(request));
    if let Some(cancel_rx) = cancel_rx.as_mut() {
        tokio::select! {
            _ = wait_cancelled(cancel_rx) => Ok(None),
            result = call => match result {
                Ok(response) => response
                    .map(|response| {
                        Some(ChatRunResult {
                            response,
                            streamed_text: false,
                        })
                    })
                    .map_err(RuntimeError::Provider),
                Err(_) => Err(RuntimeError::Provider(crate::ProviderError::Timeout {
                    seconds: timeout.as_secs(),
                })),
            },
        }
    } else {
        match call.await {
            Ok(response) => response
                .map(|response| {
                    Some(ChatRunResult {
                        response,
                        streamed_text: false,
                    })
                })
                .map_err(RuntimeError::Provider),
            Err(_) => Err(RuntimeError::Provider(crate::ProviderError::Timeout {
                seconds: timeout.as_secs(),
            })),
        }
    }
}

async fn chat_stream_with_cancel<P>(
    provider: Arc<P>,
    request: ChatRequest,
    timeout: std::time::Duration,
    cancel_rx: &mut Option<watch::Receiver<bool>>,
    sink: &mut (impl RuntimeEventSink + Send),
) -> Result<Option<ChatRunResult>, RuntimeError>
where
    P: LLMProvider,
{
    let (event_tx, event_rx) = async_channel::unbounded();
    let call = tokio::time::timeout(timeout, provider.chat_stream(request, event_tx));
    futures::pin_mut!(call);

    let mut streamed_text = false;
    let mut stream_events_open = true;

    loop {
        if let Some(cancel_rx) = cancel_rx.as_mut() {
            tokio::select! {
                _ = wait_cancelled(cancel_rx) => return Ok(None),
                event = event_rx.recv(), if stream_events_open => {
                    match event {
                        Ok(ChatStreamEvent::TextDelta { text }) => {
                            if !text.is_empty() {
                                streamed_text = true;
                                sink.send(RuntimeEvent::TextDelta { text }).await;
                            }
                        }
                        Err(_) => stream_events_open = false,
                    }
                }
                result = &mut call => {
                    streamed_text = drain_stream_events(&event_rx, sink, streamed_text).await;
                    return finish_chat_result(result, timeout, streamed_text);
                }
            }
        } else {
            tokio::select! {
                event = event_rx.recv(), if stream_events_open => {
                    match event {
                        Ok(ChatStreamEvent::TextDelta { text }) => {
                            if !text.is_empty() {
                                streamed_text = true;
                                sink.send(RuntimeEvent::TextDelta { text }).await;
                            }
                        }
                        Err(_) => stream_events_open = false,
                    }
                }
                result = &mut call => {
                    streamed_text = drain_stream_events(&event_rx, sink, streamed_text).await;
                    return finish_chat_result(result, timeout, streamed_text);
                }
            }
        }
    }
}

async fn drain_stream_events(
    event_rx: &async_channel::Receiver<ChatStreamEvent>,
    sink: &mut (impl RuntimeEventSink + Send),
    mut streamed_text: bool,
) -> bool {
    while let Ok(event) = event_rx.try_recv() {
        match event {
            ChatStreamEvent::TextDelta { text } if !text.is_empty() => {
                streamed_text = true;
                sink.send(RuntimeEvent::TextDelta { text }).await;
            }
            ChatStreamEvent::TextDelta { .. } => {}
        }
    }
    streamed_text
}

fn finish_chat_result(
    result: Result<Result<ChatResponse, crate::ProviderError>, tokio::time::error::Elapsed>,
    timeout: std::time::Duration,
    streamed_text: bool,
) -> Result<Option<ChatRunResult>, RuntimeError> {
    match result {
        Ok(response) => response
            .map(|response| {
                Some(ChatRunResult {
                    response,
                    streamed_text,
                })
            })
            .map_err(RuntimeError::Provider),
        Err(_) => Err(RuntimeError::Provider(crate::ProviderError::Timeout {
            seconds: timeout.as_secs(),
        })),
    }
}

async fn check_permission_with_cancel<T>(
    executor: Arc<T>,
    call: &crate::tools::ToolCall,
    cancel_rx: &mut Option<watch::Receiver<bool>>,
) -> Option<PermissionDecision>
where
    T: ToolExecutor,
{
    if let Some(cancel_rx) = cancel_rx.as_mut() {
        tokio::select! {
            _ = wait_cancelled(cancel_rx) => None,
            decision = executor.check_permission(call) => Some(decision),
        }
    } else {
        Some(executor.check_permission(call).await)
    }
}

async fn execute_tool_with_cancel<T>(
    executor: Arc<T>,
    call: &crate::tools::ToolCall,
    timeout: std::time::Duration,
    cancel_rx: &mut Option<watch::Receiver<bool>>,
) -> Option<Result<ToolCallResult, crate::ToolExecutionError>>
where
    T: ToolExecutor,
{
    let execution = tokio::time::timeout(timeout, executor.execute(call));
    if let Some(cancel_rx) = cancel_rx.as_mut() {
        tokio::select! {
            _ = wait_cancelled(cancel_rx) => None,
            result = execution => Some(match result {
                Ok(result) => result,
                Err(_) => Err(crate::ToolExecutionError::Timeout),
            }),
        }
    } else {
        Some(match execution.await {
            Ok(result) => result,
            Err(_) => Err(crate::ToolExecutionError::Timeout),
        })
    }
}

fn is_cancelled(cancel_rx: &Option<watch::Receiver<bool>>) -> bool {
    cancel_rx
        .as_ref()
        .is_some_and(|cancel_rx| *cancel_rx.borrow())
}

async fn wait_cancelled(cancel_rx: &mut watch::Receiver<bool>) {
    if *cancel_rx.borrow() {
        return;
    }
    while cancel_rx.changed().await.is_ok() {
        if *cancel_rx.borrow() {
            return;
        }
    }
}

/// Handle to cancel a running agent loop.
pub struct CancelHandle {
    _tx: watch::Sender<bool>,
}

impl CancelHandle {
    /// Cancel the running agent loop.
    pub fn cancel(&self) {
        let _ = self._tx.send(true);
    }
}
