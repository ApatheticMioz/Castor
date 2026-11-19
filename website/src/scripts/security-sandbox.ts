/**
 * Security Sandbox — defense-in-depth stack + Live Attack-Vector Monitor.
 * Single source of truth for the 5 path layers (src/tools/sandbox/mod.rs),
 * the 3 companion runtime gates, and the 7 probeable vector cases.
 * All exported data tables live strictly above boot(); every browser
 * access is inside functions, and boot() sits at the very bottom —
 * zero TDZ bugs, SSR-safe module import.
 */

export interface PathLayer {
  level: 'L1' | 'L2' | 'L3' | 'L4' | 'L5';
  fn: string;
  role: string;
}

export const PATH_LAYERS: PathLayer[] = [
  { level: 'L1', fn: 'resolve_workspace_root', role: 'Canonical root anchor' },
  { level: 'L2', fn: 'verify_symlink_containment', role: 'Realpath symlink escape refusal' },
  { level: 'L3', fn: 'refuse_out_of_tree', role: 'Strict boundary containment' },
  { level: 'L4', fn: 'normalize_traversal', role: 'Null-byte / tilde / device-name / .. collapse' },
  { level: 'L5', fn: 'check_binary', role: 'Magic-number sniff → BinaryFileError · pure-Rust lopdf PDF parsing' },
];

export interface CompanionGate {
  name: string;
  coord: string;
  desc: string;
}

export const COMPANION_GATES: CompanionGate[] = [
  {
    name: 'Linux Landlock Kernel Confinement',
    coord: 'src/tools/shell/process.rs',
    desc: 'Every shell child is confined to the workspace root by a Landlock ruleset at exec; unsupported filesystems fail open to the lexical gates, never to an unconstrained process.',
  },
  {
    name: 'Lexical Shell Scanner + Dead-Man Fuse',
    coord: 'src/tools/shell/scanner.rs',
    desc: 'Static lexical scan of the command line: destructive targets, protected roots, and a synthetic canary that trips FatalDeadManFuseError before any execution — the command is not run, and it is not retried.',
  },
  {
    name: '27-Grammar Tree-Sitter In-Memory AST Gate',
    coord: 'src/tools/ast.rs',
    desc: 'Every file commit is parsed in memory against 27 bundled tree-sitter grammars first. Malformed code is rejected before it ever touches disk — a syntax failure rolls back atomically.',
  },
];

export interface VectorTraceStep {
  label: string;
  status: 'pass' | 'trip' | 'allow';
}

export interface VectorCase {
  id: string;
  input: string;
  verdict: 'blocked' | 'allowed';
  verdictLabel: string;
  tripLayer: string;
  refusalStruct: string;
  source: string;
  /** Per-layer outcomes in pipeline order: L1..L5, then runtime gates. */
  trace: VectorTraceStep[];
}

const PIPELINE_LABELS = [
  'L1 resolve_workspace_root',
  'L2 verify_symlink_containment',
  'L3 refuse_out_of_tree',
  'L4 normalize_traversal',
  'L5 check_binary',
  'Runtime gates',
];

function traceAt(tripIndex: number, allow: boolean): VectorTraceStep[] {
  return PIPELINE_LABELS.map((label, i) => {
    if (allow) return { label, status: 'allow' };
    if (i < tripIndex) return { label, status: 'pass' };
    if (i === tripIndex) return { label, status: 'trip' };
    return { label: `${label} (not reached)`, status: 'pass' as const };
  });
}

