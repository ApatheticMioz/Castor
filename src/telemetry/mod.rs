//! Telemetry: a single event stream plus derived statistics.
//!
//! Locked decision: there is exactly ONE event stream. The tracing-JSONL
//! ledger (`<state>/telemetry/events.jsonl`) is the ledger; there is NO
//! second stats writer. Statistics are *derived* on demand by walking the
//! existing ledgers (`sessions/*/events.jsonl` + `tasks/*.json`)
//! rather than maintained by a parallel writer.
//!
//! Derivation is best-effort: missing or corrupt ledger lines are skipped
//! silently (the ledgers are the authority; this module is a read-only
//! projection over them).
//!
//! Submodules:
//! - [`emit`] — tracing-subscriber initialization + the `FileSink` JSONL writer.
//! - [`derive`] — parsing a single session's ledger into per-session aggregates,
//!   fanned out across the session directories in parallel chunks.
//! - [`rollup`] — fold the per-session aggregates + task records into [`Stats`],
//!   render the dashboard card, and write `stats.json`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

mod derive;
mod emit;
mod rollup;

pub use rollup::{
    derive_stats, format_stats_card, format_stats_card_styled, write_stats_json,
};
pub use emit::init_tracing;

/// Baseline frontier-model pricing (USD per million tokens).
const PROMPT_COST_PER_MILLION: f64 = 2.0;
const COMPLETION_COST_PER_MILLION: f64 = 10.0;

/// Default average system electrical power consumption in kilowatts (250 W).
const DEFAULT_SYSTEM_POWER_KW: f64 = 0.25;

/// Default electrical draw in kilowatts while the GPU is idle (i.e. a session
/// is alive but no model compute is in flight) — 30 W.
const DEFAULT_IDLE_POWER_KW: f64 = 0.03;

/// Default effective electricity rate in USD per kilowatt-hour ($0.18/kWh).
const DEFAULT_ELECTRICITY_COST_PER_KWH: f64 = 0.18;

/// The benchmark model the cost-saved figure is quoted against.
const BENCHMARK_MODEL: &str = "Claude Sonnet 5";

fn default_electricity_rate() -> f64 {
    DEFAULT_ELECTRICITY_COST_PER_KWH
}

fn default_system_power_watts() -> u32 {
    (DEFAULT_SYSTEM_POWER_KW * 1000.0).round() as u32
}

fn system_power_kw() -> f64 {
    std::env::var("CASTOR_SYSTEM_WATTS")
        .or_else(|_| std::env::var("CASTOR_POWER_WATTS"))
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|w| w / 1000.0)
        .unwrap_or(DEFAULT_SYSTEM_POWER_KW)
}

fn electricity_rate_usd() -> f64 {
    std::env::var("CASTOR_ELECTRICITY_RATE")
        .or_else(|_| std::env::var("CASTOR_ELECTRICITY_RATE_USD"))
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(DEFAULT_ELECTRICITY_COST_PER_KWH)
}

/// Electrical draw in kilowatts while the GPU is idle, honoring the
/// `CASTOR_IDLE_WATTS` override (falls back to the 30 W default).
fn idle_power_kw() -> f64 {
    std::env::var("CASTOR_IDLE_WATTS")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|w| w / 1000.0)
        .unwrap_or(DEFAULT_IDLE_POWER_KW)
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

