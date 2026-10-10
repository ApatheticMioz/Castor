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
#[cfg(test)]
use serde_json::Value;
use tracing_subscriber::prelude::*;

/// Baseline frontier-model pricing (USD per million tokens), ported from
/// `mcp-castor/src/telemetry.js` (Claude Sonnet 5 tier).
const PROMPT_COST_PER_MILLION: f64 = 2.0;
const COMPLETION_COST_PER_MILLION: f64 = 10.0;

/// Default average system electrical power consumption in kilowatts (250 W).
const DEFAULT_SYSTEM_POWER_KW: f64 = 0.25;

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
            total_cached_tokens: 0,
            total_cache_eligible_prompt_tokens: 0,
            total_turns: 0,
            total_sessions: 0,
            total_tasks_completed: 0,
            total_tasks_failed: 0,
            total_tasks_cancelled: 0,
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
            electricity_rate_usd: DEFAULT_ELECTRICITY_COST_PER_KWH,
            system_power_watts: (DEFAULT_SYSTEM_POWER_KW * 1000.0).round() as u32,
            net_savings_usd: 0.0,
            reasoning_ratio: None,
            edit_write_ratio: None,
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

#[derive(Default)]
struct SessionRollup {
    total_sessions: u64,
    total_turns: u64,
    total_prompt_tokens: u64,
    total_cached_tokens: u64,
    total_cache_eligible_prompt_tokens: u64,
    total_completion_tokens: u64,
    total_reasoning_tokens: u64,
    total_tool_calls: u64,
    total_tool_errors: u64,
    tool_calls: BTreeMap<String, u64>,
    tool_errors: BTreeMap<String, u64>,
    duration_sum: u64,
    duration_count: u64,
    first_ms: Option<i128>,
    last_ms: Option<i128>,
    daily: BTreeMap<String, DayBucket>,
}

impl SessionRollup {
    fn merge(&mut self, other: SessionRollup) {
        self.total_sessions += other.total_sessions;
        self.total_turns += other.total_turns;
        self.total_prompt_tokens += other.total_prompt_tokens;
        self.total_cached_tokens += other.total_cached_tokens;
        self.total_cache_eligible_prompt_tokens += other.total_cache_eligible_prompt_tokens;
        self.total_completion_tokens += other.total_completion_tokens;
        self.total_reasoning_tokens += other.total_reasoning_tokens;
        self.total_tool_calls += other.total_tool_calls;
        self.total_tool_errors += other.total_tool_errors;
        for (name, n) in other.tool_calls {
            *self.tool_calls.entry(name).or_insert(0) += n;
        }
        for (name, n) in other.tool_errors {
            *self.tool_errors.entry(name).or_insert(0) += n;
        }
        self.duration_sum += other.duration_sum;
        self.duration_count += other.duration_count;
        if let Some(f) = other.first_ms {
            match self.first_ms {
                None => self.first_ms = Some(f),
                Some(cur) if f < cur => self.first_ms = Some(f),
                _ => {}
            }
        }
        if let Some(l) = other.last_ms {
            match self.last_ms {
                None => self.last_ms = Some(l),
                Some(cur) if l > cur => self.last_ms = Some(l),
                _ => {}
            }
        }
        for (day, b) in other.daily {
            let entry = self.daily.entry(day).or_default();
            entry.turns += b.turns;
            entry.prompt_tokens += b.prompt_tokens;
            entry.cached_tokens += b.cached_tokens;
            entry.cache_eligible_prompt_tokens += b.cache_eligible_prompt_tokens;
            entry.completion_tokens += b.completion_tokens;
            entry.reasoning_tokens += b.reasoning_tokens;
            entry.tool_calls += b.tool_calls;
            entry.tool_errors += b.tool_errors;
            entry.sessions += b.sessions;
            if let Some(d) = b.duration_ms {
                entry.duration_ms = Some(entry.duration_ms.unwrap_or(0) + d);
            }
        }
    }
}

fn add_session_to_rollup(rollup: &mut SessionRollup, session: &SessionAgg, opts: &StatsOptions) {
    rollup.total_sessions += 1;
    rollup.total_turns += session.turns;
    rollup.total_prompt_tokens += session.prompt_tokens;
    rollup.total_cached_tokens += session.cached_tokens;
    rollup.total_cache_eligible_prompt_tokens += session.cache_eligible_prompt_tokens;
    rollup.total_completion_tokens += session.completion_tokens;
    rollup.total_reasoning_tokens += session.reasoning_tokens;
    rollup.total_tool_calls += session.tool_calls;
    rollup.total_tool_errors += session.tool_errors;
    for (name, n) in &session.tool_calls_by_name {
        *rollup.tool_calls.entry(name.clone()).or_insert(0) += n;
    }
    for (name, n) in &session.tool_errors_by_name {
        *rollup.tool_errors.entry(name.clone()).or_insert(0) += n;
    }
    if let Some(d) = session.duration_ms {
        rollup.duration_sum += d;
        rollup.duration_count += 1;
    }
    if opts.by_day {
        if let Some(first_day) = session.first_ms.map(utc_day) {
            rollup.daily.entry(first_day).or_default().sessions += 1;
        }
        for (day, ev) in &session.events_by_day {
            let b = rollup.daily.entry(day.clone()).or_default();
            b.turns += ev.turns;
            b.prompt_tokens += ev.prompt_tokens;
            b.cached_tokens += ev.cached_tokens;
            b.cache_eligible_prompt_tokens += ev.cache_eligible_prompt_tokens;
            b.completion_tokens += ev.completion_tokens;
            b.reasoning_tokens += ev.reasoning_tokens;
            b.tool_calls += ev.tool_calls;
            b.tool_errors += ev.tool_errors;
        }
        if let Some((day, d)) = session.last_ms.map(utc_day).zip(session.duration_ms) {
            let b = rollup.daily.entry(day).or_default();
            b.duration_ms = Some(b.duration_ms.unwrap_or(0) + d);
        }
    }
    if let Some(first_ms) = session.first_ms {
        match rollup.first_ms {
            None => rollup.first_ms = Some(first_ms),
            Some(cur) if first_ms < cur => rollup.first_ms = Some(first_ms),
            _ => {}
        }
    }
    if let Some(last_ms) = session.last_ms {
        match rollup.last_ms {
            None => rollup.last_ms = Some(last_ms),
            Some(cur) if last_ms > cur => rollup.last_ms = Some(last_ms),
            _ => {}
        }
    }
}

