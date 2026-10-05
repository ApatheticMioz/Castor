//! Watchdog: stall detection for the evo lineage.
//!
//! Ported from `mcp-castor/src/harness/evo/watchdog.js` — only the
//! detection rule is ported (stagnation = no fitness improvement).
//! The JS version tracks consecutive rejections; the Rust version
//! adapts this to the lineage context:
//! - Stalled when the newest commit is older than 7 days, OR
//! - Stalled when the last 5 commits show no fitness improvement.

use crate::evo::lineage::{Lineage, LineageNode};

/// The watchdog verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogVerdict {
    /// The lineage is empty (no commits yet).
    Empty,
    /// The lineage is healthy (not stalled).
    Healthy,
    /// The lineage is stalled: the newest commit is older than 7 days.
    StalledOld,
    /// The lineage is stalled: the last 5 commits show no fitness improvement.
    StalledNoImprovement,
}

impl WatchdogVerdict {
    /// Whether the verdict indicates a stall.
    pub fn is_stalled(&self) -> bool {
        matches!(self, Self::StalledOld | Self::StalledNoImprovement)
    }
}

/// The number of days after which a commit is considered "old".
const STALL_DAYS: u64 = 7;
/// The number of recent commits to check for fitness improvement.
const IMPROVEMENT_WINDOW: usize = 5;

/// Evaluate the watchdog verdict for a lineage.
///
/// - Empty lineage → `Empty`
/// - Newest commit older than 7 days → `StalledOld`
/// - Last 5 commits show no fitness improvement → `StalledNoImprovement`
/// - Otherwise → `Healthy`
pub fn evaluate(lineage: &Lineage, now_ms: u64) -> WatchdogVerdict {
    let nodes = lineage.nodes();
    if nodes.is_empty() {
        return WatchdogVerdict::Empty;
    }

    // Check if the newest commit is older than 7 days.
    let newest = &nodes[nodes.len() - 1];
    if let Some(newest_ms) = parse_iso8601_to_ms(&newest.created_at) {
        let age_ms = now_ms.saturating_sub(newest_ms);
        if age_ms >= STALL_DAYS * 86_400_000 {
            return WatchdogVerdict::StalledOld;
        }
    }

    // Check if the last 5 commits show no fitness improvement.
    let window: Vec<&LineageNode> = nodes.iter().rev().take(IMPROVEMENT_WINDOW).collect();
    let scored: Vec<&LineageNode> = window
        .iter()
        .rev()
        .filter(|n| n.fitness.is_some())
        .copied()
        .collect();

    if scored.len() >= 2 {
        let oldest_fitness = scored[0].fitness.unwrap();
        let newest_fitness = scored[scored.len() - 1].fitness.unwrap();
        if newest_fitness <= oldest_fitness {
            return WatchdogVerdict::StalledNoImprovement;
        }
    }

    WatchdogVerdict::Healthy
}

/// Parse an ISO-8601 UTC string to epoch milliseconds.
///
/// Expected format: `YYYY-MM-DDTHH:MM:SS.mmmZ` (as produced by
/// [`crate::evo::optimizer::iso8601_utc`]).
fn parse_iso8601_to_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.len() < 20 {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: i64 = s[5..7].parse().ok()?;
    let day: i64 = s[8..10].parse().ok()?;
    let hour: i64 = s[11..13].parse().ok()?;
    let min: i64 = s[14..16].parse().ok()?;
    let sec: i64 = s[17..19].parse().ok()?;
    let millis: i64 = if s.len() > 20 && s.as_bytes()[19] == b'.' {
        s[20..23].parse().ok()?
    } else {
        0
    };

    let days = civil_to_days(year, month, day);
    let secs = days * 86400 + hour * 3600 + min * 60 + sec;
    Some((secs * 1000 + millis) as u64)
}

/// Convert a civil date to days since 1970-01-01 (inverse of Hinnant's
/// `civil_from_days` used in `iso8601_utc`).
fn civil_to_days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, parents: &[&str], fitness: Option<f64>, created: &str) -> LineageNode {
        LineageNode {
            id: id.into(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            fitness,
            artifact_ref: format!("artifact://{id}"),
            created_at: created.into(),
        }
    }

    #[test]
    fn empty_lineage_is_empty() {
        let lin = Lineage::default();
        assert_eq!(evaluate(&lin, 1_000_000), WatchdogVerdict::Empty);
    }

    #[test]
    fn old_commit_is_stalled() {
        let mut lin = Lineage::default();
        // A commit from 2020-01-01 (long ago).
        lin.add_node(node("a", &[], Some(1.0), "2020-01-01T00:00:00.000Z"))
            .unwrap();
        // Now is 2026-01-01.
        let now_ms = 1_767_225_600_000;
        assert_eq!(evaluate(&lin, now_ms), WatchdogVerdict::StalledOld);
    }

    #[test]
    fn no_improvement_is_stalled() {
        let mut lin = Lineage::default();
        // 5 commits with the same fitness (no improvement).
        for i in 0..5 {
            let id = format!("n{i}");
            let parents: Vec<String> = if i == 0 {
                vec![]
            } else {
                vec![format!("n{}", i - 1)]
            };
            let created = format!("2026-01-0{}T00:00:00.000Z", i + 1);
            let parents_refs: Vec<&str> = parents.iter().map(|s| s.as_str()).collect();
            lin.add_node(node(&id, &parents_refs, Some(0.5), &created))
                .unwrap();
        }
        // Now is 2026-01-06 (recent, so not StalledOld).
        let now_ms = 1_767_225_600_000 + 5 * 86_400_000;
        assert_eq!(
            evaluate(&lin, now_ms),
            WatchdogVerdict::StalledNoImprovement
        );
    }

    #[test]
    fn improving_is_healthy() {
        let mut lin = Lineage::default();
        // 5 commits with increasing fitness.
        for i in 0..5 {
            let id = format!("n{i}");
            let parents: Vec<String> = if i == 0 {
                vec![]
            } else {
                vec![format!("n{}", i - 1)]
            };
            let created = format!("2026-01-0{}T00:00:00.000Z", i + 1);
            let fitness = 0.1 * (i as f64 + 1.0);
            let parents_refs: Vec<&str> = parents.iter().map(|s| s.as_str()).collect();
            lin.add_node(node(&id, &parents_refs, Some(fitness), &created))
                .unwrap();
        }
        let now_ms = 1_767_225_600_000 + 5 * 86_400_000;
        assert_eq!(evaluate(&lin, now_ms), WatchdogVerdict::Healthy);
    }

    #[test]
    fn parse_iso8601_roundtrip() {
        // 1970-01-01T00:00:00.000Z → 0
        assert_eq!(parse_iso8601_to_ms("1970-01-01T00:00:00.000Z"), Some(0));
        // 2026-01-01T00:00:00.000Z → 1_767_225_600_000
        assert_eq!(
            parse_iso8601_to_ms("2026-01-01T00:00:00.000Z"),
            Some(1_767_225_600_000)
        );
    }
}
