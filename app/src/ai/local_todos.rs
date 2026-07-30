//! Durable local plan/todo state for the Ollama agent runtime.
//!
//! Tools update an in-process list and emit Warp `UpdateTodos` task messages so
//! the existing todo UI and `derive_todo_lists_from_root_task` replay stay in sync.

use std::collections::HashSet;

use local_agent_runtime::tools::schema::{ToolSchema, ToolSchemaBuilder};
use local_agent_runtime::{ToolCall, ToolCallResult, ToolExecutionError};
use serde::Serialize;
use warp_multi_agent_api as api;

const MAX_TODOS: usize = 20;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LocalTodoItem {
    pub id: String,
    pub title: String,
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct LocalTodoState {
    pub pending: Vec<LocalTodoItem>,
    pub completed: Vec<LocalTodoItem>,
    /// True after the first CreateTodoList / non-empty update for this session.
    list_started: bool,
}

impl LocalTodoState {
    pub fn snapshot_json(&self) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "pending": self.pending,
            "completed": self.completed,
        }))
        .unwrap_or_else(|_| r#"{"pending":[],"completed":[]}"#.to_string())
    }

    pub fn prompt_section(&self) -> Option<String> {
        if self.pending.is_empty() && self.completed.is_empty() {
            return None;
        }
        let mut lines = vec!["## Current todos".to_string()];
        for item in &self.completed {
            lines.push(format!("- [x] {} — {}", item.id, item.title));
        }
        for item in &self.pending {
            lines.push(format!("- [ ] {} — {}", item.id, item.title));
        }
        lines.push(
            "Use update_todos with the FULL remaining pending list only (REPLACE, not merge — \
omitted ids disappear). Use mark_todos_completed to finish items."
                .to_string(),
        );
        Some(lines.join("\n"))
    }

    /// Apply UpdateTodos operations from prior task messages (resume / multi-turn).
    pub fn hydrate_from_tasks(&mut self, tasks: &[api::Task]) {
        *self = LocalTodoState::default();
        for task in tasks {
            for message in &task.messages {
                let Some(api::message::Message::UpdateTodos(update)) = &message.message else {
                    continue;
                };
                let Some(op) = &update.operation else {
                    continue;
                };
                self.apply_proto_operation(op);
            }
        }
    }

    fn apply_proto_operation(&mut self, op: &api::message::update_todos::Operation) {
        use api::message::update_todos::Operation;
        match op {
            Operation::CreateTodoList(create) => {
                self.list_started = true;
                self.pending = create
                    .initial_todos
                    .iter()
                    .map(local_item_from_proto)
                    .collect();
                self.completed.clear();
            }
            Operation::UpdatePendingTodos(update) => {
                self.list_started = true;
                self.pending = update
                    .updated_pending_todos
                    .iter()
                    .map(local_item_from_proto)
                    .collect();
            }
            Operation::MarkTodosCompleted(mark) => {
                for id in &mark.todo_ids {
                    if let Some(pos) = self.pending.iter().position(|t| &t.id == id) {
                        let item = self.pending.remove(pos);
                        self.completed.push(item);
                    }
                }
            }
        }
    }
}

fn local_item_from_proto(item: &api::TodoItem) -> LocalTodoItem {
    LocalTodoItem {
        id: item.id.clone(),
        title: item.title.clone(),
        description: item.description.clone(),
    }
}

fn proto_item(item: &LocalTodoItem) -> api::TodoItem {
    api::TodoItem {
        id: item.id.clone(),
        title: item.title.clone(),
        description: item.description.clone(),
    }
}

/// Side effect to persist into the Warp task stream after a local tool finishes.
#[derive(Debug, Clone)]
pub enum LocalTodoSideEffect {
    UpdateTodos(api::message::UpdateTodos),
}

