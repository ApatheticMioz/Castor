/**
 * Spec Ledger — 3-tab hardware switcher interactivity.
 * Hairline tablist: click + full keyboard support (arrow keys, Home/End,
 * roving tabindex). Panels swap instantly; no motion under
 * prefers-reduced-motion (the global reduced-motion block in global.css
 * already zeroes any residual transitions).
 */

type TabId = 'ledger' | 'live' | 'tiers';

const TAB_IDS: TabId[] = ['ledger', 'live', 'tiers'];

function initSpecLedger(root: HTMLElement): void {
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

  // Sanity: ensure exactly the ledger panel is visible on init.
  void TAB_IDS;
  activate(tabs[0]);
}

if (typeof document !== 'undefined') {
  document.addEventListener('astro:after-swap', () => {
    const root = document.getElementById('telemetry');
    if (root) initSpecLedger(root);
  });

  const root = document.getElementById('telemetry');
  if (root) initSpecLedger(root);
}
