/**
 * Spec Ledger — 3-tab hardware switcher + staggered tool-distribution bars.
 * The odometer counters on Tab 1 are driven by the Layout's global
 * `[data-count-to]` bootstrap; this module owns tab activation and the
 * scaleX(0) → scaleX(1) bar animation on viewport entry (instant under
 * prefers-reduced-motion). All browser work lives in boot() at the bottom.
 */

type TabId = 'ledger' | 'live' | 'tiers';

const BAR_STAGGER_MS = 70;

function initTabs(root: HTMLElement): void {
  const tabs = Array.from(root.querySelectorAll<HTMLButtonElement>('[role="tab"]'));
  const panels = Array.from(root.querySelectorAll<HTMLElement>('.sl-panel'));
  if (tabs.length === 0) return;

  const panelFor = (tab: HTMLButtonElement): HTMLElement | null => {
    const id = tab.id.replace(/^tab-/, '');
    return root.querySelector<HTMLElement>(`#panel-${id}`);
  };

  const activate = (tab: HTMLButtonElement): void => {
    for (const t of tabs) {
      const selected = t === tab;
      t.setAttribute('aria-selected', selected ? 'true' : 'false');
      t.tabIndex = selected ? 0 : -1;
      t.classList.toggle('is-active', selected);
    }
    for (const p of panels) {
      p.hidden = p !== panelFor(tab);
    }
    // Entering a tab may reveal new bars — refresh their trigger.
    refreshBars(root);
  };

  tabs.forEach((tab, i) => {
    tab.addEventListener('click', () => activate(tab));
    tab.addEventListener('keydown', (e: KeyboardEvent) => {
      let next = -1;
      if (e.key === 'ArrowRight') next = (i + 1) % tabs.length;
      else if (e.key === 'ArrowLeft') next = (i - 1 + tabs.length) % tabs.length;
      else if (e.key === 'Home') next = 0;
      else if (e.key === 'End') next = tabs.length - 1;
      if (next >= 0) {
        e.preventDefault();
        activate(tabs[next]);
        tabs[next].focus();
      }
    });
  });

  // Exactly the first panel is visible on init.
  activate(tabs[0]);
}

/* ── Staggered horizontal bars ─────────────────────────────────────── */

function armBars(root: HTMLElement): void {
  const bars = Array.from(root.querySelectorAll<HTMLElement>('.sl-toolbar'));
  for (const bar of bars) {
    bar.style.setProperty('--bar-final', `${bar.dataset.barWidth ?? '100'}%`);
    bar.classList.add('is-armed');
  }
  if (bars.length === 0) return;

  const reduced = window.matchMedia('(prefers-reduced-motion: reduce)');
  if (reduced.matches) {
    // Instant: final widths, no motion.
    for (const bar of bars) bar.classList.remove('is-armed');
    return;
  }

  const grow = (): void => {
    bars.forEach((bar, i) => {
      bar.style.setProperty('--bar-delay', `${i * BAR_STAGGER_MS}ms`);
      bar.classList.remove('is-armed');
      bar.classList.add('is-grown');
    });
  };

  // Trigger from the section's visible panel container.
  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (e.isIntersecting) {
          grow();
          io.disconnect();
        }
      }
    },
    { threshold: 0.25 },
  );
  io.observe(root);
}

let barsArmed = false;
function refreshBars(root: HTMLElement): void {
  if (barsArmed) return;
  barsArmed = true;
  armBars(root);
}

/* ── bootSpecLedger() — the only DOM bootstrap, strictly at bottom ──── */

function bootSpecLedger(): void {
  const root = document.getElementById('telemetry');
  if (!root) return;
  initTabs(root);
  refreshBars(root);
}

if (typeof document !== 'undefined') {
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', bootSpecLedger, { once: true });
  } else {
    bootSpecLedger();
  }
  document.addEventListener('astro:after-swap', bootSpecLedger);
}
