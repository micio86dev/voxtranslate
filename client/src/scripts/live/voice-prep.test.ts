// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';
import { prepareVoice, type VoicePrepState } from './voice-prep';

function setup(over: { getMedia?: () => Promise<MediaStream>; record?: () => Promise<Blob | null>; clone?: () => Promise<{ voice_id: string | null }> } = {}) {
  const stop = vi.fn();
  const stream = { getTracks: () => [{ stop }, { stop }] } as unknown as MediaStream;
  const states: VoicePrepState[] = [];
  const progress: number[] = [];
  const record = vi.fn(async (_s: MediaStream, onProgress?: (f: number) => void) => {
    onProgress?.(0.4);
    return new Blob(['x']);
  });
  const clone = vi.fn(async (_b: Blob, _l?: string) => ({ voice_id: 'V' as string | null }));
  const deps = {
    getMedia: over.getMedia ?? (async () => stream),
    record: (over.record as never) ?? (record as never),
    clone: (over.clone as never) ?? (clone as never),
  };
  const run = () => prepareVoice('it', (s) => states.push(s), (f) => progress.push(f), deps);
  return { run, states, progress, record, clone, stop };
}

describe('prepareVoice', () => {
  it('records, clones in the host language, reports the progress and releases the microphone', async () => {
    const h = setup();
    expect(await h.run()).toBe(true);
    expect(h.states).toEqual(['recording', 'saving', 'cloned']);
    expect(h.progress).toEqual([0.4]);
    expect(h.clone.mock.calls[0][1]).toBe('it');
    expect(h.stop).toHaveBeenCalledTimes(2);
  });

  it('a denied or missing microphone is a failure, with nothing to release', async () => {
    const h = setup({ getMedia: async () => { throw new Error('denied'); } });
    expect(await h.run()).toBe(false);
    expect(h.states).toEqual(['failed']);
    expect(h.clone).not.toHaveBeenCalled();
  });

  it('too little speech (no sample) is a failure and nothing is uploaded; the microphone is still released', async () => {
    const h = setup({ record: async () => null });
    expect(await h.run()).toBe(false);
    expect(h.states).toEqual(['recording', 'failed']);
    expect(h.clone).not.toHaveBeenCalled();
    expect(h.stop).toHaveBeenCalledTimes(2);
  });

  it('a clone the server could not make (no voice id) is a failure', async () => {
    const h = setup({ clone: async () => ({ voice_id: null }) });
    expect(await h.run()).toBe(false);
    expect(h.states).toEqual(['recording', 'saving', 'failed']);
  });

  it('an exception anywhere is a failure, never a throw', async () => {
    const h = setup({ clone: async () => { throw new Error('boom'); } });
    expect(await h.run()).toBe(false);
    expect(h.states.at(-1)).toBe('failed');
    expect(h.stop).toHaveBeenCalledTimes(2);
  });

  it('uses the real microphone and recorder when none is injected', async () => {
    const stop = vi.fn();
    const getUserMedia = vi.fn(async () => ({ getTracks: () => [{ stop }] }) as unknown as MediaStream);
    Object.defineProperty(globalThis, 'navigator', { value: { mediaDevices: { getUserMedia } }, configurable: true });
    const states: VoicePrepState[] = [];
    const ok = await prepareVoice('it', (s) => states.push(s), undefined, { record: async () => null });
    expect(getUserMedia).toHaveBeenCalledWith({ audio: true });
    expect(ok).toBe(false);
    expect(stop).toHaveBeenCalled();
  });
});
