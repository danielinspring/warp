# Local Agent Runtime — Competitive Gap Report

Date: 2026-07-22  
Subject: `crates/local_agent_runtime` + Warp Ollama bridge (`app/src/ai/local_runtime_*`)  
Compared against: Claude Code, Hermes Agent (Nous), Aider, Cursor Agent, and Warp’s own cloud/local Oz agent surface

## Executive summary

Warp’s local Ollama agent (`local_agent_runtime`) is a **credible V1 coding loop**: stream → tool call → execute via Warp’s action pipeline → feed results back. It already covers the core “read / search / shell / edit / MCP / skill-read” path that users need for basic agentic coding.

It is **not yet competitive** with best-selling coding agents as a product harness. The gap is less “missing one tool” and more **missing the systems that make long, reliable, autonomous work succeed**: plan/todo loops, permission modes, context compaction, subagents, web/browser, hooks, and model-quality adaptation for weaker local LLMs.

**Bottom line:** treat this as a solid foundation for core tool use, not a finished peer to Claude Code or Hermes. This report avoids assigning a percentage-complete score because the capabilities differ too much in impact and implementation cost to weight them credibly.

---

## Current baseline (as of this audit)

Code is ahead of older `dan_docs` milestones. Present capabilities:

| Area | Status |
| --- | --- |
| Provider | Ollama via OpenAI-compatible `/v1/chat/completions`, streaming |
| Loop | Multi-turn tool loop, timeouts, cancel + paired cancel tool results; Warp integration currently caps a run at 10 turns |
| Built-in tools | `run_shell_command`, `read_files`, `grep`, `file_glob_v2`, `search_codebase`, `edit_files` |
| MCP | Schemas from `RequestParams.mcp_context`; `mcp__…` tools + `read_mcp_resource` routed into Warp actions |
| Skills | `read_skill` when skills are available for the request |
| Concurrency | Built-in read-only tools batched concurrently; mutating/interactive and MCP tools serial |
| Prompt | Dynamic prompt (cwd, shell, memory flag, capability flags, tool list) |
| Validation | Closed built-in schemas with manual bridge validation; MCP validation is delegated downstream |
| Permissions | Runtime returns `Allow` for known tools by design; real confirmations remain in Warp’s action UI |
| Context | Per-tool-result character truncation only; no token-budget enforcement or transcript compaction |
| Restored history | Lossy for some tool types: prior `edit_files`, MCP, and skill calls are not fully reconstructed for later turns |
| Gate | `FeatureFlag::LocalOllamaRuntimeToolUse` |

Prompt still advertises planning / ask-user / web / computer-use / research / orchestration as *settings*, but **those are not executable tools** unless separately advertised — so local Ollama cannot actually use them today.

---

## Comparison matrix

