import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
vi.mock('../auth', () => ({ HTTP_BASE: 'http://api.test', authHeaders: () => ({}) }));

import type { ApiResult, StaffDesk, WaitingCustomer } from './api';
import { backoffMs, BACKOFF_MAX_MS, DeskPoller, POLL_MS, type DeskSnapshot } from './desk-poll';

const desk = (id: string, waiting: number): StaffDesk => ({
  id, org_id: 'o', org_name: 'Acme', name: `Desk ${id}`, location_label: null, default_lang: 'en', waiting,
});
const cust = (id: string): WaitingCustomer => ({ session_id: id, guest_name: id, guest_lang: 'it', waited_secs: 1 });
const ok = <T>(data: T): ApiResult<T> => ({ ok: true, data });
const fail = (status: number, code = 'http_error'): ApiResult<never> => ({ ok: false, status, code });

interface Rig {
  poller: DeskPoller;
  snapshots: DeskSnapshot[];
  errors: Array<{ status: number; failures: number }>;
  authLost: ReturnType<typeof vi.fn>;
  fetchDesks: ReturnType<typeof vi.fn>;
  fetchWaiting: ReturnType<typeof vi.fn>;
  hide(hidden: boolean): void;
}

let hidden = false;
let visListeners: Array<() => void> = [];

function rig(over: { desks?: () => Promise<ApiResult<{ desks: StaffDesk[] }>>; waiting?: (id: string) => Promise<ApiResult<{ waiting: WaitingCustomer[] }>> } = {}): Rig {
  const snapshots: DeskSnapshot[] = [];
  const errors: Rig['errors'] = [];
  const authLost = vi.fn();
  const fetchDesks = vi.fn(over.desks ?? (async () => ok({ desks: [desk('d1', 0)] })));
  const fetchWaiting = vi.fn(over.waiting ?? (async () => ok({ waiting: [] })));
  const poller = new DeskPoller({
    fetchDesks,
    fetchWaiting,
    onSnapshot: (s) => snapshots.push(s),
    onError: (e, failures) => errors.push({ status: e.status, failures }),
    onAuthLost: authLost,
    isHidden: () => hidden,
    onVisibilityChange: (cb) => {
      visListeners.push(cb);
      return () => {
        visListeners = visListeners.filter((l) => l !== cb);
      };
    },
    random: () => 0.5,
  });
  return {
    poller, snapshots, errors, authLost, fetchDesks, fetchWaiting,
    hide(h) {
      hidden = h;
      for (const l of [...visListeners]) l();
    },
  };
}

beforeEach(() => {
  vi.useFakeTimers();
  hidden = false;
  visListeners = [];
});
afterEach(() => vi.useRealTimers());

describe('backoffMs', () => {
  it('doubles from the poll interval, caps, and jitters between half and all of it', () => {
    expect(backoffMs(1, () => 1)).toBe(POLL_MS * 2);
    expect(backoffMs(2, () => 1)).toBe(POLL_MS * 4);
    expect(backoffMs(20, () => 1)).toBe(BACKOFF_MAX_MS);
    expect(backoffMs(2, () => 0)).toBe(POLL_MS * 2);
    expect(backoffMs(0, () => 1)).toBe(POLL_MS);
  });
});

describe('polling cadence', () => {
  it('polls at once, then every 2 s', async () => {
    const r = rig();
    r.poller.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.fetchDesks).toHaveBeenCalledTimes(1);
    expect(r.snapshots).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(r.fetchDesks).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(r.fetchDesks).toHaveBeenCalledTimes(3);
    r.poller.stop();
  });

  it('asks for the queue only of desks that have somebody waiting, and publishes both', async () => {
    const r = rig({
      desks: async () => ok({ desks: [desk('d1', 2), desk('d2', 0), desk('d3', 1)] }),
      waiting: async (id) => ok({ waiting: id === 'd1' ? [cust('a'), cust('b')] : [cust('c')] }),
    });
    r.poller.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.fetchWaiting.mock.calls.map((c) => c[0]).sort()).toEqual(['d1', 'd3']);
    const snap = r.snapshots[0];
    expect(snap.desks.map((d) => d.id)).toEqual(['d1', 'd2', 'd3']);
    expect(snap.waiting.d1.map((w) => w.session_id)).toEqual(['a', 'b']);
    expect(snap.waiting.d2).toEqual([]);
    expect(snap.waiting.d3.map((w) => w.session_id)).toEqual(['c']);
    r.poller.stop();
  });

  it('start is idempotent: a second start does not double the cadence', async () => {
    const r = rig();
    r.poller.start();
    r.poller.start();
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(r.fetchDesks).toHaveBeenCalledTimes(2);
    r.poller.stop();
  });
});

