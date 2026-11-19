/**
 * Castor — Ground-Truth Telemetry & Architecture Data Layer
 * All values are 100% verified as of November 19, 2026.
 * Source of record: `castor stats` (76-day ledger Sep 4 – Nov 19, 2026),
 * `nvidia-smi` + vLLM 0.33.0 runtime snapshot, `docs/engine-profiles.md`,
 * `scripts/bonsai/README.md`, Qwen3.8-27B model card (Aug 14, 2026),
 * and the Castor `src/` codebase.
 */

/* ── Types ─────────────────────────────────────────────────────────── */

export interface Ledger76Day {
  windowStart: string;
  windowEnd: string;
  days: number;
  totalTokens: number;
  totalTokensDisplay: string;
  promptTokens: number;
  promptTokensDisplay: string;
  cachedTokens: number;
  cachedTokensDisplay: string;
  cacheHitPct: number;
  completionTokens: number;
  completionTokensDisplay: string;
  reasoningTokens: number;
  reasoningTokensDisplay: string;
  reasoningSharePct: number;
  turns: number;
  sessions: number;
  toolCalls: number;
  toolSuccessPct: number;
  surgicalEditRatio: number;
  surgicalEditDetail: string;
  gpuComputeHours: number;
  gpuComputeDisplay: string;
  sustainedDecodeTps: number;
  novDecodeP50Tps: number;
  novDecodeP95Tps: number;
  novDecodeMaxTps: number;
  peakSpecBurstTps: number;
  allTimePrefillTps: number;
  novPrefillAvgTps: number;
  novPrefillP95Tps: number;
  novPrefillMaxTps: number;
  cloudCounterfactualUsd: number;
  electricityUsd: number;
  electricityKwh: number;
  electricityRateUsdPerKwh: number;
  netSavingsUsd: number;
  netSavingsPct: number;
}

export interface HardwareSnapshot {
  asOf: string;
  gpu: {
    model: string;
    vramTotalMiB: number;
    vramUsedMiB: number;
    powerDrawW: number;
    powerLimitW: number;
    tempC: number;
    driver: string;
  };
  engine: {
    name: string;
    model: string;
    maxModelLen: number;
    liveDraftAcceptancePct: number;
    meanAcceptanceLen: number;
    liveAvgTtftS: number;
    livePrefixHitPct: number;
  };
}

export interface VramTier {
  vram: string;
  engine: string;
  context: string;
  decode: string;
  highlight?: boolean;
}

export interface CloudModel {
  name: string;
  vendor: 'anthropic' | 'openai' | 'google';
  priceInPer1m: number;
  priceOutPer1m: number;
  priceNote?: string;
  aaScore?: number;
  note?: string;
}

export interface QwenBench {
  suite: string;
  score: number;
}

export interface ArchitectureSpec {
  pathLayers: number;
  securityTestVectors: number;
  securityBlocked: number;
  securityAllowed: number;
  treeSitterGrammars: number;
  mcpTools: number;
  toolCount: {
    leanCore: number;
    ast: number;
    paper: number;
  };
  ports: {
    engine: number;
    status: number;
    proxy: number;
  };
  links: {
    github: string;
    npm: string;
    email: string;
  };
}

/* ── Data: Live 76-day ledger (Sep 4 – Nov 19, 2026, `castor stats`) ─ */

export const LEDGER_76DAY: Ledger76Day = {
  windowStart: '2026-09-04',
  windowEnd: '2026-11-19',
  days: 76,
  totalTokens: 2_619_320_414,
  totalTokensDisplay: '2.62B',
  promptTokens: 2_560_177_056,
  promptTokensDisplay: '2.56B',
  cachedTokens: 2_389_785_370,
  cachedTokensDisplay: '2.39B',
  cacheHitPct: 93.7,
  completionTokens: 59_214_272,
  completionTokensDisplay: '59.21M',
  reasoningTokens: 50_676_986,
  reasoningTokensDisplay: '50.68M',
  reasoningSharePct: 85.6,
  turns: 46_862,
  sessions: 1_130,
  toolCalls: 63_901,
  toolSuccessPct: 96.9,
  surgicalEditRatio: 3.59,
  surgicalEditDetail: '7,624 edit_file vs 2,125 write_file',
  gpuComputeHours: 208.37,
  gpuComputeDisplay: '208h 22m',
  sustainedDecodeTps: 70.8,
  novDecodeP50Tps: 75.5,
  novDecodeP95Tps: 116.6,
  novDecodeMaxTps: 139.7,
  peakSpecBurstTps: 1_995.7,
  allTimePrefillTps: 2_987,
  novPrefillAvgTps: 6_892,
  novPrefillP95Tps: 16_365,
  novPrefillMaxTps: 19_384,
  cloudCounterfactualUsd: 5_712.5,
  electricityUsd: 9.38,
  electricityKwh: 52.1,
  electricityRateUsdPerKwh: 0.18,
  netSavingsUsd: 5_703.12,
  netSavingsPct: 99.84,
};

/* ── Data: Live hardware & vLLM 0.33.0 snapshot (Nov 19, 2026) ─────── */

