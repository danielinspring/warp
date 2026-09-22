use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use local_agent_runtime::{
    ChatRequest, ChatResponse, ChatStopReason, FinishReason, LLMProvider, LifecycleHooks, Message,
    ProviderCapabilities, ProviderError, ToolCall,
};
use tokio::sync::Notify;

use super::*;
use crate::provider::ProviderConfig;
use crate::test_support::{
    query_request, request, shell_call, shell_result, task, tool_call_message, tool_call_result,
    user_query_message,
};

/// Replays scripted responses in order across every provider the factory creates.
#[derive(Clone, Default)]
struct Scripted {
    responses: Arc<Mutex<VecDeque<ChatResponse>>>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    delay: Duration,
}

impl Scripted {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into())),
            ..Default::default()
        }
    }

    fn slow(delay: Duration) -> Self {
        Self {
            delay,
            ..Self::new(vec![text("too late")])
        }
    }
}

impl ProviderFactory for Scripted {
    fn create(&self, _config: &ProviderConfig) -> Box<dyn LLMProvider> {
        Box::new(self.clone())
    }
}

#[async_trait::async_trait]
impl LLMProvider for Scripted {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        tokio::time::sleep(self.delay).await;
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(ProviderError::EmptyResponse)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_calling: true,
            vision: false,
        }
    }

    fn name(&self) -> &str {
        "scripted"
    }
}

fn text(text: &str) -> ChatResponse {
    ChatResponse {
        text: text.to_string(),
        tool_calls: vec![],
        stop_reason: ChatStopReason::Stop,
    }
}

fn tool_use(calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        text: String::new(),
        tool_calls: calls,
        stop_reason: ChatStopReason::ToolUse,
    }
}

#[derive(Default)]
struct StopObserver {
    reason: Mutex<Option<FinishReason>>,
    stopped: Notify,
}

#[async_trait::async_trait]
impl LifecycleHooks for StopObserver {
    async fn on_stop(&self, reason: &FinishReason) {
        *self.reason.lock().unwrap() = Some(reason.clone());
        self.stopped.notify_one();
    }
}

async fn start(state: ServerState) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        serve(listener, state).await.unwrap();
    });
    format!("http://{address}")
}

async fn start_scripted(provider: Scripted) -> String {
    start(ServerState::new(Arc::new(provider))).await
}

async fn post_events(
    base_url: &str,
    path: &str,
    request: &api::Request,
) -> Vec<api::ResponseEvent> {
    let body = reqwest::Client::new()
        .post(format!("{base_url}{path}"))
        .header("content-type", "application/x-protobuf")
        .body(request.encode_to_vec())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    parse_sse(&body)
}

fn parse_sse(body: &str) -> Vec<api::ResponseEvent> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| {
            let bytes = BASE64_URL_SAFE
                .decode(data.trim().trim_matches('"'))
                .expect("SSE data is URL-safe base64");
            api::ResponseEvent::decode(bytes.as_slice()).expect("SSE data is a ResponseEvent")
        })
        .collect()
}

fn kinds(events: &[api::ResponseEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match &event.r#type {
            Some(api::response_event::Type::Init(_)) => "init",
            Some(api::response_event::Type::ClientActions(_)) => "actions",
            Some(api::response_event::Type::Finished(_)) => "finished",
            None => "empty",
        })
        .collect()
}

fn actions(events: &[api::ResponseEvent]) -> Vec<&api::client_action::Action> {
    events
        .iter()
        .filter_map(|event| match &event.r#type {
            Some(api::response_event::Type::ClientActions(actions)) => Some(&actions.actions),
            _ => None,
        })
        .flatten()
        .filter_map(|action| action.action.as_ref())
        .collect()
}

fn messages(events: &[api::ResponseEvent]) -> Vec<&api::Message> {
    actions(events)
        .into_iter()
        .filter_map(|action| match action {
            api::client_action::Action::AddMessagesToTask(add) => Some(&add.messages),
            _ => None,
        })
        .flatten()
        .collect()
}

fn agent_outputs(events: &[api::ResponseEvent]) -> Vec<String> {
    messages(events)
        .into_iter()
        .filter_map(|message| match &message.message {
            Some(api::message::Message::AgentOutput(output)) => Some(output.text.clone()),
            _ => None,
        })
        .collect()
}

fn finished_reason(events: &[api::ResponseEvent]) -> &api::response_event::stream_finished::Reason {
    let Some(api::response_event::Type::Finished(finished)) = &events.last().unwrap().r#type else {
        panic!("expected the stream to end with Finished");
    };
    finished.reason.as_ref().unwrap()
}

