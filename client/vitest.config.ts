import { defineConfig } from 'vitest/config';

// Unit tests for the client's browser-glue modules. Browser APIs (WebRTC, audio,
// canvas, IndexedDB, SpeechSynthesis, service worker) are mocked with minimal fakes;
// modules needing a DOM opt into jsdom per-file via a `// @vitest-environment jsdom`
// pragma on the first line.
export default defineConfig({
  test: {
    include: ['src/scripts/**/*.test.ts'],
    environment: 'node',
    coverage: {
      provider: 'v8',
      // Every module under src/scripts is unit-tested EXCEPT app.ts — the ~6.3k-line
      // call/session controller, which is exercised by the Playwright e2e suite (its
      // own monocart coverage report), not unit tests. Type-only and test files are
      // dropped from the measured set. `all` (vitest default) reports untouched files
      // too, so a new untested module surfaces as a gap instead of hiding.
      include: ['src/scripts/**/*.ts'],
      exclude: ['src/scripts/**/*.test.ts', 'src/scripts/app.ts', 'src/scripts/**/types.ts'],
      reporter: ['text', 'json-summary'],
      reportsDirectory: './coverage-unit',
      thresholds: { lines: 85, functions: 85 },
    },
  },
});
