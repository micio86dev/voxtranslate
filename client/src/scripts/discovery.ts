// Public-conversation discovery for the /world page.
//
// Deliberately pure and DOM-free so vitest can exercise it in a node environment,
// and deliberately CLIENT-side: the server's `GET /rooms` already returns only
// public rooms with at least one person in them (server `rooms.rs::public_rooms`),
// so the remaining rules — capacity, occupancy weighting, "show me different ones"
// — are presentation concerns. A dedicated discovery endpoint would duplicate the
// eligibility logic and create a second source of truth for it.

/** One participant, as `GET /rooms` reports them. */
export interface DiscoveryMember {
  name: string;
  lang: string;
  avatar?: string | null;
}

/** One live public room, as `GET /rooms` reports it. */
export interface DiscoveryRoom {
  room: string;
  count: number;
  participants: DiscoveryMember[];
}

/** Mesh capacity — mirrors the server's `rooms::MAX_PEERS`. */
export const MAX_ROOM = 4;

/** How many conversations one batch of discovery shows. */
export const DISCOVERY_LIMIT = 5;

/**
 * Rooms a visitor can actually walk into.
 *
 * A 0/4 room is never shown: a page full of empty rooms reads as "nobody uses
 * this". A 4/4 room is never shown either — tapping it would just bounce with
 * `room_full`. The server already guarantees public + non-empty, so this is the
 * capacity guard plus a defensive lower bound.
 */
export const eligible = (rooms: DiscoveryRoom[]): DiscoveryRoom[] =>
  rooms.filter((r) => r.count >= 1 && r.count < MAX_ROOM);

/**
 * Sampling weight. A room with 2-3 people is one join away from feeling like a
 * real conversation, so it outranks a room where someone is sitting alone — the
 * point is to CONSOLIDATE a small audience, not scatter it across empty rooms.
 */
export const weightOf = (r: DiscoveryRoom): number => (r.count >= 2 ? 3 : 1);

/** Weighted sample without replacement. Mutates a copy, never the caller's array. */
function sample(pool: DiscoveryRoom[], n: number, rng: () => number): DiscoveryRoom[] {
  const rest = [...pool];
  const out: DiscoveryRoom[] = [];
  while (out.length < n && rest.length) {
    const total = rest.reduce((sum, r) => sum + weightOf(r), 0);
    let target = rng() * total;
    // Fall back to the last item so a floating-point overshoot still picks something.
    let idx = rest.length - 1;
    for (let i = 0; i < rest.length; i++) {
      target -= weightOf(rest[i]);
      if (target < 0) {
        idx = i;
        break;
      }
    }
    out.push(rest.splice(idx, 1)[0]);
  }
  return out;
}

export interface PickOptions {
  /** How many rooms to return (default `DISCOVERY_LIMIT`). */
  limit?: number;
  /** Room codes currently on screen, so "show me more" feels like more. */
  exclude?: Set<string>;
  /** Injectable for deterministic tests. */
  rng?: () => number;
}

/**
 * Choose the next batch of conversations to show.
 *
 * Fresh rooms (not currently displayed) come first. When there aren't enough of
 * them we top up from the excluded ones rather than rendering a short page —
 * repeating a room the user has already seen beats showing them two cards and a
 * gap. There is no persistent history: `exclude` is just what's on screen now.
 */
export function pick(rooms: DiscoveryRoom[], opts: PickOptions = {}): DiscoveryRoom[] {
  const { limit = DISCOVERY_LIMIT, exclude = new Set<string>(), rng = Math.random } = opts;
  if (limit <= 0) return [];
  const pool = eligible(rooms);
  const out = sample(
    pool.filter((r) => !exclude.has(r.room)),
    limit,
    rng,
  );
  if (out.length < limit) {
    out.push(
      ...sample(
        pool.filter((r) => exclude.has(r.room)),
        limit - out.length,
        rng,
      ),
    );
  }
  return out;
}

/**
 * The distinct languages being spoken in a room, in join order.
 *
 * `auto` is dropped rather than rendered: it means server-side detection hasn't
 * resolved yet (see `rooms.rs::set_lang`), so showing it would advertise a
 * language nobody is speaking.
 */
export function roomLanguages(r: DiscoveryRoom): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const p of r.participants) {
    if (!p.lang || p.lang === 'auto' || seen.has(p.lang)) continue;
    seen.add(p.lang);
    out.push(p.lang);
  }
  return out;
}