fn get_file_meta(p: &Path) -> Option<(u64, u64)> {
    let meta = fs::metadata(p).ok()?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some((size, mtime))
}

type SessionParseResult = (PathBuf, Option<(u64, u64)>, SessionAgg);

fn parse_chunk_of_sessions(paths: &[PathBuf]) -> Vec<SessionParseResult> {
    let mut results = Vec::with_capacity(paths.len());
    for path in paths {
        if !path.is_dir() {
            continue;
        }
        let events_path = path.join("events.jsonl");
        let meta = get_file_meta(&events_path);
        let Ok(raw) = fs::read_to_string(&events_path) else {
            continue;
        };
        if let Some(agg) = parse_session_events(&raw, None) {
            results.push((path.clone(), meta, agg));
        }
    }
    results
}

fn process_session_dirs_and_update(
    paths: &[PathBuf],
    opts: &StatsOptions,
    cache_sessions: &mut BTreeMap<String, CachedSession>,
    cache_dirty: &mut bool,
) -> SessionRollup {
    let mut rollup = SessionRollup::default();
    let results = parse_chunk_of_sessions(paths);
    for (dir, meta, agg) in results {
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if let Some((size, mtime)) = meta {
            cache_sessions.insert(
                name,
                CachedSession {
                    mtime_ms: mtime,
                    file_size: size,
                    is_final: agg.is_final,
                    agg: agg.clone(),
                },
            );
            *cache_dirty = true;
        }
        if let Some(cutoff) = opts.since_ms {
            match (agg.first_ms, agg.last_ms) {
                (Some(f), _) if f >= cutoff => {
                    add_session_to_rollup(&mut rollup, &agg, opts);
                }
                (Some(_), Some(l)) if l < cutoff => {}
                (Some(_), Some(_)) => {
                    let events_path = dir.join("events.jsonl");
                    if let Some(filtered) = fs::read_to_string(events_path)
                        .ok()
                        .and_then(|raw| parse_session_events(&raw, opts.since_ms))
                    {
                        add_session_to_rollup(&mut rollup, &filtered, opts);
                    }
                }
                _ => {}
            }
        } else {
            add_session_to_rollup(&mut rollup, &agg, opts);
        }
    }
    rollup
}

