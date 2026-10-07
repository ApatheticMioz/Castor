//! SSE response pipeline: stateful UTF-8 reassembly, mid-stream error
//! translation, multimodal guard, and the runaway-repetition circuit breaker.
//!
//! This is a faithful port of `mcp-castor/stream_proxy.js` +
//! `mcp-castor/src/repetition_detector.js`. The UTF-8 reassembly uses the
//! `encoding_rs` incremental decoder (the Rust equivalent of Node's
//! `TextDecoder` with `{ stream: true }`): partial multi-byte sequences are
//! held back across chunk boundaries and never emitted, and invalid bytes
//! map to U+FFFD (the `errors="replace"` contract).

use encoding_rs::{CoderResult, Decoder, UTF_8};

// ---------------------------------------------------------------------------
// Repetition detector (port of `repetition_detector.js`)
// ---------------------------------------------------------------------------

/// Characters that appear in long consecutive runs in model output (git-diff
/// '+' hunks, code, URLs, JSON, math).
const CODE_REPEAT_CHARS: &[char] = &[
    '+', '.', '/', '\\', '<', '>', '|', ':', ';', '(', ')', '[', ']', '{', '}', '\'', '"', '!',
    '?', ',', '~', '^', '&', '%', '$', '@',
];

/// Whitespace / standard markdown divider characters.
const DIVIDER_CHARS: &[char] = &['-', '=', '*', '#', ' ', '\t', '\n', '_'];

const CODE_LIMIT: usize = 1000;
const DIVIDER_LIMIT: usize = 250;
const DEFAULT_LIMIT: usize = 100;
const PATTERN_REPEAT_COUNT: usize = 40;
/// Rolling window length for the multi-character pattern detector.
const ROLLING_WINDOW: usize = 1500;

/// A detected runaway-repetition event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repetition {
    /// "character" or "pattern".
    pub r#type: String,
    /// The repeated unit.
    pub pattern: String,
    /// The run length at detection time.
    pub count: usize,
}

/// Stateful detector for runaway repetition in a streamed text stream.
///
/// Feed chunks via [`RepetitionDetector::feed`]; it tracks the trailing
/// single-character run and a rolling window of recent text for
/// multi-character pattern loops.
#[derive(Debug, Default)]
pub struct RepetitionDetector {
    last_char: char,
    char_repeat_count: usize,
    rolling: String,
}

impl RepetitionDetector {
    pub fn new() -> Self {
        Self::default()
    }

    fn limit_for(ch: char) -> usize {
        if CODE_REPEAT_CHARS.contains(&ch) {
            CODE_LIMIT
        } else if DIVIDER_CHARS.contains(&ch) {
            DIVIDER_LIMIT
        } else {
            DEFAULT_LIMIT
        }
    }

    /// Feed a chunk of streamed text. Returns a [`Repetition`] when
    /// degenerate repetition is detected, or `None` otherwise.
    pub fn feed(&mut self, text: &str) -> Option<Repetition> {
        if text.is_empty() {
            return None;
        }

        // 1. Single-character consecutive repetition.
        for ch in text.chars() {
            if ch == self.last_char {
                self.char_repeat_count += 1;
                if self.char_repeat_count >= Self::limit_for(ch) {
                    return Some(Repetition {
                        r#type: "character".to_string(),
                        pattern: ch.to_string(),
                        count: self.char_repeat_count,
                    });
                }
            } else {
                self.last_char = ch;
                self.char_repeat_count = 1;
            }
        }

        // 2. Multi-character pattern repetition.
        self.rolling.push_str(text);
        if self.rolling.len() > ROLLING_WINDOW {
            let excess = self.rolling.len() - ROLLING_WINDOW;
            let cut = self
                .rolling
                .char_indices()
                .nth(excess)
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.rolling = self.rolling[cut..].to_string();
        }
        let len = self.rolling.len();
        for unit_len in 4..=32 {
            let needed = unit_len * PATTERN_REPEAT_COUNT;
            if len < needed {
                continue;
            }
            let unit = &self.rolling[len - unit_len..];
            let is_pure = unit
                .chars()
                .all(|c| CODE_REPEAT_CHARS.contains(&c) || DIVIDER_CHARS.contains(&c));
            if is_pure {
                continue;
            }
            let mut is_rep = true;
            for r in 1..PATTERN_REPEAT_COUNT {
                let seg_start = len - (r + 1) * unit_len;
                let seg = &self.rolling[seg_start..len - r * unit_len];
                if seg != unit {
                    is_rep = false;
                    break;
                }
            }
            if is_rep {
                return Some(Repetition {
                    r#type: "pattern".to_string(),
                    pattern: unit.to_string(),
                    count: PATTERN_REPEAT_COUNT,
                });
            }
        }

        None
    }
}

