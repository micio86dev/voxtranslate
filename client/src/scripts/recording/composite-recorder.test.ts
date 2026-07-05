import { describe, it, expect, vi, beforeEach } from 'vitest';

// The recorder orchestrates compositor + mixer + MediaRecorder; the two
// collaborators have their own unit tests, so they are class-mocked here and
// the orchestration (wiring, roster diffing, stop/cleanup) is asserted.
const h = vi.hoisted(() => ({
  compositors: [] as any[],
  mixers: [] as any[],
}));

vi.mock('./canvas-compositor', () => ({
  CanvasCompositor: class FakeCompositor {
    canvasTrack = { kind: 'video', stop: vi.fn() };
    start = vi.fn();
    setSources = vi.fn();
    updateSource = vi.fn();
    setVideoOff = vi.fn();
    stop = vi.fn();
    captureStream = vi.fn(() => ({
      getVideoTracks: () => [this.canvasTrack],
      getTracks: () => [this.canvasTrack],
    }));
    constructor() {
      h.compositors.push(this);
    }
  },
}));

vi.mock('./audio-mixer', () => ({
  AudioMixer: class FakeMixer {
    mixedTrack = { kind: 'audio' };
    stream = { getAudioTracks: () => [this.mixedTrack] };
    add = vi.fn();
    remove = vi.fn();
    close = vi.fn();
    constructor() {
      h.mixers.push(this);
    }
  },
}));

import { CompositeRecorder } from './composite-recorder';
import type { ParticipantSource } from './types';

let lastRec: FakeMediaRecorder | undefined;
let recSupported: (t: string) => boolean = () => true;

class FakeMediaRecorder {
  static isTypeSupported(t: string) {
    return recSupported(t);
  }
  state = 'recording';
  mimeType = 'video/webm;codecs=vp9,opus';
  ondataavailable: ((e: { data: Blob }) => void) | null = null;
  onerror: ((e: unknown) => void) | null = null;
  onstop: (() => void) | null = null;
  startArgs: unknown[] = [];
  stopCalls = 0;
  constructor(
    public stream: any,
    public opts: any,
  ) {
    lastRec = this;
  }
  start(timeslice?: number) {
    this.startArgs.push(timeslice);
  }
  stop() {
    this.stopCalls += 1;
    this.state = 'inactive';
    // A real recorder flushes one final dataavailable before firing onstop.
    this.ondataavailable?.({ data: new Blob(['tail']) });
    this.onstop?.();
  }
}
(globalThis as any).MediaRecorder = FakeMediaRecorder;
(globalThis as any).MediaStream = class {
  constructor(public tracks: any[]) {}
};

const audioTrack = (id: string) => ({ id, kind: 'audio' });
const streamWith = (a: unknown) =>
  ({ getAudioTracks: () => (a ? [a] : []), getVideoTracks: () => [] }) as any;
const src = (peerId: string, stream: any = null, videoOff = false): ParticipantSource => ({
  peerId,
  name: peerId,
  stream,
  videoOff,
});

