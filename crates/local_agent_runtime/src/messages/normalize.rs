//! Message normalization utilities.
//!
//! Handles truncation, merging, and other transformations needed
//! before sending messages to the LLM provider.

use super::{ConversationHistory, Message, SystemMessage};
use crate::config::ContextBudget;
use crate::error::RuntimeError;
use crate::tools::schema::ToolSchema;

/// Build the model-facing view of a transcript without mutating persisted history.
pub fn model_messages(
    messages: &[Message],
    tools: &[ToolSchema],
    budget: &ContextBudget,
) -> Result<Vec<Message>, RuntimeError> {
    let messages = messages
        .iter()
        .cloned()
        .map(|mut message| {
            if let Message::ToolResult(tool_msg) = &mut message {
                tool_msg.result.content =
                    budget_tool_result_content(&tool_msg.result.content, budget);
            }
            message
        })
        .collect::<Vec<_>>();

    let Some(max_input_tokens) = budget.max_input_tokens else {
        return Ok(messages);
    };
    let chars_per_token = budget.approximate_chars_per_token.max(1);
    let available_tokens = max_input_tokens.saturating_sub(budget.reserved_output_tokens);
    let tool_chars = serde_json::to_string(tools).map_or(0, |tools| tools.chars().count());
    let tool_tokens = estimate_tokens(tool_chars, chars_per_token);
    let available_message_tokens = available_tokens.saturating_sub(tool_tokens);
    let available_message_chars = available_message_tokens.saturating_mul(chars_per_token);

    let (system_messages, mut turns) = split_into_turns(messages);
    let preserve_recent_turns = budget.preserve_recent_turns.max(1);
    let mut summaries = Vec::new();
    let mut model_messages = assemble_messages(&system_messages, &summaries, &turns, budget);

    while message_chars(&model_messages) > available_message_chars
        && turns.len() > preserve_recent_turns
    {
        let omitted = turns.remove(0);
        summaries.push(summarize_turn(&omitted));
        model_messages = assemble_messages(&system_messages, &summaries, &turns, budget);
    }

    if message_chars(&model_messages) > available_message_chars {
        compact_tool_results(&mut turns);
        model_messages = assemble_messages(&system_messages, &summaries, &turns, budget);
    }

    let required_chars = message_chars(&model_messages);
    if required_chars > available_message_chars {
        return Err(RuntimeError::ContextBudgetExceeded {
            estimated_tokens: estimate_tokens(
                required_chars.saturating_add(tool_chars),
                chars_per_token,
            ),
            limit: max_input_tokens,
        });
    }

    Ok(model_messages)
}

fn budget_tool_result_content(content: &str, budget: &ContextBudget) -> String {
    let original_chars = content.chars().count();
    if original_chars <= budget.max_tool_result_chars {
        return content.to_string();
    }

    let kept = content
        .chars()
        .take(budget.max_tool_result_chars)
        .collect::<String>();
    serde_json::json!({
        "truncated": true,
        "original_chars": original_chars,
        "kept_chars": budget.max_tool_result_chars,
        "content": kept,
    })
    .to_string()
}

fn split_into_turns(messages: Vec<Message>) -> (Vec<Message>, Vec<Vec<Message>>) {
    let mut system_messages = Vec::new();
    let mut turns = Vec::<Vec<Message>>::new();

    for message in messages {
        match message {
            Message::System(_) => system_messages.push(message),
            Message::User(_) => turns.push(vec![message]),
            Message::Assistant(_) | Message::ToolResult(_) => {
                if let Some(turn) = turns.last_mut() {
                    turn.push(message);
                } else {
                    turns.push(vec![message]);
                }
            }
        }
    }

    (system_messages, turns)
}

fn assemble_messages(
    system_messages: &[Message],
    summaries: &[String],
    turns: &[Vec<Message>],
    budget: &ContextBudget,
) -> Vec<Message> {
    let mut messages = system_messages.to_vec();
    if !summaries.is_empty() {
        let mut content = serde_json::json!({
            "compacted": true,
            "omitted_turns": summaries.len(),
            "summaries": summaries,
        })
        .to_string();
        content = content
            .chars()
            .take(budget.max_compaction_summary_chars)
            .collect();
        messages.push(Message::System(SystemMessage { content }));
    }
    messages.extend(turns.iter().flatten().cloned());
    messages
}

fn summarize_turn(turn: &[Message]) -> String {
    let mut user = String::new();
    let mut assistant = String::new();
    let mut tools = Vec::new();
    for message in turn {
        match message {
            Message::System(message) => {
                if assistant.is_empty() {
                    assistant = message.content.clone();
                }
            }
            Message::User(message) => user = message.content.clone(),
            Message::Assistant(message) => {
                if !message.content.is_empty() {
                    assistant = message.content.clone();
                }
                tools.extend(message.tool_calls.iter().map(|call| call.name.clone()));
            }
            Message::ToolResult(_) => {}
        }
    }

    serde_json::json!({
        "user": truncate(&user, 240),
        "assistant": truncate(&assistant, 240),
        "tools": tools,
    })
    .to_string()
}

fn compact_tool_results(turns: &mut [Vec<Message>]) {
    let Some((latest_turn, earlier_turns)) = turns.split_last_mut() else {
        return;
    };
    for turn in earlier_turns {
        compact_tool_results_in_turn(turn);
    }
    if message_chars(latest_turn) > 100_000 {
        compact_tool_results_in_turn(latest_turn);
    }
}

fn compact_tool_results_in_turn(turn: &mut [Message]) {
    for message in turn {
        if let Message::ToolResult(tool_result) = message {
            let original_chars = tool_result.result.content.chars().count();
            tool_result.result.content = serde_json::json!({
                "compacted": true,
                "original_chars": original_chars,
            })
            .to_string();
        }
    }
}

fn message_chars(messages: &[Message]) -> usize {
    messages.iter().map(message_char_count).sum()
}

fn message_char_count(message: &Message) -> usize {
    match message {
        Message::System(message) => message.content.chars().count(),
        Message::User(message) => message.content.chars().count(),
        Message::Assistant(message) => {
            message.content.chars().count()
                + message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        call.id.chars().count()
                            + call.name.chars().count()
                            + call.arguments.to_string().chars().count()
                    })
                    .sum::<usize>()
        }
        Message::ToolResult(message) => {
            message.call_id.chars().count() + message.result.content.chars().count()
        }
    }
}

fn estimate_tokens(chars: usize, chars_per_token: usize) -> usize {
    chars.saturating_add(chars_per_token - 1) / chars_per_token
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

impl ConversationHistory {
    /// Mutable access to messages (for normalization passes).
    pub(crate) fn messages_mut(&mut self) -> &mut Vec<Message> {
        &mut self.messages
    }
}

#[cfg(test)]
#[path = "normalize_tests.rs"]
mod tests;