describe('visibility', () => {
  it('does not poll while the page is hidden, and polls again the moment it is shown', async () => {
    const r = rig();
    r.poller.start();
    await vi.advanceTimersByTimeAsync(0);
    r.hide(true);
    await vi.advanceTimersByTimeAsync(POLL_MS * 10);
    expect(r.fetchDesks).toHaveBeenCalledTimes(1);
    r.hide(false);
    await vi.advanceTimersByTimeAsync(0);
    expect(r.fetchDesks).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(r.fetchDesks).toHaveBeenCalledTimes(3);
    r.poller.stop();
  });

  it('started while hidden, it waits for the page to be shown', async () => {
    hidden = true;
    const r = rig();
    r.poller.start();
    await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    expect(r.fetchDesks).not.toHaveBeenCalled();
    r.hide(false);
    await vi.advanceTimersByTimeAsync(0);
    expect(r.fetchDesks).toHaveBeenCalledTimes(1);
    r.poller.stop();
  });

  it('a poll already in flight when the page is hidden does not schedule another', async () => {
    let release: (v: ApiResult<{ desks: StaffDesk[] }>) => void = () => {};
    const r = rig({ desks: () => new Promise((res) => (release = res)) });
    r.poller.start();
    r.hide(true);
    release(ok({ desks: [desk('d1', 0)] }));
    await vi.advanceTimersByTimeAsync(POLL_MS * 5);
    expect(r.fetchDesks).toHaveBeenCalledTimes(1);
    r.poller.stop();
  });

  it('showing the page while a poll is in flight does not start a second one', async () => {
    let release: (v: ApiResult<{ desks: StaffDesk[] }>) => void = () => {};
    const r = rig({ desks: () => new Promise((res) => (release = res)) });
    r.poller.start();
    r.hide(true);
    r.hide(false);
    expect(r.fetchDesks).toHaveBeenCalledTimes(1);
    release(ok({ desks: [] }));
    await vi.advanceTimersByTimeAsync(0);
    r.poller.stop();
  });
});

describe('failure', () => {
  it('backs off with jitter on a failed poll, reports it, and recovers to the normal cadence', async () => {
    let n = 0;
    const r = rig({ desks: async () => (++n <= 2 ? fail(503) : ok({ desks: [desk('d1', 0)] })) });
    r.poller.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.errors).toEqual([{ status: 503, failures: 1 }]);
    // random()=0.5 -> 0.75 of 2 * POLL_MS.
    await vi.advanceTimersByTimeAsync(backoffMs(1, () => 0.5) - 1);
    expect(r.fetchDesks).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(r.fetchDesks).toHaveBeenCalledTimes(2);
    expect(r.errors[1]).toEqual({ status: 503, failures: 2 });
    await vi.advanceTimersByTimeAsync(backoffMs(2, () => 0.5));
    expect(r.fetchDesks).toHaveBeenCalledTimes(3);
    expect(r.snapshots).toHaveLength(1);
    // Healthy again: back to the 2 s cadence, failures reset.
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(r.fetchDesks).toHaveBeenCalledTimes(4);
    r.poller.stop();
  });

  it('a dead network is an error too, not a crash', async () => {
    const r = rig({ desks: async () => fail(0, 'network') });
    r.poller.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.errors).toEqual([{ status: 0, failures: 1 }]);
    r.poller.stop();
  });

  it('401 stops polling for good and says so once', async () => {
    const r = rig({ desks: async () => fail(401) });
    r.poller.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.authLost).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(BACKOFF_MAX_MS * 3);
    expect(r.fetchDesks).toHaveBeenCalledTimes(1);
    expect(r.errors).toEqual([]);
    // Showing the page again must not resurrect it.
    r.hide(false);
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(r.fetchDesks).toHaveBeenCalledTimes(1);
  });

  it('401 on a queue fetch stops polling too', async () => {
    const r = rig({ desks: async () => ok({ desks: [desk('d1', 1)] }), waiting: async () => fail(401) });
    r.poller.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.authLost).toHaveBeenCalledTimes(1);
    expect(r.snapshots).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(POLL_MS * 5);
    expect(r.fetchDesks).toHaveBeenCalledTimes(1);
  });

  it('a failed queue fetch keeps the desks and the last known queue, and counts as a failure', async () => {
    let call = 0;
    const r = rig({
      desks: async () => ok({ desks: [desk('d1', 1)] }),
      waiting: async () => (++call === 1 ? ok({ waiting: [cust('a')] }) : fail(500, 'server_error')),
    });
    r.poller.start();
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(r.snapshots).toHaveLength(2);
    expect(r.snapshots[1].waiting.d1.map((w) => w.session_id)).toEqual(['a']);
    expect(r.errors).toEqual([{ status: 500, failures: 1 }]);
    r.poller.stop();
  });
});

describe('stop and refresh', () => {
  it('stop cancels the timer, ignores a poll in flight and unhooks visibility', async () => {
    let release: (v: ApiResult<{ desks: StaffDesk[] }>) => void = () => {};
    const r = rig({ desks: () => new Promise((res) => (release = res)) });
    r.poller.start();
    r.poller.stop();
    release(ok({ desks: [desk('d1', 0)] }));
    await vi.advanceTimersByTimeAsync(POLL_MS * 5);
    expect(r.snapshots).toHaveLength(0);
    expect(r.fetchDesks).toHaveBeenCalledTimes(1);
    expect(visListeners).toHaveLength(0);
    expect(vi.getTimerCount()).toBe(0);
  });

  it('pollNow refreshes immediately without doubling the cadence', async () => {
    const r = rig();
    r.poller.start();
    await vi.advanceTimersByTimeAsync(0);
    r.poller.pollNow();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.fetchDesks).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(r.fetchDesks).toHaveBeenCalledTimes(3);
    r.poller.stop();
  });
});
