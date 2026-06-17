// AudioWorklet processor for Premium mic capture (spec 0093). Converts Float32 →
// Int16 off the main thread and posts ~100 ms PCM chunks. Served as a static
// same-origin file (not a blob: URL) so the CSP `worker-src 'self'` allows it.
class PcmCaptureProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this._buf = [];
    this._count = 0;
    this._target = Math.round(sampleRate * 0.1); // ~100ms at the context rate (24k)
  }
  process(inputs) {
    const ch = inputs[0] && inputs[0][0];
    if (ch && ch.length) {
      const out = new Int16Array(ch.length);
      for (let i = 0; i < ch.length; i++) {
        let s = ch[i];
        if (s > 1) s = 1;
        else if (s < -1) s = -1;
        out[i] = s < 0 ? s * 0x8000 : s * 0x7fff;
      }
      this._buf.push(out);
      this._count += out.length;
      if (this._count >= this._target) {
        const merged = new Int16Array(this._count);
        let o = 0;
        for (const b of this._buf) {
          merged.set(b, o);
          o += b.length;
        }
        this.port.postMessage(merged.buffer, [merged.buffer]);
        this._buf = [];
        this._count = 0;
      }
    }
    return true;
  }
}
registerProcessor('pcm-capture-processor', PcmCaptureProcessor);
