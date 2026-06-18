import { describe, expect, it } from 'vitest';

import { base64ToFloat32, base64ToInt16, floatTo16, int16ToFloat, shouldPlay } from './pcm';

describe('floatTo16', () => {
  it('maps the full range and clamps overshoot', () => {
    const out = floatTo16(new Float32Array([0, 1, -1, 2, -2]));
    expect(Array.from(out)).toEqual([0, 32767, -32768, 32767, -32768]);
  });
});

describe('int16ToFloat', () => {
  it('round-trips the extremes', () => {
    const out = int16ToFloat(new Int16Array([0, 32767, -32768]));
    expect(out[0]).toBe(0);
    expect(out[1]).toBeCloseTo(0.99997, 4);
    expect(out[2]).toBe(-1);
  });
});

describe('base64ToInt16', () => {
  it('decodes little-endian PCM16', () => {
    // [256, -1] little-endian = bytes 00 01 FF FF.
    const b64 = btoa(String.fromCharCode(0x00, 0x01, 0xff, 0xff));
    expect(Array.from(base64ToInt16(b64))).toEqual([256, -1]);
  });
  it('drops a trailing odd byte instead of throwing', () => {
    const b64 = btoa(String.fromCharCode(0x01, 0x00, 0x7f)); // 1, then a lone byte
    expect(Array.from(base64ToInt16(b64))).toEqual([1]);
  });
});

describe('base64ToFloat32', () => {
  it('decodes straight to normalized floats', () => {
    // 32767 (0x7FFF) little-endian = FF 7F.
    const b64 = btoa(String.fromCharCode(0xff, 0x7f));
    const out = base64ToFloat32(b64);
    expect(out).toHaveLength(1);
    expect(out[0]).toBeCloseTo(0.99997, 4);
  });
});

describe('shouldPlay', () => {
  it('accepts the first frame and strictly newer ones, drops stale/dupes', () => {
    expect(shouldPlay(0, -1)).toBe(true);
    expect(shouldPlay(5, 4)).toBe(true);
    expect(shouldPlay(4, 4)).toBe(false); // duplicate
    expect(shouldPlay(3, 4)).toBe(false); // out of order
  });

  it('accepts a reset-to-0 (upstream reconnect restarts audio_seq at 0)', () => {
    // After a Gemini/OpenAI reconnect the server's per-connection seq restarts at 0;
    // the client must keep playing instead of dropping every frame below the old
    // high-water mark (which silenced the translated voice after the first reconnect).
    expect(shouldPlay(0, 50)).toBe(true);
    expect(shouldPlay(1, 0)).toBe(true); // the new stream then climbs normally
  });
});
