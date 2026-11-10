//! Message shaping, think-tag stripping, delimiter sanitization, and metrics projection.

use crate::engine::{Message, Metrics};

pub(crate) fn msg(role: &str, content: impl Into<String>) -> Message {
    Message {
        role: role.into(),
        content: content.into(),
        tool_calls: Vec::new(),
        tool_call_id: None,
    }
}

/// Strip reasoning traces from an assistant message before persisting it in
/// multi-turn conversation history, preventing context bloat and prompt-cache
/// invalidation.
pub(crate) fn strip_think_tags(content: &str) -> String {
    strip_reasoning_tags(content, concat!("<th", "ink>"), concat!("</th", "ink>"))
}

/// Generic reasoning-tag stripper: removes `open_tag`..`close_tag` spans from
/// `content` and trims a dangling open tag.
pub(crate) fn strip_reasoning_tags(content: &str, open_tag: &str, close_tag: &str) -> String {
    let mut result = content.to_string();
    while let Some(start) = result.find(open_tag) {
        let abs = start;
        if let Some(rel_end) = result[abs..].find(close_tag) {
            result.replace_range(abs..abs + rel_end + close_tag.len(), "");
        } else {
            result.truncate(abs);
            break;
        }
    }
    result.trim().to_string()
}

/// Insert a zero-width space after `<` in any occurrence of `open_tag` or
/// `close_tag` inside `text`, so downstream tool-call parsers never see a
/// valid XML-like delimiter embedded in reasoning content.
pub(crate) fn sanitize_tool_delimiters(text: &str, open_tag: &str, close_tag: &str) -> String {
    let mut result = text.to_string();
    for tag in [open_tag, close_tag] {
        let mut from = 0;
        while from <= result.len() {
            match result[from..].find(tag) {
                Some(rel) => {
                    let pos = from + rel;
                    result.insert(pos + 1, '\u{200b}');
                    from = pos + 1 + tag.len();
                }
                None => break,
            }
        }
    }
    result
}

/// True when the model closed its reasoning block (emitted `close_tag`) and
/// still has non-empty text after the *last* `close_tag` — i.e. a "severed"
/// stop where deliberation ended but no tool call followed. Returns false
/// when `close_tag` is absent (no closed reasoning block to sever on).
pub(crate) fn is_severed_stop(content: &str, close_tag: &str) -> bool {
    match content.rfind(close_tag) {
        Some(pos) => !content[pos + close_tag.len()..].trim().is_empty(),
        None => false,
    }
}

/// The per-turn `metrics` object embedded in a `dispatch` event.
pub(crate) fn metrics_json(m: &Metrics) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    if let Some(p) = m.prompt_tokens {
        o.insert("prompt_tokens".into(), serde_json::json!(p));
    }
    if let Some(k) = m.cached_tokens {
        o.insert("cached_tokens".into(), serde_json::json!(k));
    }
    if let Some(c) = m.completion_tokens {
        o.insert("completion_tokens".into(), serde_json::json!(c));
    }
    if let Some(r) = m.reasoning_tokens {
        o.insert("reasoning_tokens".into(), serde_json::json!(r));
    }
    if let Some(r) = m.tokens_per_sec {
        o.insert("tokens_per_sec".into(), serde_json::json!(r));
    }
    if let Some(t) = m.ttft_ms {
        o.insert("ttft_ms".into(), serde_json::json!(t));
    }
    o.insert("total_ms".into(), serde_json::json!(m.total_ms));
    serde_json::Value::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_reasoning_tags_removes_open_and_close_span() {
        let open = concat!("<th", "ink>");
        let close = concat!("</th", "ink>");
        let content = format!("before {open} reasoning {close} after");
        assert_eq!(strip_reasoning_tags(&content, open, close), "before  after");
    }

    #[test]
    fn strip_reasoning_tags_truncates_dangling_open_tag() {
        let open = concat!("<th", "ink>");
        let close = concat!("</th", "ink>");
        assert_eq!(
            strip_reasoning_tags(&format!("start {open} tail"), open, close),
            "start"
        );
    }

    #[test]
    fn strip_reasoning_tags_is_noop_when_absent() {
        let open = concat!("<th", "ink>");
        let close = concat!("</th", "ink>");
        assert_eq!(
            strip_reasoning_tags("no tags here", open, close),
            "no tags here"
        );
    }

    #[test]
    fn sanitize_tool_delimiters_inserts_zwsp_after_open_angle() {
        let open = concat!("<th", "ink>");
        let close = concat!("</th", "ink>");
        let text = format!("here {open} body {close} done");
        let out = sanitize_tool_delimiters(&text, open, close);
        assert!(out.contains('\u{200b}'), "expected ZWSP: {out:?}");
        assert!(!out.contains(open), "raw open tag survived: {out:?}");
        assert!(!out.contains(close), "raw close tag survived: {out:?}");
        assert_eq!(out.chars().count(), text.chars().count() + 2);
    }

    #[test]
    fn is_severed_stop_true_when_trailing_text_after_close() {
        let close = concat!("</th", "ink>");
        assert!(is_severed_stop(
            &format!("reason {close} trailing answer"),
            close
        ));
    }

    #[test]
    fn is_severed_stop_false_when_close_at_end_or_absent() {
        let close = concat!("</th", "ink>");
        assert!(!is_severed_stop(&format!("reason {close}"), close));
        assert!(!is_severed_stop("no close tag", close));
    }
}

