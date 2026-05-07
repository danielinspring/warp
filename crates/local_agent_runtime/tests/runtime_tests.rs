//! Integration tests for the local agent runtime.

use futures::StreamExt;
use local_agent_runtime::provider::{ChatRequest, ChatResponse, ChatStopReason, ChatStreamEvent};
use local_agent_runtime::{
    AgentRuntime, FinishReason, LLMProvider, Message, PermissionDecision, ProviderCapabilities,
    ProviderError, RuntimeConfig, RuntimeEvent, ToolCall, ToolCallResult, ToolExecutionError,
    ToolExecutor, ToolSchema, ToolSchemaBuilder,
};

/// A mock LLM provider that returns scripted responses.
struct MockProvider {
    responses: std::sync::Arc<std::sync::Mutex<Vec<ChatResponse>>>,
    requests: std::sync::Arc<std::sync::Mutex<Vec<ChatRequest>>>,
}

impl MockProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: std::sync::Arc::new(std::sync::Mutex::new(responses)),
            requests: Default::default(),
        }
    }

    /// Create a provider that returns a single text response.
    fn text_only(text: &str) -> Self {
        Self::new(vec![ChatResponse {
            text: text.to_string(),
            tool_calls: vec![],
            stop_reason: ChatStopReason::Stop,
        }])
    }

    /// Create a provider that first requests a tool call, then responds with text.
    fn with_tool_call(tool_name: &str, args: serde_json::Value, final_text: &str) -> Self {
        Self::new(vec![
            ChatResponse {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: tool_name.to_string(),
                    arguments: args,
                }],
                stop_reason: ChatStopReason::ToolUse,
            },
            ChatResponse {
                text: final_text.to_string(),
                tool_calls: vec![],
                stop_reason: ChatStopReason::Stop,
            },
        ])
    }
}

#[async_trait::async_trait]
impl LLMProvider for MockProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        self.requests.lock().unwrap().push(request);
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            return Err(ProviderError::EmptyResponse);
        }
        Ok(responses.remove(0))
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_calling: true,
            vision: false,
        }
    }

    fn name(&self) -> &str {
        "mock"
    }
}

/// A mock tool executor.
struct MockExecutor {
    tools: Vec<ToolSchema>,
    permission: PermissionDecision,
    result: ToolCallResult,
    calls: std::sync::Arc<std::sync::Mutex<Vec<ToolCall>>>,
}

impl MockExecutor {
    fn allow_all() -> Self {
        Self {
            tools: vec![
                ToolSchemaBuilder::new("run_shell_command", "Run a shell command")
                    .required_string("command", "The command to run")
                    .build(),
                ToolSchemaBuilder::new("read_file", "Read a file")
                    .required_string("path", "The file path to read")
                    .build(),
            ],
            permission: PermissionDecision::Allow,
            result: ToolCallResult::success("command output here"),
            calls: Default::default(),
        }
    }

    fn deny_all(reason: &str) -> Self {
        Self {
            tools: vec![],
            permission: PermissionDecision::Deny {
                reason: reason.to_string(),
            },
            result: ToolCallResult::error("should not reach here"),
            calls: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for MockExecutor {
    fn available_tools(&self) -> Vec<ToolSchema> {
        self.tools.clone()
    }

    async fn check_permission(&self, _call: &ToolCall) -> PermissionDecision {
        self.permission.clone()
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolCallResult, ToolExecutionError> {
        self.calls.lock().unwrap().push(call.clone());
        Ok(self.result.clone())
    }

    async fn on_permission_response(&self, _call: &ToolCall, granted: bool) -> PermissionDecision {
        if granted {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny {
                reason: "user denied".to_string(),
            }
        }
    }
}

struct SlowProvider;

#[async_trait::async_trait]
impl LLMProvider for SlowProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        Ok(ChatResponse {
            text: "too late".to_string(),
            tool_calls: vec![],
            stop_reason: ChatStopReason::Stop,
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    fn name(&self) -> &str {
        "slow"
    }
}

struct StreamingProvider {
    response: ChatResponse,
    deltas: Vec<String>,
}

impl StreamingProvider {
    fn text(deltas: &[&str]) -> Self {
        Self {
            response: ChatResponse {
                text: deltas.join(""),
                tool_calls: vec![],
                stop_reason: ChatStopReason::Stop,
            },
            deltas: deltas.iter().map(|delta| delta.to_string()).collect(),
        }
    }
}

#[async_trait::async_trait]
impl LLMProvider for StreamingProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        Err(ProviderError::EmptyResponse)
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
        event_tx: async_channel::Sender<ChatStreamEvent>,
    ) -> Result<ChatResponse, ProviderError> {
        for delta in &self.deltas {
            let _ = event_tx
                .send(ChatStreamEvent::TextDelta {
                    text: delta.clone(),
                })
                .await;
        }
        Ok(self.response.clone())
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            vision: false,
        }
    }

    fn name(&self) -> &str {
        "streaming"
    }
}

#[tokio::test]
async fn test_simple_text_response() {
    let provider = MockProvider::text_only("Hello! I can help you with that.");
    let executor = MockExecutor::allow_all();
    let config = RuntimeConfig::default();

    let runtime = AgentRuntime::new(provider, executor, config);
    let (events, _messages) = runtime
        .run_to_completion("test-model", vec![], "Hi there")
        .await
        .unwrap();

    // Should have: TurnStarted, TextCompleted, TurnCompleted, Finished
    assert!(events
        .iter()
        .any(|e| matches!(e, RuntimeEvent::TurnStarted { turn: 1 })));
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::TextCompleted { text } if text == "Hello! I can help you with that.")));
    assert!(events.iter().any(|e| matches!(
        e,
        RuntimeEvent::Finished {
            reason: FinishReason::Done
        }
    )));
}