// ---------------------------------------------------------------------------
// Guard marker
// ---------------------------------------------------------------------------

/// Stable prefix of the stream-proxy guard marker (for downstream matching).
pub const GUARD_MARKER_PREFIX: &str = "[StreamProxy Guard: Runaway repetition loop (";

/// Template for the full stream-proxy guard marker. The `${type}` and
/// `${pattern}` placeholders are interpolated by the breaker.
pub const GUARD_MARKER_TEMPLATE: &str = "\n\n[StreamProxy Guard: Runaway repetition loop (${type}: ${pattern}) detected and safely truncated]\n\n";

// ---------------------------------------------------------------------------
// Incremental UTF-8 reassembler
// ---------------------------------------------------------------------------

/// Incremental UTF-8 decoder that holds partial multi-byte sequences across
/// chunk boundaries and replaces invalid bytes with U+FFFD.
///
/// This is the Rust equivalent of Node's `TextDecoder("utf-8", { stream: true })`.
#[derive(Debug)]
pub struct Utf8Reassembler {
    decoder: Decoder,
}

impl Utf8Reassembler {
    pub fn new() -> Self {
        Self {
            decoder: UTF_8.new_decoder(),
        }
    }

    /// Decode a chunk of bytes, appending the result to `out`.
    /// Partial multi-byte sequences at the end are held back (not emitted).
    pub fn feed(&mut self, chunk: &[u8], out: &mut String) {
        let mut offset = 0;
        loop {
            let remaining = chunk.len() - offset;
            let needed = self
                .decoder
                .max_utf8_buffer_length(remaining)
                .unwrap_or(remaining);
            out.reserve(needed);
            let (result, read, _) = self.decoder.decode_to_string(&chunk[offset..], out, false);
            offset += read;
            if result == CoderResult::InputEmpty {
                break;
            }
            // OutputFull: loop again with the remaining bytes.
        }
    }

    /// Flush any held-back bytes at end of stream.
    pub fn finish(&mut self, out: &mut String) {
        let needed = self.decoder.max_utf8_buffer_length(0).unwrap_or(0);
        out.reserve(needed);
        let (result, _, _) = self.decoder.decode_to_string(&[], out, true);
        debug_assert!(result == CoderResult::InputEmpty);
    }
}

impl Default for Utf8Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// SSE sanitizer
// ---------------------------------------------------------------------------

/// Stateful SSE sanitizer over byte chunks.
///
/// Feed upstream byte chunks via [`SseSanitizer::feed`]; it reassembles
/// partial UTF-8 sequences, splits SSE lines, detects mid-stream error
/// frames, and trips the repetition circuit breaker. Call
/// [`SseSanitizer::finish`] at end of stream to flush held-back bytes.
#[derive(Debug)]
pub struct SseSanitizer {
    reassembler: Utf8Reassembler,
    detector: RepetitionDetector,
    line_buf: String,
    done: bool,
}

impl Default for SseSanitizer {
    fn default() -> Self {
        Self::new()
    }
}

impl SseSanitizer {
    pub fn new() -> Self {
        Self {
            reassembler: Utf8Reassembler::new(),
            detector: RepetitionDetector::new(),
            line_buf: String::new(),
            done: false,
        }
    }

    /// Feed a chunk of upstream bytes. Returns the output to write to the
    /// client. Returns an empty string if the stream is already terminated.
    pub fn feed(&mut self, chunk: &[u8]) -> String {
        if self.done {
            return String::new();
        }

        let mut text = String::new();
        self.reassembler.feed(chunk, &mut text);
        if text.is_empty() {
            return String::new();
        }

        self.line_buf.push_str(&text);

        let mut out = String::new();
        while let Some(pos) = self.line_buf.find('\n') {
            let line = self.line_buf[..pos].to_string();
            self.line_buf = self.line_buf[pos + 1..].to_string();

            if let Some(special) = self.process_line(&line) {
                out.push_str(&special);
                return out;
            }
            out.push_str(&line);
            out.push('\n');
        }

        out
    }

    /// Flush at end of stream. Returns any remaining output.
    pub fn finish(&mut self) -> String {
        if self.done {
            return String::new();
        }

        let mut text = String::new();
        self.reassembler.finish(&mut text);
        if !text.is_empty() {
            self.line_buf.push_str(&text);
        }

        let mut out = String::new();
        if !self.line_buf.is_empty() {
            let line = std::mem::take(&mut self.line_buf);
            if let Some(special) = self.process_line(&line) {
                out.push_str(&special);
            } else {
                out.push_str(&line);
                if !line.ends_with('\n') {
                    out.push('\n');
                }
            }
        }

        out
    }