fn assert_done(events: &[api::ResponseEvent]) {
    assert!(matches!(
        finished_reason(events),
        api::response_event::stream_finished::Reason::Done(_)
    ));
}

#[tokio::test]
async fn text_only_turn_streams_init_task_output_and_done() {
    let provider = Scripted::new(vec![text("Hello!")]);
    let base_url = start_scripted(provider.clone()).await;

    let events = post_events(&base_url, "/ai/multi-agent", &query_request("Hi")).await;

    assert_eq!(kinds(&events), vec!["init", "actions", "finished"]);
    let Some(api::response_event::Type::Init(init)) = &events[0].r#type else {
        panic!("expected Init");
    };
    assert_eq!(init.conversation_id, "conversation_1");
    assert!(
        actions(&events)
            .iter()
            .any(|action| matches!(action, api::client_action::Action::CreateTask(_)))
    );
    assert_eq!(agent_outputs(&events), vec!["Hello!"]);
    assert_done(&events);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn client_tool_call_ends_the_stream_with_a_tool_call_message() {
    let provider = Scripted::new(vec![
        tool_use(vec![shell_call("call_1", "ls")]),
        text("must wait for the client"),
    ]);
    let base_url = start_scripted(provider.clone()).await;

    let events = post_events(&base_url, "/ai/multi-agent", &query_request("List files")).await;

    assert_eq!(kinds(&events), vec!["init", "actions", "finished"]);
    let persisted = messages(&events);
    assert_eq!(persisted.len(), 1);
    let Some(api::message::Message::ToolCall(tool_call)) = &persisted[0].message else {
        panic!("expected a ToolCall message");
    };
    assert_eq!(tool_call.tool_call_id, "call_1");
    assert!(matches!(
        tool_call.tool,
        Some(api::message::tool_call::Tool::RunShellCommand(_))
    ));
    assert!(agent_outputs(&events).is_empty());
    assert_done(&events);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn tool_result_continuation_echoes_the_result_then_answers() {
    let provider = Scripted::new(vec![text("a and b")]);
    let base_url = start_scripted(provider.clone()).await;
    let call = shell_call("call_1", "ls");
    let mut continuation = request(vec![tool_call_result(
        "call_1",
        Some(shell_result("ls", "a\nb\n", 0)),
    )]);
    continuation.task_context = Some(api::request::TaskContext {
        tasks: vec![task(
            "task_1",
            vec![
                user_query_message("task_1", "List files", None),
                tool_call_message("task_1", &call, None),
            ],
        )],
    });

    let events = post_events(&base_url, "/ai/multi-agent", &continuation).await;

    assert_eq!(
        kinds(&events),
        vec!["init", "actions", "actions", "finished"]
    );
    let persisted = messages(&events);
    let Some(api::message::Message::ToolCallResult(result)) = &persisted[0].message else {
        panic!("expected the echoed ToolCallResult first");
    };
    assert_eq!(result.tool_call_id, "call_1");
    assert!(matches!(
        result.result,
        Some(api::message::tool_call_result::Result::RunShellCommand(_))
    ));
    assert!(!persisted[0].server_message_data.is_empty());
    assert_eq!(persisted[0].task_id, "task_1");
    assert!(
        !actions(&events)
            .iter()
            .any(|action| matches!(action, api::client_action::Action::CreateTask(_)))
    );
    assert_eq!(agent_outputs(&events), vec!["a and b"]);
    assert_done(&events);

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].messages.iter().any(|message| matches!(
        message,
        Message::ToolResult(result) if result.call_id == "call_1" && result.result.content.contains("a\\nb")
    )));
    assert!(matches!(
        requests[0].messages.last(),
        Some(Message::User(user)) if user.text_content().starts_with("Tool results are above")
    ));
}