export const VECTOR_CASES: VectorCase[] = [
  {
    id: 'v1',
    input: 'read_file("../../etc/passwd")',
    verdict: 'blocked',
    verdictLabel: 'BLOCKED',
    tripLayer: 'L4 normalize_traversal',
    refusalStruct:
      "PathEscape(\"../../etc/passwd resolves outside the workspace root\") — traversal collapsed, target off-boundary, binding refusal emitted",
    source: 'src/tools/sandbox/tests.rs:527-548 · security_vector_table_137',
    trace: traceAt(3, false).map((s, i) => (i === 3 ? { ...s, label: 'L4 normalize_traversal · TRIP' } : s)),
  },
  {
    id: 'v2',
    input: 'read_file("workspace/escape_link") → realpath /root/.ssh',
    verdict: 'blocked',
    verdictLabel: 'BLOCKED',
    tripLayer: 'L2 verify_symlink_containment',
    refusalStruct:
      'SymlinkEscape("workspace/escape_link resolves to /root/.ssh, outside the workspace root") — realpath containment refused, binding directive attached',
    source: 'src/tools/sandbox/tests.rs:527-548 · security_vector_table_137',
    trace: traceAt(1, false).map((s, i) => (i === 1 ? { ...s, label: 'L2 verify_symlink_containment · TRIP' } : s)),
  },
  {
    id: 'v3',
    input: 'read_file("workspace/CON.txt")',
    verdict: 'blocked',
    verdictLabel: 'BLOCKED',
    tripLayer: 'L4 normalize_traversal',
    refusalStruct:
      "DeviceName(\"CON\") — Windows reserved device name (CON, PRN, AUX, NUL, COM1–9, LPT1–9) refused before path resolution",
    source: 'src/tools/sandbox/tests.rs:527-548 · security_vector_table_137',
    trace: traceAt(3, false).map((s, i) => (i === 3 ? { ...s, label: 'L4 normalize_traversal · TRIP (DeviceName)' } : s)),
  },
  {
    id: 'v4',
    input: 'read_file("assets/logo.png")',
    verdict: 'blocked',
    verdictLabel: 'BLOCKED',
    tripLayer: 'L5 check_binary',
    refusalStruct:
      "BinaryFile { file: \"assets/logo.png\", detected_type: \"png\", reason: \"magic-number sniff\" } — text-only refusal routed to the vision envelope, never ingested",
    source: 'src/tools/sandbox/tests.rs:527-548 · security_vector_table_137',
    trace: traceAt(4, false).map((s, i) => (i === 4 ? { ...s, label: 'L5 check_binary · TRIP (BinaryFile)' } : s)),
  },
  {
    id: 'v5',
    input: 'bash("rm -rf / && echo ok")',
    verdict: 'blocked',
    verdictLabel: 'BLOCKED',
    tripLayer: 'Lexical Shell Scanner',
    refusalStruct:
      'ShellPolicy(DeadManFuse) — protected root "/" in a destructive command; synthetic canary tripped, execution halted before fork — "Do not retry this command"',
    source: 'src/tools/shell/scanner.rs · src/tools/shell/tests.rs:563',
    trace: traceAt(5, false).map((s, i) => (i === 5 ? { ...s, label: 'Runtime: Lexical Shell Scanner · TRIP (ShellPolicy)' } : s)),
  },
  {
    id: 'v6',
    input: 'edit_file("src/main.rs", "fn main( {")',
    verdict: 'blocked',
    verdictLabel: 'BLOCKED',
    tripLayer: 'Tree-Sitter AST Gate',
    refusalStruct:
      'SyntaxError("expected \')\' after parameter list, found \'{\'") — in-memory tree-sitter parse failed, atomic rollback, file untouched on disk',
    source: 'src/tools/ast.rs · in-memory AST gate before disk commit',
    trace: traceAt(5, false).map((s, i) => (i === 5 ? { ...s, label: 'Runtime: Tree-Sitter AST Gate · TRIP (SyntaxError)' } : s)),
  },
  {
    id: 'v7',
    input: 'edit_file("src/lib.rs", valid Rust diff)',
    verdict: 'allowed',
    verdictLabel: 'ALLOWED (exit 0)',
    tripLayer: 'L1–L5 + AST Gate',
    refusalStruct:
      'No refusal emitted — all five path layers pass, the tree-sitter gate parses clean, the write commits within the workspace root',
    source: 'src/tools/sandbox/tests.rs:527-548 · security_vector_table_137 (14 allowed)',
    trace: traceAt(5, true),
  },
];

export const VECTOR_SUMMARY = {
  total: 137,
  blocked: 123,
  allowed: 14,
  source: 'src/tools/sandbox/tests.rs:527-548',
};

/* ── Client wiring (guarded for SSR import) ─────────────────────────── */

const REDUCED =
  typeof window !== 'undefined' &&
  typeof window.matchMedia === 'function' &&
  window.matchMedia('(prefers-reduced-motion: reduce)').matches;