| Capability | Warp local Ollama | Claude Code | Hermes Agent | Aider | Cursor Agent |
| --- | --- | --- | --- | --- | --- |
| Local / BYO model | ✅ Ollama | Cloud/API-hosted Claude, including enterprise hosts; no local Ollama backend | ✅ many providers + local | ✅ Ollama/LM Studio | Partial (cloud-first) |
| Shell + filesystem read/search | ✅ | ✅ | ✅ | ✅ | ✅ |
| Structured file edit | ✅ `edit_files` (reviewed diffs) | ✅ Edit/Write + staleness guards | ✅ file tools | ✅ git-aware apply | ✅ multi-file / Composer |
| Semantic codebase search | ✅ `search_codebase`, when the repository is indexed | via Grep/Glob/Agent | weaker native | repo-map (tree-sitter) | ✅ strong |
| MCP as first-class tools | ✅ wired | ✅ deep (stdio/sse/http/ws) | ✅ deep + catalog/OAuth | limited | ✅ |
| Skills / agent skills | Partial (`read_skill` only) | ✅ SkillTool + plugins | ✅ create/self-improve skills | CLAUDE.md-like conventions | rules / docs |
| Plan mode / todos | ❌ | ✅ EnterPlanMode, TodoWrite, Task* | ✅ task delegation patterns | chat-driven | ✅ plan/agent modes |
| Ask user / clarify | ❌ as tool | ✅ AskUserQuestion | ✅ clarify flows | interactive CLI | chat |
| Web search / fetch | ❌ | ✅ WebSearch/WebFetch | ✅ web + browser toolsets | optional | ✅ |
| Browser / computer use | ❌ | ✅ (gated) | ✅ browser toolset | ❌ | limited |
| Subagents / orchestration | ❌ | ✅ AgentTool, swarms, teams | ✅ `delegate_task`, parallel | ❌ | parallel agents (weaker coop) |
| Background processes / PTY | ❌ | ✅ Bash background + UI | ✅ `process` + PTY terminal | ❌ | ❌ |
| Permission modes | Warp UI confirmations exist; local runtime modes/policy propagation are missing | ✅ default/plan/auto/acceptEdits/bypass + classifiers | ✅ safe toolsets + security audit | trust model | approval UX |
| Hooks / lifecycle intercept | ❌ | ✅ PreToolUse/Stop/Permission hooks | plugins + customizations | ❌ | limited |
| Context compaction | Truncation only | ✅ auto/micro/session compact | trajectory compression / memory | repo-map budgeting | product compaction |
| Cross-session memory | Flag + Warp memory path | CLAUDE.md + memory systems | strong (FTS5, Honcho, skill learning) | git history | Memories/rules |
| Multi-provider routing | Ollama-focused | Anthropic (+ enterprise hosts) | 30+ providers | any OpenAI-compat | multi-model |
| Git-native workflow | via shell only | via Bash + hooks | via tools | ✅ auto-commit specialty | PR/agent flows |
| Extensibility | Warp actions bridge | tools + MCP + plugins + hooks | toolsets + MCP + plugins + ACP | conventions | extensions |
| Product polish / autonomy | Early | Best-in-class autonomous CLI | Best-in-class open local harness | Best open surgical editor | Best IDE pairing |

---

## What it lacks (ranked)

### P0 — Correctness and long-run reliability

1. **Lossless transcript restoration**  
   Later turns do not faithfully reconstruct every tool call already supported by the live loop. In particular, prior `edit_files`, MCP, and skill calls can be dropped while their results are restored in a different representation. Fix tool-call/result pairing and result serialization before adding more tools; otherwise follow-up turns can reason from incomplete or orphaned history.

2. **Context-window enforcement, compaction, and recovery**  
   Only per-tool-result character truncation exists, with no use of the request’s context-window limit or whole-transcript token budget. Add request budgeting first, then micro-compaction and transcript compaction, plus retry/recovery for context overflow and max-output termination. Without these, multi-turn Ollama sessions can exceed a local model’s window or silently degrade.

3. **Provider and loop recovery**  
   Add retry/backoff for transient provider errors, malformed tool-call repair where safe, max-token continuation, and loop-stall detection. The current hard 10-turn integration limit should become model/task-aware after transcript reliability and budgeting are in place.

4. **Ask-user / clarify tool**  
   Present as a capability flag in the prompt, not an executable local tool. Warp already has a client action that can be bridged, but the local runtime also needs a resumable permission/question protocol rather than treating an `Ask` decision as a skipped tool.

5. **Runtime permission modes and policy propagation**  
   Warp’s action model already provides a real confirmation floor, so this is not an absence-of-safety bug. The gap is carrying autonomy/isolation policy into the local runtime and supporting modes such as `default`, `accept_edits`, and `plan`, with allow/deny rules and resumable approval decisions.

### P1 — Autonomy and capability breadth

6. **Plan / todo / progress tools**  
   Durable task state helps long refactors avoid drift. Unlike `ask_user_question`, a Claude-style todo/plan tool is not currently a simple local `AIAgentActionType` bridge; it needs a local state model and UI/protocol design.

