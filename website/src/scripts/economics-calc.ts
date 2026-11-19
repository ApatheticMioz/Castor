/**
 * Castor — Economics Engine (pure, isomorphic)
 *
 * Shared by the EconomicsCalc frontmatter (SSR initial render) and the
 * client bundle (live recomputation on slider/toggle change).
 *
 * Calibration — all values from the 76-day Castor ledger (Sep 4 – Nov 19, 2026):
 *  - Agentic token mix: 97.7% prompt / 2.3% output tokens.
 *  - Cloud cache: the "Real Agentic" profile pays 30% of the prompt rate
 *    for the 70%-discounted cached portion (no inflated cloud bills).
 *  - Local energy: 52.1 kWh per 2.62B tokens ≈ 19.89 kWh / 1B tokens
 *    at ~250 W sustained on an RTX 3090.
 *  - Hardware: $850 used RTX 3090 per 4-engineer node.
 */

import type { CloudModel } from '../data/telemetry';

/* ── Verified calibration constants ─────────────────────────────────── */

export const PROMPT_SHARE = 0.977;
export const OUTPUT_SHARE = 0.023;
export const CACHED_PROMPT_PAY_FRACTION = 0.3; // 70% cache discount → pay 30%
export const KWH_PER_1B_TOKENS = 19.89;
export const USED_RTX3090_USD = 850;
export const ENGINEERS_PER_NODE = 4;

/* ── Types ──────────────────────────────────────────────────────────── */

export interface CalcInputs {
  model: CloudModel;
  teamSize: number; // 1–100 engineers
  tokensPerEngineerM: number; // 20–500M tokens / month / engineer
  localOffloadPct: number; // 50–95%
  electricityRate: number; // $/kWh
  useRealAgenticCache: boolean; // true = 70% cached-prompt discount
}

export interface CalcOutputs {
  totalTokensM: number;
  /** $ per 1M tokens at the active cache assumption. */
  costPerM: number;
  cloudOnlyUsd: number;
  hybridCloudUsd: number;
  hybridLocalKwh: number;
  hybridLocalUsd: number;
  hybridTotalUsd: number;
  monthlySavingsUsd: number;
  annualSavingsUsd: number;
  nodes: number;
  hardwareCostUsd: number;
  paybackMonths: number | null;
}

/* ── Core computation ───────────────────────────────────────────────── */

export function computeEconomics(i: CalcInputs): CalcOutputs {
  const pf = i.useRealAgenticCache ? CACHED_PROMPT_PAY_FRACTION : 1;
  const costPerM =
    PROMPT_SHARE * i.model.priceInPer1m * pf +
    OUTPUT_SHARE * i.model.priceOutPer1m;

  const totalTokensM = i.teamSize * i.tokensPerEngineerM;
  const localTokensM = totalTokensM * (i.localOffloadPct / 100);
  const cloudTokensM = totalTokensM - localTokensM;

  const cloudOnlyUsd = totalTokensM * costPerM;
  const hybridCloudUsd = cloudTokensM * costPerM;
  const hybridLocalKwh = (localTokensM / 1000) * KWH_PER_1B_TOKENS;
  const hybridLocalUsd = hybridLocalKwh * i.electricityRate;
  const hybridTotalUsd = hybridCloudUsd + hybridLocalUsd;

  const monthlySavingsUsd = cloudOnlyUsd - hybridTotalUsd;
  const annualSavingsUsd = monthlySavingsUsd * 12;

  const nodes = Math.max(1, Math.ceil(i.teamSize / ENGINEERS_PER_NODE));
  const hardwareCostUsd = nodes * USED_RTX3090_USD;
  const paybackMonths =
    monthlySavingsUsd > 0 ? hardwareCostUsd / monthlySavingsUsd : null;

  return {
    totalTokensM,
    costPerM,
    cloudOnlyUsd,
    hybridCloudUsd,
    hybridLocalKwh,
    hybridLocalUsd,
    hybridTotalUsd,
    monthlySavingsUsd,
    annualSavingsUsd,
    nodes,
    hardwareCostUsd,
    paybackMonths,
  };
}

/* ── Display formatters ─────────────────────────────────────────────── */

export function fmtUsd(n: number, digits = 0): string {
  return `$${n.toLocaleString('en-US', {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  })}`;
}

export function fmtUsdCompact(n: number): string {
  if (n >= 1_000_000) return `$${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `$${(n / 1_000).toFixed(1)}k`;
  return `$${n.toFixed(0)}`;
}

export function fmtKwh(n: number): string {
  return `${n >= 100 ? n.toFixed(0) : n.toFixed(1)} kWh`;
}

export function fmtPayback(months: number | null): string {
  if (months === null) return 'N/A — no net savings at these inputs';
  if (months < 2) return `≈ ${(months * 4.345).toFixed(1)} weeks`;
  if (months < 12) return `≈ ${months.toFixed(1)} months`;
  return `≈ ${(months / 12).toFixed(1)} years`;
}

export function fmtTokensM(m: number): string {
  return m >= 1000 ? `${(m / 1000).toFixed(2)}B` : `${Math.round(m)}M`;
}

/* ── Cross-component conversion hook ──────────────────────────────────
 * The economics instrument publishes its live state on `window` so
 * downstream conversion surfaces (DeploymentTiers mailto bodies) can
 * personalize themselves with the visitor's actual workload and savings.
 * Emitted on every state change (preset click, slider input, model
 * select, cache toggle) and once at hydration to cover the SSR default.
 */

export interface EconomicsUpdateDetail {
  modelName: string;
  teamSize: number;
  monthlyTokensM: number;
  annualSavingsFmt: string;
  hybridMonthlyFmt: string;
}

export const ECONOMICS_UPDATE_EVENT = 'castor:economics-update';

export function dispatchEconomicsUpdate(detail: EconomicsUpdateDetail): void {
  window.dispatchEvent(new CustomEvent<EconomicsUpdateDetail>(ECONOMICS_UPDATE_EVENT, { detail }));
}
