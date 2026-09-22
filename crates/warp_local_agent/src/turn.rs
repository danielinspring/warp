//! Runs one planned turn on the runtime and streams the resulting protobuf events.

use std::sync::Arc;

use async_stream::stream;
use futures::{Stream, StreamExt as _};
use local_agent_runtime::{
    AgentRuntime, CancelHandle, CompositeHooks, ContextBudget, FnTelemetrySink, LifecycleHooks,
    LoggingHooks, RuntimeConfig, RuntimeEvent, RuntimeTelemetrySink, TelemetryLifecycleHooks,
    ToolNameDenyHooks,
};
use warp_multi_agent_api as api;

use crate::event_mapper::{EventMapper, add_messages};
use crate::executor::ServiceToolExecutor;
use crate::provider::{DynProvider, ProviderFactory};
use crate::request::TurnPlan;

/// Start the run for `plan`. The stream yields `Init`, the echoed tool results, then every event
/// the runtime produces until it finishes; dropping the handle does not stop the run, `cancel` does.
pub fn run_turn(
    plan: TurnPlan,
    factory: &dyn ProviderFactory,
    extra_hooks: Option<Arc<dyn LifecycleHooks>>,
) -> (
    impl Stream<Item = api::ResponseEvent> + Send + 'static,
    CancelHandle,
) {
    let TurnPlan {
        ids,
        provider,
        model_family,
        registry,
        system_prompt,
        context_window_limit,
        initial_messages,
        user_input,
        echo_messages,
    } = plan;

    let executor = ServiceToolExecutor::new(Arc::clone(&registry), model_family);
    let config = RuntimeConfig {
        system_prompt: Some(system_prompt),
        context_budget: ContextBudget {
            max_input_tokens: context_window_limit,
            ..Default::default()
        },
        ..Default::default()
    };
    let telemetry: Arc<dyn RuntimeTelemetrySink> = Arc::new(FnTelemetrySink(|event| {
        tracing::info!(
            target: "local_runtime_telemetry",
            ?event,
            "local agent runtime telemetry"
        );
    }));
    let runtime = AgentRuntime::new(DynProvider(factory.create(&provider)), executor, config)
        .with_hooks(lifecycle_hooks(Arc::clone(&telemetry), extra_hooks))
        .with_telemetry(telemetry);

    let mut mapper = EventMapper::new(
        ids.conversation_id,
        ids.request_id,
        ids.run_id,
        ids.task_id,
        ids.task_exists,
        registry,
    );
    let (mut runtime_events, cancel) = runtime.run(provider.model, initial_messages, user_input);

    let events = stream! {
        yield mapper.init_event();
        mapper.mark_init_sent();
        if !echo_messages.is_empty() {
            let task_id = mapper.task_id.clone();
            yield mapper.transaction(vec![add_messages(&task_id, echo_messages)]);
        }
        while let Some(event) = runtime_events.next().await {
            let finished = matches!(event, RuntimeEvent::Finished { .. });
            for proto_event in mapper.map_event(&event) {
                yield proto_event;
            }
            if finished {
                break;
            }
        }
    };
    (events, cancel)
}

/// Trusted in-process hooks: logging, telemetry, the optional deny list, and any hooks the server
/// was configured with.
///
/// Set `WARP_LOCAL_AGENT_DENIED_TOOLS=tool_a,tool_b` to block tools by exact name before they are
/// executed or handed to the client.
fn lifecycle_hooks(
    telemetry: Arc<dyn RuntimeTelemetrySink>,
    extra_hooks: Option<Arc<dyn LifecycleHooks>>,
) -> Arc<dyn LifecycleHooks> {
    let mut hooks: Vec<Arc<dyn LifecycleHooks>> = vec![
        Arc::new(LoggingHooks),
        Arc::new(TelemetryLifecycleHooks::new(telemetry)),
    ];
    if let Ok(raw) = std::env::var("WARP_LOCAL_AGENT_DENIED_TOOLS") {
        let denied_tools = raw
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if !denied_tools.is_empty() {
            hooks.push(Arc::new(ToolNameDenyHooks { denied_tools }));
        }
    }
    hooks.extend(extra_hooks);
    Arc::new(CompositeHooks::new(hooks))
}
