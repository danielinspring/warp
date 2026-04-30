//! Core agent runtime — the event-driven loop that drives LLM↔tool interactions.
//!
//! The runtime is generic over `LLMProvider` and `ToolExecutor`, making it
//! testable and extractable to a separate service later.

use futures::channel::mpsc;
use tokio::sync::watch;

use crate::config::RuntimeConfig;
use crate::error::RuntimeError;
use crate::events::{FinishReason, RuntimeEvent, StopReason};
use crate::messages::{ConversationHistory, Message};
use crate::messages::normalize::truncate_tool_results;
use crate::provider::{ChatRequest, ChatStopReason, LLMProvider};
use crate::tools::{PermissionDecision, ToolCallResult, ToolExecutor};

/// The local agent runtime.
///
/// Drives the LLM→tool→result loop, yielding [`RuntimeEvent`]s
/// to the caller via a channel.
pub struct AgentRuntime<P: LLMProvider, T: ToolExecutor> {
    provider: P,
    executor: T,
    config: RuntimeConfig,
}

impl<P: LLMProvider, T: ToolExecutor> AgentRuntime<P, T> {
    pub fn new(provider: P, executor: T, config: RuntimeConfig) -> Self {
        Self {
            provider,
            executor,
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
        let tools = self.executor.available_tools();

        // We can't move self into the task, so we need the provider and executor
        // to be Arc'd or the runtime to be consumed. For now, we use a design
        // where `run` takes ownership-like semantics via the struct fields
        // being behind Arc internally in real usage. For the initial scaffold,
        // we'll work synchronously.
        //
        // In practice, the caller will wrap P and T in Arc. For now, provide
        // `run_to_completion` as the simpler sync-style API.
        let _ = (tx, cancel_rx, config, tools, model, initial_messages, user_input);

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
        let mut history = ConversationHistory::new();

        // Load system prompt
        if let Some(ref prompt) = self.config.system_prompt {
            history = ConversationHistory::with_system_prompt(prompt.clone());
        }

        // Load initial messages into history
        for msg in initial_messages {
            history.messages_mut().push(msg);
        }

        // Add the new user input
        history.push_user(user_input);

        let tools = self.executor.available_tools();
        let mut events: Vec<RuntimeEvent> = Vec::new();
        let mut turn: u32 = 0;

        loop {
            turn += 1;
            if turn > self.config.max_turns {
                events.push(RuntimeEvent::Finished {
                    reason: FinishReason::MaxTurns,
                });
                return Ok((events, history.messages().to_vec()));
            }

            events.push(RuntimeEvent::TurnStarted { turn });

            // Truncate large tool results before sending to LLM
            truncate_tool_results(&mut history, self.config.max_tool_result_chars);

            // Call the LLM
            let request = ChatRequest {
                model: model.to_string(),
                messages: history.messages().to_vec(),
                tools: tools.clone(),
            };

            let response = self.provider.chat(request).await.map_err(RuntimeError::Provider)?;

            // Emit text if present
            if !response.text.is_empty() {
                events.push(RuntimeEvent::TextCompleted {
                    text: response.text.clone(),
                });
            }

            // If no tool calls, we're done
            if response.tool_calls.is_empty() {
                history.push_assistant(response.text, vec![]);
                events.push(RuntimeEvent::TurnCompleted {
                    reason: match response.stop_reason {
                        ChatStopReason::MaxTokens => StopReason::MaxTokens,
                        _ => StopReason::EndTurn,
                    },
                });
                events.push(RuntimeEvent::Finished {
                    reason: FinishReason::Done,
                });
                return Ok((events, history.messages().to_vec()));
            }

            // We have tool calls
            events.push(RuntimeEvent::ToolCallsRequested {
                calls: response.tool_calls.clone(),
            });
            events.push(RuntimeEvent::TurnCompleted {
                reason: StopReason::ToolUse,
            });

            // Record assistant message with tool calls
            history.push_assistant(response.text, response.tool_calls.clone());

            // Execute each tool call
            let mut should_stop = false;
            for call in &response.tool_calls {
                // Check permission
                let permission = self.executor.check_permission(call).await;

                match permission {
                    PermissionDecision::Allow => {
                        events.push(RuntimeEvent::ToolExecutionStarted {
                            call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                        });

                        match self.executor.execute(call).await {
                            Ok(result) => {
                                events.push(RuntimeEvent::ToolResult {
                                    call_id: call.id.clone(),
                                    result: result.clone(),
                                });
                                history.push_tool_result(&call.id, result);
                            }
                            Err(e) => {
                                let error_result = ToolCallResult::error(format!(
                                    "Tool execution failed: {}",
                                    e
                                ));
                                events.push(RuntimeEvent::ToolResult {
                                    call_id: call.id.clone(),
                                    result: error_result.clone(),
                                });
                                history.push_tool_result(&call.id, error_result);
                            }
                        }
                    }
                    PermissionDecision::Ask => {
                        // Emit permission required event
                        events.push(RuntimeEvent::PermissionRequired { call: call.clone() });

                        // In the async/channel version, we'd wait for a response.
                        // In run_to_completion, we treat Ask as Deny for now
                        // (the caller should use the channel-based `run` for interactive flows).
                        let error_result = ToolCallResult::error(
                            "Permission required — tool execution skipped in non-interactive mode",
                        );
                        history.push_tool_result(&call.id, error_result.clone());
                        events.push(RuntimeEvent::ToolResult {
                            call_id: call.id.clone(),
                            result: error_result,
                        });

                        if self.config.stop_on_permission_denied {
                            should_stop = true;
                            break;
                        }
                    }
                    PermissionDecision::Deny { reason } => {
                        let error_result =
                            ToolCallResult::error(format!("Permission denied: {}", reason));
                        history.push_tool_result(&call.id, error_result.clone());
                        events.push(RuntimeEvent::ToolResult {
                            call_id: call.id.clone(),
                            result: error_result,
                        });

                        if self.config.stop_on_permission_denied {
                            should_stop = true;
                            break;
                        }
                    }
                }
            }

            if should_stop {
                events.push(RuntimeEvent::Finished {
                    reason: FinishReason::Done,
                });
                return Ok((events, history.messages().to_vec()));
            }

            // Continue the loop — feed tool results back to the LLM
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
