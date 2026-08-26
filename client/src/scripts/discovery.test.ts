import { describe, expect, it } from 'vitest';

import {
  DISCOVERY_LIMIT,
  MAX_ROOM,
  eligible,
  pick,
  roomLanguages,
  weightOf,
  type DiscoveryRoom,
} from './discovery';

/** Build a room with `count` participants, all speaking `langs` (cycled). */
const room = (name: string, count: number, langs: string[] = ['en']): DiscoveryRoom => ({
  room: name,
  count,
  participants: Array.from({ length: count }, (_, i) => ({
    name: `${name}-${i}`,
    lang: langs[i % langs.length],
    avatar: null,
  })),
});

/** Deterministic LCG so the weighted sampling is reproducible in tests. */
function seeded(seed: number): () => number {
  let s = seed >>> 0;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 0x100000000;
  };
}

describe('constants mirror the server', () => {
  it('caps a room at the mesh size', () => {
    expect(MAX_ROOM).toBe(4);
  });
  it('shows five conversations at a time', () => {
    expect(DISCOVERY_LIMIT).toBe(5);
  });
});

describe('eligible', () => {
  it('keeps rooms with 1 to 3 people', () => {
    const rooms = [room('a', 1), room('b', 2), room('c', 3)];
    expect(eligible(rooms).map((r) => r.room)).toEqual(['a', 'b', 'c']);
  });

  it('drops empty rooms — a wall of 0/4 reads as a dead product', () => {
    expect(eligible([room('ghost', 0)])).toEqual([]);
  });

  it('drops full rooms — joining one would just bounce with room_full', () => {
    expect(eligible([room('full', 4)])).toEqual([]);
  });

  it('drops counts above capacity defensively', () => {
    expect(eligible([room('over', 9)])).toEqual([]);
  });
});

describe('weightOf', () => {
  it('favours rooms that are nearly a conversation', () => {
    expect(weightOf(room('a', 2))).toBeGreaterThan(weightOf(room('b', 1)));
    expect(weightOf(room('c', 3))).toBe(weightOf(room('d', 2)));
  });
});

describe('pick', () => {
  const many = Array.from({ length: 12 }, (_, i) => room(`r${i}`, (i % 3) + 1));

  it('never returns more than the limit', () => {
    expect(pick(many, { limit: 5, rng: seeded(1) })).toHaveLength(5);
  });

  it('returns every eligible room when there are fewer than the limit', () => {
    const out = pick([room('a', 1), room('b', 2)], { limit: 5, rng: seeded(2) });
    expect(out.map((r) => r.room).sort()).toEqual(['a', 'b']);
  });

  it('never repeats a room within one batch', () => {
    const out = pick(many, { limit: 5, rng: seeded(3) });
    expect(new Set(out.map((r) => r.room)).size).toBe(out.length);
  });

  it('excludes rooms already on screen when alternatives exist', () => {
    const exclude = new Set(['r0', 'r1', 'r2', 'r3', 'r4']);
    const out = pick(many, { limit: 5, exclude, rng: seeded(4) });
    expect(out.some((r) => exclude.has(r.room))).toBe(false);
  });

  it('tops up from the excluded pool rather than showing a short page', () => {
    const rooms = [room('a', 1), room('b', 2), room('c', 3)];
    const out = pick(rooms, { limit: 5, exclude: new Set(['a', 'b']), rng: seeded(5) });
    // Only 'c' is fresh, so the other two come back rather than leaving gaps.
    expect(out).toHaveLength(3);
    expect(out[0].room).toBe('c');
    expect(out.map((r) => r.room).sort()).toEqual(['a', 'b', 'c']);
  });

  it('excludes ineligible rooms even when they are not in the exclude set', () => {
    const out = pick([room('empty', 0), room('full', 4), room('live', 2)], {
      limit: 5,
      rng: seeded(6),
    });
    expect(out.map((r) => r.room)).toEqual(['live']);
  });

  it('is deterministic for a given rng', () => {
    const a = pick(many, { limit: 5, rng: seeded(7) }).map((r) => r.room);
    const b = pick(many, { limit: 5, rng: seeded(7) }).map((r) => r.room);
    expect(a).toEqual(b);
  });

  it('surfaces busier rooms more often than lonely ones', () => {
    const rng = seeded(11);
    const pool = [room('lonely', 1), room('busy', 2)];
    let busyFirst = 0;
    for (let i = 0; i < 400; i++) {
      if (pick(pool, { limit: 1, rng })[0].room === 'busy') busyFirst++;
    }
    // 3:1 weighting → ~75%. Generous bounds so the test is not flaky.
    expect(busyFirst).toBeGreaterThan(260);
    expect(busyFirst).toBeLessThan(340);
  });

  it('returns nothing when there is nothing joinable', () => {
    expect(pick([], { rng: seeded(8) })).toEqual([]);
    expect(pick([room('empty', 0)], { rng: seeded(9) })).toEqual([]);
  });

  it('defaults to the discovery limit', () => {
    expect(pick(many, { rng: seeded(10) })).toHaveLength(DISCOVERY_LIMIT);
  });
});

describe('roomLanguages', () => {
  it('dedupes so three Italians read as one language', () => {
    expect(roomLanguages(room('a', 3, ['it', 'it', 'es']))).toEqual(['it', 'es']);
  });

  it('skips "auto" — detection is still pending, it is not a language', () => {
    expect(roomLanguages(room('a', 2, ['auto', 'ja']))).toEqual(['ja']);
  });

  it('preserves the order people joined in', () => {
    expect(roomLanguages(room('a', 3, ['ja', 'it', 'es']))).toEqual(['ja', 'it', 'es']);
  });

  it('is empty when nobody has a resolved language yet', () => {
    expect(roomLanguages(room('a', 2, ['auto', 'auto']))).toEqual([]);
  });
});
