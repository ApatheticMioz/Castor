//! Telemetry: a single event stream plus derived statistics.
//!
#![allow(dead_code)]
//!
//! Locked decision: there is exactly ONE event stream. The tracing-JSONL
//! ledger (`<state>/telemetry/events.jsonl`) is the ledger; there is NO
//! second stats writer. Statistics are *derived* on demand by walking the
//! existing ledgers (`sessions/*/events.jsonl` + `tasks/*.json`) — the same
//! fields the JS `mcp-castor/src/telemetry.js` reports, but computed from
//! the ledgers rather than maintained by a parallel writer.
//!
//! Derivation is best-effort: missing or corrupt ledger lines are skipped
//! silently (the ledgers are the authority; this module is a read-only
//! projection over them).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing_subscriber::prelude::*;

/// Baseline frontier-model pricing (USD per million tokens), ported from
/// `mcp-castor/src/telemetry.js` (Claude Sonnet 5 tier).
const PROMPT_COST_PER_MILLION: f64 = 2.0;
const COMPLETION_COST_PER_MILLION: f64 = 10.0;

/// The benchmark model the cost-saved figure is quoted against.
const BENCHMARK_MODEL: &str = "Claude Sonnet 5";

/// Derived statistics over the session + task ledgers.
///
/// Field names mirror the JS `DEFAULT_STATS` object so the two stay
/// comparable. Averages are `None` when no ledger reported a finite value
/// for that field (never zero-filled).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub total_completion_tokens: u64,
    pub total_reasoning_tokens: u64,
    pub total_prompt_tokens: u64,
    pub total_turns: u64,
    pub total_sessions: u64,
    pub total_tasks_completed: u64,
    pub total_tasks_failed: u64,
    pub total_tasks_cancelled: u64,
    /// Mean session duration in ms (over sessions that reported one).
    pub avg_duration_ms: Option<f64>,
    pub total_tool_calls: u64,
    pub total_tool_errors: u64,
    /// Per-tool call counts (name → count).
    pub tool_calls: BTreeMap<String, u64>,
    /// Per-tool error counts (name → count).
    pub tool_errors: BTreeMap<String, u64>,
    pub benchmark_model: String,
    /// API cost that the derived token volume would have cost at the
    /// benchmark model's frontier rates (the "cost saved" figure).
    pub estimated_cost_saved_usd: f64,
    /// ISO-8601 UTC of the earliest recorded session event, if any.
    pub first_recorded_session: Option<String>,
    /// ISO-8601 UTC of the latest recorded session event, if any.
    pub last_recorded_session: Option<String>,
    /// ISO-8601 UTC at which these stats were derived.
    pub last_updated: String,
    /// Accumulator for the duration average (not serialized).
    #[serde(skip)]
    duration_sum: u64,
    /// Count of sessions that reported a duration (not serialized).
    #[serde(skip)]
    duration_count: u64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            total_completion_tokens: 0,
            total_reasoning_tokens: 0,
            total_prompt_tokens: 0,
            total_turns: 0,
            total_sessions: 0,
            total_tasks_completed: 0,
            total_tasks_failed: 0,
            total_tasks_cancelled: 0,
            avg_duration_ms: None,
            total_tool_calls: 0,
            total_tool_errors: 0,
            tool_calls: BTreeMap::new(),
            tool_errors: BTreeMap::new(),
            benchmark_model: BENCHMARK_MODEL.to_string(),
            estimated_cost_saved_usd: 0.0,
            first_recorded_session: None,
            last_recorded_session: None,
            last_updated: String::new(),
            duration_sum: 0,
            duration_count: 0,
        }
    }
}

/// Initialize the single tracing-JSONL event stream.
///
/// Writes one JSON object per line to `<state_dir>/telemetry/events.jsonl`
/// (append mode, created on demand) and mirrors `warn`-and-above events to
/// stderr (the MCP-mode operator surface). The level filter honors the
/// `RUST_LOG` environment variable (defaulting to `info`).
///
/// This installs the *global* default subscriber, so it must be called at
/// most once per process.
pub fn init_tracing(state_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let telemetry_dir = state_dir.join("telemetry");
    fs::create_dir_all(&telemetry_dir)?;
    let events_path = telemetry_dir.join("events.jsonl");

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(FileSink { path: events_path })
        .with_filter(env_filter);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_filter(tracing_subscriber::EnvFilter::new("warn"));

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .try_init()?;

    Ok(())
}

/// A `MakeWriter` that appends to a fixed file path (one open per write).
///
/// Telemetry must never break the inference path: if the file (or its
/// parent directory) cannot be opened, the writer degrades to a no-op
/// sink instead of panicking.
struct FileSink {
    path: PathBuf,
}

