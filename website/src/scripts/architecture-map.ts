/**
 * Architecture Map — dual-agent microkernel schematic + live subsystem inspector.
 * Single source of truth for the 6 hardware subsystems. Top-level DOM access is
 * guarded so the module is importable from Astro front matter during SSR.
 */

export interface Subsystem {
  id: string;
  /** Short label rendered in the schematic node. */
  name: string;
  /** Verified Rust source coordinate. */
  coord: string;
  /** Port / protocol tag. */
  tag: string;
  /** Concise technical mechanism. */
  mechanism: string;
  /** Exact bullet invariants from src/. */
  invariants: string[];
}

export const SUBSYSTEMS: Subsystem[] = [
  {
    id: 'mcp',
    name: 'MCP Stdio (3 Tools)',
    coord: 'src/mcp/mod.rs',
    tag: 'stdio · JSON-RPC · coworker / task / server',
    mechanism:
      'Single stdio MCP surface exposed to the cloud lead architect — no network listener. The tool surface is feature-gated at compile time: lean 8-tool core, +structural AST surgery, +OpenAlex paper lookup; the worker runs as a spawned castor subprocess with prompt validation at dispatch.',
    invariants: [
      'Exactly 3 MCP tools exposed — coworker, task, server — over stdio only; zero MCP network listener',
      'Tool surface is compile-time gated: lean core (8 tools), `ast` adds ast_search/ast_replace, `paper` adds paper_lookup',
      'Empty-prompt and bad-reasoning-effort inputs are rejected at dispatch, before a worker process is ever spawned',
    ],
  },
  {
    id: 'dgi',
    name: 'DGI Logit Gate',
    coord: 'src/mcp/dgi.rs',
    tag: 'logprob classifier · pre-dispatch gate',
    mechanism:
      'Model-classified log-odds gate on the dispatch decision: `delta_z = logP(OVERLOADED) − logP(ADMIT)`. A monolithic multi-subsystem prompt is rejected when `delta_z > 1.5`; near-ties with `|Δz| ≤ 1.5` route to Review instead of auto-admitting an ambiguous task.',
    invariants: [
      'Reject only when `delta_z = logP(OVERLOADED) − logP(ADMIT) > 1.5`',
      'Near-tie band `|Δz| ≤ 1.5` → Review(1): ambiguous monoliths are never auto-admitted',
      'Verdict tokens are pinned in `src/mcp/dgi_anchors.json` — the decision is reproducible across provider runs',
    ],
  },
  {
    id: 'sem',
    name: 'Lease Semaphore',
    coord: 'src/task/semaphore/mod.rs',
    tag: 'O_EXCL file lease · KV-pool admission',
    mechanism:
      'Cross-process task lease claimed with an atomic `O_EXCL` file create and freed by an ownership-checked unlink. A 90% soft ceiling on the aggregate committed-token pool halts new admission, and KV-fill priority ranking lets the heavier / senior active task finish first so its KV footprint is freed without cache thrashing.',
    invariants: [
      'Lease = atomic `O_EXCL` create + ownership-checked free — no PID races, no stale-claim ambiguity',
      'New admission halts when aggregate committed tokens exceed the 90% soft ceiling',
      'KV-fill priority ranking (`src/task/semaphore/ranking.rs`): the task with the higher KV footprint is prioritized to complete and free its cache',
    ],
  },
  {
    id: 'wait',
    name: 'Zero-Turn :18021',
    coord: 'src/task/wait.rs',
    tag: 'loopback 127.0.0.1:18021 · long-poll',
    mechanism:
      'Cross-instance task waiters block on a per-task broadcast channel or a 500 ms disk-mirror poll. A wake with no new turn is a zero-invalidation result — 200 with unchanged state, zero token spend. Failure maps to 200 and a malformed disk file to 400; the endpoint never emits 500.',
    invariants: [
      '`DISK_POLL_MS = 500` — the disk-mirror long-poll interval for cross-instance waiters',
      'Zero-invalidation: a wake without new turn data costs nothing and changes no state',
      'Failure ⇒ 200, malformed disk file ⇒ 400 — the endpoint contract never returns 500',
    ],
  },
  {
    id: 'proxy',
    name: 'SSE Proxy :18022',
    coord: 'src/proxy/sanitize.rs',
    tag: 'loopback 127.0.0.1:18022 · OpenAI-compatible SSE',
    mechanism:
      'Loopback-only SSE sanitizer between the cloud lead architect and the local engine, speaking OpenAI chat / stream. A repetition circuit breaker trips at 40 repeats of a short pattern or `ceil(768 / unit_chars)` repeats of a longer unit and cuts the stream with a terminal reason; image tokens are stripped to prevent text-only 400s.',
    invariants: [
      '`PATTERN_REPEAT_COUNT = 40` for short fragments; span scales up to `MIN_PATTERN_SPAN_CHARS = 768` for long units',
      'Tripped streams are cut with an explicit terminal reason — no unbounded degenerate output',
      'Loops the cloud architect into an OpenAI-compatible local endpoint with zero public exposure',
    ],
  },
  {
    id: 'runner',
    name: 'In-Process Runner',
    coord: 'src/runner/mod.rs',
    tag: 'autonomous turn loop · stall watchdog',
    mechanism:
      'Per-turn stall watchdog (`src/runner/watchdog.rs`) wrapping each `engine.chat` call. On a wall-clock timeout, a runaway repetition loop, or a reasoning-token-budget stall (`finish_reason = length`, no tool calls, reasoning tokens over the per-turn budget) the turn is retried once at the ORIGINAL `reasoning_effort` — a `[System: …]` nudge is appended after the shared prefix, so the warm KV cache is untouched (zero invalidation). A sliding-window loop detector breaks repeated actions and a salvage pass persists the partial report when the session hits its reasoning ceiling.',
    invariants: [
      'Stall (timeout / repetition loop / reasoning-token budget) ⇒ exactly one retry at the ORIGINAL reasoning_effort — the tier never downshifts to "low" mid-session, because a mid-session change would break the prefix cache',
      'The retry is a tail nudge appended after the shared prefix — the KV prefix cache stays warm (zero invalidation); `retry_effort` is logged on the `turn_stall_retry` event',
      'Repetition breaker, loop detector, and salvage: degenerate turns are cut and the partial output is preserved to `.scratch/`',
    ],
  },
];

