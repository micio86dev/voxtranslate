import { describe, it, expect, vi, beforeEach } from 'vitest';

import { isRestrictedVerdict } from './restricted-net';

// A fetch stub whose per-URL behavior we control: reachable ⇒ resolves (opaque
// response), blocked ⇒ rejects (connection reset / timeout).
function fetchStub(reachable: (url: string) => boolean) {
  return vi.fn((url: string) =>
    reachable(url) ? Promise.resolve({} as Response) : Promise.reject(new Error('reset')),
  );
}

describe('isRestrictedVerdict (pure core)', () => {
  it('is restricted only when EVERY probe fails', () => {
    expect(isRestrictedVerdict([false, false])).toBe(true);
    // One reachable endpoint ⇒ open network (a single provider outage must not trip it).
    expect(isRestrictedVerdict([false, true])).toBe(false);
    expect(isRestrictedVerdict([true, true])).toBe(false);
  });

  it('fails open on an empty probe set', () => {
    expect(isRestrictedVerdict([])).toBe(false);
  });
});

describe('isRestrictedNetwork (probing)', () => {
  beforeEach(() => {
    vi.resetModules(); // clears the per-page in-flight cache between cases
    vi.restoreAllMocks();
  });

  it('flags restricted when all probes are reset (GFW)', async () => {
    vi.stubGlobal('fetch', fetchStub(() => false));
    const { isRestrictedNetwork } = await import('./restricted-net');
    expect(await isRestrictedNetwork()).toBe(true);
  });

  it('is not restricted when the canary is reachable (open network / VPN)', async () => {
    vi.stubGlobal('fetch', fetchStub(() => true));
    const { isRestrictedNetwork } = await import('./restricted-net');
    expect(await isRestrictedNetwork()).toBe(false);
  });

  it('is not restricted when only one endpoint is down (provider outage, not GFW)', async () => {
    vi.stubGlobal(
      'fetch',
      fetchStub((url) => url.includes('cartesia')), // gstatic blocked, cartesia up
    );
    const { isRestrictedNetwork } = await import('./restricted-net');
    expect(await isRestrictedNetwork()).toBe(false);
  });

  it('probes once per page load and reuses the verdict', async () => {
    const stub = fetchStub(() => false);
    vi.stubGlobal('fetch', stub);
    const { isRestrictedNetwork, RESTRICTED_PROBE_URLS } = await import('./restricted-net');
    await isRestrictedNetwork();
    await isRestrictedNetwork(); // cached — no new fetches
    expect(stub).toHaveBeenCalledTimes(RESTRICTED_PROBE_URLS.length);
  });

  it('fails open (not restricted) when the probe mechanism is unavailable', async () => {
    // An older browser / stripped environment without fetch must NOT be forced onto
    // relay — run the normal path instead. (Real GFW keeps fetch and rejects instead.)
    vi.stubGlobal('fetch', undefined);
    const { isRestrictedNetwork } = await import('./restricted-net');
    expect(await isRestrictedNetwork()).toBe(false);
  });

  it('re-probes after resetRestrictedNetwork()', async () => {
    const stub = fetchStub(() => false);
    vi.stubGlobal('fetch', stub);
    const { isRestrictedNetwork, resetRestrictedNetwork, RESTRICTED_PROBE_URLS } = await import(
      './restricted-net'
    );
    await isRestrictedNetwork();
    resetRestrictedNetwork();
    await isRestrictedNetwork();
    expect(stub).toHaveBeenCalledTimes(RESTRICTED_PROBE_URLS.length * 2);
  });
});
