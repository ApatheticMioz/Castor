//! Telemetry: a single event stream plus derived statistics.
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
    /// Per-UTC-day aggregates (key `YYYY-MM-DD`), populated only when derived
    /// with `by_day`. `None` means daily bucketing was not requested, in
    /// which case the field is omitted from JSON entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily: Option<BTreeMap<String, DayBucket>>,
    /// Accumulator for the duration average (not serialized).
    #[serde(skip)]
    duration_sum: u64,
    /// Count of sessions that reported a duration (not serialized).
    #[serde(skip)]
    duration_count: u64,
    /// Earliest event across all ledgers, in epoch milliseconds (not
    /// serialized; rendered into `first_recorded_session` at the end).
    #[serde(skip)]
    first_ms: Option<i128>,
    /// Latest event across all ledgers, in epoch milliseconds (not serialized;
    /// rendered into `last_recorded_session` at the end).
    #[serde(skip)]
    last_ms: Option<i128>,
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
            daily: None,
            duration_sum: 0,
            duration_count: 0,
            first_ms: None,
            last_ms: None,
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
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(f) => Box::new(f),
            Err(_) => Box::new(NoopWriter),
        }
    }
}

/// Per-UTC-day bucket of session-activity aggregates.
///
/// Keys are civil dates in the `YYYY-MM-DD` form (UTC). `duration_ms` is `None`
/// when no session attributed to that day reported a duration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DayBucket {
    pub turns: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    /// Sum of reported session durations (ms) attributed to this day; `None`
    /// when none of the sessions in the day reported a duration.
    pub duration_ms: Option<u64>,
    /// Number of session records attributed to this day (by their first
    /// timestamp) — i.e. how many sessions had activity in the day.
    pub sessions: u64,
}

/// Options controlling how [`derive_stats`] projects the ledgers.
///
/// All fields are optional. The default (`StatsOptions::default()`, used by
/// callers that do not care about windowing or bucketing) preserves the
/// historical behavior exactly: no time filter, no daily bucketing.
#[derive(Debug, Clone, Copy, Default)]
pub struct StatsOptions {
    /// Inclusive lower bound on event timestamps, in epoch milliseconds
    /// (`timestamp_ms >= since_ms`). `None` = no time filter.
    ///
    /// Callers compute this as `now_ms - <user duration>` (e.g. from
    /// `humantime::parse_duration` on a `--since 24h` argument); the derivation
    /// itself stays pure and wall-clock-free so tests can pin it to fixed
    /// timestamps.
    pub since_ms: Option<i128>,
    /// Aggregate per UTC civil day (`YYYY-MM-DD`).
    pub by_day: bool,
}

