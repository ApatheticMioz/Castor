// @ts-check
import { defineConfig } from 'astro/config';
import tailwindcss from '@tailwindcss/vite';

// Dual-target deployment:
//   Vercel         → base "/"            (default; ASTRO_BASE unset)
//   GitHub Pages   → base "/Castor"      (ASTRO_BASE=/Castor in the Pages workflow)
// Every root-relative asset path in src/ is derived from
// `import.meta.env.BASE_URL`, which mirrors this `base` at build time.
export default defineConfig({
  base: process.env.ASTRO_BASE || '/',
  site: process.env.ASTRO_SITE || 'https://castor-technologies.vercel.app',
  output: 'static',
  vite: {
    plugins: [tailwindcss()],
  },
  markdown: {
    shikiConfig: {
      theme: 'github-dark-default',
    },
  },
});
