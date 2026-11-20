// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { recordVoiceSample } from './voice-sample';

let level = 0.2;
let recorders: FakeRecorder[] = [];
let startThrows = false;
let ctorThrows = false;
let stopThrows = false;

class FakeRecorder {
  static isTypeSupported = (t: string) => t === 'audio/webm';
  mimeType = 'audio/webm';
  ondataavailable: ((e: { data: Blob }) => void) | null = null;
  onstop: (() => void) | null = null;
  constructor(readonly stream: unknown, readonly opts?: { mimeType?: string }) {
    if (ctorThrows) throw new Error('unsupported');
    recorders.push(this);
  }
  start(): void {
    if (startThrows) throw new Error('start');
    this.ondataavailable?.({ data: new Blob(['chunk']) });
  }
  stop(): void {
    if (stopThrows) throw new Error('stop');
    this.onstop?.();
  }
}

class FakeCtx {
  closed = false;
  createAnalyser() {
    return {
      fftSize: 0,
      getFloatTimeDomainData: (buf: Float32Array) => buf.fill(level),
    };
  }
  createMediaStreamSource() {
    return { connect: () => {} };
  }
  close() {
    this.closed = true;
    return Promise.resolve();
  }
}

beforeEach(() => {
  vi.useFakeTimers();
  level = 0.2;
  recorders = [];
  startThrows = ctorThrows = stopThrows = false;
  (globalThis as unknown as { MediaRecorder: unknown }).MediaRecorder = FakeRecorder;
  (window as unknown as { AudioContext: unknown }).AudioContext = FakeCtx;
});
afterEach(() => vi.useRealTimers());

describe('recordVoiceSample (Meet\'s recorder, shared)', () => {
  it('resolves with the clip once enough actual speech was heard, reporting progress up to 1', async () => {
    const progress: number[] = [];
    const p = recordVoiceSample({} as MediaStream, (f) => progress.push(f));
    await vi.advanceTimersByTimeAsync(3600);
    const blob = await p;
    expect(blob).toBeInstanceOf(Blob);
    expect(blob?.type).toBe('audio/webm');
    expect(progress.at(-1)).toBe(1);
    expect(progress[0]).toBeGreaterThan(0);
    expect(recorders[0].opts).toEqual({ mimeType: 'audio/webm' });
  });

  it('resolves null when there was only room noise by the time ceiling (8 s)', async () => {
    level = 0.001;
    const p = recordVoiceSample({} as MediaStream);
    await vi.advanceTimersByTimeAsync(8100);
    expect(await p).toBeNull();
  });

  it('resolves null when the recorder cannot be built or started, or refuses to stop', async () => {
    ctorThrows = true;
    expect(await recordVoiceSample({} as MediaStream)).toBeNull();
    ctorThrows = false;
    startThrows = true;
    expect(await recordVoiceSample({} as MediaStream)).toBeNull();
    startThrows = false;
    stopThrows = true;
    const p = recordVoiceSample({} as MediaStream);
    await vi.advanceTimersByTimeAsync(3600);
    expect(await p).toBeNull();
  });

  it('lets the browser pick the container when none of the preferred types is supported', async () => {
    FakeRecorder.isTypeSupported = () => false;
    const p = recordVoiceSample({} as MediaStream);
    await vi.advanceTimersByTimeAsync(3600);
    await p;
    expect(recorders[0].opts).toBeUndefined();
    FakeRecorder.isTypeSupported = (t: string) => t === 'audio/webm';
  });
});
