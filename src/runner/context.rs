//! Token counting, in-flight ceilings, and session message persistence.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::engine::{Message, ToolCall};
use crate::state::StateDir;

use super::messages::strip_think_tags;

pub(crate) const CONTEXT_CEILING_DIRECTIVE: &str =
    "[System: Working context budget has reached 90%. Do not execute further tools. \
     Synthesize your final conclusions and response immediately.]";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedMessage {
    pub(crate) role: String,
    pub(crate) content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tool_calls: Vec<PersistedToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedToolCall {
    pub(crate) index: usize,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

impl From<&PersistedMessage> for Message {
    fn from(p: &PersistedMessage) -> Self {
        Message {
            role: p.role.clone(),
            content: p.content.clone(),
            tool_calls: p
                .tool_calls
                .iter()
                .map(|t| ToolCall {
                    index: t.index,
                    id: t.id.clone(),
                    name: t.name.clone(),
                    arguments: t.arguments.clone(),
                })
                .collect(),
            tool_call_id: p.tool_call_id.clone(),
        }
    }
}

pub(crate) fn to_persisted(m: &Message) -> PersistedMessage {
    let content = if m.role == "assistant" {
        strip_think_tags(&m.content)
    } else {
        m.content.clone()
    };
    PersistedMessage {
        role: m.role.clone(),
        content,
        tool_calls: m
            .tool_calls
            .iter()
            .map(|t| PersistedToolCall {
                index: t.index,
                id: t.id.clone(),
                name: t.name.clone(),
                arguments: t.arguments.clone(),
            })
            .collect(),
        tool_call_id: m.tool_call_id.clone(),
    }
}

pub(crate) fn session_messages_path(state: &StateDir, session_id: &str) -> PathBuf {
    state.sessions().join(session_id).join("messages.json")
}

pub(crate) fn save_messages(state: &StateDir, session_id: &str, messages: &[Message]) {
    let path = session_messages_path(state, session_id);
    let sanitized: Vec<PersistedMessage> = messages
        .iter()
        .filter(|m| m.role != "system")
        .map(to_persisted)
        .collect();
    if let Some(parent) = path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        warn!(session = %session_id, "failed to create session history dir: {e}");
        return;
    }
    match serde_json::to_vec_pretty(&sanitized) {
        Ok(bytes) => {
            if let Err(e) = fs::write(&path, bytes) {
                warn!(session = %session_id, "failed to persist session history: {e}");
            }
        }
        Err(e) => {
            warn!(session = %session_id, "failed to serialize session history: {e}");
        }
    }
}

pub(crate) fn load_messages(state: &StateDir, session_id: &str) -> Option<Vec<Message>> {
    let path = session_messages_path(state, session_id);
    let bytes = fs::read(&path).ok()?;
    let persisted: Vec<PersistedMessage> = serde_json::from_slice(&bytes).ok()?;
    Some(persisted.iter().map(Message::from).collect())
}

pub(crate) fn approx_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

pub(crate) fn history_tokens(messages: &[Message]) -> usize {
    messages.iter().map(|m| approx_tokens(&m.content)).sum()
}

pub(crate) fn watermark_tokens(max_context: usize) -> usize {
    (max_context as u64 * 75 / 100) as usize
}

pub(crate) fn in_flight_ceiling_tokens(max_context: usize) -> usize {
    (max_context as u64 * 90 / 100) as usize
}

pub fn resumed_history_tokens(state: &StateDir, session_id: &str, max_context: Option<u32>) -> u32 {
    let Some(mc) = max_context else {
        return 0;
    };
    if mc == 0 {
        return 0;
    }
    let Some(history) = load_messages(state, session_id) else {
        return 0;
    };
    if history.is_empty() {
        return 0;
    }
    let wm = watermark_tokens(mc as usize);
    if history_tokens(&history) <= wm {
        history_tokens(&history) as u32
    } else {
        0
    }
}

pub(crate) fn should_resume(
    history: &[Message],
    new_user_prompt: &str,
    max_context: usize,
) -> bool {
    if max_context == 0 {
        return true;
    }
    let wm = watermark_tokens(max_context);
    let est = history_tokens(history) + approx_tokens(new_user_prompt);
    est <= wm
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::messages::msg;

    fn tmp_state() -> StateDir {
        let dir = std::env::temp_dir().join(format!("castor_test_ctx_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        StateDir::new(&dir)
    }

    fn tc(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            index: 0,
            id: id.into(),
            name: name.into(),
            arguments: args.into(),
        }
    }

    #[test]
    fn save_and_load_round_trips_sanitized_non_system_history() {
        let state = tmp_state();
        let messages = vec![
            msg("system", "SYS"),
            msg("user", "do the thing"),
            Message {
                role: "assistant".into(),
                content: "<think> reasoning 42 </think> the answer is 42".into(),
                tool_calls: vec![tc("c1", "bash", r#"{"cmd":"ls"}"#)],
                tool_call_id: None,
            },
            Message {
                role: "tool".into(),
                content: "file1".into(),
                tool_calls: Vec::new(),
                tool_call_id: Some("c1".into()),
            },
        ];
        save_messages(&state, "roundtrip", &messages);

        let path = state.sessions().join("roundtrip").join("messages.json");
        assert!(path.exists(), "messages.json must be written: {path:?}");

        let loaded = load_messages(&state, "roundtrip").expect("history must load");
        assert_eq!(loaded.len(), 3, "system must be dropped; got {loaded:?}");
        assert!(!loaded.iter().any(|m| m.role == "system"));

        let assistant = loaded.iter().find(|m| m.role == "assistant").unwrap();
        assert!(!assistant.content.contains("<think>"));
        assert!(assistant.content.contains("42"));
        assert_eq!(assistant.tool_calls.len(), 1);
        assert_eq!(assistant.tool_calls[0].name, "bash");
        assert_eq!(assistant.tool_calls[0].id, "c1");

        let tool = loaded.iter().find(|m| m.role == "tool").unwrap();
        assert_eq!(tool.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(tool.content, "file1");

        let _ = std::fs::remove_dir_all(state.root());
    }

    #[test]
    fn load_messages_absent_file_is_none() {
        let state = tmp_state();
        let loaded = load_messages(&state, "never_saved");
        assert!(loaded.is_none());
        let _ = std::fs::remove_dir_all(state.root());
    }

    #[test]
    fn watermark_within_budget_resumes() {
        let history = vec![msg("user", "a".repeat(400))];
        assert!(
            should_resume(&history, &"b".repeat(100), 1000),
            "history within the 75% watermark must resume"
        );
    }

    #[test]
    fn watermark_over_budget_rolls_over() {
        let history = vec![msg("user", "a".repeat(400))];
        assert!(
            !should_resume(&history, &"b".repeat(100), 100),
            "history over the 75% watermark must roll over"
        );
    }

    #[test]
    fn watermark_disabled_when_max_context_zero() {
        let history = vec![msg("user", "a".repeat(1_000_000))];
        assert!(
            should_resume(&history, "prompt", 0),
            "zero max_context disables the watermark — always resume"
        );
    }
}