#[tokio::test]
async fn test_streaming_text_response_emits_deltas_without_completed_text() {
    let provider = StreamingProvider::text(&["streamed ", "hello"]);
    let executor = MockExecutor::allow_all();
    let config = RuntimeConfig::default();

    let runtime = AgentRuntime::new(provider, executor, config);
    let (events, messages) = runtime
        .run_to_completion("test-model", vec![], "Hi there")
        .await
        .unwrap();

    let deltas = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deltas, vec!["streamed ", "hello"]);
    assert!(!events
        .iter()
        .any(|event| matches!(event, RuntimeEvent::TextCompleted { .. })));

    let Some(Message::Assistant(message)) = messages.last() else {
        panic!("expected final assistant message");
    };
    assert_eq!(message.content, "streamed hello");
}

#[tokio::test]
async fn test_tool_call_flow() {
    let provider = MockProvider::with_tool_call(
        "run_shell_command",
        serde_json::json!({"command": "ls /tmp"}),
        "Here are the files in /tmp.",
    );
    let executor = MockExecutor::allow_all();
    let config = RuntimeConfig::default();

    let runtime = AgentRuntime::new(provider, executor, config);
    let (events, _messages) = runtime
        .run_to_completion("test-model", vec![], "List files in /tmp")
        .await
        .unwrap();

    // Should have tool call events
    assert!(events
        .iter()
        .any(|e| matches!(e, RuntimeEvent::ToolCallsRequested { .. })));
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolExecutionStarted { tool_name, .. } if tool_name == "run_shell_command")));
    assert!(events
        .iter()
        .any(|e| matches!(e, RuntimeEvent::ToolResult { .. })));
    // Should have final text
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::TextCompleted { text } if text == "Here are the files in /tmp.")));
    assert!(events.iter().any(|e| matches!(
        e,
        RuntimeEvent::Finished {
            reason: FinishReason::Done
        }
    )));
}

#[tokio::test]
async fn test_run_streams_events() {
    let provider = MockProvider::text_only("streamed hello");
    let executor = MockExecutor::allow_all();
    let config = RuntimeConfig::default();

    let runtime = AgentRuntime::new(provider, executor, config);
    let (mut events, _cancel) =
        runtime.run("test-model".to_string(), vec![], "Hi there".to_string());

    let mut collected = Vec::new();
    while let Some(event) = tokio::time::timeout(std::time::Duration::from_secs(1), events.next())
        .await
        .unwrap()
    {
        let finished = matches!(event, RuntimeEvent::Finished { .. });
        collected.push(event);
        if finished {
            break;
        }
    }

    assert!(collected
        .iter()
        .any(|e| matches!(e, RuntimeEvent::TurnStarted { turn: 1 })));
    assert!(collected
        .iter()
        .any(|e| matches!(e, RuntimeEvent::TextCompleted { text } if text == "streamed hello")));
    assert!(collected.iter().any(|e| matches!(
        e,
        RuntimeEvent::Finished {
            reason: FinishReason::Done
        }
    )));
}

#[tokio::test]
async fn test_tool_schemas_are_sent_to_provider() {
    let provider = MockProvider::text_only("done");
    let requests = provider.requests.clone();
    let executor = MockExecutor::allow_all();
    let config = RuntimeConfig::default();

    let runtime = AgentRuntime::new(provider, executor, config);
    runtime
        .run_to_completion("test-model", vec![], "Hi there")
        .await
        .unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let tool_names = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(tool_names, vec!["run_shell_command", "read_file"]);
}

