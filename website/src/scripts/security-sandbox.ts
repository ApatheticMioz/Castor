/**
 * Security Sandbox — defense-in-depth stack + interactive security vector probe.
 * Single source of truth for the 5 path layers (src/tools/sandbox/mod.rs), the 3
 * companion runtime gates, and the 7 probeable vector cases. Top-level DOM
 * access is guarded so the module is importable from Astro front matter (SSR).
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
  window.matchMedia &&
  window.matchMedia('(prefers-reduced-motion: reduce)').matches;

function probeVector(vector: VectorCase): void {
  const verdictEl = document.getElementById('security-vector-verdict');
  const structEl = document.getElementById('security-vector-struct');
  const sourceEl = document.getElementById('security-vector-source');
  const tripLabelEl = document.getElementById('security-vector-triplayer');
  if (!verdictEl || !structEl || !sourceEl || !tripLabelEl) return;

  const steps = Array.from(
    document.querySelectorAll<HTMLElement>('#security-vector-probe [data-step]'),
  );
  if (steps.length === 0) return;

  // A layer is "not reached" once the trace has tripped upstream of it.
  const tripIndex = vector.trace.findIndex((t) => t.status === 'trip');
  const notReached = (i: number): boolean =>
    vector.verdict === 'blocked' && tripIndex >= 0 && i > tripIndex;

  // Reset: all steps muted/pending, verdict pending.
  steps.forEach((s, i) => {
    s.classList.remove('is-pass', 'is-trip', 'is-allow', 'is-pending', 'is-muted');
    const status = s.querySelector('[data-status]');
    if (notReached(i)) {
      s.classList.add('is-muted');
      if (status) status.textContent = '—';
    } else {
      s.classList.add('is-pending');
      if (status) status.textContent = '···';
    }
  });
  verdictEl.classList.remove('is-blocked', 'is-allowed', 'is-pending');
  verdictEl.classList.add('is-pending');
  verdictEl.textContent = 'TRACING…';
  structEl.textContent = '—';
  sourceEl.textContent = '—';
  tripLabelEl.textContent = '—';

  const reveal = (s: HTMLElement, i: number): void => {
    const step = vector.trace[i];
    const status = s.querySelector('[data-status]');
    if (notReached(i)) return; // stays muted
    s.classList.remove('is-pending');
    s.classList.add(
      step.status === 'trip' ? 'is-trip' : step.status === 'allow' ? 'is-allow' : 'is-pass',
    );
    if (status) status.textContent = step.status === 'trip' ? 'TRIP' : 'PASS';
  };

  const finalize = (): void => {
    steps.forEach((s, i) => reveal(s, i));
    const blocked = vector.verdict === 'blocked';
    verdictEl.classList.remove('is-pending');
    verdictEl.classList.add(blocked ? 'is-blocked' : 'is-allowed');
    verdictEl.textContent = blocked ? 'BLOCKED' : 'ALLOWED (exit 0)';
    structEl.textContent = vector.refusalStruct;
    sourceEl.textContent = vector.source;
    tripLabelEl.textContent = vector.tripLayer;
  };

  if (REDUCED) {
    finalize();
    return;
  }

  // Staggered layer-by-layer reveal, ~140 ms per layer.
  const delay = 140;
  steps.forEach((s, i) => {
    if (notReached(i)) return;
    setTimeout(() => {
      reveal(s, i);
      if (i === steps.length - 1) finalize();
    }, delay * (i + 1));
  });
}

function initSecuritySandbox(): void {
  const root = document.getElementById('security');
  if (!root) return;

  const probe = root.querySelector<HTMLElement>('#security-vector-probe');
  if (!probe) return;

  for (const vector of VECTOR_CASES) {
    const btn = root.querySelector<HTMLElement>(`[data-vector-id="${vector.id}"]`);
    if (btn) {
      btn.addEventListener('click', () => probeVector(vector));
      btn.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          probeVector(vector);
        }
      });
    }
  }
}

if (typeof document !== 'undefined') {
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', () => initSecuritySandbox(), { once: true });
  } else {
    initSecuritySandbox();
  }
  document.addEventListener('astro:after-swap', () => initSecuritySandbox());
}
