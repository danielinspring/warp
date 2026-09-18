//! Core agent runtime — the event-driven loop that drives LLM↔tool interactions.
//!
//! The runtime is generic over `LLMProvider` and `ToolExecutor`, making it
//! testable and extractable to a separate service later.

use std::sync::Arc;

use futures::channel::mpsc;
use futures::future::join_all;
use futures::SinkExt;
use instant::Instant;
use tokio::sync::watch;

use crate::config::RuntimeConfig;
use crate::error::RuntimeError;
use crate::events::{FinishReason, RuntimeEvent, StopReason};
use crate::hooks::{LifecycleHooks, NoopHooks, PreToolDecision};
use crate::messages::normalize::model_messages;
use crate::messages::{ConversationHistory, Message, UserMessage};
use crate::provider::{ChatRequest, ChatResponse, ChatStopReason, ChatStreamEvent, LLMProvider};
use crate::telemetry::{NoopTelemetrySink, RuntimeTelemetryEvent, RuntimeTelemetrySink};
use crate::tools::{PermissionDecision, ToolCall, ToolCallResult, ToolExecutor, ToolSafetyClass};

/// The local agent runtime.
///
/// Drives the LLM→tool→result loop, yielding [`RuntimeEvent`]s
/// to the caller via a channel.
pub struct AgentRuntime<P: LLMProvider, T: ToolExecutor> {
    provider: Arc<P>,
    executor: Arc<T>,
    config: RuntimeConfig,
    hooks: Arc<dyn LifecycleHooks>,
    telemetry: Arc<dyn RuntimeTelemetrySink>,
}

impl<P: LLMProvider, T: ToolExecutor> AgentRuntime<P, T> {
    pub fn new(provider: P, executor: T, config: RuntimeConfig) -> Self {
        Self {
            provider: Arc::new(provider),
            executor: Arc::new(executor),
            config,
            hooks: Arc::new(NoopHooks),
            telemetry: Arc::new(NoopTelemetrySink),
        }
    }

