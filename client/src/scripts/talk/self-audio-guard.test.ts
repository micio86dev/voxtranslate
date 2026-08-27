import { describe, it, expect, vi } from 'vitest';
import {
  SelfAudioGuard,
  startLevelMonitor,
  BARGE_IN_MS,
  BARGE_IN_RMS,
  LEVEL_SAMPLE_MS,
  type LevelSource,
} from './self-audio-guard';

function makeGuard() {
  let clock = 0;
  const gates: boolean[] = [];
  const cancelPlayback = vi.fn();
  const onBargeIn = vi.fn();
  const guard = new SelfAudioGuard({
    setGated: (g) => gates.push(g),
    cancelPlayback,
    onBargeIn,
    now: () => clock,
  });
  return {
    guard,
    gates,
    cancelPlayback,
    onBargeIn,
    advance: (ms: number) => {
      clock += ms;
    },
  };
}

/** Hold the level above the threshold for `ms`, sampling as the real monitor would. */
function sustain(
  h: ReturnType<typeof makeGuard>,
  ms: number,
  level = BARGE_IN_RMS + 0.05,
): void {
  for (let t = 0; t <= ms; t += LEVEL_SAMPLE_MS) {
    h.guard.onLevel(level);
    h.advance(LEVEL_SAMPLE_MS);
  }
}

describe('gating on playback edges', () => {
  it('closes when translated audio starts and opens the instant it stops', () => {
    const h = makeGuard();
    h.guard.onPlaybackChange(true);
    expect(h.guard.isGated()).toBe(true);
    expect(h.gates).toEqual([true]);

    h.guard.onPlaybackChange(false);
    expect(h.guard.isGated()).toBe(false);
    expect(h.gates).toEqual([true, false]);
  });

  it('never gates on a timer — only on an edge', () => {
    // The brief is explicit (§44): no fixed delay after a response. Time passing with
    // no edge must change nothing at all.
    const h = makeGuard();
    h.advance(60_000);
    expect(h.gates).toEqual([]);
    expect(h.guard.isGated()).toBe(false);
  });

  it('is idempotent on repeated edges', () => {
    const h = makeGuard();
    h.guard.onPlaybackChange(true);
    h.guard.onPlaybackChange(true);
    h.guard.onPlaybackChange(false);
    h.guard.onPlaybackChange(false);
    expect(h.gates).toEqual([true, false]);
    expect(h.guard.stats().gateCount).toBe(1);
  });
});

describe('barge-in', () => {
  it('cancels the translation when someone talks over it', () => {
    const h = makeGuard();
    h.guard.onPlaybackChange(true);
    sustain(h, BARGE_IN_MS);

    expect(h.cancelPlayback).toHaveBeenCalledTimes(1);
    expect(h.onBargeIn).toHaveBeenCalledTimes(1);
    // The microphone reopens immediately — the interrupter is speaking right now.
    expect(h.guard.isGated()).toBe(false);
    expect(h.gates).toEqual([true, false]);
    expect(h.guard.stats().bargeCount).toBe(1);
  });

  it('ignores a brief peak', () => {
    // A cough, a door, a syllable of echo residue. Not an interruption.
    const h = makeGuard();
    h.guard.onPlaybackChange(true);
    sustain(h, BARGE_IN_MS - LEVEL_SAMPLE_MS * 2);
    expect(h.cancelPlayback).not.toHaveBeenCalled();
    expect(h.guard.isGated()).toBe(true);
  });

  it('requires the energy to be continuous', () => {
    // The translation's own audio peaks rhythmically. Summing those peaks across the
    // quiet between them would cancel the very sentence we are playing.
    const h = makeGuard();
    h.guard.onPlaybackChange(true);
    for (let i = 0; i < 20; i++) {
      h.guard.onLevel(BARGE_IN_RMS + 0.1);
      h.advance(LEVEL_SAMPLE_MS);
      h.guard.onLevel(0.01); // a dip resets the run
      h.advance(LEVEL_SAMPLE_MS);
    }
    expect(h.cancelPlayback).not.toHaveBeenCalled();
    expect(h.guard.isGated()).toBe(true);
  });

  it('ignores quiet input entirely', () => {
    const h = makeGuard();
    h.guard.onPlaybackChange(true);
    sustain(h, BARGE_IN_MS * 3, BARGE_IN_RMS - 0.01);
    expect(h.cancelPlayback).not.toHaveBeenCalled();
  });

  it('does nothing when no translation is playing', () => {
    // Nothing to interrupt: the microphone is already flowing, so loud input is just
    // someone talking, which is the normal case.
    const h = makeGuard();
    sustain(h, BARGE_IN_MS * 2);
    expect(h.cancelPlayback).not.toHaveBeenCalled();
    expect(h.gates).toEqual([]);
  });

  it('starts a fresh run for the next utterance', () => {
    const h = makeGuard();
    h.guard.onPlaybackChange(true);
    sustain(h, BARGE_IN_MS);
    expect(h.guard.stats().bargeCount).toBe(1);

    // A new translation begins; leftover loudness from the last one must not carry over
    // and cancel this one on its first sample.
    h.guard.onPlaybackChange(false);
    h.guard.onPlaybackChange(true);
    h.guard.onLevel(BARGE_IN_RMS + 0.2);
    h.advance(LEVEL_SAMPLE_MS);
    expect(h.cancelPlayback).toHaveBeenCalledTimes(1);
  });
});

