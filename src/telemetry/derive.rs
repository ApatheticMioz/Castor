//! Parse a single session's `events.jsonl` ledger into per-session
//! aggregates, and fan that parse out across the session directories in
//! parallel chunks (the read-only projection half of the derivation).
//!
//! The parser reads the **canonical** schema only; malformed lines are
//! skipped silently (derivation, not authority).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::Value;

use super::rollup::{
    add_session_to_rollup, CachedSession, SessionRollup,
};
use super::StatsOptions;

/// Per-UTC-day event aggregates for one session (the per-session half of a
/// daily bucket; the session-level fields are merged in by `derive_stats`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(super) struct DayEvents {
    pub(super) turns: u64,
    pub(super) prompt_tokens: u64,
    pub(super) cached_tokens: u64,
    #[serde(default)]
    pub(super) cache_eligible_prompt_tokens: u64,
    pub(super) completion_tokens: u64,
    pub(super) reasoning_tokens: u64,
    pub(super) tool_calls: u64,
    pub(super) tool_errors: u64,
}

/// Per-session aggregates extracted from one `events.jsonl` ledger.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub(super) struct SessionAgg {
    pub(super) turns: u64,
    pub(super) prompt_tokens: u64,
    pub(super) cached_tokens: u64,
    #[serde(default)]
    pub(super) cache_eligible_prompt_tokens: u64,
    pub(super) completion_tokens: u64,
    pub(super) reasoning_tokens: u64,
    pub(super) tool_calls: u64,
    pub(super) tool_errors: u64,
    pub(super) tool_calls_by_name: BTreeMap<String, u64>,
    pub(super) tool_errors_by_name: BTreeMap<String, u64>,
    pub(super) duration_ms: Option<u64>,
    /// Earliest event timestamp as epoch milliseconds (UTC-normalized).
    pub(super) first_ms: Option<i128>,
    /// Latest event timestamp as epoch milliseconds (UTC-normalized).
    pub(super) last_ms: Option<i128>,
    /// Per-UTC-day event aggregates (civil date `YYYY-MM-DD` → events).
    pub(super) events_by_day: BTreeMap<String, DayEvents>,
    #[serde(default)]
    pub(super) is_final: bool,
    #[serde(default)]
    pub(super) decode_duration_ms: f64,
    #[serde(default)]
    pub(super) decode_tokens: u64,
    #[serde(default)]
    pub(super) peak_decode_tps: Option<f64>,
    #[serde(default)]
    pub(super) ttft_sum_ms: f64,
    #[serde(default)]
    pub(super) ttft_count: u64,
    #[serde(default)]
    pub(super) prefill_prompt_tokens: u64,
    #[serde(default)]
    pub(super) max_prompt_tokens: u64,
    #[serde(default)]
    pub(super) max_reasoning_tokens: u64,
    /// Fastest TTFT (ms) observed in this session (best prefill latency).
    #[serde(default)]
    pub(super) min_ttft_ms: Option<f64>,
    /// Real GPU model-compute time (ms): the sum of `ttft + decode` across all
    /// qualifying turns. Tracked per turn regardless of whether the session
    /// ever wrote a `final` event, so a crashed session's compute time is not
    /// dropped to zero.
    #[serde(default)]
    pub(super) inference_compute_ms: f64,
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
    tokens_per_sec: Option<f64>,
    ttft_ms: Option<f64>,
    total_ms: Option<f64>,
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
    tokens_per_sec: Option<f64>,
    ttft_ms: Option<f64>,
    total_ms: Option<f64>,
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
    fn timing(&self) -> (Option<f64>, Option<f64>, Option<f64>) {
        if let Some(m) = &self.metrics {
            (
                m.ttft_ms.or(self.ttft_ms),
                m.total_ms.or(self.total_ms),
                m.tokens_per_sec.or(self.tokens_per_sec),
            )
        } else {
            (self.ttft_ms, self.total_ms, self.tokens_per_sec)
        }
    }

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
pub(super) struct FastTask<'a> {
    #[serde(borrow)]
    pub(super) status: Option<&'a str>,
    pub(super) ended_at: Option<u64>,
    pub(super) started_at: Option<u64>,
    pub(super) created_at: Option<u64>,
}

