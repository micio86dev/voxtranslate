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
  // Notified on the edges of speech (spec 0110). Talk to Anyone gates the microphone
  // while this graph is audible, because on one device the speaker feeds the mic.
  private onPlayingChange: ((playing: boolean) => void) | null = null;
  private playing = false;

  /**
   * Listen for the start and end of translated speech. Set before the first `enqueue`
   * so no edge is missed; pass `null` to stop listening.
   */
  setPlayingListener(fn: ((playing: boolean) => void) | null): void {
    this.onPlayingChange = fn;
  }

  /** True while translated audio is actually leaving the graph. */
  isPlaying(): boolean {
    return this.playing;
  }

  private handleWorkletMessage = (e: MessageEvent<{ playing?: boolean }>): void => {
    const playing = e.data?.playing;
    if (typeof playing !== 'boolean' || playing === this.playing) return;
    this.playing = playing;
    this.onPlayingChange?.(playing);
  };

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
      this.node.port.onmessage = this.handleWorkletMessage;
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

  /**
   * Flush queued audio + reset ordering (leave a call, engine downgrade, or a barge-in
   * that cancels a translation mid-sentence). The worklet answers with a `playing:false`
   * edge, so a gated microphone reopens without waiting for anything.
   */
  reset(): void {
    this.lastSeq.clear();
    this.node?.port.postMessage('flush');
    if (!this.node && this.playing) {
      // No graph to answer us — report the edge ourselves rather than leave a listener
      // believing audio is still playing (and, in Talk to Anyone, the microphone shut).
      this.playing = false;
      this.onPlayingChange?.(false);
    }
  }

  /** Tear everything down (on leaving the call). */
  stop(): void {
    this.reset();
    if (this.node) {
      this.node.port.onmessage = null;
      this.node.disconnect();
      this.node = null;
    }
    if (this.playing) {
      this.playing = false;
      this.onPlayingChange?.(false);
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