#[tokio::test]
async fn test_tool_result_is_fed_back_with_original_call_id() {
    let provider = MockProvider::with_tool_call(
        "run_shell_command",
        serde_json::json!({"command": "pwd"}),
        "The command ran.",
    );
    let requests = provider.requests.clone();
    let executor = MockExecutor::allow_all();
    let config = RuntimeConfig::default();

    let runtime = AgentRuntime::new(provider, executor, config);
    runtime
        .run_to_completion("test-model", vec![], "Run pwd")
        .await
        .unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|message| {
        matches!(
            message,
            Message::ToolResult(result)
                if result.call_id == "call_1" && result.result.content == "command output here"
        )
    }));
}

#[tokio::test]
async fn test_cancellation_stops_streaming_loop() {
    let provider = SlowProvider;
    let executor = MockExecutor::allow_all();
    let config = RuntimeConfig::default();

    let runtime = AgentRuntime::new(provider, executor, config);
    let (mut events, cancel) =
        runtime.run("test-model".to_string(), vec![], "Hi there".to_string());

    cancel.cancel();

    let mut saw_cancelled = false;
    while let Some(event) = tokio::time::timeout(std::time::Duration::from_secs(1), events.next())
        .await
        .unwrap()
    {
        if matches!(
            event,
            RuntimeEvent::Finished {
                reason: FinishReason::Cancelled
            }
        ) {
            saw_cancelled = true;
            break;
        }
    }

    assert!(saw_cancelled);
}

#[tokio::test]
async fn test_permission_denied_stops_when_configured() {
    let provider = MockProvider::with_tool_call(
        "run_shell_command",
        serde_json::json!({"command": "rm -rf /"}),
        "Done!",
    );
    let executor = MockExecutor::deny_all("dangerous command");
    let mut config = RuntimeConfig::default();
    config.stop_on_permission_denied = true;

    let runtime = AgentRuntime::new(provider, executor, config);
    let (events, _messages) = runtime
        .run_to_completion("test-model", vec![], "Delete everything")
        .await
        .unwrap();

    // Should finish without calling the final text turn
    assert!(events.iter().any(|e| matches!(
        e,
        RuntimeEvent::Finished {
            reason: FinishReason::Done
        }
    )));
    // Should NOT have the "Done!" text
    assert!(!events
        .iter()
        .any(|e| matches!(e, RuntimeEvent::TextCompleted { text } if text == "Done!")));
}

#[tokio::test]
async fn test_max_turns_exceeded() {
    // Provider always returns tool calls — will never naturally stop
    let responses: Vec<ChatResponse> = (0..30)
        .map(|_| ChatResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: "run_shell_command".to_string(),
                arguments: serde_json::json!({"command": "echo hi"}),
            }],
            stop_reason: ChatStopReason::ToolUse,
        })
        .collect();

    let provider = MockProvider::new(responses);
    let executor = MockExecutor::allow_all();
    let mut config = RuntimeConfig::default();
    config.max_turns = 3;

    let runtime = AgentRuntime::new(provider, executor, config);
    let (events, _messages) = runtime
        .run_to_completion("test-model", vec![], "Loop forever")
        .await
        .unwrap();

    assert!(events.iter().any(|e| matches!(
        e,
        RuntimeEvent::Finished {
            reason: FinishReason::MaxTurns
        }
    )));
}

#[tokio::test]
async fn test_provider_error_propagates() {
    let provider = MockProvider::new(vec![]); // Empty = EmptyResponse error
    let executor = MockExecutor::allow_all();
    let config = RuntimeConfig::default();

    let runtime = AgentRuntime::new(provider, executor, config);
    let result = runtime
        .run_to_completion("test-model", vec![], "Hello")
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_conversation_history_preserved() {
    let provider = MockProvider::text_only("I see the prior context.");
    let executor = MockExecutor::allow_all();
    let config = RuntimeConfig {
        system_prompt: Some("You are helpful.".to_string()),
        ..Default::default()
    };

    let initial_messages = vec![
        Message::User(local_agent_runtime::messages::UserMessage {
            content: "Previous question".to_string(),
        }),
        Message::Assistant(local_agent_runtime::messages::AssistantMessage {
            content: "Previous answer".to_string(),
            tool_calls: vec![],
        }),
    ];

    let runtime = AgentRuntime::new(provider, executor, config);
    let (events, messages) = runtime
        .run_to_completion("test-model", initial_messages, "Follow-up")
        .await
        .unwrap();

    // Should succeed
    assert!(events.iter().any(|e| matches!(
        e,
        RuntimeEvent::Finished {
            reason: FinishReason::Done
        }
    )));
    // Messages should include system + initial + user + assistant reply
    assert!(messages.len() >= 5); // system + 2 initial + user + assistant
}