/// Derive the cumulative [`Stats`] by walking the existing ledgers:
/// `sessions/*/events.jsonl` (turns, tokens, tool calls, durations,
/// first/last activity) and `tasks/*.json` (task lifecycle counts).
///
/// Missing or corrupt lines are skipped silently — this is a derivation,
/// not an authority.
pub fn derive_stats(state_dir: &Path, opts: &StatsOptions) -> Stats {
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
        let Some(session) = parse_session_events(&raw, opts.since_ms) else {
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
        // Daily bucketing: per-event aggregates go to the UTC civil day each
        // event fell on; the session itself counts once, on the day of its
        // first event; its duration is attributed to the day of its terminal
        // event.
        if opts.by_day {
            let map = stats.daily.get_or_insert_with(BTreeMap::new);
            if let Some(first_day) = session.first_ms.map(utc_day) {
                map.entry(first_day)
                    .or_insert_with(DayBucket::default)
                    .sessions += 1;
            }
            for (day, ev) in &session.events_by_day {
                let b = map.entry(day.clone()).or_insert_with(DayBucket::default);
                b.turns += ev.turns;
                b.prompt_tokens += ev.prompt_tokens;
                b.completion_tokens += ev.completion_tokens;
                b.reasoning_tokens += ev.reasoning_tokens;
                b.tool_calls += ev.tool_calls;
                b.tool_errors += ev.tool_errors;
            }
            if let Some((day, d)) = session.last_ms.map(utc_day).zip(session.duration_ms) {
                let b = map.entry(day).or_insert_with(DayBucket::default);
                b.duration_ms = Some(b.duration_ms.unwrap_or(0) + d);
            }
        }
        // The active horizon is derived from epoch milliseconds (a single
        // canonical numeric order for every ledger), then rendered to ISO-8601
        // UTC for display at the end of the derivation. This keeps the comparison correct even when legacy
        // `assistant_message` ledgers carry epoch-millis strings and Rust
        // `dispatch` ledgers carry RFC-3339 strings.
        if let Some(first_ms) = session.first_ms {
            match stats.first_ms {
                None => stats.first_ms = Some(first_ms),
                Some(cur) if first_ms < cur => stats.first_ms = Some(first_ms),
                _ => {}
            }
        }
        if let Some(last_ms) = session.last_ms {
            match stats.last_ms {
                None => stats.last_ms = Some(last_ms),
                Some(cur) if last_ms > cur => stats.last_ms = Some(last_ms),
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
        // `--since` filter: use the terminal timestamp (`ended_at`) to decide
        // whether the task completed within the window. Fall back to
        // `started_at` / `created_at` when `ended_at` is absent (a task that
        // was created but never finished has no terminal stamp).
        if let Some(cutoff) = opts.since_ms {
            let ts = v
                .get("ended_at")
                .or_else(|| v.get("started_at"))
                .or_else(|| v.get("created_at"))
                .and_then(|x| x.as_u64());
            match ts {
                Some(t) if (t as i128) < cutoff => continue,
                None => continue, // cannot prove membership; exclude
                _ => {}
            }
        }
        match v.get("status").and_then(|s| s.as_str()) {
            Some("completed") => stats.total_tasks_completed += 1,
            Some("failed") => stats.total_tasks_failed += 1,
            Some("cancelled") => stats.total_tasks_cancelled += 1,
            _ => {}
        }
    }

    // --- Derived averages / cost. ---
    stats.avg_duration_ms = (stats.duration_count > 0)
        .then(|| (stats.duration_sum as f64 / stats.duration_count as f64) * 100.0 / 100.0);
    stats.estimated_cost_saved_usd = (stats.total_prompt_tokens as f64 / 1_000_000.0)
        * PROMPT_COST_PER_MILLION
        + (stats.total_completion_tokens as f64 / 1_000_000.0) * COMPLETION_COST_PER_MILLION;
    stats.estimated_cost_saved_usd = (stats.estimated_cost_saved_usd * 100.0).round() / 100.0;
    stats.last_updated = now_iso();

    // Render the epoch-millisecond horizon accumulators to ISO-8601 UTC.
    stats.first_recorded_session = stats.first_ms.map(|ms| iso_from_ms(ms as u64));
    stats.last_recorded_session = stats.last_ms.map(|ms| iso_from_ms(ms as u64));

    stats
}

/// Write the derived stats to `<state_dir>/telemetry/stats.json` (atomic
/// write-temp-then-rename). Returns the path written.
pub fn write_stats_json(state_dir: &Path, stats: &Stats) -> std::io::Result<PathBuf> {
    let telemetry_dir = state_dir.join("telemetry");
    fs::create_dir_all(&telemetry_dir)?;
    let out = telemetry_dir.join("stats.json");
    let tmp = telemetry_dir.join(format!("stats.json.tmp.{}", std::process::id()));
    let body = serde_json::to_string_pretty(stats).map_err(std::io::Error::other)?;
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
    out.push_str(&format!(
        "  • Turns:                   {}\n",
        stats.total_turns
    ));
    out.push_str(&format!(
        "  • Sessions:                {}\n",
        stats.total_sessions
    ));
    out.push_str(&format!(
        "  • Tasks Completed:         {}\n",
        stats.total_tasks_completed
    ));
    out.push_str(&format!(
        "  • Tasks Failed:            {}\n",
        stats.total_tasks_failed
    ));
    out.push_str(&format!(
        "  • Tasks Cancelled:         {}\n",
        stats.total_tasks_cancelled
    ));
    out.push_str(&format!(
        "  • Total Tool Calls:        {} ({} errors)\n",
        stats.total_tool_calls, stats.total_tool_errors
    ));
    if let Some(avg_ms) = stats.avg_duration_ms {
        out.push_str(&format!(
            "  • Avg Session Duration:    {:.2}s\n",
            avg_ms / 1000.0
        ));
    }
    out.push('\n');

    out.push_str("🧠 TOKEN EFFICIENCY\n");
    out.push_str(&format!(
        "  • Ingested Prompt Tokens:  {}\n",
        stats.total_prompt_tokens
    ));
    out.push_str(&format!(
        "  • Generated Output Tokens: {}\n",
        stats.total_completion_tokens
    ));
    out.push_str(&format!(
        "  • Reasoning Tokens:        {}\n",
        stats.total_reasoning_tokens
    ));
    out.push('\n');

    out.push_str(&format!(
        "💰 CLOUD ARBITRAGE ({} Rates)\n",
        stats.benchmark_model
    ));
    out.push_str(&format!(
        "  • Virtual Cloud Cost:      ${:.2}\n",
        stats.estimated_cost_saved_usd
    ));
    out.push_str("  • Actual Local Cost:       $0.00\n");
    out.push_str(&format!(
        "  • NET SAVINGS:             +${:.2}\n",
        stats.estimated_cost_saved_usd
    ));
    out.push('\n');

    if let Some(daily) = &stats.daily {
        out.push_str("📅 DAILY BREAKDOWN (UTC)\n");
        if daily.is_empty() {
            out.push_str("  (no activity in the selected window)\n");
        } else {
            out.push_str("  DATE           TURNS   P-TOK  C-TOK  R-TOK  TOOLS\n");
            for (day, b) in daily {
                out.push_str(&format!(
                    "  {day}  {:>5}  {:>7}  {:>7}  {:>6}  {:>5}\n",
                    b.turns, b.prompt_tokens, b.completion_tokens, b.reasoning_tokens, b.tool_calls
                ));
            }
        }
        out.push('\n');
    }

    if !stats.tool_calls.is_empty() {
        out.push_str("🔧 TOOL USAGE BREAKDOWN\n");
        for (name, count) in &stats.tool_calls {
            let errs = stats.tool_errors.get(name).copied().unwrap_or(0);
            out.push_str(&format!(
                "  • {:<20} {:>5} calls ({} errors)\n",
                name, count, errs
            ));
        }
        out.push('\n');
    }

    if let (Some(first), Some(last)) = (&stats.first_recorded_session, &stats.last_recorded_session)
    {
        out.push_str(&format!("🕒 Active Horizon: {} → {}\n", first, last));
    }
    out
}

/// Per-UTC-day event aggregates for one session (the per-session half of a
/// daily bucket; the session-level fields are merged in by `derive_stats`).
#[derive(Default)]
struct DayEvents {
    turns: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    reasoning_tokens: u64,
    tool_calls: u64,
    tool_errors: u64,
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
    /// Earliest event timestamp as epoch milliseconds (UTC-normalized).
    first_ms: Option<i128>,
    /// Latest event timestamp as epoch milliseconds (UTC-normalized).
    last_ms: Option<i128>,
    /// Per-UTC-day event aggregates (civil date `YYYY-MM-DD` → events).
    events_by_day: BTreeMap<String, DayEvents>,
}

/// Format an epoch-millisecond instant as a UTC civil date string
/// (`YYYY-MM-DD`). All calendar math is delegated to `chrono` (`DateTime` is
/// UTC by construction).
fn utc_day(ms: i128) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".to_string())
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
///
/// The parser reads the **canonical** schema only (there is no
/// backward-compatibility fallback; historical ledgers are migrated forward
/// once by `.scratch/migrate_ledgers.py`):
/// - `dispatch` turns carrying per-turn token usage under a `metrics`
///   sub-object (`prompt_tokens` / `completion_tokens` / `reasoning_tokens`),
/// - `tool_result` events (a `name` field, an `is_error` flag),
/// - a `final` terminal (an optional `duration_ms`).
///
/// Timestamps are parsed as RFC-3339 UTC strings and normalized to integer
/// epoch milliseconds (the single order for the horizon and the `--since`
/// window). A missing or non-RFC-3339 timestamp yields no epoch value (the
/// event still counts, but cannot be windowed).
///
/// `since` is the `--since` time-window cutoff (inclusive lower bound, in
/// epoch milliseconds). `None` keeps every event; a value keeps only events
/// whose timestamp is `>=` the cutoff (an untimestamped event is excluded
/// because it cannot be proven inside the window).
fn parse_session_events(raw: &str, since: Option<i128>) -> Option<SessionAgg> {
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
        first_ms: None,
        last_ms: None,
        events_by_day: BTreeMap::new(),
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

        // Normalize this event's timestamp to epoch milliseconds (the single
        // canonical order for both the horizon and the `--since` window).
        let ms = v.get("timestamp").and_then(parse_timestamp_ms);

        // `--since` time-window filter: keep the event only when its
        // timestamp is at or after the cutoff. Without a cutoff, nothing is
        // filtered and untimestamped events pass. With a cutoff, an
        // untimestamped event cannot be proven inside the window and is
        // excluded.
        let in_window = match (since, ms) {
            (None, _) => true,
            (Some(cutoff), Some(m)) => m >= cutoff,
            (Some(_), None) => false,
        };
        if !in_window {
            continue;
        }
        any = true;

        if let Some(ms) = ms {
            agg.first_ms = Some(match agg.first_ms {
                Some(cur) => cur.min(ms),
                None => ms,
            });
            agg.last_ms = Some(match agg.last_ms {
                Some(cur) => cur.max(ms),
                None => ms,
            });
        }

        // Daily bucketing: attribute this event's contribution to its UTC
        // civil day (only for events that carry a timestamp).
        let day = ms.map(utc_day);

        match v.get("type").and_then(|t| t.as_str()) {
            // One model turn (canonical `dispatch`); carries per-turn token
            // usage under `metrics`.
            Some("dispatch") => {
                agg.turns += 1;
                let (prompt, completion, reasoning) = read_turn_tokens(&v);
                agg.prompt_tokens += prompt.unwrap_or(0);
                agg.completion_tokens += completion.unwrap_or(0);
                agg.reasoning_tokens += reasoning.unwrap_or(0);
                if let Some(day) = day {
                    let ev = agg.events_by_day.entry(day).or_default();
                    ev.turns += 1;
                    ev.prompt_tokens += prompt.unwrap_or(0);
                    ev.completion_tokens += completion.unwrap_or(0);
                    ev.reasoning_tokens += reasoning.unwrap_or(0);
                }
            }
            // One executed tool call: canonical `name` + optional `is_error`.
            Some("tool_result") => {
                if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
                    agg.tool_calls += 1;
                    let is_err = v.get("is_error").and_then(|e| e.as_bool()) == Some(true);
                    if is_err {
                        agg.tool_errors += 1;
                        *agg.tool_errors_by_name.entry(name.to_string()).or_insert(0) += 1;
                    }
                    *agg.tool_calls_by_name.entry(name.to_string()).or_insert(0) += 1;
                    if let Some(day) = day {
                        let ev = agg.events_by_day.entry(day).or_default();
                        ev.tool_calls += 1;
                        if is_err {
                            ev.tool_errors += 1;
                        }
                    }
                }
            }
            // Session terminal (canonical `final`), carrying an optional
            // `duration_ms`. The session-total completion token count is
            // deliberately NOT accumulated here — it is already the sum of the
            // per-turn metrics, so adding it would double-count.
            Some("final") => {
                if let Some(d) = v.get("duration_ms").and_then(|x| x.as_u64()) {
                    agg.duration_ms = Some(d);
                }
            }
            _ => {}
        }
    }

    any.then_some(agg)
}

