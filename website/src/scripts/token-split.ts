/**
 * Token Split Story — scroll-driven 93.7/6.3 split bar + interactive
 * 120K-context speed race (local silicon vs cloud-only frontier loop).
 * All browser access lives in boot() at the very bottom of this file —
 * every const table is declared strictly above it (zero TDZ bugs).
 * The module itself has no top-level DOM reads, so it is SSR-safe.
 */

/* ── Race truth tables (all above boot) ───────────────────────────── */

/** Race durations in seconds of visual time (scaled by rAF clock). */
const RACE = {
  localDur: 3.0,
  cloudDur: 4.4,
  /** Local lane: fraction of duration spent in the warm-prefill phase. */
  localPrefillShare: 0.6,
  /** Warm prefill throughput, November average (ledger truth). */
  prefillTps: 6892,
  /** Decode p95 (ledger truth). */
  decodeTps: 116.6,
  /** KV prefix hit share for the 120K context (ledger truth). */
  kvHitPct: 93.7,
  /** Cloud lane billable endpoint: same 120K-context, 12-turn session
   *  at Opus 5.5 $4/$20 per 1M with uncached context re-sends. */
  cloudCostEnd: 1.84,
  /** Agent turns in the session. */
  cloudTurns: 12,
  /** Local energy for the whole race session (~0.002 kWh @ 244.6 W · 3 s). */
  localKwh: '0.002',
};

type LaneState = 'armed' | 'prefill' | 'decode' | 'done' | 'cloudrun' | 'cloudwait';

interface RaceLanes {
  railLocal: HTMLDivElement | null;
  railCloud: HTMLDivElement | null;
  phaseLocal: HTMLElement | null;
  phaseCloud: HTMLElement | null;
  phaseLocalText: HTMLSpanElement | null;
  phaseCloudText: HTMLSpanElement | null;
  localCost: HTMLElement | null;
  localTel: HTMLElement | null;
  localDone: HTMLElement | null;
  cloudCost: HTMLElement | null;
  cloudTel: HTMLElement | null;
  cloudDone: HTMLElement | null;
  verdict: HTMLElement | null;
  btn: HTMLButtonElement | null;
  section: HTMLElement | null;
}

const REDUCED_MQ = '(prefers-reduced-motion: reduce)';

/* ── Pure helpers (no DOM at module scope) ─────────────────────────── */

function easeOutCubic(t: number): number {
  return 1 - Math.pow(1 - t, 3);
}

function fmtUsd(n: number, decimals: number): string {
  return `$${n.toLocaleString('en-US', {
    minimumFractionDigits: decimals,
    maximumFractionDigits: decimals,
  })}`;
}

/* ── Split bar (Part A): IntersectionObserver width 0% → 93.7% ─────── */

function initSplitBar(section: HTMLElement): void {
  const wrap = section.querySelector<HTMLElement>('#ts-split-bar');
  if (!wrap) return;

  const reduced = window.matchMedia(REDUCED_MQ).matches;
  if (reduced) {
    wrap.classList.add('is-in-view');
    return;
  }

  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (e.isIntersecting) {
          wrap.classList.add('is-in-view');
          io.disconnect();
        }
      }
    },
    { threshold: 0.35 },
  );
  io.observe(wrap);
}

/* ── Speed race (Part B) ───────────────────────────────────────────── */