fn load_session_rollups_cached(
    session_dirs: &[PathBuf],
    opts: &StatsOptions,
    num_threads: usize,
    cache_sessions: &mut BTreeMap<String, CachedSession>,
) -> (SessionRollup, bool) {
    if session_dirs.is_empty() {
        return (SessionRollup::default(), false);
    }

    let mut rollup = SessionRollup::default();
    let mut to_parse: Vec<PathBuf> = Vec::new();
    let mut cache_dirty = false;

    let current_dir_names: std::collections::HashSet<String> = session_dirs
        .iter()
        .filter_map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
        .collect();
    let initial_cache_len = cache_sessions.len();
    cache_sessions.retain(|k, _| current_dir_names.contains(k));
    if cache_sessions.len() != initial_cache_len {
        cache_dirty = true;
    }

    for path in session_dirs {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if let Some(cached) = cache_sessions.get(name) {
            if cached.is_final {
                if let Some(cutoff) = opts.since_ms {
                    match (cached.agg.first_ms, cached.agg.last_ms) {
                        (Some(f), _) if f >= cutoff => {
                            add_session_to_rollup(&mut rollup, &cached.agg, opts);
                        }
                        (Some(_), Some(l)) if l < cutoff => {}
                        (Some(_), Some(_)) => {
                            to_parse.push(path.clone());
                        }
                        _ => {}
                    }
                } else {
                    add_session_to_rollup(&mut rollup, &cached.agg, opts);
                }
                continue;
            } else {
                let events_path = path.join("events.jsonl");
                if get_file_meta(&events_path) == Some((cached.file_size, cached.mtime_ms)) {
                    if let Some(cutoff) = opts.since_ms {
                        match (cached.agg.first_ms, cached.agg.last_ms) {
                            (Some(f), _) if f >= cutoff => {
                                add_session_to_rollup(&mut rollup, &cached.agg, opts);
                            }
                            (Some(_), Some(l)) if l < cutoff => {}
                            (Some(_), Some(_)) => {
                                to_parse.push(path.clone());
                            }
                            _ => {}
                        }
                    } else {
                        add_session_to_rollup(&mut rollup, &cached.agg, opts);
                    }
                    continue;
                }
            }
        }
        to_parse.push(path.clone());
    }

    if to_parse.is_empty() {
        return (rollup, cache_dirty);
    }

    let parsed_rollup = if to_parse.len() <= 4 || num_threads <= 1 {
        process_session_dirs_and_update(&to_parse, opts, cache_sessions, &mut cache_dirty)
    } else {
        let chunk_size = to_parse.len().div_ceil(num_threads);
        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(num_threads);
            for chunk in to_parse.chunks(chunk_size.max(1)) {
                handles.push(s.spawn(move || parse_chunk_of_sessions(chunk)));
            }
            let mut chunk_rollup = SessionRollup::default();
            for h in handles {
                if let Ok(results) = h.join() {
                    for (dir, meta, agg) in results {
                        let name = dir
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or_default()
                            .to_string();
                        if let Some((size, mtime)) = meta {
                            cache_sessions.insert(
                                name,
                                CachedSession {
                                    mtime_ms: mtime,
                                    file_size: size,
                                    is_final: agg.is_final,
                                    agg: agg.clone(),
                                },
                            );
                            cache_dirty = true;
                        }
                        if let Some(cutoff) = opts.since_ms {
                            match (agg.first_ms, agg.last_ms) {
                                (Some(f), _) if f >= cutoff => {
                                    add_session_to_rollup(&mut chunk_rollup, &agg, opts);
                                }
                                (Some(_), Some(l)) if l < cutoff => {}
                                (Some(_), Some(_)) => {
                                    let events_path = dir.join("events.jsonl");
                                    if let Some(filtered) = fs::read_to_string(events_path)
                                        .ok()
                                        .and_then(|raw| parse_session_events(&raw, opts.since_ms))
                                    {
                                        add_session_to_rollup(&mut chunk_rollup, &filtered, opts);
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            add_session_to_rollup(&mut chunk_rollup, &agg, opts);
                        }
                    }
                }
            }
            chunk_rollup
        })
    };

    rollup.merge(parsed_rollup);
    (rollup, cache_dirty)
}

fn load_task_metrics_cached(
    task_files: &[PathBuf],
    since_ms: Option<i128>,
    _num_threads: usize,
    cache_tasks: &mut BTreeMap<String, CachedTask>,
) -> (TaskMetrics, bool) {
    if task_files.is_empty() {
        return (TaskMetrics::default(), false);
    }
    let mut metrics = TaskMetrics::default();
    let mut cache_dirty = false;

    let current_file_names: std::collections::HashSet<String> = task_files
        .iter()
        .filter_map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
        .collect();
    let initial_cache_len = cache_tasks.len();
    cache_tasks.retain(|k, _| current_file_names.contains(k));
    if cache_tasks.len() != initial_cache_len {
        cache_dirty = true;
    }

    for path in task_files {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let (status, ts) = if let Some(cached) = cache_tasks.get(name) {
            (cached.status.clone(), cached.timestamp_ms)
        } else {
            let Ok(raw) = fs::read_to_string(path) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<FastTask>(&raw) else {
                continue;
            };
            let status = v.status.unwrap_or_default().to_string();
            let ts = v.ended_at.or(v.started_at).or(v.created_at);
            cache_tasks.insert(
                name.to_string(),
                CachedTask {
                    status: status.clone(),
                    timestamp_ms: ts,
                },
            );
            cache_dirty = true;
            (status, ts)
        };

        if let Some(cutoff) = since_ms {
            match ts {
                Some(t) if (t as i128) < cutoff => continue,
                None => continue,
                _ => {}
            }
        }

        match status.as_str() {
            "completed" => metrics.completed += 1,
            "failed" => metrics.failed += 1,
            "cancelled" => metrics.cancelled += 1,
            _ => {}
        }
    }

    (metrics, cache_dirty)
}

#[derive(Default)]
struct TaskMetrics {
    completed: u64,
    failed: u64,
    cancelled: u64,
}

/// Derive the cumulative [`Stats`] by walking the existing ledgers:
/// `sessions/*/events.jsonl` (turns, tokens, tool calls, durations,
/// first/last activity) and `tasks/*.json` (task lifecycle counts).
///
/// Missing or corrupt lines are skipped silently — this is a derivation,
/// not an authority.
pub fn derive_stats(state_dir: &Path, opts: &StatsOptions) -> Stats {
    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1);

    let sessions_dir = state_dir.join("sessions");
    let session_dirs: Vec<PathBuf> = read_dir_or_empty(&sessions_dir)
        .into_iter()
        .filter(|p| p.is_dir())
        .collect();

    let tasks_dir = state_dir.join("tasks");
    let task_files: Vec<PathBuf> = read_dir_or_empty(&tasks_dir)
        .into_iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();

    let cache_enabled = std::env::var("CASTOR_STATS_NO_CACHE").is_err();
    let cache_path = state_dir.join("telemetry").join(".stats_cache.json");
    let mut cache: TelemetryCache = if cache_enabled {
        fs::read_to_string(&cache_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    } else {
        TelemetryCache::default()
    };

    let (session_rollup, sessions_updated) =
        load_session_rollups_cached(&session_dirs, opts, num_threads, &mut cache.sessions);
    let (task_metrics, tasks_updated) =
        load_task_metrics_cached(&task_files, opts.since_ms, num_threads, &mut cache.tasks);

    if cache_enabled && (sessions_updated || tasks_updated) {
        let telemetry_dir = state_dir.join("telemetry");
        let _ = fs::create_dir_all(&telemetry_dir);
        if let Ok(raw) = serde_json::to_string(&cache) {
            let tmp = telemetry_dir.join(format!(".stats_cache.json.tmp.{}", std::process::id()));
            if fs::write(&tmp, raw).is_ok() {
                let _ = fs::rename(tmp, cache_path);
            }
        }
    }

    let mut stats = Stats {
        total_turns: session_rollup.total_turns,
        total_sessions: session_rollup.total_sessions,
        total_tasks_completed: task_metrics.completed,
        total_tasks_failed: task_metrics.failed,
        total_tasks_cancelled: task_metrics.cancelled,
        total_prompt_tokens: session_rollup.total_prompt_tokens,
        total_cached_tokens: session_rollup.total_cached_tokens,
        total_cache_eligible_prompt_tokens: session_rollup.total_cache_eligible_prompt_tokens,
        total_completion_tokens: session_rollup.total_completion_tokens,
        total_reasoning_tokens: session_rollup.total_reasoning_tokens,
        total_tool_calls: session_rollup.total_tool_calls,
        total_tool_errors: session_rollup.total_tool_errors,
        tool_calls: session_rollup.tool_calls,
        tool_errors: session_rollup.tool_errors,
        first_ms: session_rollup.first_ms,
        last_ms: session_rollup.last_ms,
        duration_sum: session_rollup.duration_sum,
        duration_count: session_rollup.duration_count,
        total_duration_ms: session_rollup.duration_sum,
        daily: if opts.by_day {
            Some(session_rollup.daily)
        } else {
            None
        },
        ..Stats::default()
    };

    // Derived averages / cost.
    stats.avg_duration_ms = (stats.duration_count > 0)
        .then(|| (stats.duration_sum as f64 / stats.duration_count as f64) * 100.0 / 100.0);
    stats.estimated_cost_saved_usd = (stats.total_prompt_tokens as f64 / 1_000_000.0)
        * PROMPT_COST_PER_MILLION
        + (stats.total_completion_tokens as f64 / 1_000_000.0) * COMPLETION_COST_PER_MILLION;
    stats.estimated_cost_saved_usd = (stats.estimated_cost_saved_usd * 100.0).round() / 100.0;

    let kw = system_power_kw();
    let rate = electricity_rate_usd();
    let hours = stats.total_duration_ms as f64 / 3_600_000.0;
    let kwh = (hours * kw * 100.0).round() / 100.0;
    stats.estimated_energy_kwh = kwh;
    stats.estimated_electricity_cost_usd = ((kwh * rate) * 100.0).round() / 100.0;
    stats.net_savings_usd =
        ((stats.estimated_cost_saved_usd - stats.estimated_electricity_cost_usd).max(0.0) * 100.0)
            .round()
            / 100.0;
    stats.electricity_rate_usd = rate;
    stats.system_power_watts = (kw * 1000.0).round() as u32;

    stats.reasoning_ratio = if stats.total_completion_tokens > 0 {
        Some(
            ((stats.total_reasoning_tokens as f64 / stats.total_completion_tokens as f64) * 100.0)
                .round()
                / 100.0,
        )
    } else {
        None
    };
    let edits = stats.tool_calls.get("edit_file").copied().unwrap_or(0);
    let writes = stats.tool_calls.get("write_file").copied().unwrap_or(0);
    stats.edit_write_ratio = if writes > 0 {
        Some(((edits as f64 / writes as f64) * 100.0).round() / 100.0)
    } else {
        None
    };

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

struct Palette {
    bold: &'static str,
    dim: &'static str,
    bright_green: &'static str,
    yellow: &'static str,
    red: &'static str,
    reset: &'static str,
}

impl Palette {
    fn new(color: bool) -> Self {
        if color {
            Self {
                bold: "\x1b[1m",
                dim: "\x1b[2m",
                bright_green: "\x1b[92m",
                yellow: "\x1b[33m",
                red: "\x1b[31m",
                reset: "\x1b[0m",
            }
        } else {
            Self {
                bold: "",
                dim: "",
                bright_green: "",
                yellow: "",
                red: "",
                reset: "",
            }
        }
    }
}

fn fmt_grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    for (i, c) in chars.into_iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn fmt_scaled(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.2}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        fmt_grouped(n)
    }
}