/// A `Write` target that silently discards every byte.
struct NoopWriter;

impl std::io::Write for NoopWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Ok(_buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for FileSink {
    type Writer = Box<dyn std::io::Write + Send + Sync>;

    fn make_writer(&'writer self) -> Self::Writer {
        use std::fs::OpenOptions;
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match OpenOptions::new().create(true).append(true).open(&self.path) {
            Ok(f) => Box::new(f),
            Err(_) => Box::new(NoopWriter),
        }
    }
}

/// Derive the cumulative [`Stats`] by walking the existing ledgers:
/// `sessions/*/events.jsonl` (turns, tokens, tool calls, durations,
/// first/last activity) and `tasks/*.json` (task lifecycle counts).
///
/// Missing or corrupt lines are skipped silently — this is a derivation,
/// not an authority.
pub fn derive_stats(state_dir: &Path) -> Stats {
    let mut stats = Stats::default();

    // --- Sessions: one events.jsonl per session directory. ---
    let sessions_dir = state_dir.join("sessions");
    for path in read_dir_or_empty(&sessions_dir) {
        if !path.is_dir() {
            continue;
        }
        let events_path = path.join("events.jsonl");
        let Ok(raw) = fs::read_to_string(&events_path) else {
            continue; // no ledger for this session yet
        };
        let Some(session) = parse_session_events(&raw) else {
            continue; // empty / all-corrupt ledger
        };

        stats.total_sessions += 1;
        stats.total_turns += session.turns;
        stats.total_prompt_tokens += session.prompt_tokens;
        stats.total_completion_tokens += session.completion_tokens;
        stats.total_reasoning_tokens += session.reasoning_tokens;
        stats.total_tool_calls += session.tool_calls;
        stats.total_tool_errors += session.tool_errors;
        for (name, n) in session.tool_calls_by_name {
            *stats.tool_calls.entry(name).or_insert(0) += n;
        }
        for (name, n) in session.tool_errors_by_name {
            *stats.tool_errors.entry(name).or_insert(0) += n;
        }
        if let Some(d) = session.duration_ms {
            stats.duration_sum += d;
            stats.duration_count += 1;
        }
        if let Some(ts) = &session.first_ts {
            match &stats.first_recorded_session {
                None => stats.first_recorded_session = Some(ts.clone()),
                Some(cur) if ts < cur => stats.first_recorded_session = Some(ts.clone()),
                _ => {}
            }
        }
        if let Some(ts) = &session.last_ts {
            match &stats.last_recorded_session {
                None => stats.last_recorded_session = Some(ts.clone()),
                Some(cur) if ts > cur => stats.last_recorded_session = Some(ts.clone()),
                _ => {}
            }
        }
    }

    // --- Tasks: one JSON record per task. ---
    let tasks_dir = state_dir.join("tasks");
    for path in read_dir_or_empty(&tasks_dir) {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&raw) else {
            continue; // corrupt record: skip
        };
        match v.get("status").and_then(|s| s.as_str()) {
            Some("completed") => stats.total_tasks_completed += 1,
            Some("failed") => stats.total_tasks_failed += 1,
            Some("cancelled") => stats.total_tasks_cancelled += 1,
            _ => {}
        }
    }

    // --- Derived averages / cost. ---
    stats.avg_duration_ms = (stats.duration_count > 0).then(|| {
        (stats.duration_sum as f64 / stats.duration_count as f64)
            * 100.0
            / 100.0
    });
    stats.estimated_cost_saved_usd =
        (stats.total_prompt_tokens as f64 / 1_000_000.0) * PROMPT_COST_PER_MILLION
            + (stats.total_completion_tokens as f64 / 1_000_000.0) * COMPLETION_COST_PER_MILLION;
    stats.estimated_cost_saved_usd = (stats.estimated_cost_saved_usd * 100.0).round() / 100.0;
    stats.last_updated = now_iso();

    stats
}

/// Write the derived stats to `<state_dir>/telemetry/stats.json` (atomic
/// write-temp-then-rename). Returns the path written.
pub fn write_stats_json(state_dir: &Path, stats: &Stats) -> std::io::Result<PathBuf> {
    let telemetry_dir = state_dir.join("telemetry");
    fs::create_dir_all(&telemetry_dir)?;
    let out = telemetry_dir.join("stats.json");
    let tmp = telemetry_dir.join(format!("stats.json.tmp.{}", std::process::id()));
    let body = serde_json::to_string_pretty(stats)
        .map_err(std::io::Error::other)?;
    fs::write(&tmp, body)?;
    fs::rename(&tmp, &out)?;
    Ok(out)
}