pub fn update_todos_schema() -> ToolSchema {
    ToolSchemaBuilder::new(
        "update_todos",
        "REPLACE (do not merge) the entire pending todo list with the `todos` array you pass. \
Any id omitted from `todos` is REMOVED from pending — it is NOT completed and NOT kept. \
Do not re-include finished or dropped items. Use mark_todos_completed to move items to completed. \
Keep ids stable when only titles/descriptions change. Pass the full new pending list every time.",
    )
    .required_array_of_objects(
        "todos",
        "Full replacement pending list (max 20). Each object: id (string), title (string), description (optional string, default \"\"). Omit an id to drop it from pending.",
    )
    .build()
}

pub fn mark_todos_completed_schema() -> ToolSchema {
    ToolSchemaBuilder::new(
        "mark_todos_completed",
        "Mark one or more pending todos as completed by id. Call after finishing the corresponding work.",
    )
    .required_string_array(
        "completed_ids",
        "Ids of pending todos to mark complete",
    )
    .build()
}

/// Execute a local todo tool. Returns model-facing result and optional transcript side effect.
pub fn execute_todo_tool(
    call: &ToolCall,
    state: &mut LocalTodoState,
) -> Result<(ToolCallResult, Option<LocalTodoSideEffect>), ToolExecutionError> {
    match call.name.as_str() {
        "update_todos" => execute_update_todos(call, state),
        "mark_todos_completed" => execute_mark_completed(call, state),
        _ => Err(ToolExecutionError::NotFound {
            name: call.name.clone(),
        }),
    }
}

fn execute_update_todos(
    call: &ToolCall,
    state: &mut LocalTodoState,
) -> Result<(ToolCallResult, Option<LocalTodoSideEffect>), ToolExecutionError> {
    let todos = parse_todo_items(&call.arguments)?;
    let first = !state.list_started;
    state.list_started = true;
    state.pending = todos.clone();

    let operation = if first {
        api::message::update_todos::Operation::CreateTodoList(api::CreateTodoList {
            initial_todos: todos.iter().map(proto_item).collect(),
        })
    } else {
        api::message::update_todos::Operation::UpdatePendingTodos(api::UpdatePendingTodos {
            updated_pending_todos: todos.iter().map(proto_item).collect(),
        })
    };

    let side = LocalTodoSideEffect::UpdateTodos(api::message::UpdateTodos {
        operation: Some(operation),
    });

    Ok((ToolCallResult::success(state.snapshot_json()), Some(side)))
}

fn execute_mark_completed(
    call: &ToolCall,
    state: &mut LocalTodoState,
) -> Result<(ToolCallResult, Option<LocalTodoSideEffect>), ToolExecutionError> {
    let ids = required_string_array(&call.arguments, "completed_ids")?;
    if ids.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `mark_todos_completed` requires a non-empty `completed_ids` array"
                .to_string(),
        });
    }
    for id in &ids {
        if id.trim().is_empty() {
            return Err(ToolExecutionError::InvalidInput {
                reason: "`completed_ids` must not contain empty strings".to_string(),
            });
        }
    }

    let mut marked = Vec::new();
    for id in &ids {
        if let Some(pos) = state.pending.iter().position(|t| &t.id == id) {
            let item = state.pending.remove(pos);
            state.completed.push(item);
            marked.push(id.clone());
        }
    }

    if marked.is_empty() {
        return Ok((
            ToolCallResult::error(format!(
                "{{\"error\":\"no matching pending todos\",\"requested\":{ids:?},\"snapshot\":{}}}",
                state.snapshot_json()
            )),
            None,
        ));
    }

    let side = LocalTodoSideEffect::UpdateTodos(api::message::UpdateTodos {
        operation: Some(api::message::update_todos::Operation::MarkTodosCompleted(
            api::MarkTodosCompleted { todo_ids: marked },
        )),
    });

    Ok((ToolCallResult::success(state.snapshot_json()), Some(side)))
}

