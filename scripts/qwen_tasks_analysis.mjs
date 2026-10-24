#!/usr/bin/env node
/**
 * qwen_tasks_analysis.mjs — Telemetry exporter for Qwen task dispatches.
 *
 * Exports per-dispatch telemetry rows from on-disk session logs and task records.
 *
 * INPUTS
 *   --tasks-dir    Directory of per-dispatch task files:  task_<id>.json
 *                  (taskId/id, sessionId, cwd, prompt, createdAt, startedAt,
 *                   finishedAt, status, done, isError, reasoningEffort,
 *                   toolCallsCount, fileOps, ...)
 *   --sessions-dir Directory of per-session event logs:   <sessionId>/events.jsonl
 *                  (per-turn events with metrics{promptTokens, ttftMs, totalMs,
 *                   reasoningTokens, completionTokens, tokensPerSec}, plus
 *                   tool_call / continuation_injected / empty_stream_retry /
 *                   engine_empty_response / session_end events)
 *   --out          Output JSON file (array of per-dispatch rows).
 *
 * OUTPUT SCHEMA (25 fields, fixed order):
 *   taskId, sessionId, cwd, createdAt, durationSec, status, isError,
 *   endStatus, promptLen, prompt, toolCallsCount, tools, totalBash,
 *   bashCommandsSummary, filesWritten, filesReadCount, turns, promptTokens,
 *   completionTokens, thinkingTokens, ttftMedMs, rateAvgTokS, continuations,
 *   emptyStreamRetries, engineEmptyResponses
 *
 * METRICS SEMANTICS:
 *   1) promptTokens  = MAX of per-turn metrics.promptTokens across the
 *      session's events (the final context depth). When the engine does not
 *      report a non-zero promptTokens (or if no events are recorded)
 *      the value is null, distinguishing "unknown" from "zero".
 *   2) Session-cumulative fields — turns, completionTokens, thinkingTokens,
 *      tools, totalBash, ttftMedMs, continuations, emptyStreamRetries,
 *      engineEmptyResponses — are stamped ONLY on the LAST dispatch row of
 *      each session (the most recent createdAt); every other row of that
 *      session carries null to prevent accidental double-counting.
 *   3) rateAvgTokS   = mean per-turn tokens/sec. When the engine's
 *      metrics.tokensPerSec is absent or 0 for a turn, the rate is derived as
 *      completionTokens / ((totalMs - ttftMs) / 1000) (generation time only,
 *      excluding time-to-first-token). The schema has no dedicated flag, so
 *      this derivation is documented in scripts/README.md.
 *
 * Per-dispatch fields (taskId, sessionId, cwd, createdAt, durationSec, status,
 * isError, promptLen, prompt, toolCallsCount) come from the task file and are
 * present on every row. Session-level descriptive fields (endStatus,
 * bashCommandsSummary, filesWritten, filesReadCount, promptTokens,
 * rateAvgTokS) are computed from the session's events and are present on every
 * row of that session (they are state/depth, not cumulative sums, so they do
 * not double-count).
 *
 * Node >= 22, ESM, no dependencies.
 *
 * Usage:
 *   node scripts/qwen_tasks_analysis.mjs \
 *     --tasks-dir    C:/Users/Apath/.qwen/tasks \
 *     --sessions-dir C:/Users/Apath/.qwen/sessions \
 *     --out          docs/audits/telemetry/qwen_tasks_analysis_<date>.json
 */

import fs from "node:fs";
import path from "node:path";

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const args = { tasksDir: null, sessionsDir: null, out: null };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--tasks-dir") args.tasksDir = argv[++i];
    else if (a === "--sessions-dir") args.sessionsDir = argv[++i];
    else if (a === "--out") args.out = argv[++i];
    else if (a === "--help" || a === "-h") {
      process.stderr.write(
        "Usage: node qwen_tasks_analysis.mjs --tasks-dir <dir> --sessions-dir <dir> --out <file>\n"
      );
      process.exit(0);
    }
  }
  return args;
}

// ---------------------------------------------------------------------------
// Small IO helpers
// ---------------------------------------------------------------------------

/** Read + parse a JSON file; returns null on any failure. */
function readJson(p) {
  try {
    return JSON.parse(fs.readFileSync(p, "utf8"));
  } catch {
    return null;
  }
}

/**
 * Read an events.jsonl file; returns {events, skipped} or null when the file
 * is missing/unreadable. Malformed lines are skipped (counted).
 */
function readEvents(p) {
  let raw;
  try {
    raw = fs.readFileSync(p, "utf8");
  } catch {
    return null;
  }
  const events = [];
  let skipped = 0;
  for (const line of raw.split("\n")) {
    if (!line.trim()) continue;
    try {
      events.push(JSON.parse(line));
    } catch {
      skipped++;
    }
  }
  return { events, skipped };
}

