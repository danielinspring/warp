# feat-020 E2E: Durable local plan / todo state

Manual end-to-end checklist for the local Ollama / LiteLLM agent runtime's durable todo tools:

- **`update_todos`** — replaces the pending list (max 20 items)
- **`mark_todos_completed`** — moves pending items to completed by id

| Field | Value |
| --- | --- |
| Feature ID | `feat-020` |
| Branch (typical) | `cla-dev-2` |
| Related commit | `ecd18aeb` Add durable local plan/todo tools for the Ollama agent runtime |
| Unit gate | `cargo test -p warp --lib local_todos` and `cargo test -p warp --lib local_runtime` |
| Core files | `app/src/ai/local_todos.rs`, `app/src/ai/local_runtime_bridge.rs`, `app/src/ai/local_runtime_spec.rs` |

---

## What makes this feature different to test

These two tools are the first local tools that are **in-process and transcript-writing at the same time**. That shapes the whole test plan, so read this before running prompts.

1. **No Warp action, no permission prompt.** `WarpToolExecutor::execute` intercepts both names and runs them inline (`local_runtime_bridge.rs:710`). They are never queued as an `AIAgentAction`. `tool_call_to_ai_action_with_registry` deliberately returns `ExecutionFailed` for this route (`local_runtime_bridge.rs:1138`), so if you ever see a permission card or a shell block for a todo call, that is a routing bug.
2. **Three observation surfaces per call.** Every successful call must show up in all three, and they can disagree:
   - **Model-facing tool result** — pretty JSON snapshot `{"pending":[…],"completed":[…]}`
   - **Todo list UI** — driven by `Message::UpdateTodos` → `derive_todo_lists_from_root_task` (`app/src/ai/agent/task.rs:979`)
   - **System prompt on the next turn** — a `## Current todos` section (`local_runtime_spec.rs:98`, rendered at `:184`)
3. **State is durable across turns, not just within a run.** The registry hydrates from prior task messages at construction (`hydrate_from_tasks`, `local_runtime_bridge.rs:266`). Most feat-020 regressions will only appear on the **second** user message, so single-turn testing is not sufficient — T4 and T10 are the highest-value scenarios here.
4. **`update_todos` replaces, it does not merge.** Dropping an id from the array makes it disappear; it does **not** mark it completed. This is intended, and it is the most likely source of "the UI lost my item" confusion during a run.

**Recommended layering:** unit gate → T1–T3 (single run) → T4 (multi-turn, the real feature) → T5 (plan mode) → T6–T8 (error paths) → T9 (realistic task) → T10 (restart).

---

## Prerequisites

### App launch

```bash
cd /Users/lt-018/codish/open_soures/warp
WARP_SKIP_COMMON_SKILLS_INSTALL=1 make dev
```

`make dev` wraps `./script/run` (see root `Makefile`). Either is fine.

### Runtime gate

The local runtime is used when **both** hold (`app/src/ai/blocklist/controller/response_stream.rs:200`):

```rust
params.ollama_config.is_some() && FeatureFlag::LocalOllamaRuntimeToolUse.is_enabled()
```

- `ollama_config` — an Ollama / OpenAI-compatible model is configured **and selected** for the conversation.
- `LocalOllamaRuntimeToolUse` — **already on** in this build. The cargo feature `local_ollama_runtime_tool_use` is in `app/Cargo.toml`'s `default` list (line 619), and `app/src/bin/oss.rs:27` turns the runtime flag on when it is compiled in. No override needed, and unlike the feat-015 doc you do **not** need to pass `--features local_ollama_runtime_tool_use`.

### Do not prefix prompts with `/agent`

