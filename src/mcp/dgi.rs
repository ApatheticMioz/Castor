//! DGI Gatekeeper — Decomposition Granularity Index (Issue #16).
//!
//! Replaces brittle regexes, markdown table scanners, and length heuristics with:
//! 1. A 1-forward pass vLLM logit probe via guided_choice: ["ADMISSIBLE", "OVERLOADED"].
//! 2. A non-blocking soft heuristic fallback when offline.
//!
//! Domain agnosticism: no repo-specific patterns, no hardcoded file extensions,
//! and no hard regex or length limits.

use serde_json::json;

/// Verdict of the DGI Gatekeeper for a dispatch prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DgiVerdict {
    /// Bounded single-concern spec: proceed without commentary.
    Admit,
    /// Advisory DGI note: proceed, but flag for decomposition.
    Review(u32),
    /// A hard monolith classification fired (via the 1-forward pass probe): fail fast.
    Reject(Vec<String>),
}

impl DgiVerdict {
    pub fn is_reject(&self) -> bool {
        matches!(self, Self::Reject(_))
    }

    pub fn score(&self) -> Option<u32> {
        match self {
            Self::Review(s) => Some(*s),
            _ => None,
        }
    }
}

/// Query the serving engine for a 2-token single forward pass logit probe.
///
/// This evaluates semantic cognitive complexity in a single forward pass (~28ms on local GPU)
/// with 2 tokens output (ADMIT vs OVERLOADED), bypassing multi-token autoregressive reasoning loops.
pub async fn evaluate_model_probe(
    base_url: &str,
    model: &str,
    prompt: &str,
    http: &reqwest::Client,
) -> Result<DgiVerdict, String> {
    let url = format!("{}/completions", base_url.trim_end_matches('/'));
    let formatted_prompt = format!(
        "<|im_start|>system\nYou are a decomposition gatekeeper. Classify if the prompt is a cohesive, bounded single-concern task (ADMIT) or an overloaded monolithic multi-subsystem overhaul / mega-prompt (OVERLOADED). Answer with only ADMIT or OVERLOADED.<|im_end|>\n<|im_start|>user\n{prompt}<|im_end|>\n<|im_start|>assistant\n<think>\n</think>\n"
    );
    let req_body = json!({
        "model": model,
        "prompt": formatted_prompt,
        "max_tokens": 2,
        "temperature": 0.0,
        "guided_choice": ["ADMIT", "OVERLOADED"],
    });

    let resp = http
        .post(&url)
        .json(&req_body)
        .send()
        .await
        .map_err(|e| format!("connection to {url} failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("upstream HTTP {status}: {body}"));
    }
    let val: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("malformed JSON response from {url}: {e}"))?;
    let text = val["choices"][0]["text"].as_str().unwrap_or("").trim();

    if text.starts_with("OVER") || text.starts_with("over") || text.starts_with("Over") {
        Ok(DgiVerdict::Reject(vec![
            "Model-classified monolithic multi-subsystem dispatch (OVERLOADED)".to_string(),
        ]))
    } else {
        Ok(DgiVerdict::Admit)
    }
}

/// Offline or soft heuristic evaluation: advisory only, NEVER a hard reject.
pub fn evaluate(prompt: &str) -> DgiVerdict {
    // Soft heuristic: advisory review if length exceeds 2,500 chars, but never reject.
    if prompt.len() > 2500 {
        return DgiVerdict::Review(2);
    }
    DgiVerdict::Admit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_prompt_admits() {
        let v = evaluate("Fix off-by-one in src/pruner.rs line 42.");
        assert_eq!(v, DgiVerdict::Admit);
    }

    #[test]
    fn long_prompt_soft_review_never_rejects() {
        let p = "x".repeat(3000);
        let v = evaluate(&p);
        assert!(!v.is_reject(), "soft heuristic must never reject");
        assert_eq!(v, DgiVerdict::Review(2));
    }

    #[tokio::test]
    async fn model_probe_rejects_on_overloaded() {
        use axum::Json;
        use axum::Router;
        use axum::routing::post;

        async fn handler(Json(_body): Json<serde_json::Value>) -> Json<serde_json::Value> {
            Json(json!({
                "choices": [{
                    "text": "OVERLOADED",
                    "finish_reason": "stop"
                }]
            }))
        }

        let app = Router::new().route("/completions", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let http = reqwest::Client::new();
        let base_url = format!("http://{addr}");
        let verdict = evaluate_model_probe(&base_url, "test-model", "mega task", &http)
            .await
            .unwrap();

        assert!(verdict.is_reject());
    }

    #[tokio::test]
    async fn model_probe_admits_on_admissible() {
        use axum::Json;
        use axum::Router;
        use axum::routing::post;

        async fn handler(Json(_body): Json<serde_json::Value>) -> Json<serde_json::Value> {
            Json(json!({
                "choices": [{
                    "text": "ADMIT",
                    "finish_reason": "stop"
                }]
            }))
        }

        let app = Router::new().route("/completions", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let http = reqwest::Client::new();
        let base_url = format!("http://{addr}");
        let verdict = evaluate_model_probe(&base_url, "test-model", "single slice", &http)
            .await
            .unwrap();

        assert_eq!(verdict, DgiVerdict::Admit);
    }
}