fn fmt_usd_grouped(amount: f64) -> String {
    let is_neg = amount < 0.0;
    let abs = amount.abs();
    let whole = abs.trunc() as u64;
    let cents = (abs.fract() * 100.0).round() as u64;
    let prefix = if is_neg { "-$" } else { "$" };
    format!("{}{}.{:02}", prefix, fmt_grouped(whole), cents)
}

fn fmt_duration_compact(ms: u64) -> String {
    let total_secs = ms / 1000;
    if total_secs >= 3600 {
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        let s = total_secs % 60;
        format!("{h}h {m:02}m {s:02}s")
    } else if total_secs >= 60 {
        let m = total_secs / 60;
        let s = total_secs % 60;
        format!("{m}m {s:02}s")
    } else {
        format!("{:.2}s", ms as f64 / 1000.0)
    }
}

fn bar_chart(count: u64, max: u64, width: usize) -> String {
    if max == 0 || count == 0 {
        return " ".repeat(width);
    }
    let ratio = count as f64 / max as f64;
    let full = (ratio * width as f64).floor() as usize;
    if full >= width {
        return "█".repeat(width);
    }
    let remainder = (ratio * width as f64) - full as f64;
    let frac = if remainder >= 0.75 {
        "▊"
    } else if remainder >= 0.5 {
        "▌"
    } else if remainder >= 0.25 {
        "▎"
    } else if full == 0 {
        "▏"
    } else {
        ""
    };
    let bar_len = full + if frac.is_empty() { 0 } else { 1 };
    let empty = width.saturating_sub(bar_len);
    format!("{}{}{}", "█".repeat(full), frac, " ".repeat(empty))
}

fn ordinal_suffix(day: u32) -> &'static str {
    match day {
        11..=13 => "th",
        _ => match day % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    }
}

/// Format an ISO-8601 UTC timestamp to a human-readable date string
/// (e.g. `4th September, 2026 22:58 UTC`).
fn fmt_human_date(iso: &str) -> String {
    use chrono::{Datelike, Timelike};
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) {
        let day = dt.day();
        let suffix = ordinal_suffix(day);
        let month = match dt.month() {
            1 => "January",
            2 => "February",
            3 => "March",
            4 => "April",
            5 => "May",
            6 => "June",
            7 => "July",
            8 => "August",
            9 => "September",
            10 => "October",
            11 => "November",
            12 => "December",
            _ => "",
        };
        let year = dt.year();
        let hour = dt.hour();
        let min = dt.minute();
        format!("{day}{suffix} {month}, {year} {hour:02}:{min:02} UTC")
    } else if let Some((date, time)) = iso.split_once('T') {
        let clean_time = time.split('.').next().unwrap_or(time).trim_end_matches('Z');
        let short_time = if clean_time.len() >= 5 {
            &clean_time[..5]
        } else {
            clean_time
        };
        format!("{date} {short_time} UTC")
    } else {
        iso.to_string()
    }
}

/// Format a high-level visual card of operational telemetry and financial savings.
pub fn format_stats_card(stats: &Stats) -> String {
    format_stats_card_styled(stats, false)
}

