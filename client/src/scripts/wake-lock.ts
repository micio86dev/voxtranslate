// Screen Wake Lock (spec 0110).
//
// Talk to Anyone is used with the phone lying on a café table between two people who are
// talking, not touching it. Every mobile OS reads that as idle and dims, then locks —
// which suspends the AudioContext and kills the conversation. This is the one place in
// the app that needs the screen kept awake; nothing else here holds a lock.
//
// Three things make it non-trivial, and all three are handled:
//
//  * The API is absent on some browsers (older iOS Safari, some Android forks). Absence
//    is normal, not an error — the feature degrades to "the screen may dim".
//  * A lock is released BY THE BROWSER whenever the page is hidden, and is not restored
//    on return. Without re-acquiring on `visibilitychange` the lock silently stops
//    working after the first notification the user swipes away.
//  * `request()` rejects if the document is not visible, and that rejection is routine.
//
// Deliberately module-level rather than a class: there is one screen, so there is one
// lock, and two callers fighting over it is a bug not a feature.

/** The slice of `WakeLockSentinel` we use, so tests need no DOM lock implementation. */
export interface WakeLockLike {
  released: boolean;
  release: () => Promise<void>;
}

interface WakeLockCapable {
  wakeLock?: { request: (type: 'screen') => Promise<WakeLockLike> };
}

let sentinel: WakeLockLike | null = null;
/** What the caller asked for, which survives the browser dropping the lock on hide. */
let wanted = false;
let listening = false;

function api(): WakeLockCapable['wakeLock'] | undefined {
  return (navigator as Navigator & WakeLockCapable).wakeLock;
}

/** True when this browser can keep the screen awake at all. */
export function wakeLockSupported(): boolean {
  return typeof navigator !== 'undefined' && !!api();
}

/** True while a lock is actually held (not merely wanted). */
export function wakeLockHeld(): boolean {
  return sentinel !== null && !sentinel.released;
}

async function acquire(): Promise<void> {
  if (!wanted || wakeLockHeld()) return;
  const wakeLock = api();
  if (!wakeLock) return;
  try {
    sentinel = await wakeLock.request('screen');
  } catch {
    // Routine: the document was hidden, or the OS refused (low battery). The
    // `visibilitychange` handler retries when the page comes back.
    sentinel = null;
  }
  // `wanted` can flip to false while the request is in flight — a user who taps End the
  // moment they tap Start. Honour the latest intent, not the one we set out with.
  if (!wanted) await releaseSentinel();
}

async function releaseSentinel(): Promise<void> {
  const held = sentinel;
  sentinel = null;
  if (!held || held.released) return;
  try {
    await held.release();
  } catch {
    // Already gone. Nothing to do, and nothing worth telling the user.
  }
}

function onVisibilityChange(): void {
  if (document.visibilityState === 'visible') {
    void acquire();
  }
}

/**
 * Keep the screen awake until [`releaseWakeLock`] is called. Safe to call repeatedly and
 * on browsers with no support, where it resolves having done nothing.
 */
export async function requestWakeLock(): Promise<void> {
  wanted = true;
  if (!listening && typeof document !== 'undefined') {
    listening = true;
    document.addEventListener('visibilitychange', onVisibilityChange);
  }
  await acquire();
}

/** Let the screen sleep again, and stop re-acquiring on visibility changes. */
export async function releaseWakeLock(): Promise<void> {
  wanted = false;
  if (listening && typeof document !== 'undefined') {
    listening = false;
    document.removeEventListener('visibilitychange', onVisibilityChange);
  }
  await releaseSentinel();
}
