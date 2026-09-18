# GUI smoke session 2026-07-31

## Done in this session

| Item | Status | Evidence |
| --- | --- | --- |
| Commit feat-020 harden + e2e | ✅ `a11b6bdd` | git log |
| T5 plan-mode registry | ✅ unit | `todo_tools_are_always_available_and_kept_in_plan_mode` |
| T2 model+execute (proxy) | ✅ | `t2_litellm_recheck.json` + `t2_execute_snapshot.json` pending=[t2,t3] |
| T10 hydrate | ✅ unit | `t10_hydrate_from_tasks_replay` |
| Warp GUI T2/T5/T9/T10 | ⏳ blocked | Cannot drive Agent Mode input from this harness; screenpipe offline |

## Warp GUI checklist (you)

Use **PROMPTS.md** in this folder. One WarpOss window (latest make oss). Model: qwen3-coder.

After each step, paste tool result JSON or screenshot here / in chat:

- [ ] T2 GUI — pending length 2, no t1  
- [ ] T5 — `/plan` + update_todos, no file edits  
- [ ] T9 — todos + real read/grep  
- [ ] T10 — switch conv + quit/reopen + state summary  

## Automated recheck log

See `automated_residuals.log`.
