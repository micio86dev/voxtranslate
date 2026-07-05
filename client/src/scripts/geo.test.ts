// @vitest-environment jsdom
// Location opt-in banner (superadmin user map). Auth is mocked (module-load side
// effects + network); geolocation / featurePolicy / fetch are stubbed per test.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const auth = vi.hoisted(() => ({
  loggedIn: true,
  authHeaders: vi.fn((): Record<string, string> => ({ Authorization: 'Bearer tok' })),
}));

vi.mock('./auth', () => ({
  HTTP_BASE: 'https://api.test',
  isLoggedIn: () => auth.loggedIn,
  authHeaders: auth.authHeaders,
}));

import { setupGeoOptIn } from './geo';

const KEY = 'vox.geo';

function buildBanner(): HTMLElement {
  document.body.innerHTML =
    '<div id="geo-banner" class="hidden">' +
    '<button id="geo-enable"></button><button id="geo-dismiss"></button></div>';
  return document.getElementById('geo-banner')!;
}

type PositionOk = (pos: GeolocationPosition) => void;
type PositionErr = (err: GeolocationPositionError) => void;

function stubGeolocation(impl?: (ok: PositionOk, err: PositionErr) => void) {
  const getCurrentPosition = vi.fn(
    (ok: PositionOk, err: PositionErr, _opts?: PositionOptions) => impl?.(ok, err),
  );
  Object.defineProperty(navigator, 'geolocation', {
    value: { getCurrentPosition },
    configurable: true,
  });
  return getCurrentPosition;
}

function setFeaturePolicy(fp: unknown): void {
  Object.defineProperty(document, 'featurePolicy', { value: fp, configurable: true });
}

const click = (id: string): void => {
  document.getElementById(id)!.dispatchEvent(new MouseEvent('click'));
};

beforeEach(() => {
  auth.loggedIn = true;
  localStorage.clear();
  buildBanner();
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  delete (navigator as unknown as { geolocation?: unknown }).geolocation;
  delete (document as unknown as { featurePolicy?: unknown }).featurePolicy;
});

describe('setupGeoOptIn eligibility', () => {
  it('is a no-op when the banner markup is absent', () => {
    document.body.innerHTML = '';
    expect(() => setupGeoOptIn()).not.toThrow();
  });

  it('stays hidden for guests', () => {
    auth.loggedIn = false;
    stubGeolocation();
    const banner = buildBanner();
    banner.classList.remove('hidden'); // prove setup actively hides it
    setupGeoOptIn();
    expect(banner.classList.contains('hidden')).toBe(true);
  });

  it('stays hidden when the browser has no geolocation API', () => {
    const banner = buildBanner();
    setupGeoOptIn();
    expect(banner.classList.contains('hidden')).toBe(true);
  });

  it('stays hidden when the Permissions-Policy denies geolocation', () => {
    stubGeolocation();
    const allowsFeature = vi.fn((_f: string) => false);
    setFeaturePolicy({ allowsFeature });
    const banner = buildBanner();
    setupGeoOptIn();
    expect(banner.classList.contains('hidden')).toBe(true);
    expect(allowsFeature).toHaveBeenCalledWith('geolocation');
  });

  it('stays hidden once a prior choice was remembered (never nags)', () => {
    stubGeolocation();
    localStorage.setItem(KEY, 'dismissed');
    const banner = buildBanner();
    setupGeoOptIn();
    expect(banner.classList.contains('hidden')).toBe(true);
  });

  it('shows for an eligible signed-in user (featurePolicy allowing)', () => {
    stubGeolocation();
    setFeaturePolicy({ allowsFeature: () => true });
    const banner = buildBanner();
    setupGeoOptIn();
    expect(banner.classList.contains('hidden')).toBe(false);
  });

  it('fails open when reading featurePolicy throws (browser enforces instead)', () => {
    stubGeolocation();
    Object.defineProperty(document, 'featurePolicy', {
      get() {
        throw new Error('blocked');
      },
      configurable: true,
    });
    const banner = buildBanner();
    setupGeoOptIn();
    expect(banner.classList.contains('hidden')).toBe(false);
  });
});

describe('setupGeoOptIn choices', () => {
  it('"Not now" remembers the dismissal and hides the banner', () => {
    stubGeolocation();
    const banner = buildBanner();
    setupGeoOptIn();
    click('geo-dismiss');
    expect(banner.classList.contains('hidden')).toBe(true);
    expect(localStorage.getItem(KEY)).toBe('dismissed');
  });

  it('enable → GPS success posts the coordinates and remembers "granted"', () => {
    const fetchSpy = vi.fn(async () => ({}) as Response);
    vi.stubGlobal('fetch', fetchSpy);
    const getPos = stubGeolocation((ok) =>
      ok({ coords: { latitude: 45.5, longitude: 9.2, accuracy: 12 } } as GeolocationPosition),
    );
    const banner = buildBanner();
    setupGeoOptIn();
    click('geo-enable');

    expect(banner.classList.contains('hidden')).toBe(true);
    expect(getPos).toHaveBeenCalledTimes(1);
    expect(localStorage.getItem(KEY)).toBe('granted');
    expect(fetchSpy).toHaveBeenCalledTimes(1);
    const [url, init] = fetchSpy.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe('https://api.test/api/user/location');
    expect(init.method).toBe('POST');
    expect(init.headers).toMatchObject({
      Authorization: 'Bearer tok',
      'Content-Type': 'application/json',
    });
    expect(JSON.parse(init.body as string)).toEqual({
      latitude: 45.5,
      longitude: 9.2,
      accuracy: 12,
    });
  });

  it('enable → OS-level denial soft-dismisses without posting', () => {
    const fetchSpy = vi.fn(async () => ({}) as Response);
    vi.stubGlobal('fetch', fetchSpy);
    stubGeolocation((_ok, err) => err({ code: 1 } as GeolocationPositionError));
    buildBanner();
    setupGeoOptIn();
    click('geo-enable');
    expect(localStorage.getItem(KEY)).toBe('dismissed');
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it('a failed POST is swallowed (best-effort, nothing surfaces)', async () => {
    const fetchSpy = vi.fn(async () => {
      throw new Error('network down');
    });
    vi.stubGlobal('fetch', fetchSpy);
    stubGeolocation((ok) =>
      ok({ coords: { latitude: 1, longitude: 2, accuracy: 3 } } as GeolocationPosition),
    );
    buildBanner();
    setupGeoOptIn();
    click('geo-enable');
    expect(fetchSpy).toHaveBeenCalledTimes(1);
    // Flush the rejected sendLocation promise — an unhandled rejection would fail the run.
    await Promise.resolve();
    await Promise.resolve();
    expect(localStorage.getItem(KEY)).toBe('granted');
  });

  it('blocked storage fails open: still asks, and dismissing does not crash', () => {
    stubGeolocation();
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('storage blocked');
    });
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('storage blocked');
    });
    const banner = buildBanner();
    setupGeoOptIn();
    expect(banner.classList.contains('hidden')).toBe(false);
    expect(() => click('geo-dismiss')).not.toThrow();
    expect(banner.classList.contains('hidden')).toBe(true);
  });
});