function collectLanes(section: HTMLElement): RaceLanes {
  return {
    railLocal: section.querySelector<HTMLDivElement>('#ts-rail-local'),
    railCloud: section.querySelector<HTMLDivElement>('#ts-rail-cloud'),
    phaseLocal: section.querySelector<HTMLElement>('#ts-phase-local'),
    phaseCloud: section.querySelector<HTMLElement>('#ts-phase-cloud'),
    phaseLocalText: section.querySelector<HTMLSpanElement>('#ts-phase-local .ts-phase-text'),
    phaseCloudText: section.querySelector<HTMLSpanElement>('#ts-phase-cloud .ts-phase-text'),
    localCost: section.querySelector<HTMLElement>('#ts-local-cost'),
    localTel: section.querySelector<HTMLElement>('#ts-local-tel'),
    localDone: section.querySelector<HTMLElement>('#ts-local-done'),
    cloudCost: section.querySelector<HTMLElement>('#ts-cloud-cost'),
    cloudTel: section.querySelector<HTMLElement>('#ts-cloud-tel'),
    cloudDone: section.querySelector<HTMLElement>('#ts-cloud-done'),
    verdict: section.querySelector<HTMLElement>('.ts-verdict'),
    btn: section.querySelector<HTMLButtonElement>('#ts-race-btn'),
    section,
  };
}

function setPhase(el: HTMLElement | null, text: HTMLSpanElement | null, state: LaneState, msg: string): void {
  if (el) el.dataset.state = state;
  if (text) text.textContent = msg;
}

function resetRace(l: RaceLanes): void {
  if (l.railLocal) l.railLocal.style.transform = 'scaleX(0)';
  if (l.railCloud) l.railCloud.style.transform = 'scaleX(0)';
  setPhase(l.phaseLocal, l.phaseLocalText, 'armed', 'ARMED · 93.7% KV prefix hot in VRAM');
  setPhase(l.phaseCloud, l.phaseCloudText, 'armed', 'ARMED · context resends every turn — no shared KV');
  if (l.localCost) l.localCost.textContent = '$0.000';
  if (l.localTel) l.localTel.textContent = '0.0 tok/s';
  if (l.localDone) l.localDone.textContent = '—';
  if (l.cloudCost) l.cloudCost.textContent = '$0.00';
  if (l.cloudTel) l.cloudTel.textContent = 'T0 / 12';
  if (l.cloudDone) l.cloudDone.textContent = '—';
  if (l.verdict) l.verdict.classList.remove('is-on');
}

function finishLocal(l: RaceLanes): void {
  if (l.railLocal) l.railLocal.style.transform = 'scaleX(1)';
  setPhase(l.phaseLocal, l.phaseLocalText, 'done', 'DONE · 120K refactor applied · cargo test green');
  if (l.localTel) l.localTel.textContent = '116.6 tok/s p95';
  if (l.localDone) l.localDone.textContent = '100% DONE';
}

function finishCloud(l: RaceLanes): void {
  if (l.railCloud) l.railCloud.style.transform = 'scaleX(1)';
  setPhase(l.phaseCloud, l.phaseCloudText, 'done', 'DONE · 12 turns re-sent · billable session complete');
  if (l.cloudCost) l.cloudCost.textContent = fmtUsd(RACE.cloudCostEnd, 2);
  if (l.cloudTel) l.cloudTel.textContent = 'T12 / 12';
  if (l.cloudDone) l.cloudDone.textContent = '100% DONE';
  if (l.verdict) l.verdict.classList.add('is-on');
}

function runInstant(l: RaceLanes): void {
  resetRace(l);
  finishLocal(l);
  finishCloud(l);
  if (l.btn) {
    l.btn.textContent = 'REPLAY RACE ↻';
  }
}

interface RaceRun {
  raf: number;
  t0: number;
  localDone: boolean;
  cloudDone: boolean;
}