/// Format a high-level visual card of operational telemetry and financial savings.
pub fn format_stats_card(stats: &Stats) -> String {
    let mut out = String::new();
    out.push_str("┌────────────────────────────────────────────────────────────────────────┐\n");
    out.push_str("│                      CASTOR OPERATIONAL TELEMETRY                      │\n");
    out.push_str("│                Universal Cloud-to-Local Agent Microkernel              │\n");
    out.push_str("└────────────────────────────────────────────────────────────────────────┘\n\n");

    out.push_str("📊 ACTIVITY & RUNTIME\n");
    out.push_str(&format!("  • Turns:                   {}\n", stats.total_turns));
    out.push_str(&format!("  • Sessions:                {}\n", stats.total_sessions));
    out.push_str(&format!("  • Tasks Completed:         {}\n", stats.total_tasks_completed));
    out.push_str(&format!("  • Tasks Failed:            {}\n", stats.total_tasks_failed));
    out.push_str(&format!("  • Tasks Cancelled:         {}\n", stats.total_tasks_cancelled));
    out.push_str(&format!(
        "  • Total Tool Calls:        {} ({} errors)\n",
        stats.total_tool_calls, stats.total_tool_errors
    ));
    if let Some(avg_ms) = stats.avg_duration_ms {
        out.push_str(&format!("  • Avg Session Duration:    {:.2}s\n", avg_ms / 1000.0));
    }
    out.push('\n');

    out.push_str("🧠 TOKEN EFFICIENCY\n");
    out.push_str(&format!("  • Ingested Prompt Tokens:  {}\n", stats.total_prompt_tokens));
    out.push_str(&format!("  • Generated Output Tokens: {}\n", stats.total_completion_tokens));
    out.push_str(&format!("  • Reasoning Tokens:        {}\n", stats.total_reasoning_tokens));
    out.push('\n');

    out.push_str(&format!("💰 CLOUD ARBITRAGE ({} Rates)\n", stats.benchmark_model));
    out.push_str(&format!("  • Virtual Cloud Cost:      ${:.2}\n", stats.estimated_cost_saved_usd));
    out.push_str("  • Actual Local Cost:       $0.00\n");
    out.push_str(&format!("  • NET SAVINGS:             +${:.2}\n", stats.estimated_cost_saved_usd));
    out.push('\n');

    if !stats.tool_calls.is_empty() {
        out.push_str("🔧 TOOL USAGE BREAKDOWN\n");
        for (name, count) in &stats.tool_calls {
            let errs = stats.tool_errors.get(name).copied().unwrap_or(0);
            out.push_str(&format!("  • {:<20} {:>5} calls ({} errors)\n", name, count, errs));
        }
        out.push('\n');
    }

    if let (Some(first), Some(last)) = (&stats.first_recorded_session, &stats.last_recorded_session) {
        out.push_str(&format!("🕒 Active Horizon: {} → {}\n", first, last));
    }
    out
}

/// Per-session aggregates extracted from one `events.jsonl` ledger.
struct SessionAgg {
    turns: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    reasoning_tokens: u64,
    tool_calls: u64,
    tool_errors: u64,
    tool_calls_by_name: BTreeMap<String, u64>,
    tool_errors_by_name: BTreeMap<String, u64>,
    duration_ms: Option<u64>,
    first_ts: Option<String>,
    last_ts: Option<String>,
}


/// List a directory's entries, returning an empty vec when the directory is
/// missing or unreadable (a missing ledger dir is a valid state).
fn read_dir_or_empty(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten().map(|e| e.path()).collect()
}