// ---------------------------------------------------------------------------
// Session-level metric computation
// ---------------------------------------------------------------------------

/**
 * Compute the session-level metrics from a session's events.
 *
 * Returns null when there are no usable events (so callers can emit null for
 * every session-derived field — "unknown", not "zero").
 */
function computeSessionMetrics(events) {
  if (!events || events.length === 0) return null;

  const am = events.filter((e) => e.type === "assistant_message");
  const turns = am.length;

  // (1) promptTokens = MAX per-turn promptTokens (final context depth).
  //     Never 0: if the engine never reported a non-zero value, it is null.
  const pts = am
    .map((e) => (e.metrics && e.metrics.promptTokens) || 0)
    .filter((x) => x > 0);
  const promptTokens = pts.length ? Math.max(...pts) : null;

  // (2) Session-cumulative token sums.
  const completionTokens = am.reduce(
    (s, e) => s + ((e.metrics && e.metrics.completionTokens) || 0),
    0
  );
  const thinkingTokens = am.reduce(
    (s, e) => s + ((e.metrics && e.metrics.reasoningTokens) || 0),
    0
  );

  // ttftMedMs = median of non-zero ttftMs (null when none).
  const ttfts = am
    .map((e) => (e.metrics && e.metrics.ttftMs) || 0)
    .filter((x) => x > 0)
    .sort((a, b) => a - b);
  const ttftMedMs = ttfts.length ? ttfts[Math.floor(ttfts.length / 2)] : null;

  // (3) rateAvgTokS = mean per-turn tokens/sec, with honest derivation.
  //     Use the engine's tokensPerSec when present (>0); otherwise derive it
  //     from generation time = (totalMs - ttftMs) and completionTokens.
  const tpsValues = [];
  for (const e of am) {
    const m = e.metrics || {};
    if (m.tokensPerSec > 0) {
      tpsValues.push(m.tokensPerSec);
    } else {
      const genMs = (m.totalMs || 0) - (m.ttftMs || 0);
      if (genMs > 0 && m.completionTokens > 0) {
        tpsValues.push(m.completionTokens / (genMs / 1000));
      }
    }
  }
  const rateAvgTokS = tpsValues.length
    ? Math.round((tpsValues.reduce((a, b) => a + b, 0) / tpsValues.length) * 10) / 10
    : null;

  // Tool-call breakdown by name (session-cumulative).
  const tools = {};
  for (const e of events) {
    if (e.type === "tool_call" && e.name) {
      tools[e.name] = (tools[e.name] || 0) + 1;
    }
  }
  const totalBash = tools.bash || 0;

  // Recovery / anomaly event counts (session-cumulative).
  const continuations = events.filter((e) => e.type === "continuation_injected").length;
  const emptyStreamRetries = events.filter((e) => e.type === "empty_stream_retry").length;
  const engineEmptyResponses = events.filter((e) => e.type === "engine_empty_response").length;

  // Session-level descriptive fields (present on every row of the session).
  const ends = events.filter((e) => e.type === "session_end");
  const endStatus = ends.length ? ends[ends.length - 1].status : null;

  const bashCommandsSummary = events
    .filter((e) => e.type === "tool_call" && e.name === "bash")
    .map((e) => (e.args && e.args.command) || "")
    .filter((c) => c.length > 0);

  const filesWritten = events
    .filter((e) => e.type === "tool_call" && e.name === "write_file")
    .map((e) => (e.args && e.args.path) || "")
    .filter((p) => p.length > 0);

  const readPaths = new Set(
    events
      .filter((e) => e.type === "tool_call" && e.name === "read_file")
      .map((e) => (e.args && e.args.path) || "")
      .filter((p) => p.length > 0)
  );
  const filesReadCount = readPaths.size;

  return {
    turns,
    promptTokens,
    completionTokens,
    thinkingTokens,
    ttftMedMs,
    rateAvgTokS,
    tools,
    totalBash,
    continuations,
    emptyStreamRetries,
    engineEmptyResponses,
    endStatus,
    bashCommandsSummary,
    filesWritten,
    filesReadCount,
  };
}

// ---------------------------------------------------------------------------
// Row building
// ---------------------------------------------------------------------------

/**
 * Build one output row for a dispatch.
 *
 * @param {object} task          The parsed task file.
 * @param {object|null} sm       Session metrics (null when no events).
 * @param {boolean} isLast       True when this is the session's LAST dispatch
 *                               (most recent createdAt) — the only row that
 *                               carries the session-cumulative fields.
 */
