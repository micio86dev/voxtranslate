// Playback for Vox Voices: the engine synthesizes a WHOLE utterance as Float32 PCM
// @ 24 kHz, so we play it with a one-shot AudioBufferSourceNode and resolve on its
// `onended` — a precise "finished" signal the manager's serial queue relies on (vs.
// the streaming ring-buffer worklet used for continuous premium audio in
// pcm-playback.ts). Like all Web Audio, it needs a user-gesture unlock on iOS/Safari.

const SAMPLE_RATE = 24000;

type AudioCtor = typeof AudioContext;

export class PlaybackService {
  private ctx: AudioContext | null = null;
  private current: AudioBufferSourceNode | null = null;

  private ensureCtx(): AudioContext {
    if (this.ctx) return this.ctx;
    const Ctor: AudioCtor =
      window.AudioContext ??
      (window as unknown as { webkitAudioContext: AudioCtor }).webkitAudioContext;
    this.ctx = new Ctor();
    return this.ctx;
  }

  /** Resume the context inside a user gesture (iOS/Safari autoplay unlock). */
  unlock(): void {
    try {
      const ctx = this.ensureCtx();
      if (ctx.state === 'suspended') void ctx.resume().catch(() => {});
    } catch {
      /* no Web Audio (SSR/tests) */
    }
  }

  /** Play one Float32 @ 24 kHz utterance; resolves when its playback finishes. */
  play(samples: Float32Array): Promise<void> {
    return new Promise((resolve, reject) => {
      let ctx: AudioContext;
      try {
        ctx = this.ensureCtx();
      } catch (err) {
        return reject(err instanceof Error ? err : new Error('no audio context'));
      }
      if (ctx.state === 'suspended') void ctx.resume().catch(() => {});
      // Author the buffer at the engine's native 24 kHz; the context resamples to its
      // own device rate on playback.
      const buffer = ctx.createBuffer(1, samples.length, SAMPLE_RATE);
      buffer.getChannelData(0).set(samples);
      const src = ctx.createBufferSource();
      src.buffer = buffer;
      src.connect(ctx.destination);
      src.onended = () => {
        if (this.current === src) this.current = null;
        resolve();
      };
      this.current = src;
      try {
        src.start();
      } catch (err) {
        reject(err instanceof Error ? err : new Error('playback failed'));
      }
    });
  }

  /** Stop the current utterance (TTS toggled off / leaving the call). */
  stop(): void {
    if (this.current) {
      try {
        this.current.onended = null;
        this.current.stop();
      } catch {
        /* already stopped */
      }
      this.current = null;
    }
  }
}
