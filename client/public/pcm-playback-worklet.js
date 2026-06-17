// AudioWorklet processor for Premium translated-audio playback (spec 0093). A FIFO
// of Float32 chunks drained sample-by-sample into the output (silence when empty);
// `'flush'` clears it. Served as a static same-origin file (not a blob: URL) so the
// CSP `worker-src 'self'` allows it.
class PcmPlaybackProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this._queue = [];
    this._cur = null;
    this._pos = 0;
    this.port.onmessage = (e) => {
      if (e.data === 'flush') {
        this._queue = [];
        this._cur = null;
        this._pos = 0;
      } else {
        this._queue.push(e.data);
      }
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