describe('CompositeRecorder', () => {
  beforeEach(() => {
    h.compositors.length = 0;
    h.mixers.length = 0;
    lastRec = undefined;
    recSupported = () => true;
  });

  it('starts compositor + mixer and records the combined stream in 1s slices', () => {
    const a = src('a', streamWith(audioTrack('ta')));
    const b = src('b');
    const before = Date.now();
    const r = new CompositeRecorder({ sources: [a, b] });
    const comp = h.compositors[0]!;
    const mix = h.mixers[0]!;
    expect(comp.start).toHaveBeenCalledWith([a, b]);
    expect(mix.add.mock.calls).toEqual([
      ['a', a.stream],
      ['b', null],
    ]);
    // Recorder input = canvas video track + mixed audio track, vp9 preferred.
    expect(lastRec!.stream.tracks).toEqual([comp.canvasTrack, mix.mixedTrack]);
    expect(lastRec!.opts).toEqual({
      mimeType: 'video/webm;codecs=vp9,opus',
      videoBitsPerSecond: 2_500_000,
      audioBitsPerSecond: 128_000,
    });
    expect(lastRec!.startArgs).toEqual([1000]);
    expect(r.startedAt).toBeGreaterThanOrEqual(before);
    expect(r.startedAt).toBeLessThanOrEqual(Date.now());
  });

  it('omits mimeType when nothing is supported and honors custom bitrates', () => {
    recSupported = () => false; // let MediaRecorder pick its own default
    new CompositeRecorder({
      sources: [],
      videoBitsPerSecond: 1_000_000,
      audioBitsPerSecond: 64_000,
    });
    expect(lastRec!.opts).toEqual({ videoBitsPerSecond: 1_000_000, audioBitsPerSecond: 64_000 });
    expect('mimeType' in lastRec!.opts).toBe(false);
  });

  it('collects non-empty chunks and assembles the WebM on stop (idempotent)', async () => {
    const r = new CompositeRecorder({ sources: [src('a')] });
    lastRec!.ondataavailable!({ data: new Blob(['abc']) });
    lastRec!.ondataavailable!({ data: new Blob([]) }); // empty chunk → dropped

    const p1 = r.stop();
    const p2 = r.stop();
    expect(p2).toBe(p1); // hang-up racing the stop button returns the same promise
    const blob = await p1;
    expect(lastRec!.stopCalls).toBe(1); // no double-stop
    expect(blob.type).toBe('video/webm;codecs=vp9,opus');
    expect(blob.size).toBe(3 + 4); // 'abc' + the flushed 'tail'

    // Everything released: compositor loop, audio graph, canvas capture track.
    expect(h.compositors[0]!.stop).toHaveBeenCalled();
    expect(h.mixers[0]!.close).toHaveBeenCalled();
    expect(h.compositors[0]!.canvasTrack.stop).toHaveBeenCalled();
  });

  it('stop resolves immediately (default type) when the recorder already died', async () => {
    const onError = vi.fn();
    const r = new CompositeRecorder({ sources: [], onError });
    lastRec!.onerror!({ error: 'boom' }); // mid-session failure → surfaced to the UI
    expect(onError).toHaveBeenCalledWith({ error: 'boom' });

    lastRec!.ondataavailable!({ data: new Blob(['partial']) }); // chunks kept
    lastRec!.state = 'inactive'; // recorder is already dead
    lastRec!.mimeType = ''; // recorder never reported a container
    const blob = await r.stop();
    expect(lastRec!.stopCalls).toBe(0); // no stop() on an inactive recorder
    expect(blob.type).toBe('video/webm'); // fallback container type
    expect(blob.size).toBe(7); // the partial data survives for saving
  });

  it('recorder errors without an onError handler are swallowed', () => {
    new CompositeRecorder({ sources: [] });
    expect(() => lastRec!.onerror!({ e: 1 })).not.toThrow();
  });

  it('addParticipant appends or refreshes; removeParticipant unwires', () => {
    const a = src('a', streamWith(audioTrack('ta')));
    const r = new CompositeRecorder({ sources: [a] });
    const comp = h.compositors[0]!;
    const mix = h.mixers[0]!;
    mix.add.mockClear();

    const b = src('b', streamWith(audioTrack('tb')));
    r.addParticipant(b);
    expect(comp.setSources).toHaveBeenLastCalledWith([a, b]);
    expect(mix.add).toHaveBeenLastCalledWith('b', b.stream);

    const b2 = src('b', streamWith(audioTrack('tb2')));
    r.addParticipant(b2); // same peer → refreshed in place, not appended
    expect(comp.setSources).toHaveBeenLastCalledWith([a, b2]);
    expect(mix.add).toHaveBeenLastCalledWith('b', b2.stream);

    r.removeParticipant('a');
    expect(mix.remove).toHaveBeenCalledWith('a');
    expect(comp.setSources).toHaveBeenLastCalledWith([b2]);
  });

  it('updateStream swaps a stream everywhere; unknown peers are no-ops', () => {
    const a = src('a', streamWith(audioTrack('ta')));
    const r = new CompositeRecorder({ sources: [a] });
    const comp = h.compositors[0]!;
    const mix = h.mixers[0]!;
    mix.add.mockClear();

    const share = streamWith(audioTrack('screen'));
    r.updateStream('a', share); // camera ↔ screen share swap
    expect(comp.updateSource).toHaveBeenCalledWith('a', share);
    expect(mix.add).toHaveBeenCalledWith('a', share);

    r.updateStream('ghost', share);
    expect(comp.updateSource).toHaveBeenCalledTimes(1);
    expect(mix.add).toHaveBeenCalledTimes(1);
  });

  it('setVideoOff marks the source and always forwards to the compositor', () => {
    const r = new CompositeRecorder({ sources: [src('a')] });
    const comp = h.compositors[0]!;
    r.setVideoOff('a', true);
    expect(comp.setVideoOff).toHaveBeenCalledWith('a', true);
    r.setVideoOff('ghost', true); // unknown peer: no crash, still forwarded
    expect(comp.setVideoOff).toHaveBeenCalledWith('ghost', true);
  });

  it('syncRoster diffs by audio track: fresh wrappers of the same track are not re-wired', () => {
    const ta = audioTrack('ta');
    const r = new CompositeRecorder({ sources: [src('a', streamWith(ta))] });
    const comp = h.compositors[0]!;
    const mix = h.mixers[0]!;
    mix.add.mockClear();

    // Roster tick hands a NEW MediaStream wrapper around a's SAME track + a joiner.
    const roster = [src('a', streamWith(ta)), src('b', streamWith(audioTrack('tb')))];
    r.syncRoster(roster);
    expect(mix.add.mock.calls).toEqual([['b', roster[1]!.stream]]); // a untouched → no glitch
    expect(mix.remove).not.toHaveBeenCalled();
    expect(comp.setSources).toHaveBeenLastCalledWith(roster);

    // b leaves and a's actual track changes → remove + re-wire.
    mix.add.mockClear();
    const swapped = [src('a', streamWith(audioTrack('ta2')))];
    r.syncRoster(swapped);
    expect(mix.remove).toHaveBeenCalledWith('b');
    expect(mix.add.mock.calls).toEqual([['a', swapped[0]!.stream]]);

    // a's media drops entirely → re-wired with null (mixer skips it)…
    mix.add.mockClear();
    r.syncRoster([src('a')]);
    expect(mix.add.mock.calls).toEqual([['a', null]]);

    // …and a second null tick changes nothing.
    mix.add.mockClear();
    r.syncRoster([src('a')]);
    expect(mix.add).not.toHaveBeenCalled();
  });
});
