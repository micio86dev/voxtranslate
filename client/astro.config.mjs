import { defineConfig } from 'astro/config';
import vercel from '@astrojs/vercel';
import node from '@astrojs/node';

// Mostly-static client. The WebSocket server runs separately (see PUBLIC_WS_HOST).
// COVERAGE=1 produces an un-minified build with sourcemaps so Playwright V8
// coverage maps back to src/scripts/*.ts.
//
// Output stays `static` (the default): every page prerenders to HTML at build time
// EXCEPT the webinar participant page `/w/[code]`, which opts into on-demand rendering
// via `export const prerender = false` so it can server-side fetch the webinar for its
// <title>/OG tags (a shared link must preview correctly). The Vercel adapter supplies
// the serverless runtime that renders only that one route on-demand; the rest of the
// app ships as unchanged static HTML.
const coverage = process.env.COVERAGE === '1';

export default defineConfig({
  // Serverless runtime for the single on-demand route (/w/[code]); every other page
  // stays prerendered static (output defaults to `static`). An adapter is required for
  // any `prerender = false` route to build, so one is always applied.
  //
  // Production ships on Vercel (`astro build` → `.vercel/output/`). The Playwright e2e
  // webServer, however, builds with COVERAGE=1 and serves via `astro preview` — which
  // the Vercel adapter does NOT support ("does not support the preview command"). So the
  // coverage/e2e build swaps in the Node standalone adapter: same on-demand route, but
  // `astro preview` runs its server. COVERAGE=1 is set only by the e2e webServer command.
  adapter: coverage ? node({ mode: 'standalone' }) : vercel(),
  // Canonical origin for SEO: powers Astro.site so the layout emits absolute
  // canonical / Open Graph / sitemap URLs. Production domain (see CORS allowlist).
  // `import.meta.env` does not exist in the Astro config, so read process.env here.
  site: (process.env.PUBLIC_SITE_ORIGIN || 'https://app.voxtranslate.app').replace(/\/$/, ''),
  server: {
    port: 4321,
    host: true,
  },
  // The floating dev toolbar overlaps the bottom control bar; not shipped in builds.
  devToolbar: { enabled: false },
  // Inline ALL stylesheets into the HTML so the first paint isn't render-blocked
  // by separate CSS round-trips (Lighthouse flagged ~480ms of render-blocking
  // Base+index CSS). 'auto' left them linked (raw size > Vite's 4KB inline limit);
  // 'always' inlines them outright. Trade-off — the CSS is re-sent per page instead
  // of cached — is worth it on this 5-page, homepage-first static site.
  build: { inlineStylesheets: 'always' },
  vite: {
    build: {
      sourcemap: coverage ? 'inline' : false,
      minify: coverage ? false : 'esbuild',
    },
  },
});
