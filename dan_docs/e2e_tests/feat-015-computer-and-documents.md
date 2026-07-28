# feat-015 E2E: Model-gated computer and document actions

Manual end-to-end checklist for local Ollama / LiteLLM agent bridging of:

- **Documents:** `read_documents`, `edit_documents`, `create_documents` (always advertised for local agents)
- **Computer use:** `request_computer_use`, `use_computer` (only when `RequestParams.computer_use_enabled`)

| Field | Value |
| --- | --- |
| Feature ID | `feat-015` |
| Branch (typical) | `cla-dev-2` |
| Related commit | `034fcf94` Bridge local agent document and gated computer-use tools |
| Unit gate | `cargo test -p warp local_runtime --lib --features local_ollama_runtime_tool_use` |

---

## Prerequisites

### App launch

```bash
cd /Users/lt-018/codish/open_soures/warp
WARP_SKIP_COMMON_SKILLS_INSTALL=1 ./script/run --features local_ollama_runtime_tool_use
```

### Environment checklist

- [ ] Build includes feat-015 (`local_runtime_bridge` document + computer-use tools)
- [ ] Settings → Ollama / LiteLLM: Test Connection succeeds
- [ ] Model supports tool calling (e.g. `qwen3-coder` or equivalent)
- [ ] Agent input uses `/agent …` (not plain chat only)
- [ ] Working directory is a normal project (optional; documents are AI docs, not disk files)

### Computer use gate (only for section B)

`computer_use_enabled` requires all of:

1. `FeatureFlag::AgentModeComputerUse`
2. Execution profile computer-use permission **enabled** (not Never)
3. `computer_use::is_supported_on_current_platform()` (macOS usually OK)
4. `FeatureFlag::LocalComputerUse` **or** ambient agent

- [ ] AgentModeComputerUse active in this build
- [ ] LocalComputerUse active **or** testing ambient agent
- [ ] Profile: Computer use = Allow / Always ask
- [ ] macOS Accessibility / Screen Recording granted if real input is needed

If CU flags are off, **section A still validates feat-015 documents**; mark B0 = N and skip B1–B3.

---

## Unit tests (before manual)

```bash
cargo test -p warp document_and_computer_use --lib --features local_ollama_runtime_tool_use
cargo test -p warp local_runtime --lib --features local_ollama_runtime_tool_use
```

- [ ] `document_and_computer_use_tools_are_gated_and_map` passed
- [ ] Full `local_runtime` filter suite passed (expect ~41+ tests)

---

## A. Documents E2E

Documents are **Warp AI documents** (UUID), not repo files. Prefer naming tools explicitly in prompts for weak local models.

### A1. Create → read (core)

**Prompt**

```text
/agent create_documents 로 제목 "feat015-test", 내용 "# Hello from local agent" 인 문서를 하나 만들고,
반환된 document_id 로 read_documents 해서 내용을 그대로 보여줘.
shell 로 파일 만들지 마.
```

| Expect | Fail signals |
| --- | --- |
| Calls `create_documents` | Uses only shell / `edit_files` |
| Document create UI / success path | Tool not advertised |
| Calls `read_documents` with returned UUID | Invents content without tools |
| Body includes `Hello from local agent` | Crash / unpaired tool result |

**Checklist**

- [ ] `create_documents` ran
- [ ] `document_id` (UUID) returned or visible
- [ ] `read_documents` ran with that id
- [ ] Content matches create payload
- [ ] No shell file write for this task

**Result:** ☐ Pass  ☐ Fail  ☐ Blocked  
**Notes / document_id:**

```text

```

---

### A2. Edit after create

**Prompt** (same thread; paste real UUID from A1)

```text
/agent 방금 만든 문서 document_id=<UUID> 에 edit_documents 로
search "# Hello from local agent" → replace "# Hello edited" 적용한 뒤
다시 read_documents 로 확인해서 최종 내용만 보여줘.
```

| Expect | Fail signals |
| --- | --- |
| `edit_documents` with search/replace | Edit applied via shell/sed |
| Re-read shows `# Hello edited` | Stale content / wrong id |

**Checklist**

- [ ] `edit_documents` ran
- [ ] `read_documents` after edit
- [ ] Final content is `# Hello edited` (or expected replace)

**Result:** ☐ Pass  ☐ Fail  ☐ Blocked  
**Notes:**

```text

```

---

### A3. Plan mode strip (documents)

**Prompt**

```text
/plan AI document 를 만들고 내용을 고치는 절차만 계획해. create_documents/edit_documents/shell 은 호출하지 마.
```

| Expect | Fail signals |
| --- | --- |
| Plan text only | Actual create/edit tool cards |
| No mutating document tools | Plan mode still executes write tools |

**Checklist**

- [ ] Used `/plan` (Plan mode)
- [ ] No `create_documents` / `edit_documents` execution
- [ ] No shell mutation for this prompt

**Result:** ☐ Pass  ☐ Fail  ☐ Blocked  
**Notes:**

```text

```

---

### A4. Bad UUID (error path)

**Prompt**

```text
/agent read_documents 로 document_ids ["00000000-0000-0000-0000-000000000000"] 만 호출하고
결과 status/error 를 그대로 요약해.
```

| Expect | Fail signals |
| --- | --- |
| Tool error / not found structured result | App crash |
| Agent reports error honestly | Silent success with fake content |

**Checklist**

- [ ] `read_documents` ran
- [ ] Error / not-found outcome
- [ ] App remains stable

