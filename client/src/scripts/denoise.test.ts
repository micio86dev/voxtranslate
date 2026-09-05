// @vitest-environment jsdom
// Wiring tests for the RNNoise stage. The model itself is covered by
// `rnnoise-worklet.test.ts` against the real wasm; what matters here is the graph and,
// above all, that EVERY failure path still hands back a usable microphone.
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { createDenoiser, type DenoiseEnv } from './denoise';

/** Records the edges of the audio graph so the crossfade can be asserted. */
interface Edge {
  from: string;
  to: string;
}

function fakeEnv(overrides: Partial<DenoiseEnv> = {}): {
  env: DenoiseEnv;
  edges: Edge[];
  gains: { name: string; value: number }[];
  closed: () => boolean;
  posted: unknown[];
} {
  const edges: Edge[] = [];
  const gains: { name: string; value: number }[] = [];
  const posted: unknown[] = [];
  let closed = false;

  // `connect` reads `this.__name` rather than the constructor argument, so a node
  // renamed after creation (the gains, below) records its final name on every edge.
  const node = (name: string): Record<string, unknown> => ({
    __name: name,
    connect(this: { __name: string }, target: { __name: string }) {
      edges.push({ from: this.__name, to: target.__name });
      return target;
    },
    disconnect() {},
  });

  const ctx = {
    audioWorklet: { addModule: vi.fn(async () => {}) },
    createMediaStreamSource: () => node('source'),
    createGain: () => {
      const g = node('gain') as Record<string, unknown> & { gain: { value: number } };
      g.gain = { value: 0 };
      const entry = { name: '', value: 0 };
      gains.push(entry);
      // Name the gain by the value assigned to it, so wet/dry are distinguishable.
      Object.defineProperty(g.gain, 'value', {
        get: () => entry.value,
        set: (v: number) => {
          entry.value = v;
        },
      });
      (g as { __name: string }).__name = `gain${gains.length - 1}`;
      return g;
    },
    createMediaStreamDestination: () => ({
      ...node('dest'),
      stream: { getAudioTracks: () => [{ kind: 'audio', id: 'denoised' }] },
    }),
    close: async () => {
      closed = true;
    },
  };

  // AudioWorkletNode is a global constructor in the browser.
  vi.stubGlobal(
    'AudioWorkletNode',
    class {
      __name = 'worklet';
      port = { postMessage: (m: unknown) => posted.push(m), onmessage: null };
      connect(target: { __name: string }) {
        edges.push({ from: 'worklet', to: target.__name });
        return target;
      }
      disconnect() {}
    },
  );

  return {
    env: {
      createContext: () => ctx as unknown as AudioContext,
      fetchWasm: async () => new ArrayBuffer(8),
      ...overrides,
    },
    edges,
    gains,
    closed: () => closed,
    posted,
  };
}

function micStream(withVideo = false): MediaStream {
  const audio = { kind: 'audio', id: 'mic' };
  const video = { kind: 'video', id: 'cam' };
  return {
    getAudioTracks: () => [audio],
    getVideoTracks: () => (withVideo ? [video] : []),
  } as unknown as MediaStream;
}

beforeEach(() => {
  vi.unstubAllGlobals();
  // jsdom ships no MediaStream, and the module builds two of them (one to isolate the
  // input track, one to recombine denoised audio with the untouched camera).
  vi.stubGlobal(
    'MediaStream',
    class {
      _tracks: { kind: string; id: string }[];
      constructor(tracks: { kind: string; id: string }[] = []) {
        this._tracks = tracks;
      }
      getAudioTracks() {
        return this._tracks.filter((t) => t.kind === 'audio');
      }
      getVideoTracks() {
        return this._tracks.filter((t) => t.kind === 'video');
      }
    },
  );
});

describe('createDenoiser', () => {
  it('builds a wet/dry crossfade at the configured mix', async () => {
    const { env, edges, gains } = fakeEnv();
    const d = await createDenoiser(micStream(), env, 70);

    expect(d.active).toBe(true);
    // Both legs must reach the destination, or the crossfade is really a switch.
    expect(edges).toContainEqual({ from: 'source', to: 'worklet' });
    expect(edges).toContainEqual({ from: 'worklet', to: 'gain0' });
    expect(edges).toContainEqual({ from: 'gain0', to: 'dest' });
    expect(edges).toContainEqual({ from: 'source', to: 'gain1' });
    expect(edges).toContainEqual({ from: 'gain1', to: 'dest' });
    // Wet + dry must sum to unity, otherwise the toggle changes the volume too.
    expect(gains.map((g) => g.value)).toEqual([0.7, 0.30000000000000004]);
    expect(gains[0].value + gains[1].value).toBeCloseTo(1, 10);
  });

  it('hands the wasm bytes to the worklet', async () => {
    const { env, posted } = fakeEnv();
    await createDenoiser(micStream(), env, 70);
    expect(posted).toHaveLength(1);
    expect((posted[0] as { type: string }).type).toBe('wasm');
  });

  it('keeps the camera track on the returned stream', async () => {
    // Only the audio is rebuilt; dropping the video here would kill the user's camera.
    const { env } = fakeEnv();
    const d = await createDenoiser(micStream(true), env, 70);
    expect(d.stream.getVideoTracks()).toHaveLength(1);
    expect(d.stream.getAudioTracks()[0].id).toBe('denoised');
  });

  it('returns the ORIGINAL stream when the wasm cannot be fetched', async () => {
    // A 404 on the model must not cost the user their microphone.
    const input = micStream();
    const { env, closed } = fakeEnv({
      fetchWasm: async () => {
        throw new Error('404');
      },
    });
    const d = await createDenoiser(input, env, 70);
    expect(d.active).toBe(false);
    expect(d.stream).toBe(input);
    expect(closed()).toBe(true); // the half-built context is released, not leaked
  });

  it('returns the ORIGINAL stream when the worklet module is blocked', async () => {
    const input = micStream();
    const { env } = fakeEnv();
    const ctx = env.createContext(48_000) as unknown as {
      audioWorklet: { addModule: ReturnType<typeof vi.fn> };
    };
    ctx.audioWorklet.addModule.mockRejectedValue(new Error('CSP'));
    const d = await createDenoiser(input, env, 70);
    expect(d.active).toBe(false);
    expect(d.stream).toBe(input);
  });

  it('returns the ORIGINAL stream on a browser without AudioWorklet', async () => {
    const input = micStream();
    const env: DenoiseEnv = {
      createContext: () => ({ close: async () => {} }) as unknown as AudioContext,
      fetchWasm: async () => new ArrayBuffer(8),
    };
    const d = await createDenoiser(input, env, 70);
    expect(d.active).toBe(false);
    expect(d.stream).toBe(input);
  });

  it('does no work at all at 0% mix, and never touches a stream without audio', async () => {
    const input = micStream();
    const { env } = fakeEnv();
    expect((await createDenoiser(input, env, 0)).stream).toBe(input);

    const silent = { getAudioTracks: () => [], getVideoTracks: () => [] } as unknown as MediaStream;
    expect((await createDenoiser(silent, env, 70)).stream).toBe(silent);
  });

  it('closes the context on stop, and stop is safe to call twice', async () => {
    const { env, closed } = fakeEnv();
    const d = await createDenoiser(micStream(), env, 70);
    await d.stop();
    expect(closed()).toBe(true);
    await expect(d.stop()).resolves.toBeUndefined();
  });
});
