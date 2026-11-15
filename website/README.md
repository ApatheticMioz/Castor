# Castor Official Landing Page & Web Presence

This directory contains the production-ready, high-converting marketing landing page for Castor Technologies. It satisfies all criteria specified in Section 2.6 of the CS4012 Project Specification.

## Key Features Built In:
1. **Interactive Real-Time ROI Calculator**:
   - Allows prospective clients to slide developer count and daily token volume.
   - Calculates monthly Claude Sonnet token cost vs Castor GPU electricity cost.
   - Computes annual net savings (99.4% arbitrage).
2. **Ground-Truth Production Telemetry**:
   - Displays live metrics from `castor stats`: 2.56 Billion tokens processed, 93.7% cache hit rate, 45,631 turns, 99.8% energy arbitrage.
3. **Core Architectural Features**:
   - 5-layer zero-trust containment sandbox (137 vectors tested).
   - In-memory AST syntax validation gate with tree-sitter.
   - KV-cache slot semaphore with dynamic token-budget admission.
4. **Three Transparent Commercial Pricing Tiers**:
   - Tier 1: Castor Core (Free / Open Source AGPLv3).
   - Tier 2: Turnkey Swarm Setup Sprint ($1,500 one-time fixed fee for agencies).
   - Tier 3: Enterprise Air-Gap Compliance ($25,000 / year / enclave).
5. **Interactive Booking / Lead Capture Form**:
   - Collects lead name, company, team size, GPU hardware, and selected package with instant confirmation.

---

## How to Deploy to Vercel in 60 Seconds:

### Option A: Via Vercel CLI (Fastest)
```bash
# Navigate to the website directory
cd D:\LLM_Ecosystem\website

# Install vercel CLI globally if not already installed
npm install -g vercel

# Deploy directly to production
vercel --prod
```

### Option B: Via GitHub & Vercel Dashboard
1. Go to [vercel.com](https://vercel.com) and log in.
2. Click **Add New Project** -> **Import Git Repository** (`Castor`).
3. Set **Root Directory** to `website`.
4. Click **Deploy**!
5. Your live URL will be active immediately (e.g. `https://castor-technologies.vercel.app`).
