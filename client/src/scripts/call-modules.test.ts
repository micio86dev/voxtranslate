import { beforeEach, describe, expect, it, vi } from 'vitest';

// The orchestrator dynamically imports the whole in-call bundle; each member is
// mocked to a marker object so the test never drags in WebRTC/audio globals.
// Failure tests re-register './webrtc' with vi.doMock (a hoisted factory's result
// is cached across vi.resetModules, so a mutable flag would not re-evaluate).
vi.mock('./webrtc', () => ({ marker: 'webrtc' }));
vi.mock('./chat', () => ({ marker: 'chat' }));
vi.mock('./cartesia', () => ({ marker: 'cartesia' }));
vi.mock('./audio-capture', () => ({ marker: 'audio-capture' }));
vi.mock('./pcm-capture', () => ({ marker: 'pcm-capture' }));
vi.mock('./mic-meter', () => ({ marker: 'mic-meter' }));
vi.mock('./whiteboard', () => ({ marker: 'whiteboard' }));
vi.mock('./tictactoe', () => ({ marker: 'tictactoe' }));
vi.mock('./quiz', () => ({ marker: 'quiz' }));

const marker = (m: unknown): string => (m as { marker: string }).marker;

const mockWebrtcOk = (): void => {
  vi.doMock('./webrtc', () => ({ marker: 'webrtc' }));
};
const mockWebrtcBroken = (): void => {
  vi.doMock('./webrtc', () => {
    throw new Error('chunk failed');
  });
};

beforeEach(() => {
  // The module caches its promise in module scope — get a fresh registry per test.
  vi.resetModules();
});

describe('loadCallModules', () => {
  it('loads all nine in-call modules keyed by name', async () => {
    mockWebrtcOk();
    const { loadCallModules } = await import('./call-modules');
    const mods = await loadCallModules();
    expect(marker(mods.webrtc)).toBe('webrtc');
    expect(marker(mods.chat)).toBe('chat');
    expect(marker(mods.cartesia)).toBe('cartesia');
    expect(marker(mods.audioCapture)).toBe('audio-capture');
    expect(marker(mods.pcmCapture)).toBe('pcm-capture');
    expect(marker(mods.micMeter)).toBe('mic-meter');
    expect(marker(mods.whiteboard)).toBe('whiteboard');
    expect(marker(mods.tictactoe)).toBe('tictactoe');
    expect(marker(mods.quiz)).toBe('quiz');
  });

  it('caches the promise: warm at pre-join, resolve instantly at join', async () => {
    mockWebrtcOk();
    const { loadCallModules } = await import('./call-modules');
    const warm = loadCallModules();
    // Same in-flight promise while downloading…
    expect(loadCallModules()).toBe(warm);
    await warm;
    // …and the same settled promise afterwards (one fetch per page).
    expect(loadCallModules()).toBe(warm);
  });

  it('clears the cache on failure so a later attempt can retry', async () => {
    mockWebrtcBroken();
    const { loadCallModules } = await import('./call-modules');
    const first = loadCallModules();
    await expect(first).rejects.toThrow();
    // The rejected promise must NOT stay cached.
    const second = loadCallModules();
    expect(second).not.toBe(first);
    await expect(second).rejects.toThrow();
  });

  it('recovers on a fresh attempt once the network is back', async () => {
    mockWebrtcBroken();
    let mod = await import('./call-modules');
    await expect(mod.loadCallModules()).rejects.toThrow();

    // Transient blip over → a later attempt succeeds.
    vi.resetModules();
    mockWebrtcOk();
    mod = await import('./call-modules');
    const mods = await mod.loadCallModules();
    expect(marker(mods.webrtc)).toBe('webrtc');
  });
});