/// Parse one session's `events.jsonl` into per-session aggregates.
///
/// Returns `None` when the ledger is empty or every line is corrupt.
/// Malformed lines are skipped silently (derivation, not authority).
fn parse_session_events(raw: &str) -> Option<SessionAgg> {
    let mut agg = SessionAgg {
        turns: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
        reasoning_tokens: 0,
        tool_calls: 0,
        tool_errors: 0,
        tool_calls_by_name: BTreeMap::new(),
        tool_errors_by_name: BTreeMap::new(),
        duration_ms: None,
        first_ts: None,
        last_ts: None,
    };
    let mut any = false;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue; // corrupt line: skip
        };
        any = true;

        if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
            if agg.first_ts.is_none() {
                agg.first_ts = Some(ts.to_string());
            }
            agg.last_ts = Some(ts.to_string());
        }

        match v.get("type").and_then(|t| t.as_str()) {
            Some("dispatch") => {
                agg.turns += 1;
                let metrics = v.get("metrics");
                if let Some(p) = metrics.and_then(|m| m.get("promptTokens")).and_then(|x| x.as_u64())
                {
                    agg.prompt_tokens += p;
                }
                if let Some(c) =
                    metrics.and_then(|m| m.get("completionTokens")).and_then(|x| x.as_u64())
                {
                    agg.completion_tokens += c;
                }
                if let Some(r) =
                    metrics.and_then(|m| m.get("reasoningTokens")).and_then(|x| x.as_u64())
                {
                    agg.reasoning_tokens += r;
                }
            }
            Some("tool_result") => {
                agg.tool_calls += 1;
                let name = v
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                if v.get("is_error").and_then(|e| e.as_bool()) == Some(true) {
                    agg.tool_errors += 1;
                    *agg.tool_errors_by_name.entry(name.clone()).or_insert(0) += 1;
                }
                *agg.tool_calls_by_name.entry(name).or_insert(0) += 1;
            }
            // `final` carries a session-total `totalCompletionTokens`, which is
            // already the sum of the per-turn `dispatch` metrics — so we only
            // take the duration from it (accumulating the total would
            // double-count).
            Some("final") => {
                if let Some(d) = v.get("durationMs").and_then(|x| x.as_u64()) {
                    agg.duration_ms = Some(d);
                }
            }
            _ => {}
        }
    }

    any.then_some(agg)
}

/// Format epoch milliseconds as an ISO-8601 UTC string (`...Z`).
fn iso_from_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, h, m, s, millis
    )
}

