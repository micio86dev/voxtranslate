// Premium translated-audio playback (spec 0093). The server forwards OpenAI's
// translated speech as base64 PCM16 @ 24 kHz `translated_audio` frames; we decode
// them and feed an AudioWorklet ring-buffer that plays them gaplessly — Web Audio,
// NOT browser TTS. Like TTS, playback needs a user-gesture unlock on iOS/Safari.

import { base64ToFloat32, shouldPlay } from './pcm';

const SAMPLE_RATE = 24000;

// Playback worklet: a FIFO of Float32 chunks drained sample-by-sample into the
// output, silence when empty. `'flush'` clears it (leave / downgrade).
const PLAYBACK_WORKLET = `
class PcmPlaybackProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this._queue = [];
    this._cur = null;
    this._pos = 0;
    this.port.onmessage = (e) => {
      if (e.data === 'flush') { this._queue = []; this._cur = null; this._pos = 0; }
      else this._queue.push(e.data);
    };
  }
  process(_inputs, outputs) {
    const out = outputs[0] && outputs[0][0];
    if (!out) return true;
    for (let i = 0; i < out.length; i++) {
      if (!this._cur || this._pos >= this._cur.length) {
        this._cur = this._queue.shift() || null;
        this._pos = 0;
      }
      out[i] = this._cur ? this._cur[this._pos++] : 0;
    }
    return true;
  }
}
registerProcessor('pcm-playback-processor', PcmPlaybackProcessor);
`;

let workletUrl: string | null = null;
function playbackWorkletUrl(): string {
  if (!workletUrl) {
    workletUrl = URL.createObjectURL(new Blob([PLAYBACK_WORKLET], { type: 'application/javascript' }));
  }
  return workletUrl;
}

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
      await this.ctx.audioWorklet.addModule(playbackWorkletUrl());
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
