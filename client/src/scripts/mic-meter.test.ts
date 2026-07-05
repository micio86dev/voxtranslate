import { afterEach, describe, expect, it, vi } from 'vitest';
import { MicMeter, rmsLevel } from './mic-meter';

describe('rmsLevel', () => {
  it('returns 0 for an empty buffer', () => {
    expect(rmsLevel(new Uint8Array(0))).toBe(0);
  });

  it('returns 0 for silence (all samples at center 128)', () => {
    expect(rmsLevel(new Uint8Array(512).fill(128))).toBe(0);
  });

  it('approaches 1 for a full-scale square wave', () => {
    const buf = new Uint8Array(512);
    for (let i = 0; i < buf.length; i++) buf[i] = i % 2 ? 255 : 0;
    expect(rmsLevel(buf)).toBeGreaterThan(0.97);
  });

  it('scales with amplitude (half-scale DC offset → ~0.5)', () => {
    expect(rmsLevel(new Uint8Array(512).fill(192))).toBeCloseTo(0.5, 1);
  });
});

// ---- MicMeter (WebAudio + rAF, fully faked) ---------------------------------

/** The sample value getByteTimeDomainData fills the buffer with — mutate to
 *  simulate loud speech (255) vs silence (128, the unsigned-8-bit center). */
let sampleValue = 128;
/** The pending rAF callback; ticks are stepped manually for determinism. */
let rafCb: FrameRequestCallback | null = null;
let rafId = 0;

function installAudioGlobals() {
  const analyser = {
    fftSize: 2048,
    getByteTimeDomainData: vi.fn((buf: Uint8Array) => buf.fill(sampleValue)),
  };
  const source = { connect: vi.fn(), disconnect: vi.fn() };
  const close = vi.fn(async () => {});
  class FakeAudioContext {
    createMediaStreamSource = vi.fn(() => source);
    createAnalyser = vi.fn(() => analyser);
    close = close;
  }
  sampleValue = 128;
  rafCb = null;
  rafId = 0;
  vi.stubGlobal('AudioContext', FakeAudioContext);
  vi.stubGlobal(
    'requestAnimationFrame',
    vi.fn((cb: FrameRequestCallback) => {
      rafCb = cb;
      return ++rafId;
    }),
  );
  const caf = vi.fn();
  vi.stubGlobal('cancelAnimationFrame', caf);
  return { analyser, source, close, caf };
}

/** Run exactly one animation-frame tick. */
function step(): void {
  const cb = rafCb;
  rafCb = null;
  cb?.(0);
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('MicMeter', () => {
  it('wires stream → analyser at fftSize 512 and schedules the first tick', () => {
    const { analyser, source } = installAudioGlobals();
    const onLevel = vi.fn();
    new MicMeter({} as MediaStream, onLevel);
    expect(analyser.fftSize).toBe(512);
    expect(source.connect).toHaveBeenCalledWith(analyser);
    expect(rafCb).not.toBeNull(); // tick scheduled, not yet run
    expect(onLevel).not.toHaveBeenCalled();
  });

  it('attacks fast on speech and releases slowly back to 0 on silence', () => {
    installAudioGlobals();
    const onLevel = vi.fn();
    new MicMeter({} as MediaStream, onLevel);

    sampleValue = 255; // full-scale input → boosted raw clamps to 1
    step();
    expect(onLevel).toHaveBeenLastCalledWith(1);

    sampleValue = 128; // silence → decay by ×0.85 per tick, not a hard drop
    step();
    expect(onLevel).toHaveBeenLastCalledWith(0.85);
    step();
    expect(onLevel).toHaveBeenLastCalledWith(0.85 * 0.85);

    // Keep decaying: once below the 0.02 floor the reported level snaps to 0.
    for (let i = 0; i < 30; i++) step();
    expect(onLevel).toHaveBeenLastCalledWith(0);
  });

  it('keeps rescheduling ticks (continuous metering)', () => {
    installAudioGlobals();
    new MicMeter({} as MediaStream, vi.fn());
    step();
    expect(rafCb).not.toBeNull(); // tick re-armed itself
    step();
    expect(rafCb).not.toBeNull();
  });

  it('stop() cancels the loop, tears down audio, and zeroes the level', () => {
    const { source, close, caf } = installAudioGlobals();
    const onLevel = vi.fn();
    const meter = new MicMeter({} as MediaStream, onLevel);
    sampleValue = 255;
    step();
    meter.stop();
    expect(caf).toHaveBeenCalledWith(rafId);
    expect(source.disconnect).toHaveBeenCalled();
    expect(close).toHaveBeenCalled();
    expect(onLevel).toHaveBeenLastCalledWith(0);
  });
});