impl FastTask<'_> {
    /// Task execution duration in ms (`ended_at - started_at`), `None` when
    /// either bound is missing or the interval is degenerate.
    pub(super) fn execution_ms(&self) -> Option<u64> {
        let start = self.started_at?;
        let end = self.ended_at?;
        end.checked_sub(start)
    }
}

/// Format an epoch-millisecond instant as a UTC civil date string
/// (`YYYY-MM-DD`). All calendar math is delegated to `chrono` (`DateTime` is
/// UTC by construction).
pub(super) fn utc_day(ms: i128) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".to_string())
}

/// List a directory's entries, returning an empty vec when the directory is
/// missing or unreadable (a missing ledger dir is a valid state).
pub(super) fn read_dir_or_empty(dir: &Path) -> Vec<PathBuf> {
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
/// The parser reads the **canonical** schema only:
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
pub(super) fn parse_session_events(raw: &str, since: Option<i128>) -> Option<SessionAgg> {
    let mut agg = SessionAgg::default();
    let mut any = false;
    let mut prev_prompt_tokens: u64 = 0;

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
                let comp = completion.unwrap_or(0);
                agg.prompt_tokens += p;
                let c_tok = cached.or_else(|| {
                    if prev_prompt_tokens > 0 {
                        Some(prev_prompt_tokens.min(p))
                    } else if p > 131_072 {
                        // Default prefix cache ratio for large initial dispatches (>128k)
                        Some(((p as f64) * 0.9390).round() as u64)
                    } else {
                        None
                    }
                });
                if let Some(c) = c_tok {
                    agg.cached_tokens += c;
                    agg.cache_eligible_prompt_tokens += p;
                }
                prev_prompt_tokens = p;
                agg.completion_tokens += comp;
                let r = reasoning.unwrap_or(0);
                agg.reasoning_tokens += r;
                agg.max_prompt_tokens = agg.max_prompt_tokens.max(p);
                agg.max_reasoning_tokens = agg.max_reasoning_tokens.max(r);

                let (ttft_opt, total_opt, _) = ev.timing();
                if let Some(ttft) = ttft_opt
                    && ttft >= 0.0
                {
                    // A 0.0 ttft is a "not measured" sentinel, not a real
                    // prefill observation — it must not inflate the TTFT
                    // count/sum (which would bias the average) nor contribute
                    // prompt tokens into the prefill-throughput denominator.
                    if ttft > 0.0 {
                        agg.ttft_sum_ms += ttft;
                        agg.ttft_count += 1;
                        agg.min_ttft_ms = Some(agg.min_ttft_ms.map_or(ttft, |cur| cur.min(ttft)));
                        if p > 0 {
                            agg.prefill_prompt_tokens += p;
                        }
                    }
                    if let Some(total) = total_opt
                        && total > ttft
                    {
                        let decode_ms = total - ttft;
                        if decode_ms >= 50.0 && comp > 0 {
                            let turn_tps = (comp as f64) / (decode_ms / 1000.0);
                            // Generous ceiling: admit high-acceptance DFlash2
                            // speculative-decode bursts (hundreds of t/s)
                            // instead of capping them at the 250 t/s that
                            // used to silently drop them from the peak/sustained
                            // decode metrics; the `decode_ms >= 50.0` guard
                            // above still filters zero-duration division
                            // artifacts.
                            if turn_tps <= 2000.0 {
                                agg.decode_duration_ms += decode_ms;
                                agg.decode_tokens += comp;
                                // Real GPU compute time: prefill (TTFT) +
                                // decode for this turn, tracked even for
                                // sessions that never write a `final` event.
                                if ttft > 0.0 {
                                    agg.inference_compute_ms += ttft + decode_ms;
                                }
                                if comp >= 4 && decode_ms >= 80.0 {
                                    agg.peak_decode_tps = Some(
                                        agg.peak_decode_tps
                                            .map_or(turn_tps, |cur| cur.max(turn_tps)),
                                    );
                                }
                            }
                        }
                    }
                }

                if let Some(day) = day {
                    let ev_day = agg.events_by_day.entry(day).or_default();
                    ev_day.turns += 1;
                    ev_day.prompt_tokens += p;
                    if let Some(c) = c_tok {
                        ev_day.cached_tokens += c;
                        ev_day.cache_eligible_prompt_tokens += p;
                    }
                    ev_day.completion_tokens += comp;
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
/// string, an unrecognized suffix, or malformed text — yields `None`; the event
/// still counts but carries no timestamp for windowing.
#[cfg(test)]
pub(super) fn parse_timestamp_ms(v: &Value) -> Option<i128> {
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

pub(super) fn get_file_meta(p: &Path) -> Option<(u64, u64)> {
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

pub(super) type SessionParseResult = (PathBuf, Option<(u64, u64)>, SessionAgg);

pub(super) fn parse_chunk_of_sessions(paths: &[PathBuf]) -> Vec<SessionParseResult> {
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

pub(super) fn process_session_dirs_and_update(
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// Regression guard (#18): a `ttft_ms` of exactly `0.0` is a "not
    /// measured" sentinel and must NOT collapse `min_ttft_ms` (and the average)
    /// to zero. Only strictly-positive TTFTs are admitted to the min.
    #[test]
    fn min_ttft_ignores_zero_sentinel() {
        let ledger = concat!(
            // First dispatch has no measured TTFT (0.0 sentinel); it must not
            // drive the min to zero.
            "{\"type\":\"dispatch\",\"turn\":1,\"timestamp\":\"2026-10-01T12:00:00.000Z\",\"metrics\":{\"prompt_tokens\":1000,\"ttft_ms\":0.0}}\n",
            // Second dispatch has a real 250ms TTFT.
            "{\"type\":\"dispatch\",\"turn\":2,\"timestamp\":\"2026-10-01T12:00:01.000Z\",\"metrics\":{\"prompt_tokens\":1000,\"ttft_ms\":250.0}}\n",
            // Third dispatch has a 100ms TTFT — the true min.
            "{\"type\":\"dispatch\",\"turn\":3,\"timestamp\":\"2026-10-01T12:00:02.000Z\",\"metrics\":{\"prompt_tokens\":1000,\"ttft_ms\":100.0}}\n",
            // Terminal so the session is finalized.
            "{\"type\":\"final\",\"status\":\"completed\",\"duration_ms\":1000,\"timestamp\":\"2026-10-01T12:00:03.000Z\"}\n",
        );
        let agg = parse_session_events(ledger, None).expect("ledger parses");
        // Only the two strictly-positive TTFTs count as observations; the
        // 0.0 sentinel is a "not measured" marker, not a real prefill.
        assert_eq!(agg.ttft_count, 2);
        // But the min must be 100.0, not the 0.0 sentinel.
        assert_eq!(agg.min_ttft_ms, Some(100.0));

        // A session with ONLY 0.0 sentinels yields no TTFT observations at
        // all.
        let only_zero = concat!(
            "{\"type\":\"dispatch\",\"turn\":1,\"timestamp\":\"2026-10-01T12:00:00.000Z\",\"metrics\":{\"prompt_tokens\":1000,\"ttft_ms\":0.0}}\n",
            "{\"type\":\"final\",\"status\":\"completed\",\"duration_ms\":1000,\"timestamp\":\"2026-10-01T12:00:01.000Z\"}\n",
        );
        let agg0 = parse_session_events(only_zero, None).expect("ledger parses");
        assert_eq!(agg0.ttft_count, 0);
        assert_eq!(agg0.ttft_sum_ms, 0.0);
        assert_eq!(agg0.min_ttft_ms, None);
    }

    /// Regression guard: a ledger carrying non-canonical schema spellings
    /// (`assistant_message` turns, `toolName`/`isError` tool fields,
    /// `session_end` terminal, and `…ms` epoch-string timestamps) is
    /// read as an empty session — only canonical schema fields are parsed.
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
        // Epoch-millis string with `ms` suffix → rejected.
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
}
