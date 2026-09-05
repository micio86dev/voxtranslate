// The ONE place that decides how the microphone is opened.
//
// This decision used to live in five call sites — the meet pre-join, the webinar
// pre-live preview, the webinar WHIP publish, Talk to Anyone, and the dashboard voice
// assistant — and two of them got it wrong. `buildPublishConstraints` shipped no
// filtering at all, so a webinar host checked their setup through a filtered preview
// and then went on air unfiltered. Worse, that same unfiltered track is what
// `openWebinarStt` hands to the Qwen transcribe session, so room noise reached the
// translated subtitles of every viewer language. One function, one contract.
//
// What the noisy-environment toggle actually switches:
//
//   OFF → the browser's own `noiseSuppression` (exactly what shipped before)
//   ON  → the browser's filter steps aside and our RNNoise worklet takes over
//         (see `denoise.ts`), mixed at `NOISE_MIX_PERCENT`
//
// Two suppressors in series is NOT twice the suppression: the browser leaves
// artifacts and RNNoise then treats them as signal, which sounds worse than either
// alone. Hence the swap rather than the stack.
//
// `echoCancellation` and `autoGainControl` are deliberately NOT part of the toggle.
// AEC is structural for Talk to Anyone on a single device — without it the translated
// speech re-enters the microphone, which is the whole premise of `self-audio-guard.ts`.

/** localStorage key for the toggle. `vox.` prefix matches the existing convention. */
const NOISY_ENV_KEY = 'vox.noisyEnv';

/**
 * How much of the denoised signal to mix back in, as a percentage.
 *
 * Tunable via Vercel BUILD-TIME env (`PUBLIC_NOISE_MIX`), same pattern as the video
 * budgets. Default 70, not 50: at 50 half the original noise survives, and a user who
 * enables the toggle and still hears traffic concludes it is broken. Tune DOWNWARD
 * from here if voices start losing their consonants.
 */
export function noiseMixPercent(raw: unknown): number {
  const n = Number(raw);
  if (!Number.isFinite(n) || raw === '' || raw == null) return 70;
  // The value drives a pair of GainNodes: above 1 would amplify, below 0 would invert
  // the phase. Clamp rather than trust a hand-edited environment variable.
  return Math.min(100, Math.max(0, n));
}

/** Resolved once at build time — see [`noiseMixPercent`]. */
export const NOISE_MIX_PERCENT = noiseMixPercent(import.meta.env.PUBLIC_NOISE_MIX);

/** Read the toggle. Off unless the user turned it on. */
export function loadNoisyEnv(): boolean {
  try {
    return localStorage.getItem(NOISY_ENV_KEY) === '1';
  } catch {
    // Safari private mode throws on storage access; a mic preference is not worth
    // taking the app down for.
    return false;
  }
}

/** Persist the toggle. Silent on failure, for the same reason. */
export function saveNoisyEnv(on: boolean): void {
  try {
    localStorage.setItem(NOISY_ENV_KEY, on ? '1' : '0');
  } catch {
    /* preference is lost on reload; capture still works */
  }
}

/** What the caller knows about how this microphone should be opened. */
export interface MicChoice {
  /** `deviceId` from the device picker. Empty/absent means "browser default". */
  deviceId?: string;
  /** Noisy-environment mode. Defaults to the stored preference when omitted. */
  noisyEnv?: boolean;
}

/**
 * Build the audio half of a `getUserMedia` request.
 *
 * Mono on purpose: every downstream consumer (WebRTC, the PCM capture that feeds the
 * translation models, the WHIP publish) wants one channel, and a stereo capture just
 * doubles the bytes.
 */
export function buildAudioConstraints(choice: MicChoice = {}): MediaTrackConstraints {
  const noisyEnv = choice.noisyEnv ?? loadNoisyEnv();
  const deviceId = choice.deviceId?.trim();
  return {
    channelCount: 1,
    echoCancellation: true,
    autoGainControl: true,
    // The swap, not a stack — see the module header.
    noiseSuppression: !noisyEnv,
    ...(deviceId ? { deviceId: { exact: deviceId } } : {}),
  };
}