    /// Whether the stream has been terminated (done, error, or breaker).
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Process a single SSE line. Returns `Some(output)` for special lines
    /// (\[DONE\], error frames, repetition breaker) that terminate the stream,
    /// or `None` for lines that should be passed through as-is.
    fn process_line(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed == "data: [DONE]" {
            self.done = true;
            return Some("data: [DONE]\n\n".to_string());
        }
        if let Some(stripped) = trimmed.strip_prefix("data: ") {
            let payload = stripped.trim();
            if payload.starts_with('{')
                && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(payload)
            {
                // Mid-stream error frame: error present, no choices.
                if parsed.get("error").is_some() && parsed.get("choices").is_none() {
                    self.done = true;
                    return Some(format!("event: error\ndata: {parsed}\n\n"));
                }

                // Repetition circuit breaker on delta content/reasoning.
                if let Some(delta) = parsed.pointer("/choices/0/delta") {
                    let token_text = delta
                        .get("content")
                        .or_else(|| delta.get("reasoning"))
                        .or_else(|| delta.get("reasoning_content"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !token_text.is_empty()
                        && let Some(rep) = self.detector.feed(token_text)
                    {
                        self.done = true;
                        let pattern_json = serde_json::to_string(&rep.pattern).unwrap_or_default();
                        let marker = GUARD_MARKER_TEMPLATE
                            .replace("${type}", &rep.r#type)
                            .replace("${pattern}", &pattern_json);
                        let breaker = serde_json::json!({
                            "id": "chatcmpl-repetition-breaker",
                            "object": "chat.completion.chunk",
                            "created": now_secs(),
                            "model": "qwen3.8-27b",
                            "choices": [{
                                "index": 0,
                                "delta": { "content": marker },
                                "finish_reason": "stop",
                            }],
                        });
                        return Some(format!("data: {breaker}\n\ndata: [DONE]\n\n"));
                    }
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Multimodal content guard (request-side)
// ---------------------------------------------------------------------------

/// Placeholder text that replaces image blocks in the multimodal guard.
pub const IMAGE_PLACEHOLDER: &str = "[Image file omitted: Local Qwen3.8-27B runs in pure text mode for Universal 245K context. \
     Images must be inspected multimodally by the Lead Architect.]";

/// Returns whether local vision support is enabled via environment variables.
pub fn is_vision_enabled() -> bool {
    std::env::var("CASTOR_ENABLE_VISION")
        .or_else(|_| std::env::var("CASTOR_VISION"))
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Sanitize a chat-completions request body by replacing image blocks with
/// text placeholders. Returns `true` if any modification was made.
///
/// This is the request-side counterpart to the response-side [`SseSanitizer`].
/// It prevents the "At most 0 image(s) may be provided" 400 from text-only
/// engines when a client sends `image_url` / `image` content blocks.
pub fn sanitize_request_body(body: &mut serde_json::Value) -> bool {
    sanitize_request_body_with_vision(body, is_vision_enabled())
}

/// Parameterized request body sanitizer allowing callers/tests to bypass image stripping.
pub fn sanitize_request_body_with_vision(
    body: &mut serde_json::Value,
    vision_enabled: bool,
) -> bool {
    if vision_enabled {
        return false;
    }
    let mut modified = false;
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            if let Some(content) = msg.get_mut("content") {
                sanitize_item(content, &mut modified);
            }
        }
    }
    if let Some(prompt) = body.get_mut("prompt") {
        sanitize_item(prompt, &mut modified);
    }
    modified
}

/// Recursively replace image blocks with text placeholders.
fn sanitize_item(item: &mut serde_json::Value, modified: &mut bool) {
    match item {
        serde_json::Value::Array(arr) => {
            for elem in arr.iter_mut() {
                sanitize_item(elem, modified);
            }
        }
        serde_json::Value::Object(obj) => {
            let is_image =
                obj.get("type").and_then(|v| v.as_str()).is_some_and(|t| {
                    matches!(t, "image_url" | "image" | "input_image" | "image_file")
                }) || obj.contains_key("image")
                    || obj.contains_key("image_url");
            if is_image {
                *modified = true;
                *item = serde_json::json!({
                    "type": "text",
                    "text": IMAGE_PLACEHOLDER,
                });
                return;
            }
            for (_, val) in obj.iter_mut() {
                sanitize_item(val, modified);
            }
        }
        _ => {}
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_reassembler_ascii() {
        let mut r = Utf8Reassembler::new();
        let mut out = String::new();
        r.feed(b"hello", &mut out);
        assert_eq!(out, "hello");
        r.finish(&mut out);
        assert_eq!(out, "hello");
    }

    #[test]
    fn utf8_reassembler_split_multibyte() {
        // "é" is 0xC3 0xA9 in UTF-8. Split across two chunks.
        let mut r = Utf8Reassembler::new();
        let mut out = String::new();
        r.feed(b"\xC3", &mut out);
        assert_eq!(out, "", "partial multibyte must be held back");
        r.feed(b"\xA9", &mut out);
        assert_eq!(out, "\u{00E9}");
        r.finish(&mut out);
        assert_eq!(out, "\u{00E9}");
    }

    #[test]
    fn utf8_reassembler_emoji_split() {
        // "😀" is 0xF0 0x9F 0x98 0x80 in UTF-8. Split 2+2.
        let mut r = Utf8Reassembler::new();
        let mut out = String::new();
        r.feed(b"\xF0\x9F", &mut out);
        assert_eq!(out, "", "partial 4-byte seq must be held back");
        r.feed(b"\x98\x80", &mut out);
        assert_eq!(out, "\u{1F600}");
        r.finish(&mut out);
        assert_eq!(out, "\u{1F600}");
    }

    #[test]
    fn utf8_reassembler_invalid_byte() {
        // A lone 0xFF at the end of a chunk is held back (it could be the
        // start of a multi-byte sequence) and never emitted split; it is
        // resolved to U+FFFD at EOF.
        let mut r = Utf8Reassembler::new();
        let mut out = String::new();
        r.feed(b"\xFF", &mut out);
        assert_eq!(
            out, "",
            "trailing byte must be held back, not emitted split"
        );
        r.finish(&mut out);
        assert_eq!(out, "\u{FFFD}");
    }

    #[test]
    fn utf8_reassembler_invalid_in_stream() {
        // "a\xFFb" → "a\u{FFFD}b"
        let mut r = Utf8Reassembler::new();
        let mut out = String::new();
        r.feed(b"a\xFFb", &mut out);
        assert_eq!(out, "a\u{FFFD}b");
        r.finish(&mut out);
        assert_eq!(out, "a\u{FFFD}b");
    }

    #[test]
    fn sse_sanitizer_passthrough() {
        let mut s = SseSanitizer::new();
        let out = s.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        assert_eq!(
            out,
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n"
        );
        assert!(!s.is_done());
    }

    #[test]
    fn sse_sanitizer_done() {
        let mut s = SseSanitizer::new();
        let out = s.feed(b"data: [DONE]\n\n");
        assert_eq!(out, "data: [DONE]\n\n");
        assert!(s.is_done());
        // Subsequent feeds return empty.
        assert_eq!(s.feed(b"data: {\"x\":1}\n\n"), "");
    }

    #[test]
    fn sse_sanitizer_error_frame_no_done() {
        let mut s = SseSanitizer::new();
        let err = r#"{"error":{"message":"boom","type":"server_error"}}"#;
        let chunk = format!("data: {err}\n\n");
        let out = s.feed(chunk.as_bytes());
        assert!(out.starts_with("event: error\n"), "got: {out}");
        assert!(out.contains("boom"), "got: {out}");
        assert!(
            !out.contains("[DONE]"),
            "error frame must NOT append [DONE]: {out}"
        );
        assert!(s.is_done());
    }

    #[test]
    fn sse_sanitizer_repetition_breaker() {
        let mut s = SseSanitizer::new();
        // Feed 100 'a' characters in one delta → triggers DEFAULT_LIMIT (100).
        let content = "a".repeat(100);
        let chunk = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
            content
        );
        let out = s.feed(chunk.as_bytes());
        assert!(s.is_done(), "repetition breaker must trip");
        assert!(out.contains(GUARD_MARKER_PREFIX), "got: {out}");
        assert!(out.contains("finish_reason"), "got: {out}");
        assert!(out.contains("[DONE]"), "breaker must append [DONE]: {out}");
    }

    #[test]
    fn sse_sanitizer_emoji_split_across_chunks() {
        let mut s = SseSanitizer::new();
        // Build a JSON line with an emoji in the content, then split the
        // raw bytes in the middle of the 4-byte emoji sequence.
        let full = "data: {\"choices\":[{\"delta\":{\"content\":\"😀\"}}]}\n\n";
        let bytes = full.as_bytes().to_vec();
        let emoji_bytes: &[u8] = &[0xF0, 0x9F, 0x98, 0x80];
        let pos = bytes
            .windows(4)
            .position(|w| w == emoji_bytes)
            .expect("emoji bytes not found in JSON");
        let split_at = pos + 2;
        let chunk1 = &bytes[..split_at];
        let chunk2 = &bytes[split_at..];

        let out1 = s.feed(chunk1);
        let out2 = s.feed(chunk2);
        let out3 = s.finish();
        let combined = format!("{out1}{out2}{out3}");
        assert!(
            combined.contains("\u{1F600}"),
            "emoji must arrive intact, got: {combined}"
        );
        assert!(
            !combined.contains('\u{FFFD}'),
            "no replacement chars: {combined}"
        );
    }

    #[test]
    fn sse_sanitizer_invalid_byte_in_stream() {
        let mut s = SseSanitizer::new();
        // Inject an invalid byte (0xFF) into the content.
        let chunk = b"data: {\"choices\":[{\"delta\":{\"content\":\"a\xFFb\"}}]}\n\n";
        let out = s.feed(chunk);
        assert!(
            out.contains("\u{FFFD}"),
            "invalid byte must become U+FFFD, got: {out}"
        );
    }

    #[test]
    fn multimodal_guard_replaces_image_blocks() {
        let mut body = serde_json::json!({
            "model": "qwen3.8-27b",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "What is this?"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}
                ]
            }]
        });
        let modified = sanitize_request_body(&mut body);
        assert!(modified);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "text");
        assert!(
            content[1]["text"]
                .as_str()
                .unwrap()
                .starts_with("[Image file omitted")
        );
    }

    #[test]
    fn multimodal_guard_passes_text_only() {
        let mut body = serde_json::json!({
            "model": "qwen3.8-27b",
            "messages": [{
                "role": "user",
                "content": "Hello, how are you?"
            }]
        });
        let modified = sanitize_request_body(&mut body);
        assert!(!modified);
        assert_eq!(body["messages"][0]["content"], "Hello, how are you?");
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn multimodal_guard_allows_images_when_vision_enabled() {
        let mut body = serde_json::json!({
            "model": "qwen3.8-27b",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "What is this?"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}
                ]
            }]
        });
        let modified = sanitize_request_body_with_vision(&mut body, true);
        assert!(!modified);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "image_url");
    }

    #[test]
    fn multimodal_guard_blocks_images_when_vision_disabled() {
        let mut body = serde_json::json!({
            "model": "qwen3.8-27b",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}
                ]
            }]
        });
        let modified = sanitize_request_body_with_vision(&mut body, false);
        assert!(modified);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert!(
            content[0]["text"]
                .as_str()
                .unwrap()
                .contains("Image file omitted")
        );
    }

    #[test]
    fn vision_env_flags_toggle_detection() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("CASTOR_ENABLE_VISION", "1");
            assert!(is_vision_enabled());
            std::env::set_var("CASTOR_ENABLE_VISION", "0");
            assert!(!is_vision_enabled());
            std::env::remove_var("CASTOR_ENABLE_VISION");

            std::env::set_var("CASTOR_VISION", "TRUE");
            assert!(is_vision_enabled());
            std::env::set_var("CASTOR_VISION", "false");
            assert!(!is_vision_enabled());
            std::env::remove_var("CASTOR_VISION");
        }
    }

    #[test]
    fn repetition_detector_character_run() {
        let mut d = RepetitionDetector::new();
        // 100 'a' chars → DEFAULT_LIMIT.
        let result = d.feed(&"a".repeat(100));
        assert!(result.is_some());
        let rep = result.unwrap();
        assert_eq!(rep.r#type, "character");
        assert_eq!(rep.pattern, "a");
        assert_eq!(rep.count, 100);
    }

    #[test]
    fn repetition_detector_code_char_run() {
        let mut d = RepetitionDetector::new();
        // 1000 '+' chars → CODE_LIMIT.
        let result = d.feed(&"+".repeat(1000));
        assert!(result.is_some());
        let rep = result.unwrap();
        assert_eq!(rep.r#type, "character");
        assert_eq!(rep.pattern, "+");
        assert_eq!(rep.count, 1000);
    }

    #[test]
    fn repetition_detector_pattern_loop() {
        let mut d = RepetitionDetector::new();
        // "the " repeated 40 times = 160 chars.
        let text = "the ".repeat(40);
        let result = d.feed(&text);
        assert!(result.is_some(), "pattern loop must be detected");
        let rep = result.unwrap();
        assert_eq!(rep.r#type, "pattern");
        assert_eq!(rep.count, PATTERN_REPEAT_COUNT);
    }
}