fn parse_todo_items(args: &serde_json::Value) -> Result<Vec<LocalTodoItem>, ToolExecutionError> {
    let arr = args
        .get("todos")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: "Tool `update_todos` requires a `todos` array".to_string(),
        })?;

    if arr.len() > MAX_TODOS {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!("Tool `update_todos` accepts at most {MAX_TODOS} todos"),
        });
    }

    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| ToolExecutionError::InvalidInput {
                reason: format!("`todos[{i}]` must be an object"),
            })?;
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolExecutionError::InvalidInput {
                reason: format!("`todos[{i}].id` is required"),
            })?
            .trim()
            .to_string();
        let title = obj
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolExecutionError::InvalidInput {
                reason: format!("`todos[{i}].title` is required"),
            })?
            .trim()
            .to_string();
        if id.is_empty() || title.is_empty() {
            return Err(ToolExecutionError::InvalidInput {
                reason: format!("`todos[{i}]` id and title must be non-empty"),
            });
        }
        if !seen.insert(id.clone()) {
            return Err(ToolExecutionError::InvalidInput {
                reason: format!("duplicate todo id `{id}`"),
            });
        }
        let description = obj
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(LocalTodoItem {
            id,
            title,
            description,
        });
    }
    Ok(out)
}

fn required_string_array(
    args: &serde_json::Value,
    key: &str,
) -> Result<Vec<String>, ToolExecutionError> {
    let arr = args.get(key).and_then(|v| v.as_array()).ok_or_else(|| {
        ToolExecutionError::InvalidInput {
            reason: format!("Missing required string-array argument `{key}`"),
        }
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let Some(s) = item.as_str() else {
            return Err(ToolExecutionError::InvalidInput {
                reason: format!("`{key}[{i}]` must be a string"),
            });
        };
        out.push(s.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[test]
    fn update_then_complete_round_trip() {
        let mut state = LocalTodoState::default();
        let (result, side) = execute_todo_tool(
            &call(
                "update_todos",
                serde_json::json!({
                    "todos": [
                        {"id": "1", "title": "Read", "description": "a"},
                        {"id": "2", "title": "Write"}
                    ]
                }),
            ),
            &mut state,
        )
        .unwrap();
        assert!(!result.is_error);
        assert!(matches!(
            side,
            Some(LocalTodoSideEffect::UpdateTodos(ref u))
                if matches!(
                    u.operation,
                    Some(api::message::update_todos::Operation::CreateTodoList(_))
                )
        ));
        assert_eq!(state.pending.len(), 2);

        let (result2, side2) = execute_todo_tool(
            &call(
                "update_todos",
                serde_json::json!({
                    "todos": [{"id": "2", "title": "Write remaining"}]
                }),
            ),
            &mut state,
        )
        .unwrap();
        assert!(!result2.is_error);
        assert!(matches!(
            side2,
            Some(LocalTodoSideEffect::UpdateTodos(ref u))
                if matches!(
                    u.operation,
                    Some(api::message::update_todos::Operation::UpdatePendingTodos(_))
                )
        ));

        // First create already finished item 1? We replaced pending with only 2.
        // Re-seed and mark complete.
        let mut state = LocalTodoState::default();
        execute_todo_tool(
            &call(
                "update_todos",
                serde_json::json!({
                    "todos": [
                        {"id": "1", "title": "Read"},
                        {"id": "2", "title": "Write"}
                    ]
                }),
            ),
            &mut state,
        )
        .unwrap();
        let (result3, side3) = execute_todo_tool(
            &call(
                "mark_todos_completed",
                serde_json::json!({ "completed_ids": ["1"] }),
            ),
            &mut state,
        )
        .unwrap();
        assert!(!result3.is_error);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.completed.len(), 1);
        assert!(side3.is_some());
        assert!(state.prompt_section().unwrap().contains("[x] 1"));
        assert!(state.prompt_section().unwrap().contains("[ ] 2"));
    }

    #[test]
    fn rejects_duplicate_ids() {
        let mut state = LocalTodoState::default();
        let err = execute_todo_tool(
            &call(
                "update_todos",
                serde_json::json!({
                    "todos": [
                        {"id": "1", "title": "A"},
                        {"id": "1", "title": "B"}
                    ]
                }),
            ),
            &mut state,
        )
        .unwrap_err();
        assert!(matches!(err, ToolExecutionError::InvalidInput { .. }));
    }

    /// Mirrors e2e T1 → T2 → mark-complete on the shipped `execute_todo_tool` path.
    /// Second update with only t2(v2)+t3 must drop t1 (replace, not merge).
    #[test]
    fn e2e_t1_create_t2_replace_then_mark_complete() {
        let mut state = LocalTodoState::default();

        // T1 — create with exact three items from feat-020 e2e prompt.
        let (r1, side1) = execute_todo_tool(
            &call(
                "update_todos",
                serde_json::json!({
                    "todos": [
                        {"id": "t1", "title": "Read config", "description": "check settings"},
                        {"id": "t2", "title": "Write patch"},
                        {"id": "t3", "title": "Run tests"}
                    ]
                }),
            ),
            &mut state,
        )
        .unwrap();
        assert!(!r1.is_error);
        assert!(matches!(
            side1,
            Some(LocalTodoSideEffect::UpdateTodos(ref u))
                if matches!(
                    u.operation,
                    Some(api::message::update_todos::Operation::CreateTodoList(_))
                )
        ));

        let snap1: serde_json::Value = serde_json::from_str(&r1.content).unwrap();
        assert_eq!(snap1["completed"], serde_json::json!([]));
        let pending1 = snap1["pending"].as_array().expect("pending array");
        assert_eq!(pending1.len(), 3);
        assert_eq!(pending1[0]["id"], "t1");
        assert_eq!(pending1[0]["title"], "Read config");
        assert_eq!(pending1[0]["description"], "check settings");
        assert_eq!(pending1[1]["id"], "t2");
        assert_eq!(pending1[1]["title"], "Write patch");
        assert_eq!(pending1[1]["description"], "");
        assert_eq!(pending1[2]["id"], "t3");
        assert_eq!(pending1[2]["title"], "Run tests");
        assert_eq!(pending1[2]["description"], "");

        // T2 — replace with only t2(v2)+t3; t1 must disappear and not move to completed.
        let (r2, side2) = execute_todo_tool(
            &call(
                "update_todos",
                serde_json::json!({
                    "todos": [
                        {"id": "t2", "title": "Write patch (v2)"},
                        {"id": "t3", "title": "Run tests"}
                    ]
                }),
            ),
            &mut state,
        )
        .unwrap();
        assert!(!r2.is_error);
        assert!(matches!(
            side2,
            Some(LocalTodoSideEffect::UpdateTodos(ref u))
                if matches!(
                    u.operation,
                    Some(api::message::update_todos::Operation::UpdatePendingTodos(_))
                )
        ));

        let snap2: serde_json::Value = serde_json::from_str(&r2.content).unwrap();
        assert_eq!(snap2["completed"], serde_json::json!([]));
        let pending2 = snap2["pending"].as_array().expect("pending after replace");
        assert_eq!(
            pending2.len(),
            2,
            "T2 replace must leave exactly 2 pending; got {pending2:?}"
        );
        assert_eq!(pending2[0]["id"], "t2");
        assert_eq!(pending2[0]["title"], "Write patch (v2)");
        assert_eq!(pending2[0]["description"], "");
        assert_eq!(pending2[1]["id"], "t3");
        assert_eq!(pending2[1]["title"], "Run tests");
        assert!(
            pending2.iter().all(|p| p["id"] != "t1"),
            "t1 must be dropped by replace, not kept or completed: {snap2}"
        );

        // mark_todos_completed for t2.
        let (r3, side3) = execute_todo_tool(
            &call(
                "mark_todos_completed",
                serde_json::json!({ "completed_ids": ["t2"] }),
            ),
            &mut state,
        )
        .unwrap();
        assert!(!r3.is_error);
        assert!(side3.is_some());
        let snap3: serde_json::Value = serde_json::from_str(&r3.content).unwrap();
        let pending3 = snap3["pending"].as_array().unwrap();
        let completed3 = snap3["completed"].as_array().unwrap();
        assert_eq!(pending3.len(), 1);
        assert_eq!(pending3[0]["id"], "t3");
        assert_eq!(completed3.len(), 1);
        assert_eq!(completed3[0]["id"], "t2");
        assert_eq!(completed3[0]["title"], "Write patch (v2)");
    }

    #[test]
    fn t6_validation_errors_on_execute_path() {
        let cases: &[(&str, serde_json::Value, &str)] = &[
            (
                "a",
                serde_json::json!({"todos": [{"id": "x", "title": "A"}, {"id": "x", "title": "B"}]}),
                "duplicate todo id",
            ),
            (
                "b",
                serde_json::json!({"todos": [{"id": "y"}]}),
                "title` is required",
            ),
            (
                "c",
                serde_json::json!({"todos": [{"id": "", "title": "A"}]}),
                "non-empty",
            ),
            (
                "d",
                serde_json::json!({"todos": ["문자열"]}),
                "must be an object",
            ),
            ("e", serde_json::json!({}), "requires a `todos` array"),
            (
                "f",
                serde_json::json!({
                    "todos": (1..=21).map(|i| serde_json::json!({"id": format!("t{i}"), "title": format!("T{i}")})).collect::<Vec<_>>()
                }),
                "at most 20",
            ),
        ];
        for (label, args, expect) in cases {
            let mut state = LocalTodoState::default();
            // Seed so a bug that writes on error would mutate something observable.
            execute_todo_tool(
                &call(
                    "update_todos",
                    serde_json::json!({"todos": [{"id": "seed", "title": "Seed"}]}),
                ),
                &mut state,
            )
            .unwrap();
            let before = state.snapshot_json();
            let err = execute_todo_tool(&call("update_todos", args.clone()), &mut state)
                .expect_err(&format!("T6{label} should hard-error"));
            let msg = format!("{err:?}");
            assert!(
                msg.contains(expect),
                "T6{label}: expected `{expect}` in {msg}"
            );
            assert_eq!(
                state.snapshot_json(),
                before,
                "T6{label}: invalid update must not mutate state"
            );
        }
    }

    #[test]
    fn t7_mark_no_match_error_result_preserves_state() {
        let mut state = LocalTodoState::default();
        execute_todo_tool(
            &call(
                "update_todos",
                serde_json::json!({"todos": [{"id": "t3", "title": "Run tests"}, {"id": "t4", "title": "Open PR"}]}),
            ),
            &mut state,
        )
        .unwrap();
        let (result, side) = execute_todo_tool(
            &call(
                "mark_todos_completed",
                serde_json::json!({"completed_ids": ["does-not-exist"]}),
            ),
            &mut state,
        )
        .unwrap();
        assert!(result.is_error);
        assert!(side.is_none(), "no UpdateTodos side effect on zero matches");
        assert!(result.content.contains("no matching pending todos"));
        assert!(result.content.contains("does-not-exist"));
        assert_eq!(state.pending.len(), 2);
        assert!(state.completed.is_empty());
    }

    #[test]
    fn t8_mark_partial_match_success() {
        let mut state = LocalTodoState::default();
        execute_todo_tool(
            &call(
                "update_todos",
                serde_json::json!({"todos": [{"id": "t3", "title": "Run tests"}, {"id": "t4", "title": "Open PR"}]}),
            ),
            &mut state,
        )
        .unwrap();
        let (result, side) = execute_todo_tool(
            &call(
                "mark_todos_completed",
                serde_json::json!({"completed_ids": ["t3", "nope"]}),
            ),
            &mut state,
        )
        .unwrap();
        assert!(!result.is_error);
        assert!(side.is_some());
        let snap: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(snap["pending"].as_array().unwrap().len(), 1);
        assert_eq!(snap["pending"][0]["id"], "t4");
        assert_eq!(snap["completed"].as_array().unwrap().len(), 1);
        assert_eq!(snap["completed"][0]["id"], "t3");
    }

    #[test]
    fn t10_hydrate_from_tasks_replay() {
        use api::message::update_todos::Operation;
        let mut state = LocalTodoState::default();
        // Replay Create → Update pending → Mark completed as task messages would.
        let tasks = vec![api::Task {
            id: "task".into(),
            description: String::new(),
            dependencies: None,
            summary: String::new(),
            server_data: String::new(),
            messages: vec![
                api::Message {
                    id: "m1".into(),
                    task_id: "task".into(),
                    request_id: "r".into(),
                    timestamp: None,
                    server_message_data: String::new(),
                    citations: vec![],
                    fetched_memories: vec![],
                    message: Some(api::message::Message::UpdateTodos(api::message::UpdateTodos {
                        operation: Some(Operation::CreateTodoList(api::CreateTodoList {
                            initial_todos: vec![
                                api::TodoItem {
                                    id: "t1".into(),
                                    title: "Read config".into(),
                                    description: "check settings".into(),
                                },
                                api::TodoItem {
                                    id: "t2".into(),
                                    title: "Write patch".into(),
                                    description: String::new(),
                                },
                                api::TodoItem {
                                    id: "t3".into(),
                                    title: "Run tests".into(),
                                    description: String::new(),
                                },
                            ],
                        })),
                    })),
                },
                api::Message {
                    id: "m2".into(),
                    task_id: "task".into(),
                    request_id: "r".into(),
                    timestamp: None,
                    server_message_data: String::new(),
                    citations: vec![],
                    fetched_memories: vec![],
                    message: Some(api::message::Message::UpdateTodos(api::message::UpdateTodos {
                        operation: Some(Operation::UpdatePendingTodos(api::UpdatePendingTodos {
                            updated_pending_todos: vec![
                                api::TodoItem {
                                    id: "t2".into(),
                                    title: "Write patch (v2)".into(),
                                    description: String::new(),
                                },
                                api::TodoItem {
                                    id: "t3".into(),
                                    title: "Run tests".into(),
                                    description: String::new(),
                                },
                            ],
                        })),
                    })),
                },
                api::Message {
                    id: "m3".into(),
                    task_id: "task".into(),
                    request_id: "r".into(),
                    timestamp: None,
                    server_message_data: String::new(),
                    citations: vec![],
                    fetched_memories: vec![],
                    message: Some(api::message::Message::UpdateTodos(api::message::UpdateTodos {
                        operation: Some(Operation::MarkTodosCompleted(api::MarkTodosCompleted {
                            todo_ids: vec!["t2".into()],
                        })),
                    })),
                },
            ],
        }];
        state.hydrate_from_tasks(&tasks);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.pending[0].id, "t3");
        assert_eq!(state.completed.len(), 1);
        assert_eq!(state.completed[0].id, "t2");
        assert_eq!(state.completed[0].title, "Write patch (v2)");
        let section = state.prompt_section().expect("section");
        assert!(section.contains("[x] t2"));
        assert!(section.contains("[ ] t3"));
        // list_started so next update is UpdatePendingTodos not CreateTodoList
        let (r, side) = execute_todo_tool(
            &call(
                "update_todos",
                serde_json::json!({"todos": [{"id": "t3", "title": "Run tests"}, {"id": "t4", "title": "Open PR"}]}),
            ),
            &mut state,
        )
        .unwrap();
        assert!(!r.is_error);
        assert!(matches!(
            side,
            Some(LocalTodoSideEffect::UpdateTodos(ref u))
                if matches!(
                    u.operation,
                    Some(api::message::update_todos::Operation::UpdatePendingTodos(_))
                )
        ));
        assert_eq!(state.completed.len(), 1, "completed must survive pending replace");
        assert_eq!(state.completed[0].id, "t2");
        assert_eq!(state.pending.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["t3", "t4"]);
    }

    /// Replay a primary-model probe file through the shipped `execute_todo_tool` path.
    ///
    /// Set `WARP_LOCAL_TODO_PROBE` to a JSON file:
    /// ```json
    /// { "steps": [ { "name": "update_todos", "arguments": { "todos": [...] } }, ... ] }
    /// ```
    /// Optional `assert_pending_ids` / `assert_completed_ids` on the last step.
    /// When the env var is unset or empty, the test is a no-op success (CI-safe).
    #[test]
    fn primary_model_probe_drives_execute_todo_tool() {
        let Ok(path) = std::env::var("WARP_LOCAL_TODO_PROBE") else {
            return;
        };
        if path.trim().is_empty() {
            return;
        }
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read probe {path}: {e}"));
        let probe: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse probe: {e}"));
        let steps = probe["steps"]
            .as_array()
            .unwrap_or_else(|| panic!("probe.steps must be an array"));
        assert!(!steps.is_empty(), "probe.steps must be non-empty");

        let mut state = LocalTodoState::default();
        let mut last_snap: Option<serde_json::Value> = None;
        for (i, step) in steps.iter().enumerate() {
            let name = step["name"]
                .as_str()
                .unwrap_or_else(|| panic!("steps[{i}].name required"));
            let arguments = step
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| panic!("steps[{i}].arguments required"));
            let (result, _side) = execute_todo_tool(
                &ToolCall {
                    id: format!("probe-{i}"),
                    name: name.into(),
                    arguments,
                },
                &mut state,
            )
            .unwrap_or_else(|e| panic!("step {i} `{name}` failed: {e:?}"));
            let allow_error = step["allow_error"].as_bool() == Some(true);
            assert!(
                !result.is_error || allow_error,
                "step {i} `{name}` returned error result: {}",
                result.content
            );
            let parsed: serde_json::Value = serde_json::from_str(&result.content)
                .unwrap_or_else(|e| panic!("step {i} snapshot JSON: {e}"));
            // Error results may wrap the snapshot: {"error":...,"snapshot":{...}}
            let snap = if parsed.get("pending").is_some() {
                parsed
            } else if let Some(inner) = parsed.get("snapshot").cloned() {
                if let serde_json::Value::String(s) = inner {
                    serde_json::from_str(&s).unwrap_or_else(|e| panic!("nested snapshot: {e}"))
                } else {
                    inner
                }
            } else if allow_error {
                // Fall back to in-memory state after error path.
                serde_json::from_str(&state.snapshot_json()).unwrap()
            } else {
                parsed
            };
            last_snap = Some(snap);
        }

        let snap = last_snap.expect("at least one step");
        if let Some(ids) = probe["assert_pending_ids"].as_array() {
            let pending = snap["pending"].as_array().expect("pending");
            let got: Vec<&str> = pending
                .iter()
                .filter_map(|p| p["id"].as_str())
                .collect();
            let want: Vec<&str> = ids.iter().filter_map(|v| v.as_str()).collect();
            assert_eq!(got, want, "pending ids mismatch: snap={snap}");
        }
        if let Some(ids) = probe["assert_completed_ids"].as_array() {
            let completed = snap["completed"].as_array().expect("completed");
            let got: Vec<&str> = completed
                .iter()
                .filter_map(|p| p["id"].as_str())
                .collect();
            let want: Vec<&str> = ids.iter().filter_map(|v| v.as_str()).collect();
            assert_eq!(got, want, "completed ids mismatch: snap={snap}");
        }
        if let Some(path_out) = probe["write_snapshot_to"].as_str() {
            std::fs::write(
                path_out,
                serde_json::to_string_pretty(&snap).unwrap() + "\n",
            )
            .unwrap_or_else(|e| panic!("write snapshot {path_out}: {e}"));
        }
    }
}
