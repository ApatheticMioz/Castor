# Castor Technologies — Website

Production static site for [castor-technologies.vercel.app](https://castor-technologies.vercel.app).
Astro 5 (static output) + TypeScript (strict) + Tailwind CSS v4 (`@tailwindcss/vite`) + GSAP 3 + Lenis + Fontsource.

## Stack

| Layer | Package |
|---|---|
| Framework | `astro@5` (`output: 'static'`, Shiki theme `github-dark-default`) |
| Styling | `tailwindcss@4` via `@tailwindcss/vite` plugin; tokens in `src/styles/tokens.css`, theme bindings + anti-slop utilities in `src/styles/global.css` |
| Type | `tsconfig.json` extends `astro/tsconfigs/strict`; type gate via `pnpm run check` (`astro check` / `@astrojs/check`) |
| Motion | `gsap@3` (ScrollTrigger) + `lenis@1` smooth scroll, wired in `src/layouts/Layout.astro`, disabled under `prefers-reduced-motion: reduce` |
| Fonts | `@fontsource-variable/archivo` (display), `@fontsource/ibm-plex-sans`, `@fontsource/ibm-plex-mono` — self-hosted, no CDN |

## Layout

```
website/
├── astro.config.mjs          # static output, tailwindcss() Vite plugin, shiki theme, site URL
├── tsconfig.json             # extends astro/tsconfigs/strict
├── vercel.json               # framework: astro, clean URLs, security headers, immutable /_astro/*
├── pnpm-workspace.yaml       # pnpm build-script allowlist (esbuild, sharp)
├── public/
│   └── assets/               # logo.svg, favicon.svg, pure1_dioscuri_dyad.svg (from docs/assets)
└── src/
    ├── styles/
    │   ├── tokens.css        # Obsidian Hardware Instrument Chassis design tokens
    │   └── global.css        # Tailwind v4 @theme bindings, .spec-grid, .tactile-key, .tabular, reduced-motion
    ├── data/
    │   └── telemetry.ts      # 100% verified Nov 19 2026 ground-truth constants
    ├── layouts/
    │   └── Layout.astro      # meta/OG tags, favicon, Fontsource weights, Lenis↔GSAP sync
    └── pages/
        └── index.astro       # sole entry point
```

## Workflow

```bash
pnpm install       # installs; pnpm-workspace.yaml permits esbuild/sharp build scripts
pnpm dev           # local dev server
pnpm build         # static build → dist/
pnpm preview       # serve the built site
pnpm check         # astro check (strict TS)
npx tsc --noEmit   # standalone type gate
```

## Deploy (Vercel)

Zero-config: import the repo, set the root directory to `website`. `vercel.json` pins
`framework: "astro"`, clean URLs, strict security headers (`X-Content-Type-Options: nosniff`,
`X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`) and immutable caching for `/_astro/*`.

CLI equivalent: `vercel --prod` from this directory.

## Design invariants (enforced in `global.css`)

- Teenage Engineering hairline chassis: `.spec-grid` = 1px-gap grid, 1px hairline border, zero border-radius, zero drop shadows.
- `.tactile-key` = hardware-pressed key surface (inset highlight, 3px radius, pressed state).
- `.tabular` = tabular numerals for every telemetry readout.
- Signals: `--signal-local` (amber) for local silicon / primary CTA, `--signal-cloud` (cobalt) for cloud / MCP, `--signal-ok` for PASS, `--signal-block` for security refusals.
- No diffuse neon glows, no purple gradients, no rounded-2xl bloat.
- Full `prefers-reduced-motion: reduce` handling (Lenis skipped, transitions collapsed).
