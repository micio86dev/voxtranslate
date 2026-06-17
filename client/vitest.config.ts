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
        // Translation-engine selection: pure preference/lang helpers — the
        // fetch + selector rendering needs a DOM (spec 0093).
        'src/scripts/engines.ts',
        // Premium PCM helpers: pure Float32↔PCM16 + base64 decode — the
        // AudioWorklet capture/playback needs real audio APIs (spec 0093).
        'src/scripts/pcm.ts',
        // Composite recording: only the pure-math modules — the compositor /
        // mixer / recorder need real canvas + audio APIs (covered manually).
        'src/scripts/recording/layout.ts',
        'src/scripts/recording/utils.ts',
        // AI report: only the pure markdown/cost helpers — the slot UI needs a DOM.
        'src/scripts/report-md.ts',
        // Follow-up email: only the pure recipient helpers — the composer needs a DOM.
        'src/scripts/email-utils.ts',
        // In-call invite: pure link/email parsing helpers — the panel needs a DOM (spec 0080).
        'src/scripts/invite.ts',
        // Reaction throttle: pure sliding-window limiter (issue #15).
        'src/scripts/reaction-rate-limit.ts',
        // Voice-command timer: only the pure intent parser + formatters — the
        // CallTimer badge/countdown needs a DOM (covered manually) (spec 0052).
        'src/scripts/timer-intent.ts',
      ],
      reporter: ['text', 'json-summary'],
      reportsDirectory: './coverage-unit',
      thresholds: { lines: 85, functions: 85 },
    },
  },
});
