// Stable per-tab peer identity (issue #219).
//
// The peer id is what the server uses to recognise a RECONNECT: a same-id join
// evicts the stale socket and supersedes it WITHOUT a spurious `peer_left`, so
// the other participants keep one tile and an accurate count (spec 0080).
//
// Generating a fresh id on every page load broke that for reload / tab-restore:
// the client rejoined under a NEW id while its previous socket lingered as a
// ghost until the server's ping timeout, so a remaining peer briefly saw BOTH —
// an inflated participant count (3 instead of 2) and a black phantom tile with
// the rejoiner's name (issue #219). Persisting the id in sessionStorage makes a
// reload reuse it (same tab = same identity → clean server-side supersede),
// while a genuinely separate tab gets its own sessionStorage and its own id.

export const PEER_ID_KEY = 'vox-peer-id';

/** A fresh random peer id (UUID when the platform provides one). */
export function freshPeerId(): string {
  const c = (globalThis as { crypto?: Crypto }).crypto;
  if (c && typeof c.randomUUID === 'function') return c.randomUUID();
  return `id-${Math.random().toString(36).slice(2)}-${Date.now()}`;
}

/** sessionStorage if reachable, else null. Accessing it can throw in sandboxed
 *  iframes or when storage is disabled (private mode / blocked cookies). */
function safeSessionStorage(): Pick<Storage, 'getItem' | 'setItem'> | null {
  try {
    return (globalThis as { sessionStorage?: Storage }).sessionStorage ?? null;
  } catch {
    return null;
  }
}

/** The stable peer id for this tab: the persisted value when present, otherwise
 *  a fresh id stored for the next reload. Falls back to an ephemeral fresh id
 *  when storage is unavailable, so a call still works (just without the
 *  reconnect-as-same-id guarantee). */
export function resolvePeerId(
  storage: Pick<Storage, 'getItem' | 'setItem'> | null = safeSessionStorage(),
): string {
  if (storage) {
    try {
      const saved = storage.getItem(PEER_ID_KEY);
      if (saved) return saved;
      const fresh = freshPeerId();
      storage.setItem(PEER_ID_KEY, fresh);
      return fresh;
    } catch {
      /* quota / disabled mid-call — fall through to an ephemeral id */
    }
  }
  return freshPeerId();
}