/* ── Inline token rendering ───────────────────────────────────────── */
/* Backtick-wrapped fragments (e.g. `delta_z > 1.5`, `O_EXCL`) become
   styled <code> tokens with their raw backtick characters removed, and
   every segment is HTML-escaped so nothing like `>` or `&` can break the
   inspector markup. Strings without backticks pass through unchanged. */
function renderInline(text: string): string {
  return text
    .split('`')
    .map((seg, i) => {
      const esc = seg
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;');
      return i % 2 === 1 ? `<code class="am-code">${esc}</code>` : esc;
    })
    .join('');
}

/** Server-renderable detail block for one subsystem (used by the inspector panel). */
export function renderSubsystemDetail(s: Subsystem): string {
  const bullets = s.invariants
    .map((b) => `        <li class="border-l border-[var(--border-strong)] pl-3 font-mono text-[11px] leading-relaxed text-ink-secondary">${renderInline(b)}</li>`)
    .join('\n');
  return [
    `      <p class="font-display text-lg font-bold text-ink">${renderInline(s.name)}</p>`,
    `      <p class="mt-1 font-mono text-[11px] text-ink-muted">${renderInline(s.coord)} · ${renderInline(s.tag)}</p>`,
    `      <p class="mt-3 font-mono text-[11px] leading-relaxed text-ink-secondary">${renderInline(s.mechanism)}</p>`,
    `      <p class="mt-4 font-mono text-[11px] uppercase tracking-widest text-ink-muted">Invariants — from src/</p>`,
    `      <ul class="mt-2 space-y-2">`,
    bullets,
    `      </ul>`,
  ].join('\n');
}

/* ── Client wiring (guarded for SSR import) ─────────────────────────── */

const REDUCED_MQ = '(prefers-reduced-motion: reduce)';

function initArchitectureMap(): void {
  const root = document.getElementById('architecture');
  if (!root) return;

  const nodes = Array.from(root.querySelectorAll<SVGGElement>('.am-node'));
  const buttons = Array.from(root.querySelectorAll<HTMLButtonElement>('.am-select'));
  const detail = root.querySelector<HTMLElement>('#am-detail');
  if (nodes.length === 0 || !detail) return;

  const find = (id: string): Subsystem | undefined => SUBSYSTEMS.find((s) => s.id === id);

  let swapToken = 0;
  const setDetail = (sub: Subsystem): void => {
    if (window.matchMedia(REDUCED_MQ).matches) {
      swapToken++;
      detail.innerHTML = renderSubsystemDetail(sub);
      return;
    }
    // Crisp 180 ms fade/translate out, swap, fade in; token guards races.
    const token = ++swapToken;
    detail.classList.add('is-swapping');
    window.setTimeout(() => {
      if (token !== swapToken) return;
      detail.innerHTML = renderSubsystemDetail(sub);
      detail.classList.remove('is-swapping');
    }, 180);
  };

  const select = (id: string): void => {
    const sub = find(id);
    if (!sub) return;
    for (const n of nodes) {
      const active = n.dataset.id === id;
      n.setAttribute('aria-pressed', active ? 'true' : 'false');
      n.classList.toggle('is-active', active);
    }
    for (const b of buttons) {
      const active = b.dataset.id === id;
      b.classList.toggle('is-active', active);
      b.setAttribute('aria-pressed', active ? 'true' : 'false');
    }
    setDetail(sub);
  };

  nodes.forEach((n) => {
    n.addEventListener('click', () => select(n.dataset.id ?? ''));
    n.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        select(n.dataset.id ?? '');
      }
    });
  });
  buttons.forEach((b) => b.addEventListener('click', () => select(b.dataset.id ?? '')));

  // Select the first subsystem by default (matches the server-rendered detail).
  select(SUBSYSTEMS[0].id);
}

if (typeof document !== 'undefined') {
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', () => initArchitectureMap(), { once: true });
  } else {
    initArchitectureMap();
  }
  document.addEventListener('astro:after-swap', () => initArchitectureMap());
}