/// Current time as an ISO-8601 UTC string.
fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    iso_from_ms(ms)
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 → (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_state() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "castor_tel_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(p: &Path, body: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn derive_stats_exact_counts() {
        let state = tmp_state();

        // Session A: 2 turns, 1 tool call (ok), 1 tool error, a final with
        // duration. (Token counts are realistic magnitudes so the derived
        // cost figure is non-trivial after 2-decimal rounding.)
        write(
            &state.join("sessions/sa/events.jsonl"),
            concat!(
                "{\"type\":\"dispatch\",\"turn\":1,\"timestamp\":\"2026-09-05T03:58:07.228Z\",\"metrics\":{\"promptTokens\":60000,\"completionTokens\":20000,\"reasoningTokens\":5000}}\n",
                "{\"type\":\"tool_result\",\"turn\":1,\"name\":\"bash\",\"is_error\":false,\"timestamp\":\"2026-09-05T03:58:07.500Z\"}\n",
                "{\"type\":\"dispatch\",\"turn\":2,\"timestamp\":\"2026-09-05T03:58:08.000Z\",\"metrics\":{\"promptTokens\":40000,\"completionTokens\":30000,\"reasoningTokens\":5000}}\n",
                "{\"type\":\"tool_result\",\"turn\":2,\"name\":\"read_file\",\"is_error\":true,\"timestamp\":\"2026-09-05T03:58:08.200Z\"}\n",
                "{\"type\":\"final\",\"status\":\"completed\",\"durationMs\":1000,\"totalCompletionTokens\":50000,\"timestamp\":\"2026-09-05T03:58:09.000Z\"}\n"
            ),
        );

        // Session B: 1 turn, 1 tool call, no final (no duration).
        write(
            &state.join("sessions/sb/events.jsonl"),
            concat!(
                "{\"type\":\"dispatch\",\"turn\":1,\"timestamp\":\"2026-09-05T04:00:00.000Z\",\"metrics\":{\"promptTokens\":10000,\"completionTokens\":5000}}\n",
                "{\"type\":\"tool_result\",\"turn\":1,\"name\":\"bash\",\"is_error\":false,\"timestamp\":\"2026-09-05T04:00:01.000Z\"}\n"
            ),
        );

        // Tasks: one of each terminal status.
        write(
            &state.join("tasks/task_1.json"),
            "{\"id\":\"task_1\",\"status\":\"completed\"}",
        );
        write(
            &state.join("tasks/task_2.json"),
            "{\"id\":\"task_2\",\"status\":\"failed\"}",
        );
        write(
            &state.join("tasks/task_3.json"),
            "{\"id\":\"task_3\",\"status\":\"cancelled\"}",
        );

        let s = derive_stats(&state);
        assert_eq!(s.total_sessions, 2);
        assert_eq!(s.total_turns, 3);
        assert_eq!(s.total_prompt_tokens, 110_000);
        assert_eq!(s.total_completion_tokens, 55_000);
        assert_eq!(s.total_reasoning_tokens, 10_000);
        assert_eq!(s.total_tool_calls, 3);
        assert_eq!(s.total_tool_errors, 1);
        assert_eq!(s.tool_calls.get("bash"), Some(&2));
        assert_eq!(s.tool_calls.get("read_file"), Some(&1));
        assert_eq!(s.tool_errors.get("read_file"), Some(&1));
        assert_eq!(s.total_tasks_completed, 1);
        assert_eq!(s.total_tasks_failed, 1);
        assert_eq!(s.total_tasks_cancelled, 1);
        assert_eq!(s.avg_duration_ms, Some(1000.0));
        assert_eq!(
            s.first_recorded_session.as_deref(),
            Some("2026-09-05T03:58:07.228Z")
        );
        assert_eq!(
            s.last_recorded_session.as_deref(),
            Some("2026-09-05T04:00:01.000Z")
        );
        // 110k prompt @ $2/M + 55k completion @ $10/M = 0.22 + 0.55 = 0.77
        assert!((s.estimated_cost_saved_usd - 0.77).abs() < 1e-9);
        assert_eq!(s.benchmark_model, BENCHMARK_MODEL);

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn empty_state_is_zeroed() {
        let state = tmp_state();
        let s = derive_stats(&state);
        assert_eq!(s.total_sessions, 0);
        assert_eq!(s.total_turns, 0);
        assert_eq!(s.total_prompt_tokens, 0);
        assert_eq!(s.total_completion_tokens, 0);
        assert_eq!(s.total_reasoning_tokens, 0);
        assert_eq!(s.total_tool_calls, 0);
        assert_eq!(s.total_tool_errors, 0);
        assert_eq!(s.total_tasks_completed, 0);
        assert_eq!(s.total_tasks_failed, 0);
        assert_eq!(s.total_tasks_cancelled, 0);
        assert!(s.avg_duration_ms.is_none());
        assert!(s.first_recorded_session.is_none());
        assert!(s.last_recorded_session.is_none());
        assert_eq!(s.estimated_cost_saved_usd, 0.0);
        assert_eq!(s.benchmark_model, BENCHMARK_MODEL);
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn corrupt_lines_are_skipped() {
        let state = tmp_state();
        write(
            &state.join("sessions/sc/events.jsonl"),
            concat!(
                "this is not json\n",
                "{\"type\":\"dispatch\",\"turn\":1,\"timestamp\":\"2026-09-05T05:00:00.000Z\",\"metrics\":{\"promptTokens\":7}}\n",
                "{\"broken\":\n",
                "{\"type\":\"tool_result\",\"turn\":1,\"name\":\"bash\",\"is_error\":false,\"timestamp\":\"2026-09-05T05:00:01.000Z\"}\n"
            ),
        );
        let s = derive_stats(&state);
        // The two corrupt lines are dropped; the two valid ones count.
        assert_eq!(s.total_sessions, 1);
        assert_eq!(s.total_turns, 1);
        assert_eq!(s.total_prompt_tokens, 7);
        assert_eq!(s.total_tool_calls, 1);
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn write_stats_json_round_trips() {
        let state = tmp_state();
        let s = derive_stats(&state);
        let path = write_stats_json(&state, &s).unwrap();
        assert!(path.exists());
        let back: Stats =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.total_sessions, 0);
        assert_eq!(back.benchmark_model, BENCHMARK_MODEL);
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn init_tracing_writes_json_line_to_file_sink() {
        let state = tmp_state();
        init_tracing(&state).expect("init_tracing");
        tracing::info!(target: "castor.telemetry", hello = "world", "telemetry line");

        let events = state.join("telemetry/events.jsonl");
        let raw = fs::read_to_string(&events).expect("events.jsonl exists");
        let line = raw.lines().find(|l| !l.trim().is_empty()).expect("a line");
        let v: Value = serde_json::from_str(line).expect("valid JSON line");
        assert_eq!(v.get("level").and_then(|x| x.as_str()), Some("INFO"));
        assert_eq!(
            v.get("fields").and_then(|f| f.get("hello")).and_then(|x| x.as_str()),
            Some("world")
        );
        assert!(v.get("timestamp").is_some());
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn format_stats_card_renders_dashboard() {
        let state = tmp_state();
        let s = derive_stats(&state);
        let card = format_stats_card(&s);
        assert!(card.contains("CASTOR OPERATIONAL TELEMETRY"));
        assert!(card.contains("ACTIVITY & RUNTIME"));
        assert!(card.contains("TOKEN EFFICIENCY"));
        assert!(card.contains("CLOUD ARBITRAGE"));
        let _ = fs::remove_dir_all(&state);
    }
}