function runRace(l: RaceLanes): void {
  resetRace(l);
  if (l.btn) l.btn.textContent = 'REPLAY RACE ↻';

  const run: RaceRun = { raf: 0, t0: 0, localDone: false, cloudDone: false };

  const frame = (now: number): void => {
    if (run.t0 === 0) run.t0 = now;
    const elapsed = (now - run.t0) / 1000;

    // ── Lane 1 · local: prefill (0→prefillShare) then MTP decode ──────
    if (!run.localDone) {
      const lp = Math.min(1, elapsed / RACE.localDur);
      const prefillEnd = RACE.localPrefillShare;
      if (lp < prefillEnd) {
        const pp = lp / prefillEnd;
        if (l.railLocal) l.railLocal.style.transform = `scaleX(${(pp * prefillEnd).toFixed(4)})`;
        setPhase(
          l.phaseLocal,
          l.phaseLocalText,
          'prefill',
          `WARM PREFILL · ${RACE.kvHitPct}% KV HIT · ${RACE.prefillTps.toLocaleString('en-US')} tok/s`,
        );
      } else {
        const dp = (lp - prefillEnd) / (1 - prefillEnd);
        const total = prefillEnd + dp * (1 - prefillEnd);
        if (l.railLocal) l.railLocal.style.transform = `scaleX(${total.toFixed(4)})`;
        setPhase(l.phaseLocal, l.phaseLocalText, 'decode', 'MTP DECODE + AST EDIT · cargo test · 116.6 tok/s p95');
        if (dp >= 1) {
          run.localDone = true;
          finishLocal(l);
        }
      }
    }

    // ── Lane 2 · cloud: 12 turns, slow + escalating bill ─────────────
    if (!run.cloudDone) {
      const cp = Math.min(1, elapsed / RACE.cloudDur);
      if (l.railCloud) l.railCloud.style.transform = `scaleX(${cp.toFixed(4)})`;
      const turn = Math.min(RACE.cloudTurns, 1 + Math.floor(cp * RACE.cloudTurns));
      setPhase(
        l.phaseCloud,
        l.phaseCloudText,
        'cloudrun',
        `TURN ${turn} / ${RACE.cloudTurns} · re-sending ${turn === 1 ? '120K' : '120K'}-ctx uncached reads`,
      );
      if (l.cloudCost) l.cloudCost.textContent = fmtUsd(RACE.cloudCostEnd * cp, 2);
      if (l.cloudTel) l.cloudTel.textContent = `T${turn} / ${RACE.cloudTurns}`;
      if (cp >= 1) {
        run.cloudDone = true;
        finishCloud(l);
      }
    }

    if (!run.localDone || !run.cloudDone) {
      run.raf = requestAnimationFrame(frame);
    } else if (l.btn) {
      l.btn.textContent = 'REPLAY RACE ↻';
    }
  };

  run.raf = requestAnimationFrame(frame);
}

function initRace(section: HTMLElement): void {
  const lanes = collectLanes(section);
  if (!lanes.railLocal || !lanes.railCloud) return;

  const reduced = window.matchMedia(REDUCED_MQ).matches;

  if (lanes.btn) {
    lanes.btn.addEventListener('click', () => {
      // Explicit click: reduced motion gets the instant completed state
      // (no rAF playback); normal motion runs the animated race.
      if (reduced) runInstant(lanes);
      else runRace(lanes);
    });
  }

  if (reduced) {
    // Reduced motion: no automatic viewport-entry playback — land on the
    // clean completed state, but keep the demo button fully enabled for
    // explicit clicks (never a permanently disabled button).
    runInstant(lanes);
    if (lanes.btn) {
      lanes.btn.classList.remove('is-disabled');
      lanes.btn.removeAttribute('aria-disabled');
      lanes.btn.textContent = 'REPLAY RACE ↻';
    }
    return;
  }

  // Auto-play once when the chassis scrolls into view.
  let armed = false;
  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (e.isIntersecting && !armed) {
          armed = true;
          io.disconnect();
          runRace(lanes);
        }
      }
    },
    { threshold: 0.4 },
  );
  io.observe(lanes.section ?? section);
}

/* ── bootTokenSplit() — the only DOM bootstrap, strictly at bottom ──── */

function bootTokenSplit(): void {
  const section = document.getElementById('speed-race');
  if (!section) return;
  initSplitBar(section);
  initRace(section);
}

if (typeof document !== 'undefined') {
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', bootTokenSplit, { once: true });
  } else {
    bootTokenSplit();
  }
  // Astro partial reload re-mount: re-bootstrap idempotently.
  document.addEventListener('astro:after-swap', bootTokenSplit);
}
