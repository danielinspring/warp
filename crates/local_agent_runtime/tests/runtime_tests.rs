//! Integration tests for the local agent runtime.

use futures::StreamExt;
use local_agent_runtime::provider::ollama::{OllamaProvider, OllamaProviderConfig};
use local_agent_runtime::provider::{ChatRequest, ChatResponse, ChatStopReason, ChatStreamEvent};
use local_agent_runtime::{
    AgentRuntime, FinishReason, LLMProvider, Message, PermissionDecision, ProviderCapabilities,
    ProviderError, RuntimeConfig, RuntimeEvent, ToolCall, ToolCallResult, ToolExecutionError,
    ToolExecutor, ToolSafetyClass, ToolSchema, ToolSchemaBuilder,
};

/// A mock LLM provider that returns scripted responses.
struct MockProvider {
    responses: std::sync::Arc<std::sync::Mutex<Vec<ChatResponse>>>,
    requests: std::sync::Arc<std::sync::Mutex<Vec<ChatRequest>>>,
}

struct RecoveringProvider {
    failures_remaining: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    error_kind: RecoveringError,
    requests: std::sync::Arc<std::sync::Mutex<Vec<ChatRequest>>>,
}

#[derive(Clone, Copy)]
enum RecoveringError {
    Transient,
    ContextWindowExceeded,
}

#[async_trait::async_trait]
impl LLMProvider for RecoveringProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        self.requests.lock().unwrap().push(request);
        if self
            .failures_remaining
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |remaining| remaining.checked_sub(1),
            )
            .is_ok()
        {
            return Err(match self.error_kind {
                RecoveringError::Transient => ProviderError::Transient {
                    message: "temporary".to_string(),
                },
                RecoveringError::ContextWindowExceeded => ProviderError::ContextWindowExceeded {
                    message: "too many tokens".to_string(),
                },
            });
        }
        Ok(ChatResponse {
            text: "recovered".to_string(),
            tool_calls: vec![],
            stop_reason: ChatStopReason::Stop,
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_calling: true,
            vision: false,
        }
    }

    fn name(&self) -> &str {
        "recovering"
    }
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
    safety_class: ToolSafetyClass,
    delay: std::time::Duration,
}

struct LiveParityExecutor {
    calls: std::sync::Arc<std::sync::Mutex<Vec<ToolCall>>>,
}

