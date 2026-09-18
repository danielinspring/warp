# How To Create Or Edit A Local Ollama Runtime Tool

Date: 2026-05-04

This document explains how to add or edit tools exposed to the local Ollama runtime.

The local Ollama runtime uses Warp's existing tool execution pipeline. Ollama sees OpenAI-compatible tool schemas, emits tool calls, Warp executes those calls as `AIAgentAction`s, and the result is returned to Ollama and persisted in the Warp transcript as a `ToolCallResult`.

## Current Tool Surface

The current local Ollama runtime tools are:

- `run_shell_command`
- `read_files`
- `grep`
- `file_glob_v2`
- `search_codebase`

Skills and MCP servers may be shown in agent visualization, but they are currently marked `not connected to local runtime`. Do not treat skills as executable local Ollama tools until that bridge is implemented.

## Main Files

- `app/src/ai/local_runtime_bridge.rs`
  - Advertises local tool schemas.
  - Validates and maps Ollama tool-call arguments into Warp proto tools.
  - Converts proto tools into `AIAgentActionType`.
  - Converts Warp action results into local runtime `ToolCallResult` content.
  - Converts Warp action results into persisted transcript `ToolCallResult` client actions.

- `app/src/ai/blocklist/controller/response_stream.rs`
  - Receives local runtime tool requests.
  - Queues Warp actions in `BlocklistAIActionModel`.
  - Sends finished results back to the local runtime.
  - Emits persisted `ToolCallResult` messages into the Warp transcript.

- `app/src/ai/local_runtime_integration.rs`
  - Creates `WarpToolExecutor`.
  - Passes the current task id and request id into tool execution.

- `crates/ai/src/agent/action_result/convert.rs`
  - Converts `AIAgentActionResultType` variants into API tool-call result payloads.

- `app/src/ai/local_runtime_spec.rs`
  - Shows the local runtime configuration for the agent visualization.
  - Today, only `system_prompt()` and `local_tools()` feed the runtime directly.

## Adding An Existing Warp Tool

Use this path when Warp already has an `AIAgentActionType`, executor, UI rendering, and result type for the tool.

1. Add the schema in `build_tool_schemas()`.

   File: `app/src/ai/local_runtime_bridge.rs`

   Add a `ToolSchemaBuilder` entry:

   ```rust
   ToolSchemaBuilder::new(
       "tool_name",
       "Short description of when the model should use this tool.",
   )
   .required_string("arg_name", "Argument description")
   .optional_string("optional_arg", "Optional argument description")
   .build()
   ```

   Keep schemas conservative. Do not expose fields that Warp ignores.

2. Add the tool name to `is_supported_tool()`.

   ```rust
   pub fn is_supported_tool(name: &str) -> bool {
       matches!(
           name,
           "run_shell_command"
               | "read_files"
               | "grep"
               | "file_glob_v2"
               | "search_codebase"
               | "tool_name"
       )
   }
   ```

3. Map JSON arguments into the Warp proto tool.

   Add an arm in `tool_call_to_proto_tool()`:

   ```rust
   "tool_name" => Ok(Tool::SomeWarpTool(api::message::tool_call::SomeWarpTool {
       field: required_string(&call.arguments, "field", &call.name)?,
       optional_field: optional_string(&call.arguments, "optional_field").unwrap_or_default(),
   })),
   ```

   Prefer the existing helpers:

   - `required_string`
   - `optional_string`
   - `optional_bool`
   - `required_string_array`
   - `optional_string_array`

4. Map the proto tool into `AIAgentActionType`.

   Add an arm in `tool_call_to_ai_action()`:

   ```rust
   api::message::tool_call::Tool::SomeWarpTool(tool) => tool.into(),
   ```

   This requires an existing `From<SomeWarpTool> for AIAgentActionType` implementation. If that conversion does not exist, add it near the existing action conversion code instead of hand-rolling duplicate behavior in the local runtime bridge.

5. Convert the Warp action result into content for Ollama.

   Add an arm in `action_result_to_content()`:

   ```rust
   AIAgentActionResultType::SomeWarpResult(result) => {
       serde_json::json!({
           "field": result.field,
       })
       .to_string()
   }
   ```

   The local runtime `ToolCallResult.content` is what Ollama receives in the next model turn. Prefer concise JSON strings over human-only prose.