const LAYER_DELAY_MS = 140; // crisp per-layer gate latch

interface ProbeEls {
  probe: HTMLElement;
  verdict: HTMLElement | null;
  struct: HTMLElement | null;
  source: HTMLElement | null;
  tripLabel: HTMLElement | null;
  steps: HTMLElement[];
}

function collectEls(root: HTMLElement): ProbeEls | null {
  const probe = root.querySelector<HTMLElement>('#security-vector-probe');
  if (!probe) return null;
  const steps = Array.from(probe.querySelectorAll<HTMLElement>('[data-step]'));
  if (steps.length === 0) return null;
  return {
    probe,
    verdict: root.querySelector<HTMLElement>('#security-vector-verdict'),
    struct: root.querySelector<HTMLElement>('#security-vector-struct'),
    source: root.querySelector<HTMLElement>('#security-vector-source'),
    tripLabel: root.querySelector<HTMLElement>('#security-vector-triplayer'),
    steps,
  };
}

function notReachedFor(vector: VectorCase, i: number): boolean {
  const tripIndex = vector.trace.findIndex((t) => t.status === 'trip');
  return vector.verdict === 'blocked' && tripIndex >= 0 && i > tripIndex;
}

function resetTrace(els: ProbeEls, vector: VectorCase): void {
  els.steps.forEach((s, i) => {
    s.classList.remove('is-pass', 'is-trip', 'is-allow', 'is-pending', 'is-muted');
    const status = s.querySelector('[data-status]');
    if (notReachedFor(vector, i)) {
      s.classList.add('is-muted');
      if (status) status.textContent = '—';
    } else {
      s.classList.add('is-pending');
      if (status) status.textContent = '···';
    }
  });
  if (els.verdict) {
    els.verdict.classList.remove('is-blocked', 'is-allowed');
    els.verdict.textContent = 'TRACING…';
  }
  if (els.struct) els.struct.textContent = '—';
  if (els.source) els.source.textContent = '—';
  if (els.tripLabel) els.tripLabel.textContent = '—';
}

function revealStep(els: ProbeEls, vector: VectorCase, i: number): void {
  const s = els.steps[i];
  if (notReachedFor(vector, i)) return; // stays muted
  const step = vector.trace[i];
  s.classList.remove('is-pending');
  s.classList.add(step.status === 'trip' ? 'is-trip' : step.status === 'allow' ? 'is-allow' : 'is-pass');
  const status = s.querySelector('[data-status]');
  if (status) status.textContent = step.status === 'trip' ? 'TRIP' : 'PASS';
}

function finalize(els: ProbeEls, vector: VectorCase): void {
  els.steps.forEach((_s, i) => revealStep(els, vector, i));
  if (!els.verdict) return;
  const blocked = vector.verdict === 'blocked';
  els.verdict.classList.remove('is-pending');
  els.verdict.classList.add(blocked ? 'is-blocked' : 'is-allowed');
  els.verdict.textContent = blocked ? 'BLOCKED' : 'ALLOWED (exit 0)';
  if (els.struct) els.struct.textContent = vector.refusalStruct;
  if (els.source) els.source.textContent = vector.source;
  if (els.tripLabel) els.tripLabel.textContent = vector.tripLayer;
}

/**
 * Probe one vector with 140 ms per-layer gate-latch tracing.
 * Returns a Promise that resolves when the trace has fully settled,
 * so the auto-demo can chain vectors sequentially.
 */
function probeVector(
  els: ProbeEls,
  vector: VectorCase,
  root: HTMLElement,
): Promise<void> {
  const btn = root.querySelector<HTMLElement>(`[data-vector-id="${vector.id}"]`);
  if (btn) btn.classList.add('is-probing');

  resetTrace(els, vector);

  if (REDUCED) {
    finalize(els, vector);
    if (btn) btn.classList.remove('is-probing');
    return Promise.resolve();
  }

  return new Promise<void>((resolve) => {
    for (let i = 0; i < els.steps.length; i++) {
      if (notReachedFor(vector, i)) continue;
      window.setTimeout(() => {
        revealStep(els, vector, i);
      }, LAYER_DELAY_MS * (i + 1));
    }
    const holdMs = LAYER_DELAY_MS * (els.steps.length - 1) + 380;
    window.setTimeout(() => {
      finalize(els, vector);
      if (btn) btn.classList.remove('is-probing');
      resolve();
    }, holdMs);
  });
}