#[tokio::test]
async fn in_process_tool_keeps_the_run_going() {
    let provider = Scripted::new(vec![
        tool_use(vec![ToolCall {
            id: "todo_1".to_string(),
            name: "update_todos".to_string(),
            arguments: serde_json::json!({ "todos": [{ "id": "1", "title": "Ship it" }] }),
        }]),
        text("Planned."),
    ]);
    let base_url = start_scripted(provider.clone()).await;

    let events = post_events(&base_url, "/ai/multi-agent", &query_request("Plan")).await;

    let persisted = messages(&events);
    assert!(persisted.iter().any(|message| matches!(
        &message.message,
        Some(api::message::Message::ToolCall(call)) if call.tool_call_id == "todo_1" && call.tool.is_none()
    )));
    assert!(persisted.iter().any(|message| matches!(
        &message.message,
        Some(api::message::Message::UpdateTodos(_))
    )));
    assert!(persisted.iter().any(|message| matches!(
        &message.message,
        Some(api::message::Message::ToolCallResult(result)) if result.tool_call_id == "todo_1"
    )));
    assert_eq!(agent_outputs(&events), vec!["Planned."]);
    assert_done(&events);
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn passive_suggestions_reply_with_an_empty_successful_stream() {
    let base_url = start_scripted(Scripted::new(vec![text("never asked")])).await;

    let events = post_events(
        &base_url,
        "/ai/passive-suggestions",
        &query_request("anything"),
    )
    .await;

    assert_eq!(kinds(&events), vec!["init", "finished"]);
    assert_done(&events);
}

#[tokio::test]
async fn missing_provider_reports_an_internal_error() {
    let base_url = start_scripted(Scripted::new(vec![text("never asked")])).await;
    let mut request = query_request("Hi");
    request
        .settings
        .as_mut()
        .unwrap()
        .model_config
        .as_mut()
        .unwrap()
        .base = "other".to_string();

    let events = post_events(&base_url, "/ai/multi-agent", &request).await;

    assert_eq!(kinds(&events), vec!["init", "finished"]);
    assert!(matches!(
        finished_reason(&events),
        api::response_event::stream_finished::Reason::InternalError(error) if error.message.contains("other")
    ));
}

#[tokio::test]
async fn invalid_protobuf_body_is_a_bad_request() {
    let base_url = start_scripted(Scripted::default()).await;

    let response = reqwest::Client::new()
        .post(format!("{base_url}/ai/multi-agent"))
        .body(vec![0xff, 0xff, 0xff])
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn client_disconnect_cancels_the_run() {
    let observer = Arc::new(StopObserver::default());
    let state = ServerState::new(Arc::new(Scripted::slow(Duration::from_secs(30))))
        .with_lifecycle_hooks(observer.clone())
        .with_keep_alive_interval(Duration::from_millis(50));
    let base_url = start(state).await;

    let response = reqwest::Client::new()
        .post(format!("{base_url}/ai/multi-agent"))
        .header("content-type", "application/x-protobuf")
        .body(query_request("Hi").encode_to_vec())
        .send()
        .await
        .unwrap();
    let mut body = response.bytes_stream();
    let first_chunk = body.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first_chunk).contains("data:"));
    drop(body);

    tokio::time::timeout(Duration::from_secs(5), observer.stopped.notified())
        .await
        .expect("the run should stop once the client is gone");
    assert_eq!(
        *observer.reason.lock().unwrap(),
        Some(FinishReason::Cancelled)
    );
}

#[tokio::test]
async fn health_and_debug_spec_are_served() {
    let base_url = start_scripted(Scripted::default()).await;
    let client = reqwest::Client::new();

    let health = client
        .get(format!("{base_url}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), reqwest::StatusCode::OK);
    assert_eq!(health.text().await.unwrap(), "ok");

    let spec: serde_json::Value = client
        .get(format!("{base_url}/debug/spec"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(spec["system_prompt"].as_str().unwrap().contains("Warp"));
    assert!(
        spec["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "run_shell_command")
    );
}

/// A recovered tool call can carry arguments the proto form rejects. The client cannot be told
/// about such a call, so the turn must not simply end with nothing.
#[tokio::test]
async fn a_client_tool_call_that_cannot_be_expressed_as_proto_is_not_dropped() {
    let provider = Scripted::new(vec![
        tool_use(vec![ToolCall {
            id: "call_1".to_string(),
            name: "run_shell_command".to_string(),
            // `cwd` is not part of the schema, so the proto conversion rejects it.
            arguments: serde_json::json!({ "command": "ls", "cwd": "/tmp" }),
        }]),
        text("Sorry, retrying without that argument."),
    ]);
    let base_url = start_scripted(provider.clone()).await;

    let events = post_events(&base_url, "/ai/multi-agent", &query_request("list files")).await;

    // The model is told what was wrong and answers, instead of the turn ending in silence.
    assert_eq!(
        agent_outputs(&events),
        vec!["Sorry, retrying without that argument."],
        "expected the model to recover, got {:?}",
        kinds(&events)
    );
    assert_done(&events);

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].messages.iter().any(|message| matches!(
            message,
            Message::ToolResult(result)
                if result.call_id == "call_1"
                    && result.result.is_error
                    && result.result.content.contains("cwd")
        )),
        "the conversion error should reach the model as a tool result"
    );
}