export const HARDWARE_SNAPSHOT: HardwareSnapshot = {
  asOf: '2026-11-19',
  gpu: {
    model: 'NVIDIA GeForce RTX 3090',
    vramTotalMiB: 24_576,
    vramUsedMiB: 23_869,
    powerDrawW: 244.6,
    powerLimitW: 262.5,
    tempC: 56,
    driver: '616.92',
  },
  engine: {
    name: 'vLLM 0.33.0',
    model: 'Qwen3.8-27B-W4A16-AutoRound-fast',
    maxModelLen: 150_000,
    liveDraftAcceptancePct: 70.5,
    meanAcceptanceLen: 3.45,
    liveAvgTtftS: 3.22,
    livePrefixHitPct: 92.6,
  },
};

/* ── Data: Verified VRAM hardware tiers ───────────────────────────── */

export const VRAM_TIERS: VramTier[] = [
  {
    vram: '8 GB VRAM',
    engine: 'llama.cpp',
    context: '8K ctx',
    decode: '~22–26 tok/s',
  },
  {
    vram: '12 GB VRAM',
    engine: 'llama.cpp + DFlash2',
    context: '64K ctx',
    decode: '~38.6 tok/s',
  },
  {
    vram: '16 GB VRAM',
    engine: 'llama.cpp + DFlash2',
    context: '128K ctx',
    decode: '~53.0 tok/s',
  },
  {
    vram: '24 GB VRAM (RTX 3090)',
    engine: 'vLLM 0.33.0 + Qwen3.8-27B-W4A16 + MTP',
    context: '150K KV pool',
    decode: '75.5 p50 / 116.6 p95 tok/s decode · 6,892 avg / 19,384 max tok/s warm prefill',
    highlight: true,
  },
];

/* ── Data: Nov 2026 cloud frontier models & API pricing (per 1M tokens) ─ */

export const CLOUD_MODELS: CloudModel[] = [
  {
    name: 'Claude Fable 5.2',
    vendor: 'anthropic',
    priceInPer1m: 8,
    priceOutPer1m: 40,
    aaScore: 65,
  },
  {
    name: 'GPT-6.2 Astra',
    vendor: 'openai',
    priceInPer1m: 8,
    priceOutPer1m: 40,
    aaScore: 62,
  },
  {
    name: 'Claude Opus 5.5',
    vendor: 'anthropic',
    priceInPer1m: 4,
    priceOutPer1m: 20,
    aaScore: 59,
    note: '#1 Coding Agent',
  },
  {
    name: 'Gemini 4 Carbon / Argon',
    vendor: 'google',
    priceInPer1m: 4,
    priceOutPer1m: 20,
    priceNote: 'standard post-Dec 1; $2/$10 intro',
    aaScore: 60,
  },
  {
    name: 'Claude Sonnet 5.5',
    vendor: 'anthropic',
    priceInPer1m: 2,
    priceOutPer1m: 10,
    priceNote: 'Standard',
  },
  {
    name: 'GPT-6.2 Sol',
    vendor: 'openai',
    priceInPer1m: 2,
    priceOutPer1m: 10,
    priceNote: 'Standard',
  },
  {
    name: 'Claude Haiku 5.5',
    vendor: 'anthropic',
    priceInPer1m: 0.25,
    priceOutPer1m: 1.25,
    priceNote: '$0.10–$0.50 / $0.50–$2.50 · Budget',
  },
];

export const CLOUD_MODELS_VERIFIED_ON = '2026-11-16';
export const AA_BENCH_VERSION = 'AA v5.0.1';

/* ── Data: Qwen3.8-27B Apache-2.0 vendor benchmarks (Aug 14, 2026 model card) ─ */

export const QWEN38_BENCHMARKS: QwenBench[] = [
  { suite: 'SWE-bench Pro', score: 61.7 },
  { suite: 'LiveCodeBench v6', score: 90.3 },
  { suite: 'Terminal-Bench 2.1', score: 73.0 },
  { suite: 'OSWorld-Verified', score: 84.3 },
];

export const QWEN38_MODEL_CARD_DATE = '2026-08-14';
export const QWEN38_LICENSE = 'Apache-2.0';

/* ── Data: Verified sandbox & architecture specs (from `src/`) ────── */

export const ARCHITECTURE: ArchitectureSpec = {
  pathLayers: 5,
  securityTestVectors: 137,
  securityBlocked: 123,
  securityAllowed: 14,
  treeSitterGrammars: 27,
  mcpTools: 3,
  toolCount: {
    leanCore: 8,
    ast: 2,
    paper: 1,
  },
  ports: {
    engine: 18_020,
    status: 18_021,
    proxy: 18_022,
  },
  links: {
    github: 'https://github.com/ApatheticMioz/Castor',
    npm: 'https://www.npmjs.com/package/mcp-castor',
    email: 'mailto:ApatheticMioz@gmail.com',
  },
};

/* ── Formatting helpers ───────────────────────────────────────────── */

export function formatInt(n: number): string {
  return n.toLocaleString('en-US');
}

export function formatUsd(n: number): string {
  return `$${n.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
}

export function formatPct(n: number): string {
  return `${n.toLocaleString('en-US')}%`;
}

export function formatTps(n: number): string {
  return `${n.toLocaleString('en-US')}`;
}