/// Format a high-level visual card with optional ANSI color and high-density terminal layout.
pub fn format_stats_card_styled(stats: &Stats, color: bool) -> String {
    let p = Palette::new(color);
    let mut out = String::new();

    out.push_str(&format!(
        "{}{}{}\n",
        p.bold, "CASTOR OPERATIONAL TELEMETRY", p.reset
    ));
    if let (Some(first), Some(last)) = (&stats.first_recorded_session, &stats.last_recorded_session)
    {
        out.push_str(&format!(
            "{}Horizon: {} -> {}{}\n",
            p.dim,
            fmt_human_date(first),
            fmt_human_date(last),
            p.reset
        ));
    }
    out.push('\n');

    out.push_str(&format!("{}ACTIVITY & RUNTIME{}\n", p.bold, p.reset));
    out.push_str(&format!(
        "  Turns                  {:>10}\n",
        fmt_grouped(stats.total_turns)
    ));
    out.push_str(&format!(
        "  Sessions               {:>10}\n",
        fmt_grouped(stats.total_sessions)
    ));

    let total_tasks =
        stats.total_tasks_completed + stats.total_tasks_failed + stats.total_tasks_cancelled;
    if total_tasks > 0 {
        let ok_rate = (stats.total_tasks_completed as f64 / total_tasks as f64) * 100.0;
        let fail_part = if stats.total_tasks_failed > 0 {
            format!(
                ", {}{} failed{}",
                p.red,
                fmt_grouped(stats.total_tasks_failed),
                p.reset
            )
        } else {
            String::new()
        };
        let canc_part = if stats.total_tasks_cancelled > 0 {
            format!(
                ", {}{} cancelled{}",
                p.yellow,
                fmt_grouped(stats.total_tasks_cancelled),
                p.reset
            )
        } else {
            String::new()
        };
        out.push_str(&format!(
            "  Tasks                  {:>10}  {}{:.1}% ok: {} completed{}{}{}\n",
            fmt_grouped(total_tasks),
            p.dim,
            ok_rate,
            fmt_grouped(stats.total_tasks_completed),
            fail_part,
            canc_part,
            p.reset
        ));
    } else {
        out.push_str("  Tasks                           0\n");
    }

    if stats.total_duration_ms > 0 {
        let hours = stats.total_duration_ms as f64 / 3_600_000.0;
        let avg_str = stats
            .avg_duration_ms
            .map(|ms| format!("  {} avg", fmt_duration_compact(ms.round() as u64)))
            .unwrap_or_default();
        out.push_str(&format!(
            "  Active Compute         {:>9.2}h  {}{}{}\n",
            hours,
            p.dim,
            avg_str.trim_start(),
            p.reset
        ));
    }

    if stats.total_tool_calls > 0 {
        let ok_calls = stats
            .total_tool_calls
            .saturating_sub(stats.total_tool_errors);
        let ok_rate = (ok_calls as f64 / stats.total_tool_calls as f64) * 100.0;
        let err_part = if stats.total_tool_errors > 0 {
            format!(
                ", {}{} errors{}",
                p.red,
                fmt_grouped(stats.total_tool_errors),
                p.reset
            )
        } else {
            String::new()
        };
        out.push_str(&format!(
            "  Tool Calls             {:>10}  {}{:.1}% ok{}{}\n",
            fmt_grouped(stats.total_tool_calls),
            p.dim,
            ok_rate,
            err_part,
            p.reset
        ));
    } else {
        out.push_str("  Tool Calls                      0\n");
    }
    out.push('\n');

    out.push_str(&format!(
        "{}TOKEN EFFICIENCY & DYNAMICS{}\n",
        p.bold, p.reset
    ));
    let total_tokens = stats.total_prompt_tokens + stats.total_completion_tokens;
    out.push_str(&format!(
        "  Total Processed        {:>10}\n",
        fmt_scaled(total_tokens)
    ));
    out.push_str(&format!(
        "  Prompt Tokens          {:>10}\n",
        fmt_scaled(stats.total_prompt_tokens)
    ));
    if stats.total_cached_tokens > 0 {
        let denom = if stats.total_cache_eligible_prompt_tokens > 0 {
            stats.total_cache_eligible_prompt_tokens
        } else {
            stats.total_prompt_tokens.max(1)
        };
        let hit_rate = (stats.total_cached_tokens as f64 / denom as f64) * 100.0;
        out.push_str(&format!(
            "  Cached Tokens          {:>10}  {}{:.1}% cache hit{}\n",
            fmt_scaled(stats.total_cached_tokens),
            p.dim,
            hit_rate,
            p.reset
        ));
    }
    out.push_str(&format!(
        "  Output Tokens          {:>10}\n",
        fmt_scaled(stats.total_completion_tokens)
    ));
    if stats.total_reasoning_tokens > 0 {
        let r_pct = (stats.total_reasoning_tokens as f64
            / (stats.total_completion_tokens + stats.total_reasoning_tokens).max(1) as f64)
            * 100.0;
        out.push_str(&format!(
            "  Reasoning Tokens       {:>10}  {}{:.1}% of generation{}\n",
            fmt_scaled(stats.total_reasoning_tokens),
            p.dim,
            r_pct,
            p.reset
        ));
    }
    if let Some(r) = stats.reasoning_ratio {
        out.push_str(&format!(
            "  Reasoning Ratio        {:>9.2}x  {}deliberation / output{}\n",
            r, p.dim, p.reset
        ));
    }
    if let Some(r) = stats.edit_write_ratio {
        out.push_str(&format!(
            "  Surgical Edit Ratio    {:>9.2}x  {}edits / writes{}\n",
            r, p.dim, p.reset
        ));
    }
    out.push('\n');

    out.push_str(&format!(
        "{}CLOUD ARBITRAGE & ENERGY ({} Rates){}\n",
        p.bold, stats.benchmark_model, p.reset
    ));
    out.push_str(&format!(
        "  Virtual Cloud Cost     {:>10}\n",
        fmt_usd_grouped(stats.estimated_cost_saved_usd)
    ));
    let rate = if stats.electricity_rate_usd > 0.0 {
        stats.electricity_rate_usd
    } else {
        DEFAULT_ELECTRICITY_COST_PER_KWH
    };
    let watts = if stats.system_power_watts > 0 {
        stats.system_power_watts
    } else {
        (DEFAULT_SYSTEM_POWER_KW * 1000.0).round() as u32
    };
    out.push_str(&format!(
        "  Local Power Cost (Est) {:>10}  {}{:.1} kWh @ ${:.2}/kWh, {}W{}\n",
        fmt_usd_grouped(stats.estimated_electricity_cost_usd),
        p.dim,
        stats.estimated_energy_kwh,
        rate,
        watts,
        p.reset
    ));
    let net_pct = if stats.estimated_cost_saved_usd > 0.0 {
        (stats.net_savings_usd / stats.estimated_cost_saved_usd) * 100.0
    } else {
        100.0
    };
    out.push_str(&format!(
        "  Net Savings            {}{:>10}{}  {}{:.2}% net{}\n",
        p.bright_green,
        format!("+{}", fmt_usd_grouped(stats.net_savings_usd)),
        p.reset,
        p.dim,
        net_pct,
        p.reset
    ));
    out.push('\n');

    if let Some(daily) = &stats.daily {
        out.push_str(&format!("{}DAILY BREAKDOWN (UTC){}\n", p.bold, p.reset));
        if daily.is_empty() {
            out.push_str(&format!(
                "  {}(no activity in the selected window){}\n",
                p.dim, p.reset
            ));
        } else {
            out.push_str(&format!(
                "  {}{:<12} {:>8} {:>10} {:>10} {:>10} {:>8}{}\n",
                p.dim, "DATE", "TURNS", "PROMPT", "OUTPUT", "REASONING", "TOOLS", p.reset
            ));
            for (day, b) in daily {
                out.push_str(&format!(
                    "  {:<12} {:>8} {:>10} {:>10} {:>10} {:>8}\n",
                    day,
                    fmt_grouped(b.turns),
                    fmt_scaled(b.prompt_tokens),
                    fmt_scaled(b.completion_tokens),
                    fmt_scaled(b.reasoning_tokens),
                    fmt_grouped(b.tool_calls)
                ));
            }
        }
        out.push('\n');
    }

    if !stats.tool_calls.is_empty() {
        out.push_str(&format!(
            "{}TOOL RELIABILITY & DISTRIBUTION{}\n",
            p.bold, p.reset
        ));
        out.push_str(&format!(
            "  {}{:<22} {:>8} {:>7}  {:<16} {:>16}{}\n",
            p.dim, "TOOL", "CALLS", "SHARE", "DISTRIBUTION", "ERRORS (%)", p.reset
        ));
        let mut sorted_tools: Vec<(&String, &u64)> = stats.tool_calls.iter().collect();
        sorted_tools.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let max_calls = sorted_tools.first().map(|(_, c)| **c).unwrap_or(1);

        let threshold = if sorted_tools.len() > 8 {
            8
        } else {
            sorted_tools.len()
        };
        let (head, tail) = sorted_tools.split_at(threshold);

        let format_err_col = |errs: u64, total: u64| -> String {
            if errs > 0 {
                let err_rate = (errs as f64 / total.max(1) as f64) * 100.0;
                let text = format!("{} ({:.1}%)", fmt_grouped(errs), err_rate);
                let pad = 16usize.saturating_sub(text.len());
                format!("{}{}{}{}", " ".repeat(pad), p.red, text, p.reset)
            } else {
                let pad = 15usize;
                format!("{}{}{}{}", " ".repeat(pad), p.dim, "-", p.reset)
            }
        };

        for &(ref name, &count) in head {
            let bar = bar_chart(count, max_calls, 14);
            let pct = (count as f64 / stats.total_tool_calls.max(1) as f64) * 100.0;
            let errs = stats.tool_errors.get(name.as_str()).copied().unwrap_or(0);
            out.push_str(&format!(
                "  {:<22} {:>8} {:>6.1}%  {:<16} {}\n",
                name,
                fmt_grouped(count),
                pct,
                bar,
                format_err_col(errs, count),
            ));
        }

        if !tail.is_empty() {
            let tail_count: u64 = tail.iter().map(|&(_, &c)| c).sum();
            let tail_errs: u64 = tail
                .iter()
                .map(|(n, _)| stats.tool_errors.get(n.as_str()).copied().unwrap_or(0))
                .sum();
            let tail_pct = (tail_count as f64 / stats.total_tool_calls.max(1) as f64) * 100.0;
            let bar = bar_chart(tail_count, max_calls, 14);
            let label = format!("other ({} tools)", tail.len());
            out.push_str(&format!(
                "  {:<22} {:>8} {:>6.1}%  {:<16} {}\n",
                label,
                fmt_grouped(tail_count),
                tail_pct,
                bar,
                format_err_col(tail_errs, tail_count),
            ));
        }
        out.push('\n');
    }

    out
}