/// Hall-of-Fame extremes across the scanned events: the single most
/// notable values observed in the window. `None` means no event in the
/// scanned set reported a finite value for that metric (never zero-filled,
/// mirroring the `Stats` averaging policy).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ExtremesSummary {
    /// Largest single-turn prompt-token count (the biggest context ingested
    /// by any one dispatch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_prompt_tokens: Option<u64>,
    /// Largest single-turn reasoning-token count (the deepest single
    /// deliberation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_reasoning_tokens: Option<u64>,
    /// Fastest observed Time-To-First-Token in ms (best prefill latency).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_ttft_ms: Option<f64>,
    /// Peak hardware decode speed in tokens/second (fastest speculative
    /// burst; same source as `Stats::peak_decode_tps`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_decode_tps: Option<f64>,
}

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
    pub total_cached_tokens: u64,
    /// Total prompt tokens on turns that reported cache metrics.
    #[serde(default)]
    pub total_cache_eligible_prompt_tokens: u64,
    pub total_turns: u64,
    pub total_sessions: u64,
    pub total_tasks_completed: u64,
    pub total_tasks_failed: u64,
    pub total_tasks_cancelled: u64,
    /// Total task execution time in ms (sum of `ended_at - started_at` over
    /// task records that reported both bounds; wall-clock runtime while a
    /// task was actually executing).
    pub total_task_execution_ms: u64,
    /// Mean session duration in ms (over sessions that reported one).
    pub avg_duration_ms: Option<f64>,
    /// Total duration sum in ms over sessions that reported one.
    pub total_duration_ms: u64,
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
    /// Estimated local electricity cost in USD.
    pub estimated_electricity_cost_usd: f64,
    /// Estimated energy consumption in kWh.
    pub estimated_energy_kwh: f64,
    /// Real GPU model-compute time in ms: the sum of (TTFT prefill + decode)
    /// across all qualifying turns in every session, including sessions that
    /// never wrote a `final` event (crashed sessions still contribute their
    /// measured compute time).
    #[serde(default)]
    pub inference_compute_ms: u64,
    /// Electricity cost rate used in calculations (USD per kWh).
    #[serde(default = "default_electricity_rate")]
    pub electricity_rate_usd: f64,
    /// System electrical draw in watts used in calculations.
    #[serde(default = "default_system_power_watts")]
    pub system_power_watts: u32,
    /// Net operational savings in USD (virtual cloud cost - electricity cost).
    pub net_savings_usd: f64,
    /// Deliberation ratio: reasoning tokens / completion tokens.
    pub reasoning_ratio: Option<f64>,
    /// Surgical precision ratio: edit_file calls / write_file calls.
    pub edit_write_ratio: Option<f64>,
    /// Hardware decode speed in tokens/second (pure generation, excluding prefill).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_decode_tps: Option<f64>,
    /// Peak speculative decode burst speed observed (tokens/second).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_decode_tps: Option<f64>,
    /// Effective prefill throughput in tokens/second (accelerated by prefix cache).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_prefill_tps: Option<f64>,
    /// Average Time To First Token (prefill latency) in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_ttft_ms: Option<f64>,
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
    /// Hall-of-Fame extremes. `Some` only when derived with `extremes: true`
    /// (`castor stats --extremes`); `None` otherwise (omitted from JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extremes: Option<ExtremesSummary>,
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
            total_cached_tokens: 0,
            total_cache_eligible_prompt_tokens: 0,
            total_turns: 0,
            total_sessions: 0,
            total_tasks_completed: 0,
            total_tasks_failed: 0,
            total_tasks_cancelled: 0,
            total_task_execution_ms: 0,
            avg_duration_ms: None,
            total_duration_ms: 0,
            total_tool_calls: 0,
            total_tool_errors: 0,
            tool_calls: BTreeMap::new(),
            tool_errors: BTreeMap::new(),
            benchmark_model: BENCHMARK_MODEL.to_string(),
            estimated_cost_saved_usd: 0.0,
            estimated_electricity_cost_usd: 0.0,
            estimated_energy_kwh: 0.0,
            inference_compute_ms: 0,
            electricity_rate_usd: DEFAULT_ELECTRICITY_COST_PER_KWH,
            system_power_watts: (DEFAULT_SYSTEM_POWER_KW * 1000.0).round() as u32,
            net_savings_usd: 0.0,
            reasoning_ratio: None,
            edit_write_ratio: None,
            avg_decode_tps: None,
            peak_decode_tps: None,
            effective_prefill_tps: None,
            avg_ttft_ms: None,
            first_recorded_session: None,
            last_recorded_session: None,
            last_updated: String::new(),
            daily: None,
            extremes: None,
            duration_sum: 0,
            duration_count: 0,
            first_ms: None,
            last_ms: None,
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
    pub cached_tokens: u64,
    #[serde(default)]
    pub cache_eligible_prompt_tokens: u64,
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
/// callers that do not care about windowing or bucketing) applies no time
/// filter and no daily bucketing.
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
    /// Surface the Hall-of-Fame extremes section in the rendered card
    /// (`castor stats --extremes`).
    pub extremes: bool,
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::derive::parse_timestamp_ms;
    use super::*;
    use serde_json::json;

    /// Shared test fixtures for the submodule test modules: a fresh temp
    /// state directory (unique per process + invocation) and a writer that
    /// creates parent directories on demand.
    pub(super) fn tmp_state() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "castor_tel_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(super) fn write(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
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
}