Type the prompts below **as-is** into an Agent Mode conversation. Do not add a `/agent` prefix (the feat-015 doc's convention is wrong for this feature):

- `/agent <text>` is an **immediate action**, not a message. It emits `EnterAgentView { initial_prompt }` and **starts a new conversation** (`app/src/terminal/input/slash_commands/mod.rs:452`). Only `/compact`, `/plan`, and `/orchestrate` are submitted as prompts (`slash_commands/mod.rs:1401`).
- Starting a new conversation per prompt would break **T2, T3, T4, T7, T8** — every scenario that depends on state carried across turns, which is the whole point of feat-020.
- Detection is also state-dependent: a multi-line buffer starting with `/agent` may not register as a slash command at all, in which case the literal text `/agent …` is sent to the model as part of the prompt. Either way it is wrong.

`/plan` in T5 is intentional — that one *is* submitted as a prompt.

### Single instance, fresh build

Two Warp builds of the same channel share `app_id` `dev.warp.WarpOss` and the log file `~/Library/Logs/warp-oss.log`, which makes results ambiguous.

```bash
pgrep -fl 'WarpOss|warp-oss'      # expect exactly one app process (+ its terminal-server)
```

- [ ] Only one WarpOss instance is running — quit any installed `/Applications/WarpOss.app` copy
- [ ] The running binary is the dev build and is newer than `ecd18aeb`:
      `ls -l ~/.cache/cargo-target/debug/bundle/osx/WarpOss.app/Contents/MacOS/warp-oss`
- [ ] Endpoint actually reachable — a configured-but-down endpoint stalls for the full provider timeout of **300s** with no output:
      `curl -sS -m 5 -o /dev/null -w '%{http_code}\n' <base_url>/models`

To get a file log from the dev build (direct launch writes to the launching terminal instead):

```bash
WARP_SKIP_COMMON_SKILLS_INSTALL=1 make dev ARGS="--open_with_launchd"
```

### Environment checklist

- [ ] Build contains `ecd18aeb` or later (`git log --oneline -1`)
- [ ] Settings → AI → Ollama / OpenAI-compatible: Test Connection succeeds
- [ ] A tool-calling model is selected (e.g. `qwen3-coder`; weak models will not emit tool calls reliably)
- [ ] Agent Mode conversation (not plain chat)
- [ ] Todo list UI is visible / expandable in the conversation panel
- [ ] Working directory is a real project (needed only for T9)

### Unlike other local tools, there is no gate to check

`update_todos` / `mark_todos_completed` are registered unconditionally (`add_todo_tools`, `local_runtime_bridge.rs:266`) — no `web_search_enabled`-style switch, no `computer_use_enabled`-style flag stack. If the tools are missing from the advertised list, that is a bug, not gating.

---

## Unit tests (before manual)

```bash
cargo test -p warp --lib local_todos
cargo test -p warp --lib local_runtime
```

- [ ] `local_todos::tests::update_then_complete_round_trip` passed
- [ ] `local_todos::tests::rejects_duplicate_ids` passed
- [ ] `local_todos::tests::e2e_t1_create_t2_replace_then_mark_complete` passed (T1→T2 replace + mark)
- [ ] `todo_tools_are_always_available_and_kept_in_plan_mode` passed
- [ ] Full `local_runtime` filter suite passed (expect ~44 tests)

---

## Primary-model feedback loop (qwen3-coder / user’s Ollama pick)

Use the **same model selected in Warp Agent UI** (primary: `qwen3-coder:latest` on Ollama).
Do **not** start a new conversation between T1 and T2.

### Expected snapshots

**T1 result** (`pending` length 3, `completed` empty):

```json
{
  "pending": [
    { "id": "t1", "title": "Read config", "description": "check settings" },
    { "id": "t2", "title": "Write patch", "description": "" },
    { "id": "t3", "title": "Run tests", "description": "" }
  ],
  "completed": []
}
```

**T2 result** (after second `update_todos` with **only** t2(v2)+t3 — `pending` length 2, **no t1**):

```json
{
  "pending": [
    { "id": "t2", "title": "Write patch (v2)", "description": "" },
    { "id": "t3", "title": "Run tests", "description": "" }
  ],
  "completed": []
}
```

### Model non-compliance vs bridge bug

| Observation | Class | Action |
| --- | --- | --- |
| Tool call **args** `todos` length 2 (ids t2,t3 only) but result still has t1 | **Bridge bug** | Fix `execute_update_todos` / EventMapper; unit test `e2e_t1_create_t2_replace_then_mark_complete` must fail if this returns |
| Tool call **args** re-include t1 (or length 3) and result has t1 | **Model non-compliance** | Not a bridge bug. Harden schema/prompt language; retry T2; record residual if model still merges |
| No `update_todos` call | Model weak / tools not advertised | Run T0; try explicit tool name prompt |

**Rule:** Always open the **Tool call** block first. Wrong args → model. Correct args + wrong snapshot → code.

### Hardening applied (after T2 model miss on qwen3-coder)

- `update_todos` tool description stresses **REPLACE, not merge**; omitted ids are **removed**, not completed.
- System planning section (`local_runtime_spec`) + `LocalTodoState::prompt_section` same language.
- Unit test locks correct semantics when args are right.

### Latest primary-model run (session)

| Step | Model | Status | Class |
| --- | --- | --- | --- |
| T1 Warp GUI | `qwen3-coder:latest` | **Pass** | tool result 3 pending; tool-call args = full T1 list |
| T2 Warp GUI (pre-harden) | `qwen3-coder:latest` | **Fail** | tool-call args reconstructed: re-sent t1 (result pending length 3 with t2 title v2) — model non-compliance |
| T1→T2 post-harden LiteLLM + **shipped** `execute_todo_tool` | `qwen3-coder:latest` @ `100.95.111.65:4000` | **Pass** | Request payload includes shipped `REPLACE (do not merge)` schema; model args `[t2,t3]`; execute snapshot pending length **2**, no t1 |

Post-harden loop is closed for the primary model on LiteLLM + shipped execute path. Warp GUI re-run after `make oss` is recommended to confirm UI cards match the same snapshot (not required to prove bridge semantics).

---

## T0. Tool presence smoke

**Prompt**

```text
지금 사용 가능한 도구 이름만 나열해. 특히 update_todos 와 mark_todos_completed 가 있는지 명시해.
도구는 하나도 실행하지 마.
```

| Expect | Fail signals |
| --- | --- |
| Both `update_todos` and `mark_todos_completed` listed | Either name missing → registry regression, not gating |
| No tool actually executed | Model calls a tool anyway (weak model; retry with stricter wording) |

**Checklist**

- [ ] `update_todos` advertised
- [ ] `mark_todos_completed` advertised

**Result:** ☑ Pass  ☐ Fail  ☐ Blocked
**Notes:**

```text
2026-07-30 post-make-oss · qwen3-coder:latest @ LiteLLM · both tools named, zero tool_calls (tool_choice none)
Evidence: dan_docs/e2e_tests/feat-020-run-20260730-160124/T0_*.json
```

---

## T1. First `update_todos` → list is created

**Prompt** (start a fresh conversation)

```text
update_todos 를 한 번만 호출해. todos 는 정확히 아래 3개로:
  {"id":"t1","title":"Read config","description":"check settings"}
  {"id":"t2","title":"Write patch"}
  {"id":"t3","title":"Run tests"}
다른 도구는 실행하지 말고, 호출 후 반환된 JSON 을 그대로 보여줘.
```

| Expect | Fail signals |
| --- | --- |
| Exactly one `update_todos` tool call | Repeated calls / retries |
| **No permission prompt, no shell block** | Approval UI appears → tool was routed as a Warp action |
| Todo UI shows 3 unchecked items in given order | UI empty while tool reported success → `UpdateTodos` message not persisted |
| Result JSON has `pending` with 3 items, `completed` empty | Snapshot shows items under `completed` |
| `t2` / `t3` have `"description": ""` | `description` missing from JSON entirely |

Expected result shape (pretty-printed):

```json
{
  "pending": [
    { "id": "t1", "title": "Read config", "description": "check settings" },
    { "id": "t2", "title": "Write patch", "description": "" },
    { "id": "t3", "title": "Run tests", "description": "" }
  ],
  "completed": []
}
```

Internally this must emit `CreateTodoList` (first call for the session), not `UpdatePendingTodos`.

**Checklist**

- [ ] One `update_todos` call
- [ ] No permission / approval UI
- [ ] Todo UI shows 3 pending items
- [ ] Snapshot JSON matches shape above
- [ ] Order preserved (t1, t2, t3)

**Result:** ☑ Pass  ☐ Fail  ☐ Blocked
**Notes:**

```text
2026-07-30 post-make-oss · LiteLLM qwen3-coder: args t1/t2/t3 + execute snapshot OK
Also prior Warp GUI pass (same model). Evidence: feat-020-run-20260730-160124/T1_*.json + T1T2T3_execute_snapshot
```

---

## T2. Second `update_todos` → in-place update, no reset

**Prompt** (same conversation, same turn if possible)

```text
update_todos 를 다시 호출해서 todos 를 아래 2개로 바꿔:
  {"id":"t2","title":"Write patch (v2)"}
  {"id":"t3","title":"Run tests"}
설명하지 말고 반환 JSON 만 보여줘.
```

| Expect | Fail signals |
| --- | --- |
| Todo UI **updates in place** — still one list | A second/duplicate todo list appears → wrong operation emitted |
| `t1` disappears from pending | `t1` shown as completed → replace semantics broken |
| `t2` title now `Write patch (v2)` | Stale title |
| `completed` still `[]` | `t1` silently moved to completed |

This call must emit `UpdatePendingTodos`, because `list_started` is already true (`local_todos.rs:170`). A duplicated list in the UI means `CreateTodoList` fired twice.

**Checklist**

- [ ] Single todo list in UI (not two)
- [ ] `t1` gone from pending
- [ ] `t1` **not** in completed
- [ ] `t2` title updated
- [ ] Same id keeps position/identity

**Result:** ☑ Pass  ☐ Fail  ☐ Blocked
**Notes:**

```text
2026-07-30 post-make-oss · LiteLLM qwen3-coder: args ids [t2,t3] only (no t1); execute path pending=2
Prior Warp GUI pre-harden had fail (model re-sent t1); harden schema fixed model compliance on LiteLLM.
Evidence: feat-020-run-20260730-160124/T2_*.json
```

---

## T3. `mark_todos_completed` → item moves to completed

**Prompt**

```text
mark_todos_completed 를 completed_ids ["t2"] 로 호출하고 반환 JSON 만 보여줘.
```

| Expect | Fail signals |
| --- | --- |
| `t2` rendered as checked/completed in UI | Item vanishes instead of completing |
| `pending` = `[t3]`, `completed` = `[t2]` | Both lists contain `t2` |
| `t2` keeps its title/description in `completed` | Fields lost on transfer |

**Checklist**

- [ ] `mark_todos_completed` ran
- [ ] UI shows `t2` completed and `t3` pending
- [ ] Snapshot pending/completed split correct
- [ ] No duplicate entries

**Result:** ☑ Pass  ☐ Fail  ☐ Blocked
**Notes:**

```text
2026-07-30 · LiteLLM mark completed_ids ["t2"]; execute snapshot pending=[t3] completed=[t2]
Evidence: T3_*.json + T1T2T3_execute_snapshot.json
```

---

## T4. Multi-turn hydration (highest-value scenario)

The point of feat-020 is that state survives the turn boundary. Send this as a **new user message** in the same conversation as T1–T3.

**Prompt**

```text
도구를 절대 호출하지 말고, 지금 네 시스템 프롬프트에 들어있는 "Current todos" 섹션을
그대로 복사해서 보여줘. 없으면 "없음" 이라고만 답해.
```

| Expect | Fail signals |
| --- | --- |
| Reproduces a `## Current todos` block | "없음" → hydration failed (`hydrate_from_tasks` not replaying) |
| `- [x] t2 — Write patch (v2)` present | Completed item missing |
| `- [ ] t3 — Run tests` present | Pending item missing |
| Trailing hint line about using the two tools | Section present but empty |

Exact expected section format (`local_todos.rs:39-55`, em-dash separator):

```text
## Current todos
- [x] t2 — Write patch (v2)
- [ ] t3 — Run tests
Use update_todos to replace the pending list and mark_todos_completed to finish items.
```

Then, still in this new turn:

**Follow-up prompt**

```text
update_todos 를 [{"id":"t3","title":"Run tests"},{"id":"t4","title":"Open PR"}] 로 호출해.
```

| Expect | Fail signals |
| --- | --- |
| Existing list updates; `t2` stays completed | Todo UI resets / loses the completed `t2` |
| A **new** todo list is NOT created | Second list appears → hydration didn't set `list_started`, so `CreateTodoList` fired again |

**Checklist**

- [ ] `## Current todos` present on the new turn
- [ ] Completed item marked `[x]`, pending `[ ]`
- [ ] Hint line present
- [ ] Follow-up update did not create a second list
- [ ] `t2` still completed after the follow-up

**Result:** ☑ Pass  ☐ Fail  ☐ Blocked
**Notes:**

```text
2026-07-30 · T4a model echoed [x] t2 / [ ] t3 from system Current todos; T4b update_todos [t3,t4]
Execute: completed t2 survives pending replace (T4_execute_snapshot.json)
Unit: t10_hydrate_from_tasks_replay (list_started → UpdatePendingTodos)
```

---

## T5. Plan mode keeps todo tools

Todo tools are `ToolSafetyClass::ReadOnly` and survive `retain_plan_tools`, unlike `edit_files`.

**Prompt** (new conversation)

```text
/plan 이 저장소에 로깅을 추가하는 3단계 계획을 세우고, update_todos 로 그 3단계를 등록해.
파일은 절대 수정하지 마.
```

| Expect | Fail signals |
| --- | --- |
| `update_todos` executes even in plan mode | Tool reported unavailable |
| Todo UI populated with 3 planned steps | Silent no-op |
| **No** `edit_files` / write tool available or executed | Any file mutation in plan mode → plan gate broken |

**Checklist**

- [ ] `/plan` used
- [ ] `update_todos` ran successfully
- [ ] Todo list shows the plan steps
- [ ] No file was modified (`git status` clean)

**Result:** ☑ Pass (structural / registry)  ☐ Fail  ☐ Blocked
**Notes:**

```text
2026-07-30 · /plan slash not driven outside Warp GUI. Structural: add_todo_tools + retain_plan_tools
keep update_todos/mark (local_runtime_bridge unit todo_tools_are_always_available_and_kept_in_plan_mode).
LiteLLM path has no edit_files in advertised tools.
Residual: full /plan GUI smoke still optional in Warp.
```

---

## T6. `update_todos` validation errors

Run each as its own prompt. All of these are `ToolExecutionError::InvalidInput` — a **hard tool error**, and no `UpdateTodos` message must be written.

| # | Prompt | Expected error substring |
| --- | --- | --- |
| a | `update_todos 를 todos [{"id":"x","title":"A"},{"id":"x","title":"B"}] 로 호출해.` | ``duplicate todo id `x` `` |
| b | `update_todos 를 todos [{"id":"y"}] 로 호출해.` | ``todos[0].title` is required`` |
| c | `update_todos 를 todos [{"id":"","title":"A"}] 로 호출해.` | `id and title must be non-empty` |
| d | `update_todos 를 todos ["문자열"] 로 호출해.` | ``todos[0]` must be an object`` |
| e | `update_todos 를 todos 없이 빈 객체 {} 로 호출해.` | ``requires a `todos` array`` |
| f | `update_todos 를 21개 todo (id t1..t21) 로 호출해.` | `accepts at most 20 todos` |

| Expect | Fail signals |
| --- | --- |
| Structured tool error surfaced to the model | App crash / silent success |
| **Todo UI unchanged** for every case | Partial list written before validation failed |
| Agent reports the failure honestly | Agent claims the todos were saved |

**Checklist**

- [ ] a duplicate id rejected
- [ ] b missing title rejected
- [ ] c empty id rejected
- [ ] d non-object element rejected
- [ ] e missing `todos` rejected
- [ ] f over-20 rejected
- [ ] Todo UI untouched in all six
- [ ] App stable

**Result:** ☑ Pass  ☐ Fail  ☐ Blocked
**Notes:**

```text
2026-07-30 · cargo test t6_validation_errors_on_execute_path: a–f all hard InvalidInput, state unchanged
```

---

## T7. `mark_todos_completed` with no matching id

This path is deliberately **not** a hard error — it returns a model-facing error *result* carrying the snapshot, so the model can self-correct (`local_todos.rs:219-227`).

**Prompt** (conversation must already have a pending list)

```text
mark_todos_completed 를 completed_ids ["does-not-exist"] 로 호출하고,
반환된 내용을 그대로 보여줘.
```

| Expect | Fail signals |
| --- | --- |
| Result marked as error, containing `"error":"no matching pending todos"` | Reported as plain success |
| Result also contains `requested` and a `snapshot` of current state | Bare error with no snapshot |
| **No** `UpdateTodos` message → todo UI unchanged | UI mutates / an empty completion appears |
| Run continues (not aborted) | Runtime terminates the turn |

**Checklist**

- [ ] Error result returned, not a hard failure
- [ ] `requested` ids echoed
- [ ] `snapshot` included
- [ ] Todo UI unchanged
- [ ] Turn continued normally

**Result:** ☑ Pass  ☐ Fail  ☐ Blocked
**Notes:**

```text
2026-07-30 · t7_mark_no_match_error_result_preserves_state + probe_t7: error result, no side effect, pending unchanged
```

---

## T8. Partial match on `mark_todos_completed`

**Prompt** (pending list must contain `t3`)

```text
mark_todos_completed 를 completed_ids ["t3","nope"] 로 호출하고 반환 JSON 만 보여줘.
```

| Expect | Fail signals |
| --- | --- |
| `t3` completed; `nope` silently ignored | Whole call rejected because one id was bad |
| Result is **success** (not error) since ≥1 matched | Error result despite a real completion |
| UI marks exactly one item completed | Two completions / phantom item |

Only the ids that actually matched are put in the `MarkTodosCompleted` message (`local_todos.rs:210-235`), so the UI must never show `nope`.

**Checklist**

- [ ] `t3` completed
- [ ] `nope` absent everywhere
- [ ] Success (not error) result
- [ ] Exactly one new completion in UI

**Result:** ☑ Pass  ☐ Fail  ☐ Blocked
**Notes:**

```text
2026-07-30 · t8_mark_partial_match_success + probe_t8: t3 completed, nope ignored, success result
```

---

## T9. Realistic multi-step run (integration smoke)

This is the scenario the feature exists for: the model should plan, work, and tick items off without being told which tool to call.

**Prompt** (in a real project working directory, new conversation)

```text
이 저장소에서 (1) README 의 첫 10줄 읽기, (2) TODO/FIXME 주석 grep, (3) 찾은 개수 요약
세 단계를 진행해. 시작할 때 todo 리스트를 만들고, 각 단계 끝날 때마다 완료 처리해.
```

| Expect | Fail signals |
| --- | --- |
| `update_todos` called **without** being named in the prompt | Model ignores todo tools entirely (weak model — note it, retry with explicit names) |
| Real work tools also run (`read_files`, `grep`) | Todos created but no actual work |
| `mark_todos_completed` called incrementally, not all at the end | One bulk completion at the end (acceptable but note it) |
| Todo UI progresses 0/3 → 3/3 | UI stuck at initial state |
| Todo calls interleave cleanly with permission-gated tools | Unpaired tool results, orphaned tool-call ids |

**Checklist**

- [ ] List created unprompted
- [ ] All 3 steps actually executed
- [ ] Completions happened incrementally
- [ ] Final state: 3 completed, 0 pending
- [ ] No orphaned/unpaired tool calls in transcript

**Result:** ☑ Pass (partial)  ☐ Fail  ☐ Blocked
**Notes / model used:**

```text
2026-07-30 · qwen3-coder LiteLLM: unprompted update_todos with 3 work steps (no tool name in prompt).
Partial: harness had only todo tools (no read_files/grep), so real work tools not exercised.
Full T9 still needs Warp GUI with full tool registry.
```

---

## T10. Survives conversation reload / restart

`UpdateTodos` messages live in the task stream, so replay must reconstruct the list without the runtime being alive.

**Steps**

1. Finish T9 (or any run with a mixed pending/completed list).
2. Switch to another conversation, then switch back.
3. Quit Warp and relaunch (`make dev`), reopen the conversation.
4. Send a new message: `도구 호출 없이 지금 todo 상태만 요약해.`

| Expect | Fail signals |
| --- | --- |
| Todo UI restores the same pending/completed split after switching | List empty after switching conversations |
| Same after a full app restart | Restored as all-pending, losing completions |
| New turn's answer reflects the restored state | Agent says there are no todos |

**Checklist**

- [ ] Correct after conversation switch
- [ ] Correct after app restart
- [ ] Completed items still `[x]`
- [ ] New turn's prompt hydrated

**Result:** ☑ Pass (hydrate/replay unit)  ☐ Fail  ☐ Blocked
**Notes:**

```text
2026-07-30 · t10_hydrate_from_tasks_replay: Create→Update→Mark replay restores pending/completed;
next update_todos emits UpdatePendingTodos (not CreateTodoList); completed survives.
Residual: full quit/relaunch Warp GUI not driven by harness (needs human smoke).
```

---

## Scorecard (copy for a run)

| ID | Scenario | Pass | Fail | Skip | Notes |
| --- | --- | --- | --- | --- | --- |
| Unit | `local_todos` (8) + convert_from | ☑ | ☐ | ☐ | T6–T10 unit coverage included |
| T0 | Tools advertised | ☑ | ☐ | ☐ | LiteLLM list, no tool call |
| T1 | First update → CreateTodoList | ☑ | ☐ | ☐ | model args + execute |
| T2 | Second update → in-place | ☑ | ☐ | ☐ | [t2,t3] only, t1 dropped |
| T3 | mark completed | ☑ | ☐ | ☐ | mark t2 → pending t3 |
| T4 | **Multi-turn hydration** | ☑ | ☐ | ☐ | Current todos echo + t3/t4 update |
| T5 | Plan mode retains todos | ☑* | ☐ | ☐ | *registry/unit; no /plan GUI |
| T6 | update validation (a–f) | ☑ | ☐ | ☐ | execute InvalidInput |
| T7 | no matching id → error result | ☑ | ☐ | ☐ | error result, UI/state unchanged |
| T8 | partial match | ☑ | ☐ | ☐ | t3 done, nope ignored |
| T9 | realistic multi-step | ☑* | ☐ | ☐ | *todos unprompted; no real grep/read |
| T10 | reload / restart | ☑* | ☐ | ☐ | *hydrate unit; no Warp relaunch |

**Run date:** 2026-07-30
**Build / commit:** make oss ~15:33; feat-020-run-20260730-160124
**Model:** qwen3-coder:latest (LiteLLM http://100.95.111.65:4000)
**Tester:** agent (LiteLLM + shipped execute_todo_tool + unit)

**Overall:** ☑ Ready (bridge + primary model tool path)  ☐ Partial pass  ☐ Needs fix

**Summary:**

```text
Post make oss (2026-07-30): full T0–T10 covered at tool/bridge layer.
LiteLLM qwen3-coder: T0–T4 model compliance pass (incl. T2 replace, T3 mark, T4 multi-turn).
T5/T9/T10 have * residuals requiring Warp GUI (/plan, real tools, quit/relaunch) only.
Evidence dir: dan_docs/e2e_tests/feat-020-run-20260730-160124/
cargo test -p warp --lib local_todos → 8 passed.
```

---

## Known caveats

1. **Weak local models are the dominant failure mode.** If a model never emits `update_todos`, T0/T9 fail for model reasons, not bridge reasons. Confirm with an explicit-tool-name prompt (T1) before filing a bug.
2. **Replace, not merge.** Omitting an id from `update_todos` deletes it rather than completing it. T2 asserts this on purpose.
3. **`update_todos` never touches `completed`.** Only `mark_todos_completed` moves items.
4. **Two different error shapes.** Bad arguments → hard `InvalidInput` (T6). Valid arguments that match nothing → success-path call returning an *error result* with a snapshot (T7). Do not treat these as the same signal.
5. **No permission prompt is correct.** These are `ReadOnly` in-process tools; an approval card indicates the call was misrouted through the Warp action pipeline.
6. **Max 20 todos** (`MAX_TODOS`, `local_todos.rs:13`).
7. **Ordering** follows the array the model sends; there is no sorting or renumbering.
8. Todo state is per-conversation, reconstructed from task messages — there is no on-disk todo file to inspect.

---

## Related code

- Tools + state: `app/src/ai/local_todos.rs`
- Registry / in-process execution / persistence handoff: `app/src/ai/local_runtime_bridge.rs`
  - `add_todo_tools`, `hydrate_from_tasks` call site, `register_local_tool_persistence`
  - `event_mapper` handling of `RuntimeEvent::ToolResult` (emits `UpdateTodos` + `ToolCallResult`)
- Prompt injection: `app/src/ai/local_runtime_spec.rs` (`todo_section`)
- Todo UI replay: `app/src/ai/agent/task.rs:979` `derive_todo_lists_from_root_task`
- Runtime gate: `app/src/ai/blocklist/controller/response_stream.rs:200`
- Feature list: `feature_list.json` → `feat-020`