**Result:** ☐ Pass  ☐ Fail  ☐ Blocked  
**Notes:**

```text

```

---

## B. Computer use E2E

### B0. Tool presence smoke

**Prompt**

```text
/agent 지금 사용 가능한 도구 목록에 request_computer_use 와 use_computer 가 있는지 확인하고,
있으면 있다고만, 없으면 없다고만 답해. 다른 도구는 실행하지 마.
```

**Checklist**

- [ ] Agent answered clearly (present / absent)
- [ ] If **absent**: CU gate off → document-only E2E is still valid; skip B1–B3
- [ ] If **present**: continue B1

**Tools present?** ☐ Yes  ☐ No  

**Result:** ☐ Pass  ☐ Fail  ☐ Blocked (gate off expected)  
**Notes:**

```text

```

---

### B1. Request approval only

**Prompt**

```text
/agent request_computer_use 만 호출해. task_summary 는 "feat015 e2e smoke".
use_computer 는 호출하지 마. 승인/거절 결과 status 만 알려줘.
```

| Expect | Fail signals |
| --- | --- |
| Approval UI for computer use | Tool missing when gate should be on |
| Accept → `status: approved` + screenshot width/height **metadata** | Full image required in tool JSON (not expected for local) |
| Reject → cancelled/denied, no crash | Hang forever without result |

**Checklist**

- [ ] `request_computer_use` ran
- [ ] Approval UI shown (if Always ask)
- [ ] Accept path: approved + dimensions metadata **or** Reject path: cancelled
- [ ] Did **not** call `use_computer` in this prompt

**Result:** ☐ Pass  ☐ Fail  ☐ Skipped (B0 No)  
**Notes:**

```text

```

---

### B2. Minimal `use_computer` after approval

**Prompt**

```text
/agent request_computer_use 로 승인받은 뒤,
use_computer 로 action_summary "type smoke", actions 에 TypeText "feat015" 만 보내고
take_screenshot false 로 실행해. 완료 status 만 알려줘.
```

| Expect | Fail signals |
| --- | --- |
| `use_computer` with TypeText | Invalid actions JSON, no retry/error |
| `status: completed` | Crash / beach ball |
| May type into focused app — use empty editor if possible | Unintended system damage (stop if unsafe) |

**Checklist**

- [ ] Approval completed (if required)
- [ ] `use_computer` ran
- [ ] Result status completed (or clear error)
- [ ] Screenshot pixels **not** required in model-facing JSON

**Result:** ☐ Pass  ☐ Fail  ☐ Skipped (B0 No)  
**Notes:**

```text

```

---

### B3. Plan mode strip (computer use)

**Prompt**

```text
/plan 화면을 클릭해서 설정을 여는 computer use 계획만 세워. request_computer_use/use_computer 는 호출하지 마.
```

| Expect | Fail signals |
| --- | --- |
| Plan text only | Actual CU tools executed |

**Checklist**

- [ ] `/plan` used
- [ ] No `request_computer_use` / `use_computer` execution

**Result:** ☐ Pass  ☐ Fail  ☐ Skipped (optional if CU off)  
**Notes:**

```text

```

---

## Recommended 30-minute order

1. [ ] Unit tests
2. [ ] **A1** create + read
3. [ ] **A2** edit
4. [ ] **A3** plan documents
5. [ ] **A4** bad UUID
6. [ ] **B0** CU tools present?
7. [ ] If yes: **B1** → **B2** → **B3**
8. [ ] If no: record gate-off, treat document path as primary E2E for this build

---

## Scorecard (copy for a run)

| ID | Scenario | Pass | Fail | Skip | Notes |
| --- | --- | --- | --- | --- | --- |
| Unit | `local_runtime` suite | ☐ | ☐ | ☐ | |
| A1 | create + read | ☐ | ☐ | ☐ | |
| A2 | edit + re-read | ☐ | ☐ | ☐ | |
| A3 | plan strip (docs) | ☐ | ☐ | ☐ | |
| A4 | bad UUID | ☐ | ☐ | ☐ | |
| B0 | CU tools present? | ☐ | ☐ | ☐ | Y/N: |
| B1 | request_computer_use | ☐ | ☐ | ☐ | |
| B2 | use_computer TypeText | ☐ | ☐ | ☐ | |
| B3 | plan strip (CU) | ☐ | ☐ | ☐ | |

**Run date:**  
**Build / commit:**  
**Model:**  
**Tester:**  

**Overall:** ☐ Ready  ☐ Docs-only pass  ☐ Needs fix  

**Summary:**

```text

```

---

## Known caveats

1. Weak local models: put **exact tool names** in the prompt.
2. AI documents ≠ filesystem files; UUIDs come from create/result context.
3. Local CU results intentionally omit screenshot **pixels** (metadata only); multimodal image feedback is later Phase C work.
4. Missing CU tools with flags off is **expected gating**, not necessarily a bridge regression.
5. Real mouse/keyboard CU can affect the desktop — prefer short TypeText smokes in a disposable editor.

---

## Related code

- Bridge: `app/src/ai/local_runtime_bridge.rs`
- Gate: `app/src/ai/agent/api.rs` (`computer_use_enabled`)
- Executors: `app/src/ai/blocklist/action_model/execute/{read,edit,create}_documents.rs`, `request_computer_use.rs`, `use_computer.rs`
- Feature list: `feature_list.json` → `feat-015`