describe('full-duplex mode', () => {
  it('never gates when open', () => {
    const h = makeGuard();
    h.guard.setMode('open');
    h.guard.onPlaybackChange(true);
    expect(h.guard.isGated()).toBe(false);
    expect(h.gates).toEqual([]);
  });

  it('releases an already-closed gate when switched open mid-utterance', () => {
    // Otherwise the microphone stays shut until this sentence happens to end.
    const h = makeGuard();
    h.guard.onPlaybackChange(true);
    expect(h.guard.isGated()).toBe(true);
    h.guard.setMode('open');
    expect(h.guard.isGated()).toBe(false);
    expect(h.gates).toEqual([true, false]);
  });

  it('defaults to gating', () => {
    expect(new SelfAudioGuard({ setGated: () => {}, cancelPlayback: () => {} }).getMode()).toBe(
      'gate',
    );
  });
});

describe('reset', () => {
  it('releases the gate and is safe to repeat', () => {
    const h = makeGuard();
    h.guard.onPlaybackChange(true);
    h.guard.reset();
    h.guard.reset();
    expect(h.guard.isGated()).toBe(false);
    expect(h.gates).toEqual([true, false]);
  });
});

describe('startLevelMonitor', () => {
  it('samples on an interval and reports RMS', () => {
    let tick: (() => void) | null = null;
    const cleared: unknown[] = [];
    const source: LevelSource = {
      fftSize: 8,
      // 128 is the zero point of unsigned 8-bit time-domain data; 192 is +0.5.
      getByteTimeDomainData: (buf) => buf.fill(192),
    };
    const levels: number[] = [];
    const stop = startLevelMonitor(
      source,
      (l) => levels.push(l),
      ((fn: () => void, ms: number) => {
        expect(ms).toBe(LEVEL_SAMPLE_MS);
        tick = fn;
        return 7 as unknown as ReturnType<typeof setInterval>;
      }) as unknown as typeof setInterval,
      ((id: unknown) => cleared.push(id)) as unknown as typeof clearInterval,
    );

    tick!();
    tick!();
    expect(levels).toEqual([0.5, 0.5]);

    stop();
    expect(cleared).toEqual([7]);
  });

  it('uses a real interval rather than requestAnimationFrame', () => {
    // rAF is throttled or stopped in a backgrounded tab. Barge-in detection that dies
    // when the screen dims would leave the microphone gated with nothing to reopen it.
    const raf = vi.fn();
    (globalThis as unknown as Record<string, unknown>).requestAnimationFrame = raf;
    const stop = startLevelMonitor({ fftSize: 4, getByteTimeDomainData: () => {} }, () => {});
    expect(raf).not.toHaveBeenCalled();
    stop();
  });
});
