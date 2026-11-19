// Live Connect — the staff queue poller (spec 0121, B2B desks).
//
// The server has no push channel for "somebody scanned a desk's QR", so a member of staff
// finds out by asking: `my-desks` every 2 s, plus the queue of any desk with somebody in it.
// A customer waits at most a minute or so, which is why the cadence is this tight; and
// because a phone in a pocket should not keep asking, polling stops while the page is hidden
// and resumes the moment it is shown. A failing server is not hammered: the interval doubles
// (jittered, so a room of colleagues recovering together does not stampede it) up to 30 s,
// and a 401 stops everything — a signed-out tab cannot fix that by retrying.
//
// No DOM here: the page paints the snapshots. Every seam is injectable for tests.

import { deskWaiting, myDesks, type ApiResult, type StaffDesk, type WaitingCustomer } from './api';

export const POLL_MS = 2000;
export const BACKOFF_MAX_MS = 30_000;

export interface DeskSnapshot {
  desks: StaffDesk[];
  /** Queue per desk id; a desk nobody waits at has `[]`. */
  waiting: Record<string, WaitingCustomer[]>;
}

export interface PollerOptions {
  onSnapshot: (snapshot: DeskSnapshot) => void;
  /** A poll failed and will be retried; `failures` counts consecutive failed polls. */
  onError: (error: { status: number; code: string }, failures: number) => void;
  /** The account JWT was refused: polling has stopped and will not restart on its own. */
  onAuthLost: () => void;
  fetchDesks?: () => Promise<ApiResult<{ desks: StaffDesk[] }>>;
  fetchWaiting?: (deskId: string) => Promise<ApiResult<{ waiting: WaitingCustomer[] }>>;
  isHidden?: () => boolean;
  /** Subscribe to the page being hidden or shown; returns the unsubscribe. */
  onVisibilityChange?: (cb: () => void) => () => void;
  random?: () => number;
}

/** Delay before the next poll after `failures` consecutive failures (0 = healthy). */
export function backoffMs(failures: number, random: () => number = Math.random): number {
  const base = Math.min(BACKOFF_MAX_MS, POLL_MS * 2 ** failures);
  return Math.min(BACKOFF_MAX_MS, Math.round(base * (0.5 + 0.5 * random())));
}

export class DeskPoller {
  private readonly opts: PollerOptions;
  private running = false;
  private inFlight = false;
  private failures = 0;
  /** Bumped by `stop()`, so a poll that finishes afterwards knows it is stale. */
  private generation = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private unhook: (() => void) | null = null;
  private lastQueues: Record<string, WaitingCustomer[]> = {};

  constructor(opts: PollerOptions) {
    this.opts = opts;
  }

  start(): void {
    if (this.running) return;
    this.running = true;
    this.unhook = (this.opts.onVisibilityChange ?? browserVisibility)(() => this.onVisible());
    if (!this.hidden()) void this.poll();
  }

  stop(): void {
    this.running = false;
    this.generation += 1;
    this.inFlight = false;
    this.clearTimer();
    this.unhook?.();
    this.unhook = null;
  }

  /** Refresh now (after an answer was refused, say) without doubling the cadence. */
  pollNow(): void {
    if (!this.running) return;
    this.clearTimer();
    if (!this.inFlight && !this.hidden()) void this.poll();
  }

  // --- internals ---------------------------------------------------------

  private hidden(): boolean {
    return (this.opts.isHidden ?? (() => typeof document !== 'undefined' && document.hidden))();
  }

  private onVisible(): void {
    if (!this.running) return;
    if (this.hidden()) {
      this.clearTimer();
      return;
    }
    if (!this.inFlight && this.timer === null) void this.poll();
  }

  private clearTimer(): void {
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }

  private schedule(): void {
    if (!this.running || this.hidden()) return;
    const delay = this.failures === 0 ? POLL_MS : backoffMs(this.failures, this.opts.random);
    this.clearTimer();
    this.timer = setTimeout(() => {
      this.timer = null;
      void this.poll();
    }, delay);
  }

  private authLost(): void {
    this.stop();
    this.opts.onAuthLost();
  }

  private async poll(): Promise<void> {
    this.inFlight = true;
    const gen = this.generation;
    const fetchDesks = this.opts.fetchDesks ?? (() => myDesks());
    const fetchWaiting = this.opts.fetchWaiting ?? ((id: string) => deskWaiting(id));

    const res = await fetchDesks();
    if (gen !== this.generation) return;
    if (!res.ok) {
      this.inFlight = false;
      if (res.status === 401) return this.authLost();
      this.failures += 1;
      this.opts.onError({ status: res.status, code: res.code }, this.failures);
      this.schedule();
      return;
    }

    const desks = res.data.desks;
    let firstError: { status: number; code: string } | null = null;
    const queues: Record<string, WaitingCustomer[]> = {};
    await Promise.all(
      desks.map(async (d) => {
        if (d.waiting <= 0) {
          queues[d.id] = [];
          return;
        }
        const q = await fetchWaiting(d.id);
        if (q.ok) {
          queues[d.id] = q.data.waiting;
          return;
        }
        // Keep what we last knew rather than flashing an empty queue over a hiccup.
        queues[d.id] = this.lastQueues[d.id] ?? [];
        if (q.status === 401) firstError = { status: 401, code: q.code };
        else firstError ??= { status: q.status, code: q.code };
      }),
    );
    if (gen !== this.generation) return;
    this.inFlight = false;
    const failed = firstError as { status: number; code: string } | null;
    if (failed?.status === 401) return this.authLost();

    this.lastQueues = queues;
    this.opts.onSnapshot({ desks, waiting: queues });
    if (failed) {
      this.failures += 1;
      this.opts.onError(failed, this.failures);
    } else {
      this.failures = 0;
    }
    this.schedule();
  }
}

function browserVisibility(cb: () => void): () => void {
  document.addEventListener('visibilitychange', cb);
  return () => document.removeEventListener('visibilitychange', cb);
}
