// Pure PCM16 helpers for the Premium engine (spec 0093): convert between the
// browser's Float32 audio samples and the 16-bit little-endian PCM the OpenAI
// realtime API speaks, and decode the base64 audio chunks the server forwards.
// DOM-free so it's unit-testable; the AudioWorklet capture/playback wiring lives
// in pcm-capture.ts / pcm-playback.ts.

/** Convert Float32 samples in [-1, 1] to signed 16-bit PCM (clamped). Uses the
 *  full negative range (-1 → -32768) to match the capture worklet. */
export function floatTo16(samples: Float32Array): Int16Array {
  const out = new Int16Array(samples.length);
  for (let i = 0; i < samples.length; i++) {
    let s = samples[i];
    if (s > 1) s = 1;
    else if (s < -1) s = -1;
    out[i] = s < 0 ? s * 0x8000 : s * 0x7fff;
  }
  return out;
}

/** Convert signed 16-bit PCM back to Float32 in [-1, 1]. */
export function int16ToFloat(pcm: Int16Array): Float32Array {
  const out = new Float32Array(pcm.length);
  for (let i = 0; i < pcm.length; i++) out[i] = pcm[i] / 0x8000;
  return out;
}

/** Decode base64 → bytes. Uses `atob` (present in browsers and Node ≥ 16). */
export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}

/** Decode base64 PCM16 (little-endian) into Int16 samples. A trailing odd byte
 *  (never expected) is dropped rather than throwing. */
export function base64ToInt16(b64: string): Int16Array {
  const bytes = base64ToBytes(b64);
  const usable = bytes.length - (bytes.length % 2);
  // Copy into an aligned buffer (the Uint8Array may be offset) before viewing.
  const buf = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + usable);
  return new Int16Array(buf);
}

/** Decode base64 PCM16 straight to Float32 for AudioWorklet playback. */
export function base64ToFloat32(b64: string): Float32Array {
  return int16ToFloat(base64ToInt16(b64));
}

/** Whether a chunk with sequence `seq` should play given the last played `last`.
 *  Drops duplicates and out-of-order frames; `last < 0` accepts the first. WS
 *  preserves order within a connection, but a reconnect/downgrade can replay a
 *  seq, so this keeps playback monotonic. */
export function shouldPlay(seq: number, last: number): boolean {
  return seq > last;
}