#[async_trait::async_trait]
impl ToolExecutor for LiveParityExecutor {
    fn available_tools(&self) -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: "parity_step".to_string(),
            description:
                "Record exactly one numbered parity step. Call steps 1 through 15 in order."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "step": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 15,
                    }
                },
                "required": ["step"],
                "additionalProperties": false,
            }),
        }]
    }

    fn safety_class(&self, _tool_name: &str) -> ToolSafetyClass {
        ToolSafetyClass::Interactive
    }

    async fn check_permission(&self, _call: &ToolCall) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolCallResult, ToolExecutionError> {
        let mut calls = self.calls.lock().unwrap();
        let expected = calls.len() + 1;
        let actual = call.arguments["step"].as_u64().unwrap_or_default() as usize;
        if actual != expected || actual > 15 {
            return Ok(ToolCallResult::error(format!(
                "Expected step {expected}, received {actual}"
            )));
        }
        calls.push(call.clone());
        Ok(ToolCallResult::success(format!(
            "Step {actual} accepted. {}",
            if actual == 15 {
                "All steps are complete; respond with PARITY_DONE and do not call more tools."
            } else {
                "Call parity_step with the next step number."
            }
        )))
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
            safety_class: ToolSafetyClass::Interactive,
            delay: std::time::Duration::ZERO,
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
            safety_class: ToolSafetyClass::Interactive,
            delay: std::time::Duration::ZERO,
        }
    }

    fn with_safety_class(mut self, safety_class: ToolSafetyClass) -> Self {
        self.safety_class = safety_class;
        self
    }

    fn with_delay(mut self, delay: std::time::Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[async_trait::async_trait]
impl ToolExecutor for MockExecutor {
    fn available_tools(&self) -> Vec<ToolSchema> {
        self.tools.clone()
    }

    fn safety_class(&self, _tool_name: &str) -> ToolSafetyClass {
        self.safety_class
    }

    async fn check_permission(&self, _call: &ToolCall) -> PermissionDecision {
        self.permission.clone()
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolCallResult, ToolExecutionError> {
        tokio::time::sleep(self.delay).await;
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
async fn test_system_prompt_is_prepended_to_runtime_history() {
    let provider = MockProvider::text_only("done");
    let requests = provider.requests.clone();
    let executor = MockExecutor::allow_all();
    let prompt = "dynamic prompt with cwd /repo".to_string();
    let config = RuntimeConfig {
        system_prompt: Some(prompt.clone()),
        ..Default::default()
    };
    let initial_messages = vec![Message::User(local_agent_runtime::messages::UserMessage {
        content: "Earlier context".to_string(),
    })];

    let runtime = AgentRuntime::new(provider, executor, config);
    let (_events, messages) = runtime
        .run_to_completion("test-model", initial_messages, "Current request")
        .await
        .unwrap();

    let Message::System(system_message) = &messages[0] else {
        panic!("expected final runtime history to start with system prompt");
    };
    assert_eq!(system_message.content, prompt);

    let requests = requests.lock().unwrap();
    let Message::System(request_system_message) = &requests[0].messages[0] else {
        panic!("expected provider request to start with system prompt");
    };
    assert_eq!(request_system_message.content, prompt);
    assert!(matches!(
        &requests[0].messages[1],
        Message::User(message) if message.content == "Earlier context"
    ));
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
async fn test_pre_tool_lifecycle_hook_denies_without_executor() {
    use std::sync::Arc;

    use local_agent_runtime::ToolNameDenyHooks;

    let provider = MockProvider::with_tool_call(
        "run_shell_command",
        serde_json::json!({"command": "pwd"}),
        "should not reach",
    );
    let executor = MockExecutor::allow_all();
    let runtime = AgentRuntime::new(provider, executor, RuntimeConfig::default()).with_hooks(
        Arc::new(ToolNameDenyHooks {
            denied_tools: vec!["run_shell_command".to_string()],
        }),
    );
    let (events, _messages) = runtime
        .run_to_completion("test-model", vec![], "run pwd")
        .await
        .unwrap();

    assert!(events.iter().any(|e| matches!(
        e,
        RuntimeEvent::ToolResult { result, .. }
            if result.is_error && result.content.contains("blocked by trusted lifecycle policy")
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        RuntimeEvent::Finished {
            reason: FinishReason::Done
        }
    )));
}

#[tokio::test]
async fn test_permission_denied_stops_when_configured() {
    let provider = MockProvider::with_tool_call(
        "run_shell_command",
        serde_json::json!({"command": "rm -rf /"}),
        "Done!",
    );
    let executor = MockExecutor::deny_all("dangerous command");
    let config = RuntimeConfig {
        stop_on_permission_denied: true,
        ..Default::default()
    };

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
    let config = RuntimeConfig {
        max_turns: 3,
        max_repeated_tool_cycles: 10,
        ..Default::default()
    };

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

#[tokio::test]
async fn test_available_tools_refresh_each_provider_turn() {
    struct RefreshingExecutor {
        calls: std::sync::Arc<std::sync::Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl ToolExecutor for RefreshingExecutor {
        fn available_tools(&self) -> Vec<ToolSchema> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            let name = if *calls == 1 {
                "first_tool"
            } else {
                "second_tool"
            };
            vec![ToolSchemaBuilder::new(name, "dynamic tool").build()]
        }

        async fn check_permission(&self, _call: &ToolCall) -> PermissionDecision {
            PermissionDecision::Allow
        }

        async fn execute(&self, _call: &ToolCall) -> Result<ToolCallResult, ToolExecutionError> {
            Ok(ToolCallResult::success("ok"))
        }

        async fn on_permission_response(
            &self,
            _call: &ToolCall,
            granted: bool,
        ) -> PermissionDecision {
            if granted {
                PermissionDecision::Allow
            } else {
                PermissionDecision::Deny {
                    reason: "user denied".to_string(),
                }
            }
        }
    }

    let provider = MockProvider::with_tool_call("first_tool", serde_json::json!({}), "done");
    let requests = provider.requests.clone();
    let executor = RefreshingExecutor {
        calls: Default::default(),
    };

    let runtime = AgentRuntime::new(provider, executor, RuntimeConfig::default());
    runtime
        .run_to_completion("test-model", vec![], "use tools")
        .await
        .unwrap();

    let requests = requests.lock().unwrap();
    let first_turn_tools = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    let second_turn_tools = requests[1]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(first_turn_tools, vec!["first_tool"]);
    assert_eq!(second_turn_tools, vec!["second_tool"]);
}

#[tokio::test]
async fn test_read_only_tool_calls_execute_concurrently_and_preserve_result_order() {
    let responses = vec![
        ChatResponse {
            text: String::new(),
            tool_calls: vec![
                ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({}),
                },
                ToolCall {
                    id: "call_2".to_string(),
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({}),
                },
            ],
            stop_reason: ChatStopReason::ToolUse,
        },
        ChatResponse {
            text: "done".to_string(),
            tool_calls: vec![],
            stop_reason: ChatStopReason::Stop,
        },
    ];
    let provider = MockProvider::new(responses);
    let executor = MockExecutor::allow_all()
        .with_safety_class(ToolSafetyClass::ReadOnly)
        .with_delay(std::time::Duration::from_millis(120));
    let runtime = AgentRuntime::new(provider, executor, RuntimeConfig::default());

    let started = instant::Instant::now();
    let (events, _messages) = runtime
        .run_to_completion("test-model", vec![], "read twice")
        .await
        .unwrap();

    assert!(
        started.elapsed() < std::time::Duration::from_millis(220),
        "read-only tools should run as one concurrent batch"
    );
    let result_ids = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(result_ids, vec!["call_1", "call_2"]);
}

#[tokio::test]
async fn test_mutating_tool_calls_remain_serial() {
    let responses = vec![
        ChatResponse {
            text: String::new(),
            tool_calls: vec![
                ToolCall {
                    id: "call_1".to_string(),
                    name: "edit_files".to_string(),
                    arguments: serde_json::json!({}),
                },
                ToolCall {
                    id: "call_2".to_string(),
                    name: "edit_files".to_string(),
                    arguments: serde_json::json!({}),
                },
            ],
            stop_reason: ChatStopReason::ToolUse,
        },
        ChatResponse {
            text: "done".to_string(),
            tool_calls: vec![],
            stop_reason: ChatStopReason::Stop,
        },
    ];
    let provider = MockProvider::new(responses);
    let executor = MockExecutor::allow_all()
        .with_safety_class(ToolSafetyClass::Mutating)
        .with_delay(std::time::Duration::from_millis(100));
    let runtime = AgentRuntime::new(provider, executor, RuntimeConfig::default());

    let started = instant::Instant::now();
    runtime
        .run_to_completion("test-model", vec![], "edit twice")
        .await
        .unwrap();

    assert!(
        started.elapsed() >= std::time::Duration::from_millis(190),
        "mutating tools should execute serially"
    );
}

#[tokio::test]
async fn test_cancellation_after_tool_calls_emits_paired_cancelled_results() {
    let provider = MockProvider::new(vec![ChatResponse {
        text: String::new(),
        tool_calls: vec![
            ToolCall {
                id: "call_1".to_string(),
                name: "edit_files".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "call_2".to_string(),
                name: "edit_files".to_string(),
                arguments: serde_json::json!({}),
            },
        ],
        stop_reason: ChatStopReason::ToolUse,
    }]);
    let executor = MockExecutor::allow_all().with_delay(std::time::Duration::from_secs(60));
    let runtime = AgentRuntime::new(provider, executor, RuntimeConfig::default());
    let (mut events, cancel) =
        runtime.run("test-model".to_string(), vec![], "edit twice".to_string());

    let mut collected = Vec::new();
    while let Some(event) = tokio::time::timeout(std::time::Duration::from_secs(1), events.next())
        .await
        .unwrap()
    {
        if matches!(event, RuntimeEvent::ToolCallsRequested { .. }) {
            cancel.cancel();
        }
        let finished = matches!(event, RuntimeEvent::Finished { .. });
        collected.push(event);
        if finished {
            break;
        }
    }

    let cancelled_results = collected
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ToolResult { call_id, result } if result.is_error => {
                Some((call_id.as_str(), result.content.as_str()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cancelled_results.len(), 2);
    assert_eq!(cancelled_results[0].0, "call_1");
    assert_eq!(cancelled_results[1].0, "call_2");
    assert!(cancelled_results
        .iter()
        .all(|(_, content)| content.contains("\"code\":\"cancelled\"")));
    assert!(collected.iter().any(|event| matches!(
        event,
        RuntimeEvent::Finished {
            reason: FinishReason::Cancelled
        }
    )));
}

#[tokio::test]
async fn test_context_budget_truncates_only_model_facing_tool_results() {
    let provider = MockProvider::with_tool_call("read_file", serde_json::json!({}), "done");
    let requests = provider.requests.clone();
    let executor = MockExecutor {
        result: ToolCallResult::success("abcdefghijkl"),
        ..MockExecutor::allow_all().with_safety_class(ToolSafetyClass::ReadOnly)
    };
    let mut config = RuntimeConfig::default();
    config.context_budget.max_tool_result_chars = 5;
    let runtime = AgentRuntime::new(provider, executor, config);

    let (_events, messages) = runtime
        .run_to_completion("test-model", vec![], "read")
        .await
        .unwrap();

    assert!(messages.iter().any(|message| {
        matches!(
            message,
            Message::ToolResult(result) if result.result.content == "abcdefghijkl"
        )
    }));

    let requests = requests.lock().unwrap();
    let second_turn_tool_result = requests[1]
        .messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) => Some(result.result.content.as_str()),
            _ => None,
        });
    let content = second_turn_tool_result.expect("expected tool result in second turn");
    assert!(content.contains("\"truncated\":true"));
    assert!(content.contains("\"original_chars\":12"));
    assert!(content.contains("\"kept_chars\":5"));
    assert!(content.contains("\"content\":\"abcde\""));
}

#[tokio::test]
async fn test_transient_provider_failure_retries_with_bounded_backoff() {
    let requests = Default::default();
    let provider = RecoveringProvider {
        failures_remaining: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1)),
        error_kind: RecoveringError::Transient,
        requests: std::sync::Arc::clone(&requests),
    };
    let config = RuntimeConfig {
        provider_retry_initial_backoff: std::time::Duration::ZERO,
        max_provider_retries: 2,
        ..Default::default()
    };
    let runtime = AgentRuntime::new(provider, MockExecutor::allow_all(), config);

    let (_events, messages) = runtime
        .run_to_completion("test-model", vec![], "hello")
        .await
        .unwrap();

    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(matches!(
        messages.last(),
        Some(Message::Assistant(message)) if message.content == "recovered"
    ));
}

#[tokio::test]
async fn test_context_overflow_tightens_budget_and_retries_once() {
    let requests = Default::default();
    let provider = RecoveringProvider {
        failures_remaining: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1)),
        error_kind: RecoveringError::ContextWindowExceeded,
        requests: std::sync::Arc::clone(&requests),
    };
    let mut config = RuntimeConfig::default();
    config.context_budget.max_input_tokens = Some(4_096);
    let runtime = AgentRuntime::new(provider, MockExecutor::allow_all(), config);

    let (_events, _) = runtime
        .run_to_completion("test-model", vec![], "hello")
        .await
        .unwrap();

    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn test_max_token_responses_continue_without_repeating_completed_text() {
    let provider = MockProvider::new(vec![
        ChatResponse {
            text: "part one".to_string(),
            tool_calls: vec![],
            stop_reason: ChatStopReason::MaxTokens,
        },
        ChatResponse {
            text: "part two".to_string(),
            tool_calls: vec![],
            stop_reason: ChatStopReason::MaxTokens,
        },
        ChatResponse {
            text: "part three".to_string(),
            tool_calls: vec![],
            stop_reason: ChatStopReason::Stop,
        },
    ]);
    let runtime = AgentRuntime::new(
        provider,
        MockExecutor::allow_all(),
        RuntimeConfig::default(),
    );

    let (events, messages) = runtime
        .run_to_completion("test-model", vec![], "write")
        .await
        .unwrap();

    let completed_text = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::TextCompleted { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed_text, vec!["part one", "part two", "part three"]);
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, Message::Assistant(_)))
            .count(),
        3
    );
}

