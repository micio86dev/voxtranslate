import { defineConfig } from 'vitest/config';

// Unit tests for the browser-glue modules whose error/edge branches are hard to
// reach via e2e (WebRTC mesh, audio capture). Browser APIs are mocked.
export default defineConfig({
  test: {
    include: ['src/scripts/**/*.test.ts'],
    environment: 'node',
    coverage: {
      provider: 'v8',
      include: [
        'src/scripts/webrtc.ts',
        'src/scripts/audio-capture.ts',
        'src/scripts/auth.ts',
        'src/scripts/content.ts',
        // Composite recording: only the pure-math modules — the compositor /
        // mixer / recorder need real canvas + audio APIs (covered manually).
        'src/scripts/recording/layout.ts',
        'src/scripts/recording/utils.ts',
        // AI report: only the pure markdown/cost helpers — the slot UI needs a DOM.
        'src/scripts/report-md.ts',
        // Follow-up email: only the pure recipient helpers — the composer needs a DOM.
        'src/scripts/email-utils.ts',
        // Reaction throttle: pure sliding-window limiter (issue #15).
        'src/scripts/reaction-rate-limit.ts',
      ],
      reporter: ['text', 'json-summary'],
      reportsDirectory: './coverage-unit',
      thresholds: { lines: 85, functions: 85 },
    },
  },
});
