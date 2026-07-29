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
            "Use update_todos to replace the pending list and mark_todos_completed to finish items."
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
        "Replace the pending todo list for this agent run. Use for multi-step work so progress stays visible. Pass the full pending list (completed items are tracked separately via mark_todos_completed). Keep ids stable when updating titles.",
    )
    .required_array_of_objects(
        "todos",
        "Pending todos (max 20). Each object: id (string), title (string), description (optional string).",
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
}