7. **Web search + fetch**  
   Local Ollama cannot look up current docs, errors, or APIs unless the user supplies them or MCP happens to expose a suitable tool. Warp’s web capability is primarily server-side, so local support requires a new local provider/tool path rather than merely exposing an existing client action.

8. **Subagents / delegation**  
   Parallel research and isolated context windows improve autonomy on large tasks. Warp already has client-side orchestration actions that can be bridged, but local execution still needs explicit depth, concurrency, cancellation, and workspace-isolation limits.

9. **Background shell + process management**  
   Hermes `terminal`/`process` and Claude Code background Bash let agents run servers/tests without blocking the turn. Warp already has long-running shell actions, making this mainly a bridge/schema task rather than a new process subsystem.

10. **Full skills lifecycle**  
    `read_skill` is useful and reading instructions is effectively invocation for many skills. Remaining gaps are discovery/trigger quality, bundled-resource handling, progress semantics, and—only if justified—safe skill creation or updates.

11. **Hooks / policy intercept points**  
   Pre-tool, post-tool, stop, and permission hooks are how teams harden agents in CI and enterprise. Local runtime has none.

12. **Mutable mid-session tool registry**  
    The generic runtime already calls `available_tools()` every provider turn, and tests verify that behavior. The concrete Warp registry is built once per request, so MCP/skill/permission changes during a run cannot update that list. Recompute or safely mutate the request registry only if hot reconnect is a product requirement.

13. **Local-runtime telemetry**  
    No dedicated instrumentation for provider latency, tool decisions, denials, cancel pairing, or transcript orphans — hard to improve quality or debug against Claude Code-level reliability.

14. **Model-adaptive prompting for small local models**  
    Ollama users often run 7B–32B coders. Claude Code prompts are tuned for Claude; Hermes tool descriptions are aggressive and toolset-scoped. Local Warp prompt is thin and assumes strong tool discipline (`edit_files` reminders help, but are not enough).

### P2 — Differentiating / polish

15. **Computer use / browser automation** (flag exists in prompt; Warp actions are potentially bridgeable)  
16. **Multimodal image understanding** (Ollama provider currently reports no vision support)  
17. **Notebook-aware edit** (Claude Code NotebookEdit)  
18. **Multi-provider beyond Ollama** (LM Studio, OpenAI-compat, Anthropic local proxies — Hermes strength)  
19. **Git-first workflows** (Aider-style auto-commit, PR summaries)  
20. **Scheduled / always-on agent** (Hermes cron + multi-platform gateway — optional for Warp)  
21. **Serve-as-MCP / ACP host** (Hermes/Claude ecosystem interoperability)  
22. **Staleness guards & formatting-preserving edits** at the edit engine level (Claude Code FileEdit maturity)  
23. **Curated MCP catalog + OAuth onboarding** (Hermes polish)

---

## Gaps vs Warp’s *own* cloud/Oz agent

Even without leaving the Warp product, local Ollama lags the cloud agent surface:

| Cloud / Oz capable | Local Ollama today |
| --- | --- |
| Planning | Flag only |
| Ask user question | Flag only |
| Web search | Flag only |
| Computer use | Flag only |
| Research agent / orchestration | Flag only |
| Rich skills usage | `read_skill` only |
| Cloud handoff / env snapshots | Exists around local agent UI, not inside runtime intelligence |

**Implication:** parity work splits into two distinct tracks:

- **Bridge existing client actions:** ask-user, subagents/orchestration, computer use, documents, and long-running shell/process actions.
- **Build local replacements for server capabilities:** web search/fetch and durable plan/todo state do not currently have equivalent client actions that can simply be advertised to Ollama.

This distinction matters for estimates: bridge work can reuse Warp’s action UI and result pipeline; local replacements need new execution, policy, persistence, and testing.

---

## Architectural strengths to keep

Do not throw away the current design; it is a good harness skeleton:

- Provider-agnostic `LLMProvider` + `ToolExecutor` traits
- Warp bridge reuses action model, CodeDiff review, permissions UI, transcript persistence
- Safety classes + concurrent read-only batches
- Cancel synthesis for unpaired tool calls
- OpenAI-compatible tool schemas (works with Ollama and future providers)

Upgrade should **widen the bridge and deepen the loop**, not rewrite the runtime.

---

## Recommended upgrade roadmap

### Phase A — Make existing behavior reliable

1. Make restored history lossless for `edit_files`, MCP, skills, and all supported tool-call/result pairs.  
2. Enforce the actual context-window budget; add micro-compaction, transcript compaction, and context-overflow recovery.  
3. Add provider retry/backoff, max-output continuation, and loop-stall detection; revisit the integration’s hard 10-turn cap.  
4. Bridge **ask_user_question** with a resumable question/permission flow.  
5. Add runtime permission modes that map onto Warp’s existing confirmation floor (`default`, `accept_edits`, and `plan` initially).  
6. Promote prompt capability flags only when the matching executable tool is in the registry; remove misleading enabled-but-unavailable states.

### Phase B — Widen the existing Warp bridge

7. Bridge subagent / `run_agents`-style delegation with conservative depth and concurrency limits.  
8. Bridge long-running shell/process actions.  
9. Bridge computer use and document actions only where the selected local model can use them reliably.  
10. Improve skill discovery, loading, and bundled-resource support beyond the current `read_skill` path.  
11. Add pre-tool, post-tool, permission, and stop hooks.  
12. Add dedicated local-runtime telemetry.

### Phase C — Build capabilities that are currently server-side or net-new

13. Implement local web search/fetch with explicit provider, privacy, and permission policy.  
14. Build durable local plan/todo state and UI; do not assume a cloud server tool can be reused.  
15. Add multi-provider support (LM Studio / generic OpenAI-compat) through `LLMProvider`.  
16. Add model-specific prompt/tool-schema packs for Qwen, DeepSeek, and Llama tool-calling variants.  
17. Add multimodal input for providers that support vision.  
18. Add Git workflow helpers (status, commit-message draft, PR summary) as first-class tools rather than freeform shell.

---

## Suggested success metrics

| Metric | Target |
| --- | --- |
| Tool success rate on manual parity suite | Define a pinned model and target pass rate for read/edit/shell/search prompts in `dan_docs/how/local_ollama_manual_parity_prompts.md` |
| Restored-history fidelity | Every persisted supported tool call has a matching, semantically equivalent result on the next turn |
| Multi-turn (>15 tool calls) completion without context blowup | Compaction and recovery keep the run alive within the selected model’s context window |
| Bridgeable client actions usable locally | Track separately from server-only capabilities; target ≥70% only after the inventory is defined |
| User intervention rate on routine edits | Lower via `accept_edits` mode |
| Time-to-first useful edit on medium repo task | Competitive with Aider on same local model |

---

## Sources

- Warp: `crates/local_agent_runtime`, `app/src/ai/local_runtime_bridge.rs`, `local_runtime_spec.rs`, `local_runtime_integration.rs`
- Internal notes: `dan_docs/tasks/local_ollama_runtime_parity_milestones.md` (partially stale), `dan_docs/how/local_ollama_runtime_tools.md`, `dan_docs/plan/ollama_features_integration_plan.md`
- Claude Code: sibling checkout `../claude-code-main/docs/{01,03,04,05,07}-*.md` and `../claude-code-main/src/tools.ts`, reviewed 2026-07-22; competitor behavior should be rechecked against current public documentation before implementation decisions
- Hermes: NousResearch Hermes Agent public documentation for tools/toolsets, MCP, providers, memory, and delegation, reviewed 2026-07-22

---

## One-line verdict

**Keep the runtime; upgrade the harness.** First make transcript restoration, context budgeting, and provider recovery reliable. Then widen the bridge for existing client actions such as ask-user, subagents, and long-running shell; treat web search and durable plan/todo state as new local capabilities, not simple cloud-action wiring.
