// Noisy-environment capture: run the microphone through RNNoise before anyone else
// sees it (spec: mic noise suppression).
//
// The insertion point matters more than the model. `PcmCapture` builds its source from
// the SAME MediaStream that WebRTC publishes, so anything spliced in ahead of that fork
// cleans both at once — what peers hear AND what the translation models are fed. That
// second half is the real prize: room noise reaching a transcribe session becomes wrong
// words in every target language, not just an unpleasant listen.
//
//   getUserMedia (browser filter OFF — see mic-constraints.ts)
//        → AudioContext @48kHz   (RNNoise is trained at 48k and only 48k)
//           → rnnoise-worklet    (denoised)
//           → wet/dry mix        (NOISE_MIX_PERCENT)
//           → MediaStreamDestination
//        → the stream everything downstream already knows how to consume
//
// The mix is a plain crossfade rather than a model parameter because RNNoise has none:
// `rnnoise_process_frame` denoises, full stop. Two gains give a continuous, predictable
// dial over the same model, which is strictly more control than a discrete setting.
//
// Every failure here degrades to the ORIGINAL stream. A microphone that works
// unfiltered beats a microphone that does not work.

import { NOISE_MIX_PERCENT } from './mic-constraints';

/** Same-origin static assets: the CSP allows `worker-src 'self'` but not `blob:`. */
const WORKLET_URL = '/rnnoise-worklet.js';
const WASM_URL = '/rnnoise.wasm';

/** RNNoise is trained at this rate. Resampling is the browser's job, not ours. */
const RNNOISE_RATE = 48_000;

/** A live denoiser: the stream to use, and the way to take it back down. */
export interface Denoiser {
  /** The stream callers should publish and capture from. */
  stream: MediaStream;
  /** Whether RNNoise is actually running (false ⇒ `stream` is the untouched input). */
  active: boolean;
  /** Release the AudioContext and worklet. Never throws. */
  stop(): Promise<void>;
}

/** Injectable seams so the wiring can be tested without an audio device. */
export interface DenoiseEnv {
  createContext(rate: number): AudioContext;
  fetchWasm(url: string): Promise<ArrayBuffer>;
}

const defaultEnv: DenoiseEnv = {
  createContext: (sampleRate) => new AudioContext({ sampleRate }),
  fetchWasm: async (url) => {
    const res = await fetch(url);
    if (!res.ok) throw new Error(`rnnoise wasm ${res.status}`);
    return res.arrayBuffer();
  },
};

/** A denoiser that isn't one — the input, untouched, with a no-op stop. */
function passthrough(stream: MediaStream): Denoiser {
  return { stream, active: false, stop: async () => {} };
}

/**
 * Wrap `input` in an RNNoise stage and return the stream to use instead.
 *
 * Returns the input unchanged — never throws — when the browser lacks AudioWorklet,
 * when the wasm cannot be fetched, or when the mix is 0 (nothing to do). Callers can
 * treat the result as "the microphone stream" without branching.
 */
export async function createDenoiser(
  input: MediaStream,
  env: DenoiseEnv = defaultEnv,
  mixPercent: number = NOISE_MIX_PERCENT,
): Promise<Denoiser> {
  const track = input.getAudioTracks()[0];
  if (!track || mixPercent <= 0) return passthrough(input);

  let ctx: AudioContext | null = null;
  try {
    ctx = env.createContext(RNNOISE_RATE);
    if (!ctx.audioWorklet) return passthrough(input);

    // Fetch first: a failed fetch should not leave a half-built graph behind.
    const [wasm] = await Promise.all([
      env.fetchWasm(WASM_URL),
      ctx.audioWorklet.addModule(WORKLET_URL),
    ]);

    const source = ctx.createMediaStreamSource(new MediaStream([track]));
    const node = new AudioWorkletNode(ctx, 'rnnoise-processor');
    node.port.postMessage({ type: 'wasm', bytes: wasm }, [wasm]);

    // Crossfade. The worklet passes audio through until the wasm is live, so during
    // startup both legs carry the same signal and the mix is inaudible either way.
    const wet = ctx.createGain();
    const dry = ctx.createGain();
    const mix = Math.min(1, Math.max(0, mixPercent / 100));
    wet.gain.value = mix;
    dry.gain.value = 1 - mix;

    const dest = ctx.createMediaStreamDestination();
    source.connect(node);
    node.connect(wet).connect(dest);
    source.connect(dry).connect(dest);

    // Carry the video across untouched — only the audio is rebuilt.
    const out = new MediaStream([...dest.stream.getAudioTracks(), ...input.getVideoTracks()]);

    return {
      stream: out,
      active: true,
      stop: async () => {
        try {
          source.disconnect();
          node.disconnect();
          wet.disconnect();
          dry.disconnect();
          node.port.onmessage = null;
          await ctx?.close();
        } catch {
          /* already torn down */
        }
      },
    };
  } catch {
    // AudioWorklet unsupported, module blocked, wasm 404 — capture still has to work.
    try {
      await ctx?.close();
    } catch {
      /* nothing to close */
    }
    return passthrough(input);
  }
}
