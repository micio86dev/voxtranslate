// AudioWorklet processor for noisy-environment capture: RNNoise, running on the audio
// thread. Served as a static same-origin file (NOT a blob: URL) because the app's CSP
// allows `worker-src 'self'` but not `blob:` — the same constraint that shaped
// pcm-capture-worklet.js (issue #237).
//
// AudioWorkletGlobalScope has no `fetch` and no ES imports, so the wasm cannot be
// loaded here. The main thread fetches `/rnnoise.wasm` and posts the bytes in; we
// instantiate them directly rather than through the Emscripten glue, which is a JS
// module we could not import anyway. That is viable because this binary needs only two
// imports and exports its own memory:
//
//   imports   a.a → emscripten_resize_heap     a.b → emscripten_memcpy_big
//   exports   c memory · d __wasm_call_ctors · e rnnoise_init · f rnnoise_create
//             g malloc · h rnnoise_destroy · i free · j rnnoise_process_frame
//
// Those single letters are the minified export names, and they are NOT stable across
// builds: `dist/rnnoise-sync.js` (wasm inlined as base64) uses a DIFFERENT assignment
// for the same symbols. This mapping was read from `dist/rnnoise.js`, the glue that
// belongs to the `dist/rnnoise.wasm` we copy into `public/`. If the package is ever
// bumped, re-read it from there — the failure mode is malloc silently returning 0.
//
// Until the bytes arrive — and if anything about them fails — `process()` copies input
// straight to output. Capture must never go silent because a denoiser is still loading.

/** RNNoise is trained at 48 kHz and processes exactly this many samples per call. */
const FRAME = 480;

/** RNNoise wants samples in INT16 SCALE as floats, not the -1..1 Web Audio gives us.
 *  Feeding it normalized samples produces near-silence that looks like a broken model. */
const SCALE = 32768;

class RnnoiseProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    /** Wasm exports, once instantiated. Null means "pass audio through untouched". */
    this._w = null;
    this._state = 0;
    this._inPtr = 0;
    this._outPtr = 0;
    /** Samples waiting to fill a 480-frame, and denoised samples waiting to be emitted. */
    this._in = new Float32Array(0);
    this._out = new Float32Array(0);
    this.port.onmessage = (e) => this._onMessage(e.data);
  }

  _onMessage(msg) {
    if (!msg || msg.type !== 'wasm') return;
    // Instantiation is async; `process()` keeps passing audio through until it lands.
    this._instantiate(msg.bytes).catch(() => {
      // Stay in passthrough. The main thread already knows the fetch succeeded, so a
      // failure here is a broken binary or an unsupported engine — either way, audio
      // keeps flowing unfiltered, which is strictly better than silence.
      this._w = null;
      this.port.postMessage({ type: 'failed' });
    });
  }

  async _instantiate(bytes) {
    const memcpy = (dest, src, n) => {
      new Uint8Array(this._w.memory.buffer).copyWithin(dest, src, src + n);
      return dest;
    };
    const resize = (requested) => {
      // RNNoise allocates once and never grows, so this is a safety net rather than a
      // hot path. Returning 0 signals failure to the wasm, which is the honest answer
      // if the browser refuses to grow the memory.
      try {
        const pages = Math.ceil((requested - this._w.memory.buffer.byteLength) / 65536);
        this._w.memory.grow(Math.max(1, pages));
        return 1;
      } catch {
        return 0;
      }
    };
    const { instance } = await WebAssembly.instantiate(bytes, {
      a: { a: resize, b: memcpy },
    });
    const x = instance.exports;
    this._w = {
      memory: x.c,
      malloc: x.g,
      createState: x.f,
      processFrame: x.j,
    };
    x.d(); // __wasm_call_ctors
    // NOT rnnoise_init (x.e): it takes (state, model) and initialises an ALREADY
    // allocated state. Calling it bare traps on an out-of-bounds write, after which
    // malloc returns 0 and every later call fails in a way that looks unrelated.
    // rnnoise_create does the initialisation itself.
    this._state = this._w.createState(0); // 0 = the built-in model
    // One scratch buffer each way, allocated once — 480 floats is 1920 bytes.
    this._inPtr = this._w.malloc(FRAME * 4);
    this._outPtr = this._w.malloc(FRAME * 4);
    if (!this._state || !this._inPtr || !this._outPtr) {
      this._w = null;
      this.port.postMessage({ type: 'failed' });
      return;
    }
    this.port.postMessage({ type: 'ready' });
  }

  /** Append `b` after `a` in one new Float32Array. */
  static _concat(a, b) {
    const out = new Float32Array(a.length + b.length);
    out.set(a, 0);
    out.set(b, a.length);
    return out;
  }

  /** Drain every complete 480-sample frame out of `_in` and into `_out`. */
  _drainFrames() {
    const heap = new Float32Array(this._w.memory.buffer);
    const inIdx = this._inPtr >> 2;
    const outIdx = this._outPtr >> 2;
    let consumed = 0;
    const produced = [];
    while (this._in.length - consumed >= FRAME) {
      for (let i = 0; i < FRAME; i++) heap[inIdx + i] = this._in[consumed + i] * SCALE;
      this._w.processFrame(this._state, this._outPtr, this._inPtr);
      const frame = new Float32Array(FRAME);
      for (let i = 0; i < FRAME; i++) frame[i] = heap[outIdx + i] / SCALE;
      produced.push(frame);
      consumed += FRAME;
    }
    if (!consumed) return;
    this._in = this._in.slice(consumed);
    for (const f of produced) this._out = RnnoiseProcessor._concat(this._out, f);
  }

  process(inputs, outputs) {
    const input = inputs[0] && inputs[0][0];
    const output = outputs[0] && outputs[0][0];
    if (!output) return true;

    // No input yet (or the track ended): emit silence, don't spin the model.
    if (!input || !input.length) {
      output.fill(0);
      return true;
    }

    // Passthrough while the wasm loads, or forever if it failed. Never go silent.
    if (!this._w) {
      output.set(input.subarray(0, output.length));
      return true;
    }

    this._in = RnnoiseProcessor._concat(this._in, input);
    this._drainFrames();

    const n = output.length;
    if (this._out.length >= n) {
      output.set(this._out.subarray(0, n));
      this._out = this._out.slice(n);
    } else {
      // Priming only: the first ~480 samples (10 ms) before the first frame completes.
      output.fill(0);
    }
    return true;
  }
}

registerProcessor('rnnoise-processor', RnnoiseProcessor);