    /// Attach trusted lifecycle hooks (pre/post tool, permission, stop).
    pub fn with_hooks(mut self, hooks: Arc<dyn LifecycleHooks>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Attach a telemetry sink for provider/tool/run observability.
    pub fn with_telemetry(mut self, telemetry: Arc<dyn RuntimeTelemetrySink>) -> Self {
        self.telemetry = telemetry;
        self
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
        user_input: impl Into<UserMessage>,
    ) -> (mpsc::Receiver<RuntimeEvent>, CancelHandle)
    where
        P: 'static,
        T: 'static,
    {
        let user_input = user_input.into();
        let (tx, rx) = mpsc::channel(64);
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let config = self.config.clone();
        let provider = Arc::clone(&self.provider);
        let executor = Arc::clone(&self.executor);
        let hooks = Arc::clone(&self.hooks);
        let telemetry = Arc::clone(&self.telemetry);

        tokio::spawn(async move {
            let mut sink = ChannelEventSink { tx };
            let hooks_for_error = Arc::clone(&hooks);
            let telemetry_for_error = Arc::clone(&telemetry);
            let result = run_loop(
                provider,
                executor,
                config,
                hooks,
                telemetry,
                RunRequest {
                    model,
                    initial_messages,
                    user_input,
                },
                Some(cancel_rx),
                &mut sink,
            )
            .await;

            if let Err(err) = result {
                if !matches!(err, RuntimeError::Cancelled) {
                    let reason = FinishReason::Error(err.to_string());
                    telemetry_for_error.emit(RuntimeTelemetryEvent::RunFinished {
                        reason: format!("{reason:?}"),
                        turns: 0,
                    });
                    hooks_for_error.on_stop(&reason).await;
                    sink.send(RuntimeEvent::Finished { reason }).await;
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
        user_input: impl Into<UserMessage>,
    ) -> Result<(Vec<RuntimeEvent>, Vec<Message>), RuntimeError> {
        let mut sink = VecEventSink::default();
        let messages = run_loop(
            Arc::clone(&self.provider),
            Arc::clone(&self.executor),
            self.config.clone(),
            Arc::clone(&self.hooks),
            Arc::clone(&self.telemetry),
            RunRequest {
                model: model.to_string(),
                initial_messages,
                user_input: user_input.into(),
            },
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

struct RunRequest {
    model: String,
    initial_messages: Vec<Message>,
    user_input: UserMessage,
}

async fn emit_finished<S>(
    hooks: &Arc<dyn LifecycleHooks>,
    telemetry: &Arc<dyn RuntimeTelemetrySink>,
    sink: &mut S,
    reason: FinishReason,
    turns: u32,
) where
    S: RuntimeEventSink + Send,
{
    telemetry.emit(RuntimeTelemetryEvent::RunFinished {
        reason: format!("{reason:?}"),
        turns,
    });
    hooks.on_stop(&reason).await;
    sink.send(RuntimeEvent::Finished { reason }).await;
}

#[allow(clippy::too_many_arguments)]
async fn run_loop<P, T, S>(
    provider: Arc<P>,
    executor: Arc<T>,
    config: RuntimeConfig,
    hooks: Arc<dyn LifecycleHooks>,
    telemetry: Arc<dyn RuntimeTelemetrySink>,
    request: RunRequest,
    mut cancel_rx: Option<watch::Receiver<bool>>,
    sink: &mut S,
) -> Result<Vec<Message>, RuntimeError>
where
    P: LLMProvider,
    T: ToolExecutor,
    S: RuntimeEventSink + Send,
{
    let RunRequest {
        model,
        initial_messages,
        user_input,
    } = request;
    let mut history = if let Some(ref prompt) = config.system_prompt {
        ConversationHistory::with_system_prompt(prompt.clone())
    } else {
        ConversationHistory::new()
    };

    for msg in initial_messages {
        history.messages_mut().push(msg);
    }
    history.push_user_message(user_input);

    telemetry.emit(RuntimeTelemetryEvent::RunStarted {
        model: model.clone(),
        history_message_count: history.messages().len(),
    });

    let mut turn: u32 = 0;
    let mut continuation_count = 0;
    let mut previous_tool_fingerprint = None;
    let mut repeated_tool_cycles = 0;
    let mut context_budget = config.context_budget.clone();

    loop {
        if is_cancelled(&cancel_rx) {
            emit_finished(&hooks, &telemetry, sink, FinishReason::Cancelled, turn).await;
            return Err(RuntimeError::Cancelled);
        }

        turn += 1;
        tracing::debug!(turn, "local runtime provider turn starting");
        if turn > config.max_turns {
            tracing::warn!(
                turn,
                max_turns = config.max_turns,
                "local runtime stopped at max turns"
            );
            emit_finished(&hooks, &telemetry, sink, FinishReason::MaxTurns, turn).await;
            return Ok(history.messages().to_vec());
        }

        sink.send(RuntimeEvent::TurnStarted { turn }).await;

        let tools = executor.available_tools();

        let mut retried_context_overflow = false;
        let provider_started = Instant::now();
        let chat_result = loop {
            let request = ChatRequest {
                model: model.clone(),
                messages: model_messages(history.messages(), &tools, &context_budget)?,
                tools: tools.clone(),
            };
            match chat_with_provider_retries(
                Arc::clone(&provider),
                request,
                &config,
                &mut cancel_rx,
                sink,
            )
            .await
            {
                Err(RuntimeError::Provider(error))
                    if error.is_context_window_exceeded() && !retried_context_overflow =>
                {
                    retried_context_overflow = true;
                    tighten_context_budget(&mut context_budget);
                }
                Err(RuntimeError::Provider(error)) => {
                    telemetry.emit(RuntimeTelemetryEvent::ProviderError {
                        turn,
                        message: error.to_string(),
                    });
                    return Err(RuntimeError::Provider(error));
                }
                result => break result?,
            }
        };
        let provider_latency_ms = provider_started.elapsed().as_millis() as u64;
        let chat_result = match chat_result {
            Some(response) => response,
            None => {
                emit_finished(&hooks, &telemetry, sink, FinishReason::Cancelled, turn).await;
                return Err(RuntimeError::Cancelled);
            }
        };
        let response = chat_result.response;
        telemetry.emit(RuntimeTelemetryEvent::ProviderTurn {
            turn,
            latency_ms: provider_latency_ms,
            has_tool_calls: !response.tool_calls.is_empty(),
            streamed_text: chat_result.streamed_text,
        });

        if !response.text.is_empty() && !chat_result.streamed_text {
            sink.send(RuntimeEvent::TextCompleted {
                text: response.text.clone(),
            })
            .await;
        }

        if response.tool_calls.is_empty() {
            history.push_assistant(response.text, vec![]);
            if response.stop_reason == ChatStopReason::MaxTokens {
                continuation_count += 1;
                sink.send(RuntimeEvent::TurnCompleted {
                    reason: StopReason::MaxTokens,
                })
                .await;
                if continuation_count > config.max_continuations {
                    return Err(RuntimeError::MaxContinuationsExceeded {
                        max_continuations: config.max_continuations,
                    });
                }
                history.push_user(
                    "Continue exactly where the previous response stopped. Do not repeat text.",
                );
                continue;
            }
            sink.send(RuntimeEvent::TurnCompleted {
                reason: StopReason::EndTurn,
            })
            .await;
            tracing::debug!(turn, "local runtime provider turn completed");
            emit_finished(&hooks, &telemetry, sink, FinishReason::Done, turn).await;
            return Ok(history.messages().to_vec());
        }

        continuation_count = 0;
        let tool_fingerprint = serde_json::to_string(
            &response
                .tool_calls
                .iter()
                .map(|call| (&call.name, &call.arguments))
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default();
        if previous_tool_fingerprint.as_deref() == Some(tool_fingerprint.as_str()) {
            repeated_tool_cycles += 1;
        } else {
            previous_tool_fingerprint = Some(tool_fingerprint);
            repeated_tool_cycles = 1;
        }
        if repeated_tool_cycles >= config.max_repeated_tool_cycles.max(1) {
            sink.send(RuntimeEvent::ToolCallsRequested {
                calls: response.tool_calls.clone(),
            })
            .await;
            sink.send(RuntimeEvent::TurnCompleted {
                reason: StopReason::ToolUse,
            })
            .await;
            history.push_assistant(response.text, response.tool_calls.clone());
            synthesize_tool_error_results(
                &response.tool_calls,
                "repeated_tool_call_stall",
                "Tool execution stopped because the model repeated the same tool calls",
                &mut history,
                sink,
            )
            .await;
            return Err(RuntimeError::RepeatedToolCallStall {
                repeated_cycles: repeated_tool_cycles,
            });
        }

        sink.send(RuntimeEvent::ToolCallsRequested {
            calls: response.tool_calls.clone(),
        })
        .await;
        sink.send(RuntimeEvent::TurnCompleted {
            reason: StopReason::ToolUse,
        })
        .await;

        let response_text = response.text;
        let calls = response.tool_calls;
        history.push_assistant(response_text, calls.clone());
        let mut should_stop = false;
        let mut call_index = 0;
        let mut any_tool_error = false;
        let mut executed_any_tool = false;
        while call_index < calls.len() {
            if is_cancelled(&cancel_rx) {
                synthesize_cancelled_tool_results(&calls[call_index..], &mut history, sink).await;
                emit_finished(&hooks, &telemetry, sink, FinishReason::Cancelled, turn).await;
                return Err(RuntimeError::Cancelled);
            }

            if executor.safety_class_for_call(&calls[call_index]) == ToolSafetyClass::ReadOnly {
                let batch_end = calls[call_index..]
                    .iter()
                    .position(|call| {
                        executor.safety_class_for_call(call) != ToolSafetyClass::ReadOnly
                    })
                    .map(|offset| call_index + offset)
                    .unwrap_or(calls.len());
                let batch = &calls[call_index..batch_end];

                let outcomes = match execute_read_only_batch_with_cancel(
                    Arc::clone(&executor),
                    Arc::clone(&hooks),
                    batch,
                    config.tool_timeout,
                    config.stop_on_permission_denied,
                    &mut cancel_rx,
                )
                .await
                {
                    BatchResult::Completed(outcomes) => outcomes,
                    BatchResult::Cancelled => {
                        synthesize_cancelled_tool_results(&calls[call_index..], &mut history, sink)
                            .await;
                        emit_finished(&hooks, &telemetry, sink, FinishReason::Cancelled, turn)
                            .await;
                        return Err(RuntimeError::Cancelled);
                    }
                };

                for outcome in outcomes {
                    executed_any_tool = true;
                    if outcome.result.is_error {
                        any_tool_error = true;
                    }
                    emit_tool_outcome(&outcome, sink).await;
                    history.push_tool_result(&outcome.call_id, outcome.result.clone());
                    if outcome.stop_after {
                        should_stop = true;
                    }
                }

                call_index = batch_end;
                if should_stop {
                    break;
                }
                continue;
            }

            let call = &calls[call_index];
            let outcome = match execute_serial_tool_with_cancel(
                Arc::clone(&executor),
                Arc::clone(&hooks),
                call,
                config.tool_timeout,
                config.stop_on_permission_denied,
                &mut cancel_rx,
            )
            .await
            {
                SerialResult::Completed(outcome) => outcome,
                SerialResult::Cancelled => {
                    synthesize_cancelled_tool_results(&calls[call_index..], &mut history, sink)
                        .await;
                    emit_finished(&hooks, &telemetry, sink, FinishReason::Cancelled, turn).await;
                    return Err(RuntimeError::Cancelled);
                }
            };

            executed_any_tool = true;
            if outcome.result.is_error {
                any_tool_error = true;
            }
            emit_tool_outcome(&outcome, sink).await;
            history.push_tool_result(&outcome.call_id, outcome.result.clone());
            if outcome.stop_after {
                should_stop = true;
                break;
            }
            call_index += 1;
        }

        // Weak local models often ignore successful tool results and invent timeouts.
        // A short grounding cue before the next LLM turn keeps them on the tool output.
        if executed_any_tool && !should_stop {
            if any_tool_error {
                history.push_user(
                    "Tool results are above (some failed). Answer from those results only. Do not invent different errors or timeouts.",
                );
            } else {
                history.push_user(
                    "Tool results are above and succeeded. Answer the user now using only those results (e.g. command output). Do not apologize, do not invent timeouts, and do not re-run the same command unless the user asks.",
                );
            }
        }

        if should_stop {
            emit_finished(&hooks, &telemetry, sink, FinishReason::Done, turn).await;
            return Ok(history.messages().to_vec());
        }
    }
}

struct ToolOutcome {
    call: ToolCall,
    call_id: String,
    tool_name: String,
    permission_required: bool,
    started: bool,
    result: ToolCallResult,
    stop_after: bool,
}

enum BatchResult {
    Completed(Vec<ToolOutcome>),
    Cancelled,
}

enum SerialResult {
    Completed(ToolOutcome),
    Cancelled,
}

async fn execute_read_only_batch_with_cancel<T>(
    executor: Arc<T>,
    hooks: Arc<dyn LifecycleHooks>,
    calls: &[ToolCall],
    timeout: std::time::Duration,
    stop_on_permission_denied: bool,
    cancel_rx: &mut Option<watch::Receiver<bool>>,
) -> BatchResult
where
    T: ToolExecutor,
{
    let execution = join_all(calls.iter().cloned().map(|call| {
        execute_tool_without_cancel(
            Arc::clone(&executor),
            Arc::clone(&hooks),
            call,
            timeout,
            stop_on_permission_denied,
        )
    }));

    if let Some(cancel_rx) = cancel_rx.as_mut() {
        tokio::select! {
            _ = wait_cancelled(cancel_rx) => BatchResult::Cancelled,
            outcomes = execution => BatchResult::Completed(outcomes),
        }
    } else {
        BatchResult::Completed(execution.await)
    }
}

async fn execute_serial_tool_with_cancel<T>(
    executor: Arc<T>,
    hooks: Arc<dyn LifecycleHooks>,
    call: &ToolCall,
    timeout: std::time::Duration,
    stop_on_permission_denied: bool,
    cancel_rx: &mut Option<watch::Receiver<bool>>,
) -> SerialResult
where
    T: ToolExecutor,
{
    let execution = execute_tool_without_cancel(
        Arc::clone(&executor),
        hooks,
        call.clone(),
        timeout,
        stop_on_permission_denied,
    );

    if let Some(cancel_rx) = cancel_rx.as_mut() {
        tokio::select! {
            _ = wait_cancelled(cancel_rx) => SerialResult::Cancelled,
            outcome = execution => SerialResult::Completed(outcome),
        }
    } else {
        SerialResult::Completed(execution.await)
    }
}

async fn execute_tool_without_cancel<T>(
    executor: Arc<T>,
    hooks: Arc<dyn LifecycleHooks>,
    call: ToolCall,
    timeout: std::time::Duration,
    stop_on_permission_denied: bool,
) -> ToolOutcome
where
    T: ToolExecutor,
{
    tracing::debug!(
        tool_call_id = %call.id,
        tool_name = %call.name,
        safety_class = ?executor.safety_class_for_call(&call),
        "local runtime tool requested"
    );

    if let PreToolDecision::Deny { reason } = hooks.pre_tool(&call).await {
        let result = ToolCallResult::error(format!("Permission denied: {reason}"));
        hooks.post_tool(&call, &result).await;
        return ToolOutcome {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            call,
            permission_required: false,
            started: false,
            result,
            stop_after: false,
        };
    }

    let decision = hooks
        .on_permission(&call, executor.check_permission(&call).await)
        .await;

    match decision {
        PermissionDecision::Allow => {
            let result = match tokio::time::timeout(timeout, executor.execute(&call)).await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => ToolCallResult::error(format!("Tool execution failed: {error}")),
                Err(_) => ToolCallResult::error("Tool execution failed: timed out"),
            };
            hooks.post_tool(&call, &result).await;

            ToolOutcome {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                call,
                permission_required: false,
                started: true,
                result,
                stop_after: false,
            }
        }
        PermissionDecision::Ask => {
            let result = ToolCallResult::error(
                "Permission required - tool execution skipped in non-interactive mode",
            );
            hooks.post_tool(&call, &result).await;
            ToolOutcome {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                call,
                permission_required: true,
                started: false,
                result,
                stop_after: stop_on_permission_denied,
            }
        }
        PermissionDecision::Deny { reason } => {
            let result = ToolCallResult::error(format!("Permission denied: {reason}"));
            hooks.post_tool(&call, &result).await;
            ToolOutcome {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                call,
                permission_required: false,
                started: false,
                result,
                stop_after: stop_on_permission_denied,
            }
        }
    }
}

async fn emit_tool_outcome<S>(outcome: &ToolOutcome, sink: &mut S)
where
    S: RuntimeEventSink + Send,
{
    if outcome.permission_required {
        sink.send(RuntimeEvent::PermissionRequired {
            call: outcome.call.clone(),
        })
        .await;
    }
    if outcome.started {
        sink.send(RuntimeEvent::ToolExecutionStarted {
            call_id: outcome.call_id.clone(),
            tool_name: outcome.tool_name.clone(),
        })
        .await;
    }
    tracing::debug!(
        tool_call_id = %outcome.call_id,
        tool_name = %outcome.tool_name,
        is_error = outcome.result.is_error,
        "local runtime tool result"
    );
    sink.send(RuntimeEvent::ToolResult {
        call_id: outcome.call_id.clone(),
        result: outcome.result.clone(),
    })
    .await;
}

async fn synthesize_cancelled_tool_results<S>(
    calls: &[ToolCall],
    history: &mut ConversationHistory,
    sink: &mut S,
) where
    S: RuntimeEventSink + Send,
{
    for call in calls {
        let result = cancelled_tool_result();
        history.push_tool_result(&call.id, result.clone());
        sink.send(RuntimeEvent::ToolResult {
            call_id: call.id.clone(),
            result,
        })
        .await;
    }
}

async fn synthesize_tool_error_results<S>(
    calls: &[ToolCall],
    code: &str,
    message: &str,
    history: &mut ConversationHistory,
    sink: &mut S,
) where
    S: RuntimeEventSink + Send,
{
    for call in calls {
        let result = ToolCallResult::error(
            serde_json::json!({
                "error": {
                    "code": code,
                    "message": message,
                }
            })
            .to_string(),
        );
        history.push_tool_result(&call.id, result.clone());
        sink.send(RuntimeEvent::ToolResult {
            call_id: call.id.clone(),
            result,
        })
        .await;
    }
}

fn cancelled_tool_result() -> ToolCallResult {
    ToolCallResult::error(
        serde_json::json!({
            "error": {
                "code": "cancelled",
                "message": "Tool execution cancelled"
            }
        })
        .to_string(),
    )
}

struct ChatRunResult {
    response: ChatResponse,
    streamed_text: bool,
}

async fn chat_with_provider_retries<P>(
    provider: Arc<P>,
    request: ChatRequest,
    config: &RuntimeConfig,
    cancel_rx: &mut Option<watch::Receiver<bool>>,
    sink: &mut (impl RuntimeEventSink + Send),
) -> Result<Option<ChatRunResult>, RuntimeError>
where
    P: LLMProvider,
{
    let mut retries = 0;
    let mut backoff = config.provider_retry_initial_backoff;
    loop {
        match chat_with_cancel(
            Arc::clone(&provider),
            request.clone(),
            config.llm_timeout,
            cancel_rx,
            sink,
        )
        .await
        {
            Err(RuntimeError::Provider(error))
                if error.is_retryable() && retries < config.max_provider_retries =>
            {
                retries += 1;
                tracing::warn!(
                    retries,
                    max_retries = config.max_provider_retries,
                    error = %error,
                    "retrying transient local runtime provider failure"
                );
                if sleep_with_cancel(backoff, cancel_rx).await {
                    return Ok(None);
                }
                backoff = backoff.saturating_mul(2);
            }
            result => return result,
        }
    }
}

async fn sleep_with_cancel(
    duration: std::time::Duration,
    cancel_rx: &mut Option<watch::Receiver<bool>>,
) -> bool {
    if let Some(cancel_rx) = cancel_rx.as_mut() {
        tokio::select! {
            _ = wait_cancelled(cancel_rx) => true,
            _ = tokio::time::sleep(duration) => false,
        }
    } else {
        tokio::time::sleep(duration).await;
        false
    }
}

fn tighten_context_budget(budget: &mut crate::config::ContextBudget) {
    let minimum = budget.reserved_output_tokens.saturating_add(256);
    budget.max_input_tokens = Some(
        budget
            .max_input_tokens
            .map(|current| current.saturating_mul(3) / 4)
            .unwrap_or(8_192)
            .max(minimum),
    );
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