/// Per-UTC-day event aggregates for one session (the per-session half of a
/// daily bucket; the session-level fields are merged in by `derive_stats`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
struct DayEvents {
    turns: u64,
    prompt_tokens: u64,
    cached_tokens: u64,
    #[serde(default)]
    cache_eligible_prompt_tokens: u64,
    completion_tokens: u64,
    reasoning_tokens: u64,
    tool_calls: u64,
    tool_errors: u64,
}

/// Per-session aggregates extracted from one `events.jsonl` ledger.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct SessionAgg {
    turns: u64,
    prompt_tokens: u64,
    cached_tokens: u64,
    #[serde(default)]
    cache_eligible_prompt_tokens: u64,
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
    #[serde(default)]
    is_final: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct CachedSession {
    mtime_ms: u64,
    file_size: u64,
    is_final: bool,
    agg: SessionAgg,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct CachedTask {
    status: String,
    timestamp_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct TelemetryCache {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    sessions: BTreeMap<String, CachedSession>,
    #[serde(default)]
    tasks: BTreeMap<String, CachedTask>,
}

#[derive(Deserialize)]
struct FastEvent<'a> {
    #[serde(borrow)]
    r#type: Option<&'a str>,
    #[serde(borrow)]
    timestamp: Option<RawTimestamp<'a>>,
    #[serde(borrow)]
    name: Option<&'a str>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    duration_ms: Option<u64>,
    metrics: Option<FastMetrics>,
    usage: Option<FastUsage>,
    prompt_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cached_prompt_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    prompt_tokens_details: Option<FastPromptDetails>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawTimestamp<'a> {
    #[serde(borrow)]
    Str(&'a str),
    Num(i64),
}

#[derive(Deserialize)]
struct FastUsage {
    prompt_tokens_details: Option<FastPromptDetails>,
}

#[derive(Deserialize)]
struct FastMetrics {
    prompt_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cached_prompt_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    prompt_tokens_details: Option<FastPromptDetails>,
    usage: Option<FastUsage>,
}

#[derive(Deserialize)]
struct FastPromptDetails {
    cached_tokens: Option<u64>,
}

impl<'a> FastEvent<'a> {
    fn tokens(&self) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
        if let Some(m) = &self.metrics {
            let prompt = m.prompt_tokens;
            let cached = m
                .cached_tokens
                .or(m.cache_read_input_tokens)
                .or(m.cached_prompt_tokens)
                .or(m.cache_read_tokens)
                .or_else(|| {
                    m.prompt_tokens_details
                        .as_ref()
                        .and_then(|d| d.cached_tokens)
                })
                .or_else(|| {
                    m.usage
                        .as_ref()
                        .and_then(|u| u.prompt_tokens_details.as_ref())
                        .and_then(|d| d.cached_tokens)
                });
            let completion = m.completion_tokens;
            let reasoning = m.reasoning_tokens;
            (prompt, cached, completion, reasoning)
        } else {
            let prompt = self.prompt_tokens;
            let cached = self
                .cached_tokens
                .or(self.cache_read_input_tokens)
                .or(self.cached_prompt_tokens)
                .or(self.cache_read_tokens)
                .or_else(|| {
                    self.prompt_tokens_details
                        .as_ref()
                        .and_then(|d| d.cached_tokens)
                })
                .or_else(|| {
                    self.usage
                        .as_ref()
                        .and_then(|u| u.prompt_tokens_details.as_ref())
                        .and_then(|d| d.cached_tokens)
                });
            let completion = self.completion_tokens;
            let reasoning = self.reasoning_tokens;
            (prompt, cached, completion, reasoning)
        }
    }
}

#[derive(Deserialize)]
struct FastTask<'a> {
    #[serde(borrow)]
    status: Option<&'a str>,
    ended_at: Option<u64>,
    started_at: Option<u64>,
    created_at: Option<u64>,
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
    let mut agg = SessionAgg::default();
    let mut any = false;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<FastEvent>(line) else {
            continue; // corrupt line: skip
        };

        let ms = parse_timestamp_ms_from_raw(ev.timestamp.as_ref());

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

        let day = ms.map(utc_day);

        match ev.r#type {
            Some("dispatch") => {
                agg.turns += 1;
                let (prompt, cached, completion, reasoning) = ev.tokens();
                let p = prompt.unwrap_or(0);
                agg.prompt_tokens += p;
                if let Some(c) = cached {
                    agg.cached_tokens += c;
                    agg.cache_eligible_prompt_tokens += p;
                }
                agg.completion_tokens += completion.unwrap_or(0);
                agg.reasoning_tokens += reasoning.unwrap_or(0);
                if let Some(day) = day {
                    let ev_day = agg.events_by_day.entry(day).or_default();
                    ev_day.turns += 1;
                    ev_day.prompt_tokens += p;
                    if let Some(c) = cached {
                        ev_day.cached_tokens += c;
                        ev_day.cache_eligible_prompt_tokens += p;
                    }
                    ev_day.completion_tokens += completion.unwrap_or(0);
                    ev_day.reasoning_tokens += reasoning.unwrap_or(0);
                }
            }
            Some("tool_result") => {
                if let Some(name) = ev.name {
                    agg.tool_calls += 1;
                    let is_err = ev.is_error.unwrap_or(false);
                    if is_err {
                        agg.tool_errors += 1;
                        *agg.tool_errors_by_name.entry(name.to_string()).or_insert(0) += 1;
                    }
                    *agg.tool_calls_by_name.entry(name.to_string()).or_insert(0) += 1;
                    if let Some(day) = day {
                        let ev_day = agg.events_by_day.entry(day).or_default();
                        ev_day.tool_calls += 1;
                        if is_err {
                            ev_day.tool_errors += 1;
                        }
                    }
                }
            }
            Some("final") => {
                agg.is_final = true;
                if let Some(d) = ev.duration_ms {
                    agg.duration_ms = Some(d);
                }
            }
            _ => {}
        }
    }

    any.then_some(agg)
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
#[cfg(test)]
fn parse_timestamp_ms(v: &Value) -> Option<i128> {
    match v {
        Value::Number(n) => n.as_i64().map(|x| x as i128),
        Value::String(s) => parse_timestamp_str(s),
        _ => None,
    }
}