function buildRow(task, sm, isLast) {
  const createdAt =
    task.createdAt != null ? new Date(task.createdAt).toISOString() : null;
  const durationSec =
    task.finishedAt != null && task.startedAt != null
      ? Math.round((task.finishedAt - task.startedAt) / 1000)
      : null;

  // Session-cumulative fields: only on the last row, and only when we have
  // session data. Otherwise null (never 0) so downstream sums don't
  // double-count and "unknown" stays distinct from "zero".
  const cum = (field) => (isLast && sm ? sm[field] : null);

  return {
    // Per-dispatch (from the task file) — every row.
    taskId: task.id ?? task.taskId ?? null,
    sessionId: task.sessionId ?? null,
    cwd: task.cwd ?? null,
    createdAt,
    durationSec,
    status: task.status ?? null,
    isError: task.isError ?? false,
    // Session-level descriptive — every row of the session.
    endStatus: sm ? sm.endStatus : null,
    promptLen: (task.prompt || "").length,
    prompt: task.prompt || "",
    toolCallsCount: task.toolCallsCount ?? 0,
    // Session-cumulative — LAST row only.
    tools: cum("tools"),
    totalBash: cum("totalBash"),
    // Session-level descriptive — every row of the session.
    bashCommandsSummary: sm ? sm.bashCommandsSummary : null,
    filesWritten: sm ? sm.filesWritten : null,
    filesReadCount: sm ? sm.filesReadCount : null,
    // Session-cumulative — LAST row only.
    turns: cum("turns"),
    // (1) promptTokens = max per-turn (final context depth); null when unknown.
    //     Session-level descriptive — every row of the session.
    promptTokens: sm ? sm.promptTokens : null,
    // Session-cumulative — LAST row only.
    completionTokens: cum("completionTokens"),
    thinkingTokens: cum("thinkingTokens"),
    ttftMedMs: cum("ttftMedMs"),
    // (3) rateAvgTokS with honest derivation — session-level, every row.
    rateAvgTokS: sm ? sm.rateAvgTokS : null,
    // Session-cumulative — LAST row only.
    continuations: cum("continuations"),
    emptyStreamRetries: cum("emptyStreamRetries"),
    engineEmptyResponses: cum("engineEmptyResponses"),
  };
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

function main() {
  const args = parseArgs(process.argv.slice(2));
  if (!args.tasksDir || !args.sessionsDir || !args.out) {
    process.stderr.write(
      "Usage: node qwen_tasks_analysis.mjs --tasks-dir <dir> --sessions-dir <dir> --out <file>\n"
    );
    process.exit(1);
  }

  // Read all per-dispatch task files.
  let entries = [];
  try {
    entries = fs.readdirSync(args.tasksDir);
  } catch (e) {
    process.stderr.write(`error: cannot read tasks dir ${args.tasksDir}: ${e.message}\n`);
    process.exit(1);
  }
  const tasks = [];
  for (const f of entries) {
    if (!f.startsWith("task_") || !f.endsWith(".json")) continue;
    const t = readJson(path.join(args.tasksDir, f));
    if (t && (t.id || t.taskId)) tasks.push(t);
  }

  // Group by session.
  const bySession = new Map();
  for (const t of tasks) {
    const sid = t.sessionId;
    if (!bySession.has(sid)) bySession.set(sid, []);
    bySession.get(sid).push(t);
  }

  // Build rows.
  const rows = [];
  let withEvents = 0;
  let withoutEvents = 0;
  let nullPromptTokens = 0;

  for (const [sessionId, sessionTasks] of bySession) {
    const eventsPath = path.join(args.sessionsDir, sessionId, "events.jsonl");
    const read = readEvents(eventsPath);
    const events = read ? read.events : null;
    const sm = computeSessionMetrics(events);
    if (sm) withEvents++;
    else withoutEvents++;
    if (sm && sm.promptTokens == null) nullPromptTokens++;

    // The session's LAST dispatch = most recent createdAt (tie-break on id).
    const lastTask = sessionTasks.reduce((a, b) =>
      b.createdAt > a.createdAt ||
      (b.createdAt === a.createdAt && String(b.id) > String(a.id))
        ? b
        : a
    );

    for (const t of sessionTasks) {
      rows.push(buildRow(t, sm, t.id === lastTask.id));
    }
  }

  // Sort by createdAt (ISO strings sort lexicographically).
  rows.sort((a, b) => String(a.createdAt).localeCompare(String(b.createdAt)));

  fs.mkdirSync(path.dirname(path.resolve(args.out)), { recursive: true });
  fs.writeFileSync(args.out, JSON.stringify(rows, null, 2) + "\n");

  // Readable stderr summary.
  const multi = [...bySession.values()].filter((v) => v.length > 1).length;
  process.stderr.write(
    [
      `qwen_tasks_analysis: ${tasks.length} dispatches across ${bySession.size} sessions`,
      `  sessions with events: ${withEvents}; without events: ${withoutEvents}`,
      `  sessions with >1 dispatch (reused): ${multi}`,
      `  sessions with null promptTokens (unknown, not zero): ${nullPromptTokens}`,
      `  output rows: ${rows.length} -> ${args.out}`,
    ].join("\n") + "\n"
  );
}

main();