/// Read a turn's token usage from a ledger event's `metrics` sub-object.
///
/// The canonical schema places per-turn usage under `metrics` with the keys
/// `prompt_tokens` / `completion_tokens` / `reasoning_tokens`. Any absent
/// field is `None` (never a silent zero).
fn read_turn_tokens(v: &Value) -> (Option<u64>, Option<u64>, Option<u64>) {
    let src = v.get("metrics").unwrap_or(v);
    let prompt = src.get("prompt_tokens").and_then(Value::as_u64);
    let completion = src.get("completion_tokens").and_then(Value::as_u64);
    let reasoning = src.get("reasoning_tokens").and_then(Value::as_u64);
    (prompt, completion, reasoning)
}

/// Normalize a ledger `timestamp` into integer epoch milliseconds (UTC).
///
/// The canonical ledger stamps every event with an RFC-3339 UTC string
/// (`YYYY-MM-DDTHH:MM:SS.mmmZ`, as produced by [`iso_from_ms`]). This parses
/// that grammar directly via [`chrono::DateTime::parse_from_rfc3339`] and
/// returns the epoch-millis value (the single order used for the active
/// horizon and the `--since` window). A JSON number is also accepted (epoch
/// milliseconds directly). Anything else — a missing timestamp, a bare integer
/// string, a legacy `…ms` string, or malformed text — yields `None`; the event
/// still counts but carries no timestamp for windowing.
fn parse_timestamp_ms(v: &Value) -> Option<i128> {
    match v {
        Value::Number(n) => n.as_i64().map(|x| x as i128),
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s.trim())
            .ok()
            .map(|dt| dt.timestamp_millis() as i128),
        _ => None,
    }
}

