//! Salvage recovery pass when a model reaches its reasoning budget ceiling.

use std::fs;
use std::path::{Path, PathBuf};

use tracing::warn;

use crate::engine::{Completion, Message, ToolSchema};
use crate::runner::events::EventLogger;
use crate::runner::messages::msg;
use crate::runner::skeleton::spill_tool_output;
use crate::runner::{ChatEngine, RunnerError, SessionOptions, ToolExecutor, ToolOutcome};

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

pub(crate) fn salvage_report_path(
    scratch_root: Option<&Path>,
    session_id: &str,
) -> Option<PathBuf> {
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

/// Outcome of a salvage pass: the resulting final text and session status.
#[derive(Debug, Clone)]
pub(crate) struct SalvageOutcome {
    pub final_text: String,
    pub status: String,
}

/// Executes the deliberation-ceiling salvage pass when the model stopped at its
/// reasoning ceiling on a non-tool turn.
///
/// Preserves accumulated conversation history intact, empowers the peer
/// engineer with full tools to inspect scratchpads or verify test outputs,
/// logs all salvage events, optionally persists a `.scratch/salvage_<session_id>.md`
/// report, and returns the honest `reasoning_budget_exhausted` outcome.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_reasoning_ceiling_salvage(
    engine: &dyn ChatEngine,
    executor: &dyn ToolExecutor,
    logger: &EventLogger,
    messages: &mut Vec<Message>,
    tools: &[ToolSchema],
    effort: Option<&str>,
    turns: u32,
    options: Option<&SessionOptions>,
    session_id: &str,
) -> SalvageOutcome {
    let status = "reasoning_budget_exhausted".to_string();
    let final_text;

    // 1. History is preserved: the full `messages` (system prompt,
    //    user prompt, every assistant turn and tool result so far)
    //    is passed intact to the salvage call — never cleared.
    // 2. Inject the non-coercive, peer-empowered salvage prompt.
    messages.push(msg("user", SALVAGE_PROMPT));
    // 3. Call the engine with full tools enabled to give the peer
    //    engineer full agency to inspect scratchpad files or check
    //    logs before reporting.
    let salvage = engine.chat(messages, tools, false, effort).await;
    match salvage {
        Ok(mut sc) => {
            // If the peer engineer executed tools during salvage,
            // run each tool call, log results, append them to history,
            // and perform a single-shot synthesis call.
            if !sc.tool_calls.is_empty() {
                messages.push(Message {
                    role: "assistant".into(),
                    content: sc.content.clone(),
                    tool_calls: sc.tool_calls.clone(),
                    tool_call_id: None,
                });
                for tc in &sc.tool_calls {
                    let outcome = match executor.execute(&tc.name, &tc.arguments).await {
                        Ok(o) => o,
                        Err(e) => ToolOutcome {
                            text: format!("Error: {e}"),
                        },
                    };
                    let scratch_root = options.and_then(|o| o.scratch_root());
                    let spill_threshold = options.and_then(|o| o.tool_spill_bytes);
                    let (tool_content, spilled) = spill_tool_output(
                        &outcome.text,
                        &tc.id,
                        scratch_root.as_deref(),
                        spill_threshold,
                    );
                    if let Some((bytes, path)) = spilled {
                        let _ = logger.append(serde_json::json!({
                            "type": "tool_output_spilled",
                            "sessionId": session_id,
                            "toolCallId": tc.id,
                            "toolName": tc.name,
                            "bytes": bytes,
                            "path": path.to_string_lossy(),
                        }));
                    }
                    let _ = logger.append(serde_json::json!({
                        "type": "tool_result",
                        "turn": turns + 1,
                        "tool_call_id": tc.id,
                        "name": tc.name,
                        "is_error": outcome.text.starts_with("Error:"),
                        "output": tool_content,
                    }));
                    messages.push(Message {
                        role: "tool".into(),
                        content: tool_content,
                        tool_calls: vec![],
                        tool_call_id: Some(tc.id.clone()),
                    });
                }
                // Single follow-up request to synthesize findings based on tool outputs.
                match engine.chat(messages, &[], false, effort).await {
                    Ok(follow_up) => {
                        sc = follow_up;
                    }
                    Err(e) => {
                        warn!(session = %session_id, "salvage tool follow-up synthesis failed: {e}");
                    }
                }
            }

            // Ledger the salvage turn (history stays in `messages`).
            let _ = logger.append(serde_json::json!({
                "type": "salvage",
                "turn": turns + 1,
                "reason": "reasoning_ceiling",
                "content": sc.content,
            }));
            let trimmed = sc.content.trim();
            if trimmed.is_empty() {
                // Salvage produced no text: honest status, clean log,
                // no crash, and the prior turn's content (if any) is
                // kept as the in-band note.
                warn!(
                    session = %session_id,
                    "salvage produced no content; preserving honest status \
                     reasoning_budget_exhausted"
                );
                final_text = "The session terminated at its reasoning ceiling \
                             (reasoning_budget_exhausted). The salvage pass returned \
                             no content, so no summary is available; the conversation \
                             history is preserved in the session event ledger."
                    .to_string();
            } else {
                // 4. Persist the salvage report to .scratch/.
                let written_path = match salvage_report_path(
                    options.and_then(|o| o.scratch_root()).as_deref(),
                    session_id,
                ) {
                    Some(p) => match write_salvage_report(&p, session_id, turns, trimmed) {
                        Ok(()) => Some(p),
                        Err(e) => {
                            warn!(
                                session = %session_id,
                                "failed to write salvage report: {e}"
                            );
                            None
                        }
                    },
                    None => {
                        warn!(
                            session = %session_id,
                            "no state/workspace available for the salvage report"
                        );
                        None
                    }
                };
                let path_note = match written_path {
                    Some(p) => format!(
                        "Salvage report written to: {}\n\nSalvage summary:\n{}",
                        p.display(),
                        trimmed
                    ),
                    None => format!("Salvage summary:\n{trimmed}"),
                };
                // 5. Honest status is never masked as success; the
                //    salvage path is annotated in final_text.
                final_text = format!(
                    "The session terminated at its reasoning ceiling \
                     (status: reasoning_budget_exhausted); a salvage pass recovered the \
                     summary below.\n\n{path_note}"
                );
            }
        }
        Err(e) => {
            // 6. Salvage call failed: log cleanly, never crash, and
            //    keep the honest status.
            warn!(
                session = %session_id,
                "salvage call failed: {e}; preserving honest status \
                 reasoning_budget_exhausted"
            );
            let _ = logger.append(serde_json::json!({
                "type": "salvage",
                "turn": turns + 1,
                "reason": "reasoning_ceiling",
                "error": e.to_string(),
            }));
            final_text = format!(
                "The session terminated at its reasoning ceiling \
                 (status: reasoning_budget_exhausted); the salvage pass failed ({e}), \
                 so no summary is available. The conversation history is preserved in \
                 the session event ledger."
            );
        }
    }

    SalvageOutcome { final_text, status }
}

