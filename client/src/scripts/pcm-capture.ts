// Premium mic capture (spec 0093). OpenAI's realtime-translation API wants
// PCM16 @ 24 kHz, but MediaRecorder only gives WebM/Opus — so when the Premium
// engine is active we capture a parallel PCM stream instead. An AudioContext
// forced to 24 kHz makes the browser resample the mic for us; an AudioWorklet
// converts Float32 → Int16 off the main thread and posts ~100 ms chunks, which we
// stream to the server as binary WS frames. Drop-in for AudioCapture (same
// start/stop/setMuted/setStream/restart surface) so app.ts can swap by engine.

const TARGET_SAMPLE_RATE = 24000;

// AudioWorklet processor source (runs in the audio render thread; no imports
// allowed there, so the Float32→Int16 conversion is inlined). Batches to ~100 ms.
const CAPTURE_WORKLET = `
class PcmCaptureProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this._buf = [];
    this._count = 0;
    this._target = Math.round(sampleRate * 0.1); // ~100ms
  }
  process(inputs) {
    const ch = inputs[0] && inputs[0][0];
    if (ch && ch.length) {
      const out = new Int16Array(ch.length);
      for (let i = 0; i < ch.length; i++) {
        let s = ch[i];
        if (s > 1) s = 1; else if (s < -1) s = -1;
        out[i] = s < 0 ? s * 0x8000 : s * 0x7fff;
      }
      this._buf.push(out);
      this._count += out.length;
      if (this._count >= this._target) {
        const merged = new Int16Array(this._count);
        let o = 0;
        for (const b of this._buf) { merged.set(b, o); o += b.length; }
        this.port.postMessage(merged.buffer, [merged.buffer]);
        this._buf = [];
        this._count = 0;
      }
    }
    return true;
  }
}
registerProcessor('pcm-capture-processor', PcmCaptureProcessor);
`;

let workletUrl: string | null = null;
function captureWorkletUrl(): string {
  if (!workletUrl) {
    workletUrl = URL.createObjectURL(new Blob([CAPTURE_WORKLET], { type: 'application/javascript' }));
  }
  return workletUrl;
}

export class PcmCapture {
  private stream: MediaStream;
  private ws: WebSocket;
  private ctx: AudioContext | null = null;
  private node: AudioWorkletNode | null = null;
  private src: MediaStreamAudioSourceNode | null = null;
  private active = false;
  private starting = false;

  constructor(stream: MediaStream, ws: WebSocket) {
    this.stream = stream;
    this.ws = ws;
  }

  /** Point at a new MediaStream after a device change (call while stopped). */
  setStream(stream: MediaStream): void {
    this.stream = stream;
  }

  /** Begin capture. Async work is kicked off fire-and-forget so the call site
   *  matches AudioCapture's synchronous `start()`. */
  start(): void {
    if (this.active || this.starting) return;
    void this._start();
  }

  private async _start(): Promise<void> {
    const track = this.stream.getAudioTracks()[0];
    if (!track) return;
    this.starting = true;
    try {
      const Ctx = window.AudioContext ?? (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
      this.ctx = new Ctx({ sampleRate: TARGET_SAMPLE_RATE });
      await this.ctx.audioWorklet.addModule(captureWorkletUrl());
      // The track may have been replaced while we awaited; re-read it.
      const live = this.stream.getAudioTracks()[0];
      if (!live) {
        this.starting = false;
        this.stop();
        return;
      }
      this.src = this.ctx.createMediaStreamSource(new MediaStream([live]));
      this.node = new AudioWorkletNode(this.ctx, 'pcm-capture-processor');
      this.node.port.onmessage = (e: MessageEvent<ArrayBuffer>) => {
        if (this.ws.readyState === WebSocket.OPEN) this.ws.send(e.data);
      };
      // A node only runs when it's pulled by the graph; route it to a muted sink.
      const sink = this.ctx.createGain();
      sink.gain.value = 0;
      this.src.connect(this.node).connect(sink).connect(this.ctx.destination);
      this.sendControl('start'); // open the OpenAI session(s) before audio flows
      this.active = true;
    } catch {
      this.stop();
    } finally {
      this.starting = false;
    }
  }

  stop(): void {
    const wasActive = this.active;
    this.active = false;
    if (this.node) {
      this.node.port.onmessage = null;
      this.node.disconnect();
      this.node = null;
    }
    if (this.src) {
      this.src.disconnect();
      this.src = null;
    }
    if (this.ctx) {
      void this.ctx.close().catch(() => {});
      this.ctx = null;
    }
    if (wasActive) this.sendControl('stop');
  }

  /** Stop and start fresh (parity with AudioCapture; PCM has no header to reset,
   *  so this is just a clean restart, e.g. after a language change). */
  restart(): void {
    this.stop();
    this.start();
  }

  /** Mute = stop sending audio; unmute = resume. */
  setMuted(muted: boolean): void {
    if (muted) this.stop();
    else this.start();
  }

  private sendControl(type: 'start' | 'stop'): void {
    if (this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify({ type }));
  }
}
