# Local Ollama Manual Tool Parity Prompts

Date: 2026-05-05

Use this checklist to manually test the current local Ollama runtime tools:

- `run_shell_command`
- `read_files`
- `grep`
- `file_glob_v2`
- `search_codebase`

Run Warp with the local runtime feature enabled:

```bash
cargo run -p warp --bin warp-oss --features local_ollama_runtime_tool_use
```

Then configure Ollama in Settings -> AI -> Ollama and start an Ollama-backed agent conversation.

For each prompt, verify:

- [ ] The expected tool action appears in the UI.
- [ ] The assistant answer uses the real tool result.
- [ ] A follow-up question can refer to the previous tool result.
- [ ] The conversation does not show an orphaned tool call after the result completes.

## Shell Command Tool

Expected tool: `run_shell_command`

Use prompts that ask for command output, not general explanation.

- [ ] "Use a shell command to print the current working directory, then tell me the exact path."
- [ ] "Use a shell command to run `git status --short` in this repo and summarize whether the worktree is clean."
- [ ] "Use a shell command to list the top-level entries in this repository and identify which ones look like documentation folders."
- [ ] "Use a shell command to count Rust files under `app/src/ai` and tell me the count plus the command you ran."
- [ ] "Use a shell command to show the first 10 lines of `app/Cargo.toml`, then summarize the package name and default run target."

Follow-up check:

- [ ] "What command did you run in the previous step, and what was the key output?"

## Read Files Tool

Expected tool: `read_files`

Use prompts that ask for specific file contents.

- [ ] "Read `app/src/ai/local_runtime_bridge.rs` and list the local Ollama tool names currently advertised."
- [ ] "Read `dan_docs/how/local_ollama_runtime_tools.md` and summarize the steps for adding an existing Warp tool."
- [ ] "Read `app/src/ai/local_runtime_integration.rs` and tell me where `WarpToolExecutor` is created."
- [ ] "Read `app/src/ai/blocklist/controller/response_stream.rs` and explain how local runtime tool requests are queued."
- [ ] "Read `crates/local_agent_runtime/src/provider/ollama.rs` and tell me whether the provider appears to use streaming or non-streaming chat completions."

Follow-up check:

- [ ] "Which file did you read last, and what line or function was most relevant?"

## Grep Tool

Expected tool: `grep`

Use prompts that ask for exact symbol or text matches.

- [ ] "Search the codebase for `LocalOllamaRuntimeToolUse` and tell me the most relevant files where it is defined or checked."
- [ ] "Search for `action_result_to_tool_call_result_client_actions` and explain where it is called."
- [ ] "Search for `ToolExecutionRequest` and summarize the request flow from runtime to UI action execution."
- [ ] "Search for `not connected to local runtime` and explain what currently has that status."
- [ ] "Search for `build_tool_schemas` and tell me where the local Ollama tools are advertised."

Follow-up check:

- [ ] "Which grep result was most important, and why?"

## File Glob Tool

Expected tool: `file_glob_v2`

Use prompts that ask for matching file paths, not file contents.

- [ ] "Find all Markdown files under `dan_docs` and group them by folder."
- [ ] "Find Rust files under `app/src/ai` whose path contains `local_runtime`."
- [ ] "Find files named `SKILL.md` under `.agents` and summarize how many there are."
- [ ] "Find Cargo manifest files under `app` and `crates/local_agent_runtime`."
- [ ] "Find test files under `crates/local_agent_runtime/src` and list their paths."

Follow-up check:

- [ ] "How many paths did the previous file glob return, and which one looked most relevant?"

## Search Codebase Tool

Expected tool: `search_codebase`

Use prompts that ask for conceptual discovery instead of exact text.

- [ ] "Find the implementation that routes Ollama requests through the local runtime when the feature flag is enabled."
- [ ] "Find where Warp converts local runtime tool calls into existing Warp agent actions."
- [ ] "Find where completed local runtime tool results are persisted into the conversation transcript."
- [ ] "Find the code that describes which local runtime features are active versus display-only."
- [ ] "Find the current Ollama provider implementation and summarize how it sends chat completion requests."

Follow-up check:

- [ ] "Based on the previous search, which file should I edit first if I want to add a new local Ollama tool?"

## Pass Criteria

Mark the manual parity pass as successful when:

- [ ] All five tools can be invoked from UI prompts.
- [ ] Tool results are visible through the assistant's answer.
- [ ] Tool-call/result pairing survives follow-up questions.
- [ ] No prompt leaves a completed tool call without a completed tool result.
- [ ] Errors or denied actions are shown as tool results instead of breaking the conversation.

## Notes

- These prompts intentionally mention the desired behavior, because the goal is to test the tool bridge, not model planning quality.
- Some models may choose a different tool if the prompt is ambiguous. Re-run with more direct wording such as "Use the file search tool" or "Use a shell command".
- Skills and MCP are not part of this parity pass yet; they are display-only for local Ollama runtime today.
