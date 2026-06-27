import { defineConfig } from 'astro/config';

// Static client. The WebSocket server runs separately (see PUBLIC_WS_HOST).
// COVERAGE=1 produces an un-minified build with sourcemaps so Playwright V8
// coverage maps back to src/scripts/*.ts.
const coverage = process.env.COVERAGE === '1';

export default defineConfig({
  // Canonical origin for SEO: powers Astro.site so the layout emits absolute
  // canonical / Open Graph / sitemap URLs. Production domain (see CORS allowlist).
  site: 'https://voxtranslate.app',
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
