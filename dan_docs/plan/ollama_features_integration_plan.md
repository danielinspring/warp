# Plan: Integrate Skills, MCP, Prompts, and Memory into Local Ollama Runtime

## Objective
The local Ollama agent runtime currently uses a hardcoded system prompt and a fixed set of V1 terminal tools (`run_shell_command`, `read_files`, etc.). This plan outlines the architectural steps required to integrate Warp's advanced Agent features—Memory, dynamic Prompts, Skills, and MCP (Model Context Protocol)—into the local execution loop.

## 1. Dynamic Prompts & Memory Integration

**Background:**
Warp's "Memory" (user-defined rules and facts) is represented as `AIMemory` and `AIFact`. In the cloud path, the backend injects these into the system prompt if `is_memory_enabled` is true.

**Implementation Steps:**
*   **Fetch Memory Locally:** In `app/src/ai/local_runtime_integration.rs`, utilize `RequestParams.is_memory_enabled` to determine if memory should be injected.
*   **Query Fact Model:** Retrieve the active `AIFact::Memory` items from the local state (e.g., via `AIFactModel` or by passing them down through `RequestParams`).
*   **Dynamic System Prompt Construction:**
    *   Remove the hardcoded `SYSTEM_PROMPT` constant.
    *   Build a dynamic prompt generator that combines the base persona ("You are a coding assistant running locally via Ollama...") with:
        *   System context (OS, Shell type).
        *   Active Memory rules.
*   **Update Runtime Config:** Pass the dynamically generated prompt string into `RuntimeConfig::system_prompt` when instantiating `AgentRuntime`.

## 2. MCP (Model Context Protocol) Integration

**Background:**
`RequestParams` already contains an `mcp_context` populated by `TemplatableMCPServerManager`. This context holds available MCP servers, tools, and resources.

**Implementation Steps:**
*   **Schema Translation:** Create a utility to translate MCP tools (`api::mcp_tool::McpTool`) and resources into OpenAI-compatible `ToolSchema` objects that the Ollama runtime can understand.
*   **Dynamic Tool Injection:** Pass the translated MCP schemas into `WarpToolExecutor` when it is instantiated in `run_with_local_runtime`.
*   **Tool Execution Routing:**
    *   Update `WarpToolExecutor::execute()` to detect when an MCP tool or resource is called.
    *   Map these calls to Warp's existing proto tool actions: `Tool::CallMcpTool` and `Tool::ReadMcpResource`.
    *   Route the execution request through the `BlocklistAIActionModel` bridge, wait for the `AIAgentActionResultType::CallMCPTool` (or `ReadMCPResource`), and translate the result back into a local runtime `ToolCallResult`.

## 3. Skills Integration

**Background:**
Warp Skills are defined in `SKILL.md` files. For cloud agents, skills are either appended to the context window or exposed as specific tool callbacks.

**Implementation Steps:**
*   **Skill Schema Advertising:** Similar to MCP, translate available, bundled skills into `ToolSchema` definitions so the Ollama model knows they exist.
*   **Bridge Execution:** Update `WarpToolExecutor` to map skill invocations into Warp's action pipeline (e.g., mapping to a proto action representing skill execution or context retrieval).
*   **Context Loading:** Alternatively (or additionally), if a skill needs to be loaded as context rather than a tool, append the skill's `SKILL.md` contents directly into the initial `ConversationHistory` messages before starting the `AgentRuntime::run` loop.

## 4. Verification & Testing

*   **Unit Tests:** Add tests in `crates/local_agent_runtime/tests/` to verify that dynamic prompts are correctly prepended to the message history.
*   **Bridge Tests:** Add tests in `app/src/ai/local_runtime_bridge.rs` to ensure MCP tool schemas are correctly translated and routed to the `BlocklistAIActionModel`.
*   **Manual Validation:**
    *   Enable the `LocalOllamaRuntimeToolUse` feature flag.
    *   Add a Memory rule in Settings and verify it influences the Ollama model's behavior.
    *   Connect a local MCP server and ask the Ollama model to use an MCP-provided tool.