#[tokio::test]
async fn test_repeated_tool_call_cycles_stop_with_paired_error_result() {
    let repeated_response = ChatResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": "a.rs" }),
        }],
        stop_reason: ChatStopReason::ToolUse,
    };
    let provider = MockProvider::new(vec![
        repeated_response.clone(),
        repeated_response.clone(),
        repeated_response,
    ]);
    let executor = MockExecutor::allow_all();
    let executed_calls = executor.calls.clone();
    let config = RuntimeConfig {
        max_repeated_tool_cycles: 3,
        ..Default::default()
    };
    let runtime = AgentRuntime::new(provider, executor, config);

    let error = runtime
        .run_to_completion("test-model", vec![], "loop")
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        local_agent_runtime::RuntimeError::RepeatedToolCallStall { repeated_cycles: 3 }
    ));
    assert_eq!(executed_calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn test_fifteen_tool_call_parity_run_preserves_every_pair() {
    let mut responses = (0..15)
        .map(|index| ChatResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: format!("call_{index}"),
                name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": format!("{index}.rs") }),
            }],
            stop_reason: ChatStopReason::ToolUse,
        })
        .collect::<Vec<_>>();
    responses.push(ChatResponse {
        text: "done".to_string(),
        tool_calls: vec![],
        stop_reason: ChatStopReason::Stop,
    });
    let provider = MockProvider::new(responses);
    let executor = MockExecutor::allow_all().with_safety_class(ToolSafetyClass::ReadOnly);
    let executed_calls = executor.calls.clone();
    let runtime = AgentRuntime::new(
        provider,
        executor,
        RuntimeConfig {
            max_repeated_tool_cycles: 2,
            ..Default::default()
        },
    );

    let (_events, messages) = runtime
        .run_to_completion("test-model", vec![], "inspect")
        .await
        .unwrap();

    assert_eq!(executed_calls.lock().unwrap().len(), 15);
    let call_ids = messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(message) => Some(
                message
                    .tool_calls
                    .iter()
                    .map(|call| call.id.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect::<std::collections::HashSet<_>>();
    let result_ids = messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(message) => Some(message.call_id.clone()),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(call_ids.len(), 15);
    assert_eq!(call_ids, result_ids);
}

#[tokio::test]
#[ignore = "requires a live Ollama endpoint and model"]
async fn manual_live_ollama_fifteen_tool_call_parity() {
    let base_url =
        std::env::var("OLLAMA_BASE_URL").expect("OLLAMA_BASE_URL must identify the test endpoint");
    let model = std::env::var("OLLAMA_MODEL").expect("OLLAMA_MODEL must identify the pinned model");
    let provider = OllamaProvider::new(OllamaProviderConfig {
        base_url,
        api_key: None,
        timeout_secs: 120,
        ..Default::default()
    });
    let calls = Default::default();
    let executor = LiveParityExecutor {
        calls: std::sync::Arc::clone(&calls),
    };
    let runtime = AgentRuntime::new(
        provider,
        executor,
        RuntimeConfig {
            max_turns: 30,
            max_repeated_tool_cycles: 3,
            context_budget: local_agent_runtime::ContextBudget {
                max_input_tokens: Some(262_144),
                ..Default::default()
            },
            system_prompt: Some(
                "You are running a tool-loop parity test. Call parity_step sequentially with step 1 through step 15. Call exactly one step at a time. After step 15 succeeds, respond only PARITY_DONE."
                    .to_string(),
            ),
            ..Default::default()
        },
    );

    let (_events, messages) = runtime
        .run_to_completion(&model, vec![], "Begin the parity test now.")
        .await
        .unwrap();

    assert_eq!(calls.lock().unwrap().len(), 15);
    assert!(matches!(
        messages.last(),
        Some(Message::Assistant(message)) if message.content.contains("PARITY_DONE")
    ));
}
