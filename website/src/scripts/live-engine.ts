/*
 * Castor — Live Engine Stage (embedded dark OLED hardware bench)
 *
 *  - Canvas 2D oscilloscope: dual-trace cobalt prefill + amber decode/MTP
 *    bursts, scanhead callout. DPR capped at 2, IntersectionObserver
 *    auto-pause offscreen, one rich static frame under reduced motion.
 *  - Live 48-block VRAM KV-cache memory matrix animating with the mode:
 *    reused-prefix (cobalt) / active-decode (amber) / free cells.
 *  - Upgraded MCP protocol console: smooth line streaming with a live
 *    character-by-character typing effect on the newest line plus a
 *    blinking amber block cursor on that single active line only.
 *  - REPLAY restarts a fresh streaming dispatch burst; PAUSE/RESUME
 *    freezes the simulation clock.
 *
 * CRITICAL INVARIANT: every `const` / `let` table and function declaration
 * in this module lives ABOVE `boot()`, which is wired at the very bottom.
 */

(() => {
  // SSR guard: Astro evaluates this module on the server during a static
  // build where `document` is undefined; all work is browser-only.
  if (typeof document === 'undefined') return;

  /* ── Deterministic RNG (mulberry32) ─────────────────────────────── */

  function mulberry32(seed: number): () => number {
    let a = seed >>> 0;
    return () => {
      a = (a + 0x6d2b79f5) | 0;
      let t = Math.imul(a ^ (a >>> 15), 1 | a);
      t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  }

  /* ── Constants (ground truth: Nov 19, 2026) ─────────────────────── */

  type Mode = 'warm' | 'mtp' | 'swarm';

  const TICK_MS = 50; // 20 Hz simulation
  const WINDOW = 140; // ~7 s visible scope
  const PREFILL_MAX = 20_000; // tok/s (peak 19,384)
  const DECODE_MAX = 160; // tok/s (peak 139.7)
  const CLOUD_COST_PER_TOKEN = 2.18e-6; // blended counterfactual $/token
  const COBALT = '#3B82F6';
  const AMBER = '#F97316';
  const BONE = '#F3F5F8';
  const INK_MUTED = '#646D7D';
  const GRID = 'rgba(255,255,255,0.06)';
  const GRID_STRONG = 'rgba(255,255,255,0.1)';
  const MONO = '"IBM Plex Mono", ui-monospace, monospace';
  const KV_CELLS = 48; // VRAM KV-cache memory matrix width
  // Console typing runs on a 16 ms quantum (≈55 chars/s).
  const MODE_SEED: Record<Mode, number> = {
    warm: 20261119,
    mtp: 1938427,
    swarm: 418021,
  };
  const MODE_LABEL: Record<Mode, string> = {
    warm: 'WARM KV PREFILL',
    mtp: 'MTP SPECULATIVE DECODE',
    swarm: '4× SWARM CONURRENCY',
  };

  interface Sim {
    mode: Mode;
    rng: () => number;
    pre: number[]; // per-slot prefill velocity (tok/s)
    dec: number; // decode rate (tok/s)
    burstLeft: number; // prefill peak envelope remaining (ms)
    burstLevel: number;
    mtpLeft: number; // MTP decode burst remaining (ms)
    mtpLevel: number;
    mtpActive: boolean;
    stallLeft: number; // swarm slot stall remaining (ms)
    stallSlot: number;
    kv: number; // KV cache hit %
    cost: number; // cloud counterfactual ($)
    t: number;
    charge: boolean;
  }

  interface TraceSample {
    pre: number[];
    dec: number;
    burst: number; // 1 while an MTP decode burst is active
  }

  /* ── Simulation step (one 50 ms tick) ───────────────────────────── */

  function makeSim(mode: Mode, reduced: boolean): Sim {
    const seed = reduced
      ? MODE_SEED[mode]
      : ((Date.now() & 0xffff) ^ (MODE_SEED[mode] * 31)) & 0x7fffffff;
    const rng = mulberry32(seed);
    const pre =
      mode === 'swarm'
        ? [1700, 1900, 2100, 2300].map((v) => v + rng() * 200)
        : mode === 'warm'
          ? [6892]
          : [2400];
    return {
      mode,
      rng,
      pre,
      dec: mode === 'swarm' ? 82 : 75.5,
      burstLeft: 0,
      burstLevel: 0,
      mtpLeft: 0,
      mtpLevel: 0,
      mtpActive: false,
      stallLeft: 0,
      stallSlot: 0,
      kv: mode === 'warm' ? 93.7 : 92.6,
      cost: 0,
      t: 0,
      charge: true,
    };
  }

  function tick(s: Sim, dt: number): void {
    s.t += dt;
    const r = s.rng;
    const k = Math.min(1, (dt / TICK_MS) * 0.35);

    if (s.mode === 'swarm') {
      // 4× O_EXCL token-budgeted slots sharing one warm KV prefix.
      if (s.stallLeft <= 0 && r() < 0.03) {
        s.stallSlot = Math.floor(r() * 4);
        s.stallLeft = 250 + r() * 450;
      }
      if (s.stallLeft > 0) s.stallLeft -= dt;
      for (let i = 0; i < 4; i++) {
        const stalled = s.stallLeft > 0 && i === s.stallSlot;
        const target = stalled
          ? 180
          : 1700 + 620 * Math.sin(s.t / 900 + i * 1.3) + (r() - 0.5) * 700;
        s.pre[i] += (target - s.pre[i]) * k;
      }
      const decTarget = (s.stallLeft > 0 ? 71 : 82) + (r() - 0.5) * 8;
      s.dec += (decTarget - s.dec) * k;
      const kvTarget = s.stallLeft > 0 ? 87.5 : 92.6;
      s.kv += (kvTarget + (r() - 0.5) * 0.8 - s.kv) * k;
    } else {
      // Single active sequence.
      if (s.burstLeft <= 0 && r() < 0.05) {
        s.burstLeft = 250 + r() * 750;
        s.burstLevel =
          s.mode === 'warm' ? 16365 + r() * 3019 : 3200 + r() * 2200;
      }
      if (s.burstLeft > 0) s.burstLeft -= dt;
      const preTarget =
        s.burstLeft > 0
          ? s.burstLevel * (0.8 + r() * 0.2)
          : (s.mode === 'warm' ? 6892 : 2400) * (0.88 + r() * 0.24);
      s.pre[0] += (preTarget - s.pre[0]) * k;

      if (s.mode === 'mtp') {
        // MTP: 70.5% draft acceptance, 3.45 tokens/step → clustered
        // multi-token burst spikes riding 75.5 p50 → 116.6 p95 → 139.7 max.
        if (s.mtpLeft <= 0) {
          s.mtpActive = r() < 0.16;
          if (s.mtpActive) {
            s.mtpLeft = 160 + r() * 340;
            s.mtpLevel = 116.6 + r() * 23.1;
          }
        } else {
          s.mtpLeft -= dt;
        }
        const decTarget = s.mtpActive
          ? s.mtpLevel * (0.92 + r() * 0.08)
          : 75.5 + (r() - 0.5) * 10;
        s.dec += (decTarget - s.dec) * k;
        const kvTarget = 92.6;
        s.kv += (kvTarget + (r() - 0.5) * 0.5 - s.kv) * k;
      } else {
        s.dec += (75.5 + (r() - 0.5) * 8 - s.dec) * k;
        const kvTarget = 93.7;
        s.kv += (kvTarget + (r() - 0.5) * 0.6 - s.kv) * k;
      }
    }

    if (s.charge) {
      const tokPerSec = s.pre.reduce((a, b) => a + b, 0) + s.dec;
      s.cost += (tokPerSec * dt) / 1000 * CLOUD_COST_PER_TOKEN;
    }
  }

  /* ── MCP protocol trace scripts (mode-synced) ────────────────────── */

  type TraceLine = [tag: string, cls: string, text: string];

  const TRACE: Record<Mode, TraceLine[]> = {
    warm: [
      ['[CLOUD → MCP]', 't-cloud', 'castor_coworker(session: "auth-refactor", cwd: ".")'],
      ['[DGI GATE]', 't-muted', 'logP(OVERLOADED) − logP(ADMIT) = −2.41 → ADMIT (warm bypass)'],
      ['[LOCAL:3090]', 't-local', 'search_code("verify_token") → 4 files · 94.1% KV prefix hit'],
      ['[KV POOL]', 't-cloud', 'prefix match 93.7% · TTFT 3.22 s · 150K pool · no recompute'],
      ['[CLOUD → MCP]', 't-cloud', 'castor_coworker(session: "auth-refactor", cwd: ".")'],
      ['[DGI GATE]', 't-muted', 'logP(OVERLOADED) − logP(ADMIT) = −2.41 → ADMIT (warm bypass)'],
      ['[LOCAL:3090]', 't-local', 'search_code("verify_token") → 4 files · 94.1% KV prefix hit'],
      ['[KV POOL]', 't-cloud', 'prefix match 93.7% · TTFT 3.22 s · 150K pool · no recompute'],
    ],
    mtp: [
      ['[CLOUD → MCP]', 't-cloud', 'castor_coworker(session: "auth-refactor", cwd: ".")'],
      ['[MTP DRAFT]', 't-local', 'draft 4 → accept 3.45 tok/step · 70.5% acceptance'],
      ['[LOCAL:3090]', 't-local', 'decode burst 139.7 tok/s · spec window +40.9%'],
      ['[MTP DRAFT]', 't-local', 'draft 4 → accept 3.45 tok/step · 70.5% acceptance'],
      ['[CLOUD → MCP]', 't-cloud', 'castor_coworker(session: "auth-refactor", cwd: ".")'],
      ['[MTP DRAFT]', 't-local', 'draft 4 → accept 3.45 tok/step · 70.5% acceptance'],
      ['[LOCAL:3090]', 't-local', 'decode burst 139.7 tok/s · spec window +40.9%'],
      ['[MTP DRAFT]', 't-local', 'draft 4 → accept 3.45 tok/step · 70.5% acceptance'],
    ],
    swarm: [
      ['[CLOUD → MCP]', 't-cloud', 'castor_coworker(session: "auth-refactor", cwd: ".")'],
      ['[SLOTS]', 't-ink', 'O_EXCL slots 4/4 · token budget 150K · 90% soft ceiling'],
      ['[LOCAL:3090]', 't-local', 'slot #2 stall 0.4 s · zero-invalidation recovery'],
      ['[KV POOL]', 't-cloud', 'shared prefix · 4 sessions · 150K aggregate pool'],
      ['[CLOUD → MCP]', 't-cloud', 'castor_coworker(session: "auth-refactor", cwd: ".")'],
      ['[SLOTS]', 't-ink', 'O_EXCL slots 4/4 · token budget 150K · 90% soft ceiling'],
      ['[LOCAL:3090]', 't-local', 'slot #3 stall 0.4 s · zero-invalidation recovery'],
      ['[KV POOL]', 't-cloud', 'shared prefix · 4 sessions · 150K aggregate pool'],
    ],
  };

  /* ── Oscilloscope renderer ───────────────────────────────────────── */

  function renderScope(
    ctx: CanvasRenderingContext2D,
    w: number,
    h: number,
    s: Sim,
    trace: TraceSample[],
    scanFrac: number,
  ): void {
    ctx.clearRect(0, 0, w, h);

    const left = 42;
    const right = w - 14;
    const top = 14;
    const bottom = h - 20;
    const plotW = right - left;
    const plotH = bottom - top;

    // Grid: 1px hairline lines, denser at 50% level.
    ctx.lineWidth = 1;
    for (let i = 0; i <= 4; i++) {
      const y = Math.round(top + (plotH * i) / 4) + 0.5;
      ctx.strokeStyle = i === 2 ? GRID_STRONG : GRID;
      ctx.beginPath();
      ctx.moveTo(left, y);
      ctx.lineTo(right, y);
      ctx.stroke();
    }
    for (let i = 0; i <= 8; i++) {
      const x = Math.round(left + (plotW * i) / 8) + 0.5;
      ctx.strokeStyle = i === 4 ? GRID_STRONG : GRID;
      ctx.beginPath();
      ctx.moveTo(x, top);
      ctx.lineTo(x, bottom);
      ctx.stroke();
    }

    // Axis ticks + labels.
    ctx.font = `9px ${MONO}`;
    ctx.fillStyle = INK_MUTED;
    ctx.textAlign = 'right';
    ctx.textBaseline = 'middle';
    for (let i = 0; i <= 4; i++) {
      const y = top + (plotH * i) / 4;
      const v = PREFILL_MAX * (1 - i / 4);
      ctx.fillText(v >= 1000 ? `${(v / 1000).toFixed(0)}K` : `${v.toFixed(0)}`, left - 6, y);
    }
    ctx.textAlign = 'center';
    ctx.textBaseline = 'top';
    for (let i = 0; i <= 8; i++) {
      const x = left + (plotW * i) / 8;
      ctx.fillText(`-${((8 - i) / 8) * 7.0}s`, x, bottom + 6);
    }
    // Dual axis captions.
    ctx.textAlign = 'left';
    ctx.fillStyle = COBALT;
    ctx.fillText('PREFILL tok/s', left + 6, top + 4);
    ctx.fillStyle = AMBER;
    ctx.textAlign = 'right';
    ctx.fillText('DECODE tok/s →', right - 6, top + 4);

    const xOf = (i: number) =>
      left + plotW * (1 - (trace.length - 1 - i) / (WINDOW - 1));
    const yOf = (v: number) => bottom - (v / PREFILL_MAX) * plotH;

    // ── Cobalt: prefill velocity traces (+ KV-reuse fill beneath) ───
    if (s.mode === 'swarm') {
      // 4 slot traces, each 1.5px cobalt at graduated opacity.
      for (let slot = 0; slot < 4; slot++) {
        ctx.globalAlpha = slot === 0 ? 1 : 0.42;
        ctx.strokeStyle = COBALT;
        ctx.lineWidth = 1.5;
        ctx.beginPath();
        for (let i = 0; i < trace.length; i++) {
          const x = xOf(i);
          const y = yOf(trace[i].pre[slot]);
          if (i === 0) ctx.moveTo(x, y);
          else ctx.lineTo(x, y);
        }
        ctx.stroke();
        ctx.globalAlpha = 1;
      }
    } else {
      // Gradient fill beneath the prefill trace (viewport-internal only).
      const grad = ctx.createLinearGradient(0, top, 0, bottom);
      grad.addColorStop(0, 'rgba(59,130,246,0.16)');
      grad.addColorStop(1, 'rgba(59,130,246,0)');
      ctx.fillStyle = grad;
      ctx.beginPath();
      ctx.moveTo(xOf(0), bottom);
      for (let i = 0; i < trace.length; i++) ctx.lineTo(xOf(i), yOf(trace[i].pre[0]));
      ctx.lineTo(xOf(trace.length - 1), bottom);
      ctx.closePath();
      ctx.fill();

      ctx.strokeStyle = COBALT;
      ctx.lineWidth = 1.5;
      ctx.beginPath();
      for (let i = 0; i < trace.length; i++) {
        const x = xOf(i);
        const y = yOf(trace[i].pre[0]);
        if (i === 0) ctx.moveTo(x, y);
        else ctx.lineTo(x, y);
      }
      ctx.stroke();
    }

    // ── Amber: decode rate + MTP draft burst bars ───────────────────
    ctx.fillStyle = AMBER;
    for (let i = 0; i < trace.length; i++) {
      if (trace[i].burst <= 0) continue;
      const x = xOf(i);
      const barH = (trace[i].dec / DECODE_MAX) * plotH;
      ctx.globalAlpha = 0.16;
      ctx.fillRect(x - 1.5, bottom - barH, 3, barH);
    }
    ctx.globalAlpha = 1;

    // Decode polyline mapped onto a 160 tok/s axis.
    ctx.strokeStyle = AMBER;
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    for (let i = 0; i < trace.length; i++) {
      const x = xOf(i);
      const y = bottom - (trace[i].dec / DECODE_MAX) * plotH;
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.stroke();

    // ── Scanhead marker with live readout tag ───────────────────────
    if (trace.length === 0) return; // guard: initial layout before priming

    const sx = Math.round(left + plotW * scanFrac) + 0.5;
    ctx.strokeStyle = 'rgba(255,255,255,0.3)';
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(sx, top);
    ctx.lineTo(sx, bottom);
    ctx.stroke();

    const cur = trace[trace.length - 1];
    const tagY = cur && s.mode === 'warm' ? yOf(cur.pre[0]) : top + 18;
    const tagText =
      s.mode === 'swarm'
        ? `Σ ${Math.round(cur.pre.reduce((a, b) => a + b, 0)).toLocaleString('en-US')} tok/s · 4×`
        : `${Math.round(cur.pre[0]).toLocaleString('en-US')} tok/s`;
    ctx.font = `10px ${MONO}`;
    const tw = ctx.measureText(tagText).width;
    const boxW = tw + 12;
    const bx = sx + 8 + boxW > right ? sx - 8 - boxW : sx + 8;
    ctx.fillStyle = '#0B0D11';
    ctx.fillRect(bx, tagY - 9, boxW, 18);
    ctx.strokeStyle = 'rgba(255,255,255,0.28)';
    ctx.strokeRect(bx + 0.5, tagY - 8.5, boxW - 1, 17);
    ctx.fillStyle = COBALT;
    ctx.fillRect(bx, tagY - 9, 2, 18);
    ctx.fillStyle = BONE;
    ctx.textAlign = 'left';
    ctx.textBaseline = 'middle';
    ctx.fillText(tagText, bx + 8, tagY);

    // Mode label, bottom-left inside viewport.
    ctx.fillStyle = INK_MUTED;
    ctx.textAlign = 'right';
    ctx.textBaseline = 'bottom';
    ctx.fillText(MODE_LABEL[s.mode], right, bottom - 3);
  }

  /* ── Console streaming (char-by-char typed, blinking block cursor) ── */

  interface PendingLine {
    full: string;
    shown: number;
    row: HTMLElement;
    textSpan: HTMLElement;
    cursor: HTMLElement;
  }

  function makeRow(tag: string, cls: string): [HTMLElement, HTMLElement, HTMLElement] {
    const row = document.createElement('span');
    row.className = 'flex gap-2 overflow-hidden';
    const tagSpan = document.createElement('span');
    tagSpan.className = `shrink-0 ${cls}`;
    tagSpan.textContent = tag;
    const txt = document.createElement('span');
    txt.className = 'min-w-0 overflow-hidden text-ellipsis whitespace-nowrap text-[#949BA8]';
    const cursor = document.createElement('span');
    cursor.className = 'c-cursor';
    cursor.textContent = '▌';
    row.append(tagSpan, txt, cursor);
    return [row, txt, cursor];
  }

  function queueLine(q: PendingLine[], lines: TraceLine[], consoleEl: HTMLElement): void {
    const [tag, cls, text] = lines[Math.floor(Math.random() * lines.length)];
    const [row, textSpan, cursor] = makeRow(tag, cls);
    q.push({ full: text, shown: 0, row, textSpan, cursor });
    consoleEl.append(row);
    while (consoleEl.children.length > 5) consoleEl.firstChild?.remove();
  }

  /**
   * The amber ▌ block cursor belongs to exactly ONE line: the newest pending
   * (still-typing) line. Older lines render clean — no stacked cursors.
   */
  function setActiveCursor(q: PendingLine[]): void {
    for (const p of q) p.cursor.style.display = 'none';
    const active = q[q.length - 1];
    if (active && active.shown < active.full.length) active.cursor.style.display = 'inline';
  }

  /** Advance the active (newest) typed line; returns true when a new line
   *  should be queued. Only the newest line ever shows the cursor. */
  function typeStep(q: PendingLine[], chars: number): boolean {
    if (q.length === 0) return true;
    const active = q[q.length - 1];
    if (active.shown < active.full.length) {
      active.shown = Math.min(active.full.length, active.shown + chars);
      active.textSpan.textContent = active.full.slice(0, active.shown);
      if (active.shown >= active.full.length) setActiveCursor(q);
      return false;
    }
    // Done with this line: commit it, pop, request next.
    q.pop();
    return true;
  }

  /* ── 48-cell KV-cache memory matrix (10 Hz) ─────────────────────── */

  interface KvFrame {
    reused: number; // cobalt cells from the left
    active: number[]; // amber cell indices (active decode)
  }

  function kvFrame(mode: Mode, s: Sim): KvFrame {
    if (mode === 'warm') {
      // 93.7% of the 150K pool is a reused prefix; the trailing edge is
      // the small active-decode region.
      const reused = Math.round(
        (s.kv / 100) * KV_CELLS + (s.rng() - 0.5) * 1.4,
      );
      const clamped = Math.max(0, Math.min(KV_CELLS - 3, reused));
      const tail = KV_CELLS - 3;
      const active: number[] = [];
      for (let i = Math.max(clamped, tail); i < KV_CELLS; i++) active.push(i);
      return { reused: clamped, active };
    }
    if (mode === 'mtp') {
      // MTP: reused prefix plus surging amber speculative-decode bursts.
      const reused = Math.round(92.6 * (KV_CELLS / 100));
      const burst = s.mtpActive ? 6 + Math.floor(s.rng() * 10) : 3;
      const active: number[] = [];
      for (let i = 0; i < Math.min(burst, KV_CELLS - reused); i++) {
        active.push(reused + i);
      }
      return { reused, active };
    }
    // Swarm is handled by swarmKvFrame() (4 segmented concurrent bands).
    return { reused: 0, active: [] };
  }

  /** Apply a KV frame to the 48 LED cells. */
  function paintKv(cells: HTMLElement[], frame: KvFrame): void {
    cells.forEach((c, i) => {
      let state: '' | 'is-cobalt' | 'is-amber' = '';
      if (i < frame.reused) state = 'is-cobalt';
      else if (frame.active.includes(i)) state = 'is-amber';
      c.classList.remove('is-cobalt', 'is-amber');
      if (state) c.classList.add(state);
    });
  }

  function swarmKvFrame(s: Sim): { reusedSet: Set<number>; active: number[] } {
    const band = Math.floor(KV_CELLS / 4);
    const active: number[] = [];
    const reusedSet = new Set<number>();
    for (let b = 0; b < 4; b++) {
      const stalled = s.stallLeft > 0 && s.stallSlot === b;
      const base = Math.round(band * (stalled ? 0.45 : 0.88 + s.rng() * 0.06));
      for (let i = 0; i < base; i++) reusedSet.add(b * band + i);
      const head = Math.min(band - 1, base + (stalled ? 1 : 2));
      for (let i = base; i < head; i++) active.push(b * band + i);
    }
    return { reusedSet, active };
  }

  function updateKv(
    cells: HTMLElement[],
    mode: Mode,
    s: Sim,
  ): void {
    if (mode === 'swarm') {
      const f = swarmKvFrame(s);
      cells.forEach((c, i) => {
        let state: '' | 'is-cobalt' | 'is-amber' = '';
        if (f.reusedSet.has(i)) state = 'is-cobalt';
        else if (f.active.includes(i)) state = 'is-amber';
        c.classList.remove('is-cobalt', 'is-amber');
        if (state) c.classList.add(state);
      });
      return;
    }
    const f = kvFrame(mode, s);
    paintKv(cells, f);
  }

  /* ── Boot ────────────────────────────────────────────────────────── */

  function initLiveEngine(root: HTMLElement): void {
    const canvas = root.querySelector<HTMLCanvasElement>('#engine-scope');
    const consoleEl = root.querySelector<HTMLElement>('#engine-console');
    const pauseBtn = root.querySelector<HTMLButtonElement>('#engine-pause');
    const replayBtn = root.querySelector<HTMLButtonElement>('#engine-replay');
    const modeBtns = Array.from(
      root.querySelectorAll<HTMLButtonElement>('[data-mode]'),
    );
    const kvCells = Array.from(
      root.querySelectorAll<HTMLElement>('#engine-kv-blocks .les-kv'),
    );
    if (!canvas || !consoleEl || !pauseBtn) return;
    const scope: HTMLCanvasElement = canvas;
    const consoleBox: HTMLElement = consoleEl;
    const c2d = scope.getContext('2d');
    if (!c2d) return;
    const ctx: CanvasRenderingContext2D = c2d;

    const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
    let mode: Mode = 'warm';
    let sim = makeSim(mode, reduced);
    const trace: TraceSample[] = [];
    let paused = false;
    let visible = true;
    let rafId = 0;
    let lastTs = 0;
    let acc = 0;

    /* Readout: 10 Hz DOM updates, decoupled from the 20 Hz sim clock */
    const readout = {
      prefill: root.querySelector<HTMLElement>('#td-prefill'),
      decode: root.querySelector<HTMLElement>('#td-decode'),
      kv: root.querySelector<HTMLElement>('#td-kvhit'),
      cost: root.querySelector<HTMLElement>('#td-cloudcost'),
    };
    let readoutAcc = 0;
    function updateReadout(): void {
      if (readout.prefill) {
        readout.prefill.textContent = Math.round(
          sim.pre.reduce((a, b) => a + b, 0),
        ).toLocaleString('en-US');
      }
      if (readout.decode) readout.decode.textContent = sim.dec.toFixed(1);
      if (readout.kv) readout.kv.textContent = sim.kv.toFixed(1);
      if (readout.cost) readout.cost.textContent = `$${sim.cost.toFixed(3)}`;
    }

    /* Console: 5-line window, char-by-char typing on the newest line */
    const pending: PendingLine[] = [];
    let typeAcc = 0; // ms accumulator → chars
    function queueNext(): void {
      const lines = TRACE[mode];
      const i = Math.floor(Math.random() * lines.length);
      queueLine(pending, [lines[i]], consoleBox);
      setActiveCursor(pending);
    }
    function seedConsole(): void {
      consoleBox.replaceChildren();
      pending.length = 0;
      const lines = TRACE[mode];
      if (reduced) {
        // Steady 5 full lines, no animation, no cursor.
        for (let i = 0; i < 5; i++) {
          const [tag, cls, text] = lines[i % lines.length];
          const [row, txt, cursor] = makeRow(tag, cls);
          txt.textContent = text;
          cursor.style.display = 'none';
          consoleBox.append(row);
        }
      } else {
        for (let i = 0; i < 4; i++) {
          const [tag, cls, text] = lines[i % lines.length];
          const [row, txt, cursor] = makeRow(tag, cls);
          txt.textContent = text; // already-typed history
          cursor.style.display = 'none';
          consoleBox.append(row);
        }
        queueNext(); // newest line starts typing
        typeAcc = 0;
      }
    }
    function stepConsole(dt: number): void {
      if (reduced || paused) return;
      typeAcc += dt;
      while (typeAcc >= 16) {
        typeAcc -= 16; // ~55 chars/s at 16 ms quantum
        if (typeStep(pending, 1)) {
          // current line finished → queue the next one
          queueNext();
        }
      }
    }

    /* Mode switch: restart sim + script, update aria-selected */
    function setMode(next: Mode): void {
      if (next === mode) return;
      mode = next;
      sim = makeSim(mode, reduced);
      trace.length = 0;
      consoleBox.replaceChildren();
      pending.length = 0;
      for (const b of modeBtns) {
        b.setAttribute('aria-selected', String(b.dataset.mode === next));
      }
      if (reduced) {
        drawStaticFrame();
      } else {
        seedConsole();
      }
      updateKv(kvCells, mode, sim);
      updateReadout();
    }

    for (const b of modeBtns) {
      b.addEventListener('click', () => setMode(b.dataset.mode as Mode));
    }

    /* Pause / resume hardware key */
    pauseBtn?.addEventListener('click', () => {
      paused = !paused;
      pauseBtn.setAttribute('aria-pressed', String(paused));
      const pl = pauseBtn.querySelector<HTMLSpanElement>('[data-pause-label]');
      const rl = pauseBtn.querySelector<HTMLSpanElement>('[data-resume-label]');
      if (pl && rl) {
        pl.style.display = paused ? 'none' : 'inline';
        rl.style.display = paused ? 'inline' : 'none';
      }
      if (paused) {
        cancelAnimationFrame(rafId);
      } else if (!reduced) {
        lastTs = 0;
        rafId = requestAnimationFrame(loop);
      }
    });

    /* Replay: fresh streaming dispatch burst (new sim + fresh console) */
    replayBtn?.addEventListener('click', () => {
      sim = makeSim(mode, reduced);
      trace.length = 0;
      if (reduced) {
        drawStaticFrame();
      } else {
        seedConsole();
        if (paused) {
          // stay paused but show a fresh frozen frame
          drawFrame(0.55);
        } else if (lastTs === 0 && !visible) {
          lastTs = 0;
        }
      }
      updateKv(kvCells, mode, sim);
      updateReadout();
    });

    /* ── Main loop (20 Hz sim ticks, rAF-paced) ────────────────────── */

    function loop(ts: number): void {
      if (paused) return;
      rafId = requestAnimationFrame(loop);
      if (lastTs === 0) {
        lastTs = ts;
        return;
      }
      let dt = ts - lastTs;
      lastTs = ts;
      if (dt > 250) dt = 250; // tab was backgrounded
      acc += dt;
      readoutAcc += dt;

      let stepped = false;
      while (acc >= TICK_MS) {
        acc -= TICK_MS;
        tick(sim, TICK_MS);
        trace.push({
          pre: [...sim.pre],
          dec: sim.dec,
          burst: sim.mtpLeft > 0 ? 1 : 0,
        });
        if (trace.length > WINDOW) trace.shift();
        stepped = true;
      }

      // 10 Hz DOM readout + KV matrix updates.
      let needReadout = false;
      while (readoutAcc >= 100) {
        readoutAcc -= 100;
        updateReadout();
        if (kvCells.length) updateKv(kvCells, mode, sim);
        needReadout = true;
      }

      stepConsole(dt);

      if (stepped || needReadout) {
        drawFrame((ts % 4200) / 4200);
      }
    }

    /* ── Canvas sizing (DPR capped at 2) ───────────────────────────── */

    let dpr = 1;
    function sizeCanvas(): void {
      const rect = scope.getBoundingClientRect();
      // Fallback: if layout hasn't completed and rect is zero, use
      // parentElement dimensions (or sensible defaults) so the first
      // frame is never a 0×0 blank.
      const cw = Math.round((rect.width || (scope.parentElement?.clientWidth || 640)) * Math.min(window.devicePixelRatio || 1, 2));
      const ch = Math.round((rect.height || (scope.parentElement?.clientHeight || 220)) * Math.min(window.devicePixelRatio || 1, 2));
      dpr = Math.min(window.devicePixelRatio || 1, 2);
      const maxCw = Math.max(1, cw);
      const maxCh = Math.max(1, ch);
      if (scope.width !== maxCw || scope.height !== maxCh) {
        scope.width = maxCw;
        scope.height = maxCh;
      }
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      if (reduced) drawStaticFrame();
      else if (!paused) drawFrame(0.55);
    }

    function drawFrame(scanFrac: number): void {
      // Guard against zero-size canvas (initial layout not yet painted).
      const w = scope.width / dpr;
      const h = scope.height / dpr;
      if (w <= 0 || h <= 0) return;
      renderScope(ctx, w, h, sim, trace, scanFrac);
    }

    /* ── Reduced-motion: one deterministic rich static frame ──────── */

    function drawStaticFrame(): void {
      sim = makeSim(mode, true);
      trace.length = 0;
      for (let i = 0; i < WINDOW; i++) {
        tick(sim, TICK_MS);
        trace.push({
          pre: [...sim.pre],
          dec: sim.dec,
          burst: sim.mtpLeft > 0 ? 1 : 0,
        });
      }
      // Seed console to a steady 5 lines.
      consoleBox.replaceChildren();
      seedConsole();
      updateKv(kvCells, mode, sim);
      drawFrame(0.62);
    }

    /* ── IntersectionObserver: pause offscreen ─────────────────────── */

    const io = new IntersectionObserver(
      (entries) => {
        visible = entries[0]?.isIntersecting ?? true;
        if (!reduced && !paused) {
          if (visible) {
            lastTs = 0;
            rafId = requestAnimationFrame(loop);
          } else {
            cancelAnimationFrame(rafId);
          }
        }
      },
      { threshold: 0.05 },
    );
    io.observe(root);

    /* ── Init ──────────────────────────────────────────────────────── */

    sizeCanvas();
    seedConsole();
    updateKv(kvCells, mode, sim);
    if (reduced) {
      drawStaticFrame();
    } else {
      // Prime the trace so the first frame is rich, not empty.
      for (let i = 0; i < 60; i++) {
        tick(sim, TICK_MS);
        trace.push({ pre: [...sim.pre], dec: sim.dec, burst: sim.mtpLeft > 0 ? 1 : 0 });
      }
      drawFrame(0.55);
      if (visible) rafId = requestAnimationFrame(loop);
    }
    // Populate the readouts immediately so they are never frozen at 0.
    updateReadout();

    const ro = new ResizeObserver(() => sizeCanvas());
    ro.observe(scope);
  }

  /* ── Bootstrap (MUST be last — all const/let declarations above) ── */

  const boot = (): void => {
    const stage = document.getElementById('live-engine') as HTMLElement | null;
    if (stage && !stage.hasAttribute('data-live-init')) {
      stage.setAttribute('data-live-init', 'true');
      initLiveEngine(stage);
    }
  };

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot, { once: true });
  } else {
    boot();
  }
  document.addEventListener('astro:after-swap', () => boot());
})();
