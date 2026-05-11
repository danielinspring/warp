# Local Ollama Runtime Parity Milestones

Date: 2026-05-04

This tracker captures the missing work found by comparing Warp's local Ollama runtime implementation with `/Users/lt-018/codish/open_soures/claude-code-main`.

Update each item from `[ ]` to `[x]` as the work lands.

## Milestones

- [x] **Persist tool results into Warp transcript**
  - Add local runtime `ToolResult` events as real Warp `ToolCallResult` messages.
  - Preserve `tool_call_id` pairing between assistant tool calls and result messages.
  - Ensure later turns, resume, sharing, and transcript rendering do not see orphaned tool calls.
  - Implemented by emitting `AddMessagesToTask` with a `ToolCallResult` when the queued Warp action finishes.

- [x] **Add real Ollama response streaming**
  - Switch the Ollama provider from non-streaming `stream: false` to streaming where supported.
  - Emit `RuntimeEvent::TextDelta` for text chunks.
  - Preserve tool-call assembly while streaming.
  - Ensure UI receives incremental text instead of only `TextCompleted`.

- [x] **Build dynamic local runtime prompt and context**
  - Replace the static local runtime prompt with a prompt assembled from request context.
  - Include working directory, relevant system context, user context, memory settings, and available runtime capabilities.
  - Respect `RequestParams.is_memory_enabled`.
  - Add tests that verify the final prompt is prepended to local runtime history.

- [ ] **Connect MCP tools and resources**
  - Translate `RequestParams.mcp_context` tools/resources into OpenAI-compatible local runtime tool schemas.
  - Execute MCP tool calls through Warp's existing MCP action pipeline.
  - Respect MCP allow/deny/ask permission behavior.
  - Update `local_runtime_spec::local_mcp_servers` from display-only status to active status when connected.

- [ ] **Connect Skills**
  - Advertise available skills to the local runtime through either tool schemas or injected context.
  - Implement local runtime bridge execution for skill reads/invocations.
  - Respect working-directory skill scoping.
  - Update `local_runtime_spec::local_skills` from display-only status to active status when connected.

- [ ] **Expand supported tool surface**
  - Add edit/write capabilities when safe: file edit, file write/create, and related result mapping.
  - Consider plan, ask-user-question, subagent/agent, web, document, and computer-use tools separately.
  - Avoid advertising tools before execution, permissions, UI rendering, and result conversion are implemented.

- [x] **Add schema and value validation before execution**
  - Validate local runtime tool inputs against precise schemas before queuing Warp actions.
  - Return structured tool errors for invalid or missing arguments.
  - Add tool-specific validation for paths, grep/glob patterns, shell command options, and unsupported fields.
  - Implemented object/type/non-empty validation, unsupported-argument rejection, strict tool schemas, and focused regression tests for the current five local tools.

- [ ] **Improve permission flow semantics**
  - Make permission decisions explicit in the local runtime instead of treating supported tools as `Allow`.
  - Surface permission prompts/results in a way that the runtime and transcript can both understand.
  - Preserve user denial feedback as tool-result content for the next model turn.

- [ ] **Support safe concurrent tool execution**
  - Partition tool calls into read-only/concurrency-safe batches.
  - Run safe batches concurrently while preserving result ordering for the model.
  - Keep mutating or dependency-sensitive actions serial.

- [ ] **Handle cancellation and interruption with paired tool results**
  - When a run is cancelled after tool calls are emitted, generate matching error/cancel tool results.
  - Cancel queued and running Warp actions cleanly.
  - Avoid leaving in-progress tool IDs or transcript state dangling.

- [ ] **Add context compaction and recovery**
  - Replace simple tool-result truncation with a fuller context budget strategy.
  - Handle prompt-too-long, max-output, and retryable provider errors without corrupting tool-call/result ordering.
  - Add tests for long tool outputs and multi-turn context growth.

- [ ] **Refresh tools between local runtime turns**
  - Recompute available tools after each tool batch so newly connected MCP servers or changed permissions can affect the next provider call.
  - Keep tool schema ordering stable where possible.

- [ ] **Improve telemetry and observability**
  - Add local runtime telemetry for provider calls, tool calls, tool decisions, tool results, permission outcomes, cancellation, and runtime errors.
  - Include enough identifiers to debug transcript/tool-call pairing issues.

- [ ] **Manual parity test pass**
  - Test shell command execution.
  - Test file read.
  - Test grep.
  - Test file glob.
  - Test search codebase.
  - Test denied permission.
  - Test invalid arguments.
  - Test cancellation during tool execution.
  - Test a multi-tool, multi-turn request.
  - Test resume or follow-up after tool use.

## Known Current State

- [ ] The local Ollama runtime currently uses a narrow provider/tool loop around five V1 tools.
- [ ] MCP and skills are visible in the local runtime spec, but are not connected to execution.
- [x] The provider streams OpenAI-compatible chat completions when supported.
- [x] Tool results are fed back inside the runtime loop and persisted as Warp transcript `ToolCallResult` messages.
- [ ] Tool execution is serial.
- [ ] Context management is limited to truncating tool result strings.
