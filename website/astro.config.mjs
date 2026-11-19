// @ts-check
import { defineConfig } from 'astro/config';
import tailwindcss from '@tailwindcss/vite';
import sitemap from '@astrojs/sitemap';

// Dual-target deployment:
//   Vercel         → base "/"            (default; ASTRO_BASE unset)
//   GitHub Pages   → base "/Castor"      (ASTRO_BASE=/Castor in the Pages workflow)
// Every root-relative asset path in src/ is derived from
// `import.meta.env.BASE_URL`, which mirrors this `base` at build time.
// The sitemap plugin emits sitemap-index.xml + sitemap-0.xml with URLs
// derived from `site` + `base`, so both targets get correctly-prefixed
// absolute URLs (…/Castor/… on Pages, root on Vercel).
export default defineConfig({
  base: process.env.ASTRO_BASE || '/',
  site: process.env.ASTRO_SITE || 'https://castor-technologies.vercel.app',
  output: 'static',
  integrations: [sitemap()],
  vite: {
    plugins: [tailwindcss()],
  },
  markdown: {
    shikiConfig: {
      theme: 'github-dark-default',
    },
  },
});