/// Format epoch milliseconds as an ISO-8601 / RFC-3339 UTC string (`...Z`).
///
/// `pub(crate)` so the session event logger (`runner::events`) can stamp every
/// event with a standards-compliant timestamp. All civil-date / epoch math is
/// delegated to `chrono`.
pub(crate) fn iso_from_ms(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        // Out-of-range epoch millis (beyond chrono's bounds) must not break the
        // inference path: degrade to a zeroed RFC-3339 stamp instead of
        // panicking.
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
}

/// Current time as an ISO-8601 UTC string.
fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    iso_from_ms(ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
                "{\"type\":\"dispatch\",\"turn\":1,\"timestamp\":\"2026-09-05T03:58:07.228Z\",\"metrics\":{\"prompt_tokens\":60000,\"completion_tokens\":20000,\"reasoning_tokens\":5000}}\n",
                "{\"type\":\"tool_result\",\"turn\":1,\"name\":\"bash\",\"is_error\":false,\"timestamp\":\"2026-09-05T03:58:07.500Z\"}\n",
                "{\"type\":\"dispatch\",\"turn\":2,\"timestamp\":\"2026-09-05T03:58:08.000Z\",\"metrics\":{\"prompt_tokens\":40000,\"completion_tokens\":30000,\"reasoning_tokens\":5000}}\n",
                "{\"type\":\"tool_result\",\"turn\":2,\"name\":\"read_file\",\"is_error\":true,\"timestamp\":\"2026-09-05T03:58:08.200Z\"}\n",
                "{\"type\":\"final\",\"status\":\"completed\",\"duration_ms\":1000,\"timestamp\":\"2026-09-05T03:58:09.000Z\"}\n"
            ),
        );

        // Session B: 1 turn, 1 tool call, no final (no duration).
        write(
            &state.join("sessions/sb/events.jsonl"),
            concat!(
                "{\"type\":\"dispatch\",\"turn\":1,\"timestamp\":\"2026-09-05T04:00:00.000Z\",\"metrics\":{\"prompt_tokens\":10000,\"completion_tokens\":5000}}\n",
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

        let s = derive_stats(&state, &Default::default());
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
        let s = derive_stats(&state, &Default::default());
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
                "{\"type\":\"dispatch\",\"turn\":1,\"timestamp\":\"2026-09-05T05:00:00.000Z\",\"metrics\":{\"prompt_tokens\":7}}\n",
                "{\"broken\":\n",
                "{\"type\":\"tool_result\",\"turn\":1,\"name\":\"bash\",\"is_error\":false,\"timestamp\":\"2026-09-05T05:00:01.000Z\"}\n"
            ),
        );
        let s = derive_stats(&state, &Default::default());
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
        let s = derive_stats(&state, &Default::default());
        let path = write_stats_json(&state, &s).unwrap();
        assert!(path.exists());
        let back: Stats = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
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
            v.get("fields")
                .and_then(|f| f.get("hello"))
                .and_then(|x| x.as_str()),
            Some("world")
        );
        assert!(v.get("timestamp").is_some());
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn format_stats_card_renders_dashboard() {
        let state = tmp_state();
        let s = derive_stats(&state, &Default::default());
        let card = format_stats_card(&s);
        assert!(card.contains("CASTOR OPERATIONAL TELEMETRY"));
        assert!(card.contains("ACTIVITY & RUNTIME"));
        assert!(card.contains("TOKEN EFFICIENCY"));
        assert!(card.contains("CLOUD ARBITRAGE"));
        let _ = fs::remove_dir_all(&state);
    }

    // --- Single canonical schema (no backward-compatibility fallbacks) ---

    /// A canonical `dispatch` ledger: snake_case `metrics`, `name`/`is_error`
    /// on tool results, a `final` terminal carrying `duration_ms`, and
    /// RFC-3339 timestamps throughout.
    const CANONICAL_LEDGER: &str = concat!(
        "{\"type\":\"dispatch\",\"turn\":1,\"timestamp\":\"2026-10-01T12:00:00.000Z\",\"metrics\":{\"prompt_tokens\":1000,\"completion_tokens\":200,\"reasoning_tokens\":50}}\n",
        "{\"type\":\"tool_result\",\"turn\":1,\"name\":\"bash\",\"is_error\":true,\"timestamp\":\"2026-10-01T12:00:01.000Z\"}\n",
        "{\"type\":\"final\",\"status\":\"completed\",\"duration_ms\":1000,\"timestamp\":\"2026-10-01T12:00:02.000Z\"}\n",
    );

    #[test]
    fn parse_canonical_dispatch_ledger() {
        let agg = parse_session_events(CANONICAL_LEDGER, None).expect("canonical ledger parses");
        assert_eq!(agg.turns, 1);
        assert_eq!(agg.prompt_tokens, 1000);
        assert_eq!(agg.completion_tokens, 200);
        assert_eq!(agg.reasoning_tokens, 50);
        assert_eq!(agg.tool_calls, 1);
        assert_eq!(agg.tool_errors, 1);
        assert_eq!(agg.tool_calls_by_name.get("bash"), Some(&1));
        assert_eq!(agg.tool_errors_by_name.get("bash"), Some(&1));
        assert_eq!(agg.duration_ms, Some(1000));
        // The RFC-3339 horizon normalizes to integer epoch milliseconds.
        assert_eq!(
            agg.first_ms,
            Some(parse_timestamp_ms(&json!("2026-10-01T12:00:00.000Z")).unwrap())
        );
        assert_eq!(
            agg.last_ms,
            Some(parse_timestamp_ms(&json!("2026-10-01T12:00:02.000Z")).unwrap())
        );
        assert!(agg.first_ms.unwrap() < agg.last_ms.unwrap());
    }

    /// Regression guard for the dual-schema purge: a ledger still carrying the
    /// **legacy** spellings (`assistant_message` turns, `toolName`/`isError`
    /// tool fields, `session_end` terminal, and `…ms` epoch-string
    /// timestamps) must now be read as an *empty* session — none of the legacy
    /// fields are recognized anymore, so nothing turns into a turn/tool, and
    /// the non-RFC-3339 timestamps yield no horizon. These records only reach
    /// the reader if a session was written by a pre-migration binary and never
    /// re-migrated; the migration (or the new writer) eliminates them.
    const LEGACY_LEDGER: &str = concat!(
        "{\"timestamp\":\"1791000000000ms\",\"type\":\"assistant_message\",\"metrics\":{\"promptTokens\":60000,\"completionTokens\":20000,\"reasoningTokens\":5000}}\n",
        "{\"timestamp\":\"1791000000100ms\",\"type\":\"tool_result\",\"toolName\":\"bash\",\"isError\":false}\n",
        "{\"timestamp\":\"1791000000200ms\",\"type\":\"session_end\",\"status\":\"completed\",\"durationMs\":1000}\n",
    );

    #[test]
    fn legacy_spellings_are_no_longer_recognized() {
        let agg = parse_session_events(LEGACY_LEDGER, None).expect("ledger is well-formed JSON");
        // No canonical `dispatch` → zero turns (the `assistant_message` is
        // ignored, not matched).
        assert_eq!(agg.turns, 0);
        assert_eq!(agg.prompt_tokens, 0);
        // No canonical `name`/`is_error` → the `tool_result` is ignored.
        assert_eq!(agg.tool_calls, 0);
        assert_eq!(agg.tool_errors, 0);
        // No canonical `final`/`duration_ms` → no duration.
        assert_eq!(agg.duration_ms, None);
        // The `…ms` epoch-string timestamps are not RFC-3339 → no horizon.
        assert_eq!(agg.first_ms, None);
        assert_eq!(agg.last_ms, None);
    }

    /// `parse_timestamp_ms` reads RFC-3339 (and a JSON epoch-millis number)
    /// and returns `None` for every other shape.
    #[test]
    fn parse_timestamp_ms_reads_rfc3339_only() {
        // RFC-3339 with Z (verified against `date -u` / `python3`).
        assert_eq!(
            parse_timestamp_ms(&json!("2026-09-05T03:58:07.228Z")),
            Some(1_788_580_687_228)
        );
        // JSON number (epoch millis) is still accepted.
        assert_eq!(
            parse_timestamp_ms(&json!(1_791_000_000_000i64)),
            Some(1_791_000_000_000)
        );
        // Legacy epoch-millis string with `ms` suffix → rejected.
        assert_eq!(parse_timestamp_ms(&json!("1791000000000ms")), None);
        // Bare integer string (interpreted as epoch millis) → rejected.
        assert_eq!(parse_timestamp_ms(&json!("1791000000000")), None);
        // RFC-3339 without a timezone → rejected.
        assert_eq!(parse_timestamp_ms(&json!("2026-09-05T03:58:07")), None);
        // Not a date at all → rejected.
        assert_eq!(parse_timestamp_ms(&json!("not a date")), None);
        assert_eq!(parse_timestamp_ms(&json!(null)), None);
        assert_eq!(parse_timestamp_ms(&json!(true)), None);
    }

    /// `iso_from_ms` and `parse_timestamp_ms` are exact inverses: the
    /// writer's `now_iso` stamp (RFC-3339 `...Z`) must round-trip through the
    /// reader to the same epoch milliseconds.
    #[test]
    fn iso_from_ms_round_trips_through_reader() {
        let ms: u64 = 1_757_048_687_228; // an arbitrary non-zero instant
        let iso = iso_from_ms(ms);
        assert!(iso.ends_with('Z'), "expected Z-suffixed UTC, got {iso}");
        assert_eq!(iso.len(), 24, "YYYY-MM-DDTHH:MM:SS.mmmZ, got {iso}");
        assert_eq!(
            parse_timestamp_ms(&json!(iso)),
            Some(ms as i128),
            "iso {iso} must round-trip to {ms}"
        );
    }

    // --- --since windowing & --by-day bucketing ----------------

    /// A session whose two turns straddle a UTC midnight so each turn lands
    /// on a distinct civil day. The first turn (00:30 UTC) and its tool
    /// result are on day D; the second turn (23:15 UTC) is on day D+1.
    const TWO_DAY_LEDGER: &str = concat!(
        "{\"type\":\"dispatch\",\"turn\":1,\"timestamp\":\"2026-09-05T00:30:00.000Z\",\"metrics\":{\"prompt_tokens\":1000,\"completion_tokens\":200,\"reasoning_tokens\":50}}\n",
        "{\"type\":\"tool_result\",\"turn\":1,\"name\":\"bash\",\"is_error\":false,\"timestamp\":\"2026-09-05T00:30:01.000Z\"}\n",
        "{\"type\":\"dispatch\",\"turn\":2,\"timestamp\":\"2026-09-06T23:15:00.000Z\",\"metrics\":{\"prompt_tokens\":2000,\"completion_tokens\":400,\"reasoning_tokens\":100}}\n",
        "{\"type\":\"tool_result\",\"turn\":2,\"name\":\"read_file\",\"is_error\":true,\"timestamp\":\"2026-09-06T23:15:01.000Z\"}\n",
        "{\"type\":\"final\",\"status\":\"completed\",\"duration_ms\":5000,\"timestamp\":\"2026-09-06T23:15:02.000Z\"}\n",
    );

    /// `utc_day` renders an epoch-millis instant as its UTC civil date,
    /// independent of the local timezone. The epoch values are produced by
    /// the independent `parse_timestamp_ms` parser, so this is a cross-check
    /// that the civil-date formatting agrees with the RFC-3339 parsing.
    #[test]
    fn utc_day_renders_utc_civil_date() {
        let d1 = parse_timestamp_ms(&json!("2026-09-05T00:30:00.000Z")).unwrap();
        let d2 = parse_timestamp_ms(&json!("2026-09-06T23:15:00.000Z")).unwrap();
        assert_eq!(utc_day(d1), "2026-09-05");
        assert_eq!(utc_day(d2), "2026-09-06");
        // Epoch 0 → 1970-01-01.
        assert_eq!(utc_day(0), "1970-01-01");
    }

    /// The `--by-day` flag splits per-event aggregates into two UTC days.
    /// A session that spans a midnight is counted once (on its first day)
    /// and its duration is attributed to the day of its terminal event.
    #[test]
    fn by_day_buckets_events_by_utc_date() {
        let state = tmp_state();
        write(&state.join("sessions/sa/events.jsonl"), TWO_DAY_LEDGER);

        let opts = StatsOptions {
            by_day: true,
            ..Default::default()
        };
        let s = derive_stats(&state, &opts);
        let daily = s.daily.expect("--by-day populates the daily map");

        // Exactly two UTC days.
        assert_eq!(daily.len(), 2);

        let d0 = daily.get("2026-09-05").expect("day 1 present");
        assert_eq!(d0.turns, 1);
        assert_eq!(d0.prompt_tokens, 1000);
        assert_eq!(d0.completion_tokens, 200);
        assert_eq!(d0.reasoning_tokens, 50);
        assert_eq!(d0.tool_calls, 1);
        assert_eq!(d0.tool_errors, 0);
        assert_eq!(d0.sessions, 1);
        assert_eq!(d0.duration_ms, None);

        let d1 = daily.get("2026-09-06").expect("day 2 present");
        assert_eq!(d1.turns, 1);
        assert_eq!(d1.prompt_tokens, 2000);
        assert_eq!(d1.completion_tokens, 400);
        assert_eq!(d1.reasoning_tokens, 100);
        assert_eq!(d1.tool_calls, 1);
        assert_eq!(d1.tool_errors, 1);
        assert_eq!(d1.sessions, 0);
        assert_eq!(d1.duration_ms, Some(5000));

        let _ = fs::remove_dir_all(&state);
    }

    /// `derive_stats` with default options (no `--by-day`) must not populate
    /// the daily map — keeping the JSON shape byte-identical to before.
    #[test]
    fn default_options_omit_daily() {
        let state = tmp_state();
        write(&state.join("sessions/sa/events.jsonl"), TWO_DAY_LEDGER);
        let s = derive_stats(&state, &Default::default());
        assert!(s.daily.is_none(), "daily must be None without --by-day");
        let _ = fs::remove_dir_all(&state);
    }

    /// `--since` windowing: only events at/after the cutoff contribute.
    /// The cutoff sits between the two turns, so the day-1 turn is dropped
    /// and the day-2 turn is kept. The boundary is inclusive (`>=`).
    #[test]
    fn since_filters_events_by_time_boundary() {
        let state = tmp_state();
        write(&state.join("sessions/sa/events.jsonl"), TWO_DAY_LEDGER);

        let cutoff = parse_timestamp_ms(&json!("2026-09-06T23:15:00.000Z")).unwrap();
        let opts = StatsOptions {
            since_ms: Some(cutoff),
            ..Default::default()
        };
        let s = derive_stats(&state, &opts);
        assert_eq!(s.total_turns, 1);
        assert_eq!(s.total_prompt_tokens, 2000);
        assert_eq!(s.total_completion_tokens, 400);
        assert_eq!(s.total_reasoning_tokens, 100);
        assert_eq!(s.total_tool_calls, 1);
        assert_eq!(s.total_tool_errors, 1);
        assert_eq!(s.tool_calls.get("read_file"), Some(&1));
        assert_eq!(
            s.first_recorded_session.as_deref(),
            Some(iso_from_ms(cutoff as u64).as_str())
        );

        let opts2 = StatsOptions {
            since_ms: Some(cutoff + 1),
            ..Default::default()
        };
        let s2 = derive_stats(&state, &opts2);
        assert_eq!(s2.total_turns, 0, "the turn must fall outside cutoff+1");
        assert_eq!(
            s2.total_tool_calls, 1,
            "the later tool_result is still in-window"
        );
        assert_eq!(s2.tool_calls.get("read_file"), Some(&1));

        let _ = fs::remove_dir_all(&state);
    }

    /// `--since` + `--by-day` together: the window filters first, then the
    /// surviving events are bucketed by UTC day.
    #[test]
    fn since_and_by_day_compose() {
        let state = tmp_state();
        write(&state.join("sessions/sa/events.jsonl"), TWO_DAY_LEDGER);

        let cutoff = parse_timestamp_ms(&json!("2026-09-06T23:14:00.000Z")).unwrap();
        let opts = StatsOptions {
            since_ms: Some(cutoff),
            by_day: true,
        };
        let s = derive_stats(&state, &opts);
        let daily = s.daily.expect("--by-day populates the daily map");
        assert_eq!(daily.len(), 1, "expected a single day, got {daily:?}");
        let d = daily.get("2026-09-06").expect("day 2 present");
        assert_eq!(d.turns, 1);
        assert_eq!(d.prompt_tokens, 2000);
        assert_eq!(d.tool_calls, 1);
        let _ = fs::remove_dir_all(&state);
    }

    /// The `format_stats_card` renders the daily table when `--by-day` is
    /// active and omits it otherwise.
    #[test]
    fn format_stats_card_shows_daily_when_by_day() {
        let state = tmp_state();
        write(&state.join("sessions/sa/events.jsonl"), TWO_DAY_LEDGER);

        let s_by_day = derive_stats(
            &state,
            &StatsOptions {
                by_day: true,
                ..Default::default()
            },
        );
        let card = format_stats_card(&s_by_day);
        assert!(
            card.contains("DAILY BREAKDOWN"),
            "card should show daily: {card}"
        );
        assert!(
            card.contains("2026-09-05"),
            "card should list day 1: {card}"
        );
        assert!(
            card.contains("2026-09-06"),
            "card should list day 2: {card}"
        );

        let s_plain = derive_stats(&state, &Default::default());
        let card_plain = format_stats_card(&s_plain);
        assert!(
            !card_plain.contains("DAILY BREAKDOWN"),
            "card should omit daily when not requested: {card_plain}"
        );

        let _ = fs::remove_dir_all(&state);
    }

    /// A session with *only* untimestamped events must be counted normally
    /// without `--since`, but dropped entirely when a `--since` window is
    /// active (it cannot be proven inside the window).
    #[test]
    fn untimestamped_events_only_excluded_with_since() {
        let state = tmp_state();
        write(
            &state.join("sessions/sa/events.jsonl"),
            concat!(
                "{\"type\":\"dispatch\",\"turn\":1,\"metrics\":{\"prompt_tokens\":5}}\n",
                "{\"type\":\"tool_result\",\"turn\":1,\"name\":\"bash\",\"is_error\":false}\n"
            ),
        );

        // No window: counted normally.
        let s_plain = derive_stats(&state, &Default::default());
        assert_eq!(s_plain.total_turns, 1);
        assert_eq!(s_plain.total_tool_calls, 1);

        // With a window: the untimestamped events are excluded.
        let s_win = derive_stats(
            &state,
            &StatsOptions {
                since_ms: Some(0),
                ..Default::default()
            },
        );
        assert_eq!(s_win.total_turns, 0);
        assert_eq!(s_win.total_tool_calls, 0);
        assert_eq!(s_win.total_sessions, 0, "no in-window events → no session");

        let _ = fs::remove_dir_all(&state);
    }
}
