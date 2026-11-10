//! Salvage recovery pass when a model reaches its reasoning budget ceiling.

use std::fs;
use std::path::{Path, PathBuf};

use crate::engine::Completion;

pub(crate) const REASONING_CEILING_FINISH: &str = "length";

pub(crate) const SALVAGE_PROMPT: &str = "[System: Salvage] Reasoning budget ceiling reached for this dispatch. \
    As an autonomous peer engineer, provide your collaborative status report: \
    (1) summarize the findings, conclusions, and implementations you have reached so far, \
    (2) present any data tables, metrics, or scratchpad artifacts produced, and \
    (3) identify remaining blockers, failing test coordinates, or open questions for the next slice. \
    You have full tools available if you need to inspect scratchpad outputs or verify test logs to ground your report.";

pub(crate) fn is_reasoning_ceiling(completion: &Completion) -> bool {
    completion.tool_calls.is_empty()
        && completion
            .finish_reason
            .as_deref()
            .is_some_and(|f| f == REASONING_CEILING_FINISH)
}

pub(crate) fn salvage_report_path(scratch_root: Option<&Path>, session_id: &str) -> Option<PathBuf> {
    let base = scratch_root?;
    Some(
        base.join(".scratch")
            .join(format!("salvage_{session_id}.md")),
    )
}

pub(crate) fn write_salvage_report(
    path: &Path,
    session_id: &str,
    turns: u32,
    body: &str,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let report = format!(
        "# Salvage report — session `{session_id}`\n\n\
         - **status:** `reasoning_budget_exhausted`\n\
         - **turns consumed:** {turns}\n\n\
         The model reached its reasoning ceiling before completing the task. The \
         conversation history is preserved in the session event ledger; the plain-text \
         summary below was extracted in a single non-coercive salvage turn (tools \
         disabled, low effort).\n\n---\n\n{body}\n"
    );
    fs::write(path, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Metrics;

    #[test]
    fn detects_reasoning_ceiling_on_length_with_tokens() {
        let completion = Completion {
            content: "partial thoughts".into(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            finish_reason: Some("length".into()),
            metrics: Metrics {
                reasoning_tokens: Some(16384),
                ..Default::default()
            },
        };
        assert!(is_reasoning_ceiling(&completion));
    }

    #[test]
    fn non_length_finish_is_not_reasoning_ceiling() {
        let completion = Completion {
            content: "done".into(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            metrics: Metrics {
                reasoning_tokens: Some(16384),
                ..Default::default()
            },
        };
        assert!(!is_reasoning_ceiling(&completion));
    }
}