fn parse_timestamp_str(s: &str) -> Option<i128> {
    let s = s.trim();
    if s.ends_with("ms") || s.parse::<i64>().is_ok() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis() as i128)
}

fn parse_timestamp_ms_from_raw(ts: Option<&RawTimestamp>) -> Option<i128> {
    match ts {
        Some(RawTimestamp::Num(n)) => Some(*n as i128),
        Some(RawTimestamp::Str(s)) => parse_timestamp_str(s),
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
        "{\"type\":\"dispatch\",\"turn\":1,\"timestamp\":\"2026-10-01T12:00:00.000Z\",\"metrics\":{\"prompt_tokens\":1000,\"cached_tokens\":400,\"completion_tokens\":200,\"reasoning_tokens\":50}}\n",
        "{\"type\":\"tool_result\",\"turn\":1,\"name\":\"bash\",\"is_error\":true,\"timestamp\":\"2026-10-01T12:00:01.000Z\"}\n",
        "{\"type\":\"final\",\"status\":\"completed\",\"duration_ms\":1000,\"timestamp\":\"2026-10-01T12:00:02.000Z\"}\n",
    );

    #[test]
    fn parse_canonical_dispatch_ledger() {
        let agg = parse_session_events(CANONICAL_LEDGER, None).expect("canonical ledger parses");
        assert_eq!(agg.turns, 1);
        assert_eq!(agg.prompt_tokens, 1000);
        assert_eq!(agg.cached_tokens, 400);
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

    #[test]
    fn formatters_produce_clean_output() {
        assert_eq!(fmt_grouped(0), "0");
        assert_eq!(fmt_grouped(42), "42");
        assert_eq!(fmt_grouped(1000), "1,000");
        assert_eq!(fmt_grouped(1526923211), "1,526,923,211");

        assert_eq!(fmt_scaled(500), "500");
        assert_eq!(fmt_scaled(15_400), "15.4k");
        assert_eq!(fmt_scaled(31_650_000), "31.65M");
        assert_eq!(fmt_scaled(1_526_923_211), "1.53B");

        assert_eq!(fmt_usd_grouped(0.0), "$0.00");
        assert_eq!(fmt_usd_grouped(3370.38), "$3,370.38");

        assert_eq!(fmt_duration_compact(450), "0.45s");
        assert_eq!(fmt_duration_compact(45_000), "45.00s");
        assert_eq!(fmt_duration_compact(244_950), "4m 04s");
        assert_eq!(fmt_duration_compact(3_725_000), "1h 02m 05s");

        assert_eq!(bar_chart(0, 100, 10), "          ");
        assert_eq!(bar_chart(50, 100, 10), "█████     ");
        assert_eq!(bar_chart(100, 100, 10), "██████████");
    }

    #[test]
    fn styled_card_handles_color_and_plain() {
        let mut stats = Stats {
            total_turns: 1200,
            total_sessions: 10,
            total_tasks_completed: 8,
            total_tasks_failed: 1,
            total_tasks_cancelled: 1,
            total_prompt_tokens: 1_500_000,
            total_completion_tokens: 250_000,
            total_reasoning_tokens: 50_000,
            total_tool_calls: 500,
            total_tool_errors: 5,
            estimated_cost_saved_usd: 42.50,
            net_savings_usd: 42.50,
            ..Default::default()
        };
        stats.tool_calls.insert("bash".to_string(), 400);
        stats.tool_calls.insert("read_file".to_string(), 100);
        stats.tool_errors.insert("bash".to_string(), 5);

        let plain = format_stats_card_styled(&stats, false);
        assert!(
            !plain.contains("\x1b["),
            "plain card must not contain ANSI escape codes"
        );
        assert!(plain.contains("CASTOR OPERATIONAL TELEMETRY"));
        assert!(plain.contains("1.50M"));
        assert!(plain.contains("bash"));
        assert!(plain.contains("read_file"));
        assert!(plain.contains("+$42.50"));

        let colored = format_stats_card_styled(&stats, true);
        assert!(
            colored.contains("\x1b["),
            "colored card must contain ANSI escape codes"
        );
        assert!(colored.contains("CASTOR OPERATIONAL TELEMETRY"));
        assert!(colored.contains("+$42.50"));
    }

    #[test]
    fn styled_card_displays_cached_tokens_and_power_rate() {
        let stats = Stats {
            total_turns: 50,
            total_sessions: 2,
            total_tasks_completed: 2,
            total_prompt_tokens: 10_000_000,
            total_cache_eligible_prompt_tokens: 1_000_000,
            total_cached_tokens: 900_000,
            total_completion_tokens: 100_000,
            estimated_electricity_cost_usd: 1.25,
            electricity_rate_usd: 0.18,
            system_power_watts: 250,
            estimated_energy_kwh: 6.94,
            ..Default::default()
        };

        let card = format_stats_card_styled(&stats, false);
        assert!(
            card.contains("900.0k") && card.contains("90.0% cache hit"),
            "card should calculate cache hit percentage against eligible tokens: {card}"
        );
        assert!(
            card.contains("@ $0.18/kWh, 250W"),
            "card should display configured electricity rate and 250W system power: {card}"
        );
    }

    #[test]
    fn telemetry_cache_roundtrip() {
        let state = tmp_state();
        let cache_path = state.join(".stats_cache.json");

        let mut cache = TelemetryCache::default();
        let agg = SessionAgg {
            turns: 10,
            prompt_tokens: 5000,
            cached_tokens: 4500,
            is_final: true,
            ..Default::default()
        };
        let entry = CachedSession {
            mtime_ms: 123456789,
            file_size: 4096,
            is_final: true,
            agg,
        };
        cache.sessions.insert("session_alpha".to_string(), entry);

        let json = serde_json::to_string(&cache).expect("serialize");
        fs::write(&cache_path, &json).expect("write");

        let loaded: TelemetryCache = fs::read_to_string(&cache_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .expect("deserialize");

        assert_eq!(loaded.sessions.len(), 1);
        let loaded_entry = loaded.sessions.get("session_alpha").unwrap();
        assert_eq!(loaded_entry.agg.turns, 10);
        assert_eq!(loaded_entry.agg.cached_tokens, 4500);
        assert!(loaded_entry.is_final);

        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn telemetry_cache_speeds_up_derive_stats() {
        let state = tmp_state();
        write(
            &state.join("sessions/s1/events.jsonl"),
            concat!(
                "{\"type\":\"dispatch\",\"turn\":1,\"timestamp\":\"2026-10-06T10:00:00.000Z\",\"metrics\":{\"prompt_tokens\":1000,\"cached_tokens\":900,\"completion_tokens\":100}}\n",
                "{\"type\":\"final\",\"status\":\"completed\",\"duration_ms\":500,\"timestamp\":\"2026-10-06T10:00:01.000Z\"}\n"
            ),
        );
        write(
            &state.join("tasks/task_1.json"),
            "{\"id\":\"task_1\",\"status\":\"completed\",\"ended_at\":1760000000000}",
        );

        // First derivation: cold, populates cache
        let s1 = derive_stats(&state, &Default::default());
        assert_eq!(s1.total_sessions, 1);
        assert_eq!(s1.total_prompt_tokens, 1000);
        assert_eq!(s1.total_cached_tokens, 900);
        assert_eq!(s1.total_cache_eligible_prompt_tokens, 1000);
        assert_eq!(s1.total_tasks_completed, 1);

        // Verify cache file was written
        let cache_file = state.join("telemetry/.stats_cache.json");
        assert!(
            cache_file.exists(),
            "cache file should be written to telemetry/.stats_cache.json"
        );

        // Second derivation: warm, uses cached entry
        let s2 = derive_stats(&state, &Default::default());
        assert_eq!(s2.total_sessions, s1.total_sessions);
        assert_eq!(s2.total_prompt_tokens, s1.total_prompt_tokens);
        assert_eq!(s2.total_cached_tokens, s1.total_cached_tokens);
        assert_eq!(
            s2.total_cache_eligible_prompt_tokens,
            s1.total_cache_eligible_prompt_tokens
        );
        assert_eq!(s2.total_tasks_completed, s1.total_tasks_completed);

        let _ = fs::remove_dir_all(&state);
    }
}
