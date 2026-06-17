import { describe, expect, it, vi } from 'vitest';
import { PEER_ID_KEY, freshPeerId, resolvePeerId } from './peer-id';

/** Minimal in-memory Storage stub. */
function memStorage(initial: Record<string, string> = {}) {
  const map = new Map(Object.entries(initial));
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    _map: map,
  };
}

describe('resolvePeerId (#219 reconnect identity)', () => {
  it('reuses the persisted id across reloads (same tab = same identity)', () => {
    const storage = memStorage({ [PEER_ID_KEY]: 'peer-abc' });
    expect(resolvePeerId(storage)).toBe('peer-abc');
    expect(resolvePeerId(storage)).toBe('peer-abc'); // stable on repeat
  });

  it('generates AND persists a fresh id when none is stored', () => {
    const storage = memStorage();
    const id = resolvePeerId(storage);
    expect(id).toBeTruthy();
    // Persisted, so the next reload (a new resolvePeerId call) returns the same id.
    expect(storage._map.get(PEER_ID_KEY)).toBe(id);
    expect(resolvePeerId(storage)).toBe(id);
  });

  it('falls back to an ephemeral id when storage is unavailable', () => {
    const id = resolvePeerId(null);
    expect(id).toBeTruthy();
  });

  it('falls back to an ephemeral id when storage throws (private mode)', () => {
    const throwing = {
      getItem: vi.fn(() => {
        throw new Error('blocked');
      }),
      setItem: vi.fn(),
    };
    const id = resolvePeerId(throwing);
    expect(id).toBeTruthy();
  });

  it('freshPeerId returns distinct ids', () => {
    expect(freshPeerId()).not.toBe(freshPeerId());
  });
});