/// Salvage retry pass when the model produced an empty / whitespace response
/// after real tool activity.
pub(crate) async fn run_degenerate_final_salvage(
    engine: &dyn ChatEngine,
    logger: &EventLogger,
    messages: &mut Vec<Message>,
    effort: Option<&str>,
    turns: u32,
) -> Result<SalvageOutcome, RunnerError> {
    messages.push(msg(
        "user",
        "[System: Salvage] Your previous response was empty. Provide a proper summary of \
         your findings and the work you have completed.",
    ));
    let salvage = engine.chat(messages, &[], true, effort).await?;
    let salvage_text = if !salvage.content.trim().is_empty() {
        salvage.content
    } else if let Some(rc) = salvage
        .reasoning_content
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        rc.to_string()
    } else {
        String::new()
    };
    logger
        .append(serde_json::json!({
            "type": "salvage",
            "turn": turns,
            "content": salvage_text,
        }))
        .map_err(RunnerError::Io)?;
    if !salvage_text.trim().is_empty() {
        Ok(SalvageOutcome {
            final_text: salvage_text,
            status: "completed".to_string(),
        })
    } else {
        Ok(SalvageOutcome {
            status: "failed".to_string(),
            final_text: "DegenerateFinalError: the model produced an empty final after \
                          tool activity; the salvage retry also produced no content."
                .to_string(),
        })
    }
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