function cancelPendingProbes(root: HTMLElement): void {
  // Timers are per-run locals; nothing to cancel externally because each
  // probe's resolve is idempotent — a new probe simply re-resets the DOM.
  root.querySelectorAll<HTMLElement>('.ss-vector.is-probing').forEach((b) =>
    b.classList.remove('is-probing'),
  );
}

interface DemoState {
  running: boolean;
  token: number;
  els: ProbeEls;
  root: HTMLElement;
  btn: HTMLButtonElement | null;
}

function setDemoButton(demo: DemoState, label: 'idle' | 'running'): void {
  if (!demo.btn) return;
  switch (label) {
    case 'idle':
      demo.btn.textContent = 'AUTO-DEMO ALL 7 ▶';
      demo.btn.disabled = false;
      demo.btn.classList.remove('is-disabled');
      demo.btn.removeAttribute('aria-disabled');
      break;
    case 'running':
      demo.btn.textContent = 'STOP DEMO ■';
      demo.btn.disabled = false;
      demo.btn.classList.remove('is-disabled');
      demo.btn.removeAttribute('aria-disabled');
      break;
  }
}

function runAutoDemo(demo: DemoState): void {
  if (REDUCED) {
    // Reduced motion: explicit click still demos — instant jump through the
    // vectors (probeVector resolves each in its final state, no motion),
    // then settles on the final verdict.
    void probeVector(demo.els, VECTOR_CASES[0], demo.root).then(() =>
      probeVector(demo.els, VECTOR_CASES[VECTOR_CASES.length - 1], demo.root),
    );
    return;
  }
  if (demo.running) {
    stopAutoDemo(demo);
    return;
  }

  demo.running = true;
  demo.token++;
  const token = demo.token;
  setDemoButton(demo, 'running');

  const step = (idx: number): void => {
    if (token !== demo.token || !demo.running) return;
    if (idx >= VECTOR_CASES.length) {
      demo.running = false;
      setDemoButton(demo, 'idle');
      return;
    }
    probeVector(demo.els, VECTOR_CASES[idx], demo.root).then(() => {
      if (token !== demo.token || !demo.running) return;
      // Beat between vectors: 320 ms so each verdict is readable.
      window.setTimeout(() => step(idx + 1), 320);
    });
  };
  step(0);
}

function stopAutoDemo(demo: DemoState): void {
  demo.running = false;
  demo.token++;
  cancelPendingProbes(demo.root);
  setDemoButton(demo, 'idle');
}

function initSecuritySandbox(): void {
  const root = document.getElementById('security');
  if (!root) return;

  const els = collectEls(root);
  if (!els) return;

  const demo: DemoState = {
    running: false,
    token: 0,
    els,
    root,
    btn: root.querySelector<HTMLButtonElement>('#ss-auto-demo'),
  };

  // The auto-demo button is ALWAYS interactive: under reduced motion an
  // explicit click jumps instantly through the vectors instead of playing
  // the timed sequence — no permanently disabled button.
  setDemoButton(demo, 'idle');
  if (demo.btn) {
    demo.btn.addEventListener('click', () => runAutoDemo(demo));
  }

  // Manual vector click: always pauses the auto-demo immediately.
  for (const vector of VECTOR_CASES) {
    const btn = root.querySelector<HTMLElement>(`[data-vector-id="${vector.id}"]`);
    if (btn) {
      const manual = (): void => {
        if (demo.running) stopAutoDemo(demo);
        void probeVector(els, vector, root);
      };
      btn.addEventListener('click', manual);
      btn.addEventListener('keydown', (e: KeyboardEvent) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          manual();
        }
      });
    }
  }
}

/* ── bootSecuritySandbox() — the only DOM bootstrap, strictly at bottom */

function bootSecuritySandbox(): void {
  initSecuritySandbox();
}

if (typeof document !== 'undefined') {
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', bootSecuritySandbox, { once: true });
  } else {
    bootSecuritySandbox();
  }
  document.addEventListener('astro:after-swap', bootSecuritySandbox);
}