6. Persist the real transcript result.

   Add the result to `action_result_to_proto_tool_call_result_type()`:

   ```rust
   let request_result: RequestResult = match result.clone() {
       AIAgentActionResultType::SomeWarpResult(result) => result.try_into().ok()?,
       // existing arms...
       _ => return None,
   };

   match request_result {
       RequestResult::SomeWarpResult(result) => Some(MessageResult::SomeWarpResult(result)),
       // existing arms...
       _ => None,
   }
   ```

   If `try_into()` is missing, add a conversion in `crates/ai/src/agent/action_result/convert.rs`.

7. Add tests in `local_runtime_bridge.rs`.

   Cover all of these:

   - Schema includes the new tool.
   - `tool_call_to_ai_action()` maps sample JSON into the right `AIAgentActionType`.
   - Invalid or missing arguments return `ToolExecutionError::InvalidInput`.
   - `action_result_to_tool_result()` returns useful content for Ollama.
   - `action_result_to_tool_call_result_client_actions()` persists the correct `ToolCallResult` variant and preserves `tool_call_id`.

8. Run focused tests.

   ```bash
   cargo test -p warp local_runtime_bridge --lib
   ```

9. Smoke test in the UI.

   ```bash
   cargo run -p warp --bin warp-oss --features local_ollama_runtime_tool_use
   ```

   Configure Ollama in Settings -> AI -> Ollama, then ask for something that requires the new tool. Verify:

   - The tool action appears in the UI.
   - The assistant uses the tool result.
   - A follow-up question can refer to the tool result.
   - Reopening or resuming the conversation does not leave an orphaned tool call.

## Editing An Existing Tool

Use this path when changing one of the existing local Ollama tools.

1. If changing what Ollama can call, edit `build_tool_schemas()`.
2. If changing accepted argument names or validation, edit `tool_call_to_proto_tool()`.
3. If changing what Ollama receives after the tool runs, edit `action_result_to_content()`.
4. If changing persisted transcript shape, edit `action_result_to_proto_tool_call_result_type()` and any needed conversion in `crates/ai/src/agent/action_result/convert.rs`.
5. Update existing tests or add a regression test for the changed behavior.

When renaming arguments, consider backwards-compatible aliases if the model may still emit the older name. Existing examples:

- `read_files` accepts `paths` and `files`.
- `grep` accepts `queries`, `patterns`, or single `pattern`.
- `file_glob_v2` accepts `patterns` or single `pattern`.

## Adding A Brand-New Warp Tool

If Warp does not already have the tool, first build the regular Warp action pipeline:

1. Add a proto tool and result shape in the API layer.
2. Add an `AIAgentActionType` variant.
3. Add an `AIAgentActionResultType` variant.
4. Implement action execution in the action model.
5. Add permission behavior.
6. Add UI rendering for the action and result.
7. Add API conversion for the result.
8. Add tests for the normal Warp path.
9. Only then expose it in `local_runtime_bridge.rs`.

Do not advertise a brand-new tool to Ollama before Warp can execute it, display it, return it to the model, and persist it into the transcript.

## Permission Notes

The local runtime currently returns `PermissionDecision::Allow` for supported tool names, because Warp's action model owns the real permission and confirmation flow. This means a supported tool can still require UI confirmation once it is queued as a Warp action.

If a tool has special risk semantics, preserve those fields in the schema and proto mapping. For example, `run_shell_command` maps `is_read_only`, `is_risky`, and `uses_pager`.

## Common Failure Modes

- Tool is in the schema but not in `is_supported_tool()`.
  - The runtime may advertise a tool but deny or reject execution.

- Tool is in `is_supported_tool()` but missing from `tool_call_to_proto_tool()`.
  - The runtime accepts the name but cannot map arguments to a Warp action.

- Result content is not mapped in `action_result_to_content()`.
  - Ollama receives a generic string that may be too vague for follow-up reasoning.

- Persisted result is not mapped in `action_result_to_proto_tool_call_result_type()`.
  - Ollama may continue internally, but Warp transcript history can miss the real `ToolCallResult`.

- Schema is too broad.
  - Ollama emits fields or values Warp ignores, making behavior hard to debug.

- Tool depends on skills or MCP.
  - Skills and MCP are not yet connected to the local runtime execution bridge.

## Minimal Review Checklist

- [ ] Tool schema is accurate and conservative.
- [ ] Tool name is included in `is_supported_tool()`.
- [ ] Arguments map into the correct Warp proto tool.
- [ ] Proto tool maps into the intended `AIAgentActionType`.
- [ ] Result content is useful for the next Ollama turn.
- [ ] Result persists as a real transcript `ToolCallResult`.
- [ ] Cancellation and error cases are represented.
- [ ] Tests cover schema, mapping, invalid args, result content, and persistence.
- [ ] UI smoke test verifies the tool can be used in a follow-up conversation.
