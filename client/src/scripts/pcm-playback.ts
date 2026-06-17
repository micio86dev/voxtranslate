// Premium translated-audio playback (spec 0093). The server forwards OpenAI's
// translated speech as base64 PCM16 @ 24 kHz `translated_audio` frames; we decode
// them and feed an AudioWorklet ring-buffer that plays them gaplessly — Web Audio,
// NOT browser TTS. Like TTS, playback needs a user-gesture unlock on iOS/Safari.

import { base64ToFloat32, shouldPlay } from './pcm';

const SAMPLE_RATE = 24000;

// Served as a static same-origin file (NOT a blob: URL): the CSP allows
// `worker-src 'self'` but not `blob:` (spec 0093 prod fix).
const PLAYBACK_WORKLET_URL = '/pcm-playback-worklet.js';

type AudioCtor = typeof AudioContext;

export class PcmPlayback {
  private ctx: AudioContext | null = null;
  private node: AudioWorkletNode | null = null;
  private ready: Promise<void> | null = null;
  // Per-speaker last-played sequence, to drop out-of-order/duplicate frames.
  private lastSeq = new Map<string, number>();

  private ensure(): Promise<void> {
    if (this.ready) return this.ready;
    this.ready = (async () => {
      const Ctor: AudioCtor =
        window.AudioContext ??
        (window as unknown as { webkitAudioContext: AudioCtor }).webkitAudioContext;
      this.ctx = new Ctor({ sampleRate: SAMPLE_RATE });
      await this.ctx.audioWorklet.addModule(PLAYBACK_WORKLET_URL);
      this.node = new AudioWorkletNode(this.ctx, 'pcm-playback-processor', {
        numberOfInputs: 0,
        numberOfOutputs: 1,
        outputChannelCount: [1],
      });
      this.node.connect(this.ctx.destination);
    })();
    return this.ready;
  }

  /** Queue one translated-audio chunk for `speakerId` (base64 PCM16). */
  enqueue(speakerId: string, seq: number, pcm16Base64: string): void {
    const last = this.lastSeq.get(speakerId) ?? -1;
    if (!shouldPlay(seq, last)) return;
    this.lastSeq.set(speakerId, seq);
    void this.ensure()
      .then(() => {
        if (!this.ctx || !this.node) return;
        if (this.ctx.state === 'suspended') void this.ctx.resume().catch(() => {});
        const samples = base64ToFloat32(pcm16Base64);
        if (samples.length) this.node.port.postMessage(samples, [samples.buffer]);
      })
      .catch(() => {});
  }

  /** Resume the context inside a user gesture (iOS/Safari autoplay unlock). */
  unlock(): void {
    void this.ensure()
      .then(() => {
        if (this.ctx?.state === 'suspended') void this.ctx.resume().catch(() => {});
      })
      .catch(() => {});
  }

  /** Flush queued audio + reset ordering (leave a call / engine downgrade). */
  reset(): void {
    this.lastSeq.clear();
    this.node?.port.postMessage('flush');
  }

  /** Tear everything down (on leaving the call). */
  stop(): void {
    this.reset();
    if (this.node) {
      this.node.disconnect();
      this.node = null;
    }
    if (this.ctx) {
      void this.ctx.close().catch(() => {});
      this.ctx = null;
    }
    this.ready = null;
  }
}

/** Shared instance — one playback graph for the whole call. */
export const pcmPlayback = new PcmPlayback();
