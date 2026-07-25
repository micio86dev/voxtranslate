// Unit tests for the Web Push opt-in (spec: scheduled meetings, Phase 1e).
// navigator.serviceWorker / PushManager / Notification are minimal fakes; the
// module is best-effort and non-throwing, so every branch resolves quietly.
import { readFileSync } from 'node:fs';
import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from 'vitest';

const authState = vi.hoisted(() => ({ loggedIn: true }));
vi.mock('./auth', () => ({
  HTTP_BASE: 'http://api.test',
  authHeaders: () => ({ Authorization: 'Bearer tok' }),
  isLoggedIn: () => authState.loggedIn,
}));

import { enablePush, maybeSubscribePush } from './push';

interface BrowserOpts {
  permission?: NotificationPermission;
  grant?: NotificationPermission;
  subscription?: unknown;
  registerError?: Error;
  noServiceWorker?: boolean;
  noPushManager?: boolean;
  noNotification?: boolean;
}

/** Install fake navigator/window/Notification globals; returns the inner spies. */
function installBrowser(opts: BrowserOpts = {}): {
  register: Mock;
  subscribe: Mock;
  requestPermission: Mock;
} {
  const subscription = opts.subscription ?? {
    toJSON: () => ({
      endpoint: 'https://push.test/ep',
      keys: { p256dh: 'P256', auth: 'AUTH' },
    }),
  };
  const subscribe = vi.fn().mockResolvedValue(subscription);
  const reg = { pushManager: { subscribe } };
  const register = opts.registerError
    ? vi.fn().mockRejectedValue(opts.registerError)
    : vi.fn().mockResolvedValue(reg);
  const nav: Record<string, unknown> = { userAgent: 'vitest-ua' };
  if (!opts.noServiceWorker) nav.serviceWorker = { register, ready: Promise.resolve(reg) };
  const win: Record<string, unknown> = {};
  if (!opts.noPushManager) win.PushManager = {};
  if (!opts.noNotification) win.Notification = {};
  const requestPermission = vi.fn().mockResolvedValue(opts.grant ?? 'granted');
  vi.stubGlobal('navigator', nav);
  vi.stubGlobal('window', win);
  vi.stubGlobal('Notification', {
    permission: opts.permission ?? 'granted',
    requestPermission,
  });
  return { register, subscribe, requestPermission };
}

interface ApiOpts {
  key?: string | null;
  vapidStatus?: number;
  vapidRejects?: boolean;
  subscribeStatus?: number;
}

/** Stub fetch for the two push endpoints (VAPID key + subscribe). */
function stubApi(opts: ApiOpts = {}): Mock {
  const mock = vi.fn((url: unknown) => {
    if (String(url).endsWith('/api/push/vapid-public-key')) {
      if (opts.vapidRejects) return Promise.reject(new TypeError('network down'));
      const status = opts.vapidStatus ?? 200;
      return Promise.resolve({
        ok: status < 300,
        status,
        json: async () => ({ key: opts.key === undefined ? 'AQID' : opts.key }),
      } as unknown as Response);
    }
    const status = opts.subscribeStatus ?? 200;
    return Promise.resolve({ ok: status < 300, status, json: async () => ({}) } as unknown as Response);
  });
  vi.stubGlobal('fetch', mock);
  return mock;
}

const subscribePost = (mock: Mock): [string, RequestInit] | undefined =>
  mock.mock.calls.find(([u]) => String(u).endsWith('/api/push/subscribe')) as
    | [string, RequestInit]
    | undefined;

beforeEach(() => {
  authState.loggedIn = true;
});
afterEach(() => vi.unstubAllGlobals());

describe('maybeSubscribePush', () => {
  it('does nothing when push is unsupported', async () => {
    installBrowser({ noServiceWorker: true });
    const fetchMock = stubApi();
    await maybeSubscribePush();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('does nothing for guests', async () => {
    authState.loggedIn = false;
    installBrowser();
    const fetchMock = stubApi();
    await maybeSubscribePush();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('never prompts when permission was not yet granted', async () => {
    const { requestPermission } = installBrowser({ permission: 'default' });
    const fetchMock = stubApi();
    await maybeSubscribePush();
    expect(requestPermission).not.toHaveBeenCalled();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('silently subscribes once signed in with permission granted', async () => {
    const { register, requestPermission } = installBrowser();
    const fetchMock = stubApi();
    await maybeSubscribePush();
    expect(register).toHaveBeenCalledWith('/sw.js');
    expect(requestPermission).not.toHaveBeenCalled(); // silent path never prompts
    const post = subscribePost(fetchMock);
    expect(post).toBeDefined();
    const [, init] = post as [string, RequestInit];
    expect(init.method).toBe('POST');
    expect(init.headers).toEqual({
      Authorization: 'Bearer tok',
      'Content-Type': 'application/json',
    });
    expect(JSON.parse(String(init.body))).toEqual({
      endpoint: 'https://push.test/ep',
      keys: { p256dh: 'P256', auth: 'AUTH' },
      user_agent: 'vitest-ua',
    });
  });
});

describe('enablePush', () => {
  it('returns false when push is unsupported (no PushManager)', async () => {
    const { requestPermission } = installBrowser({ noPushManager: true });
    stubApi();
    expect(await enablePush()).toBe(false);
    expect(requestPermission).not.toHaveBeenCalled();
  });

  it('returns false when Notification is missing from window', async () => {
    installBrowser({ noNotification: true });
    stubApi();
    expect(await enablePush()).toBe(false);
  });

  it('returns false for guests', async () => {
    authState.loggedIn = false;
    installBrowser();
    stubApi();
    expect(await enablePush()).toBe(false);
  });

  it('returns false when the user denies the prompt', async () => {
    const { requestPermission } = installBrowser({ grant: 'denied' });
    const fetchMock = stubApi();
    expect(await enablePush()).toBe(false);
    expect(requestPermission).toHaveBeenCalled();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('prompts, subscribes and POSTs the subscription on the happy path', async () => {
    const { subscribe } = installBrowser();
    const fetchMock = stubApi();
    expect(await enablePush()).toBe(true);
    expect(subscribe).toHaveBeenCalledOnce();
    expect(subscribePost(fetchMock)).toBeDefined();
  });

  it('decodes the base64url VAPID key into applicationServerKey bytes', async () => {
    const { subscribe } = installBrowser();
    stubApi({ key: 'AQID' }); // plain base64: bytes 1,2,3
    expect(await enablePush()).toBe(true);
    const opts = subscribe.mock.calls[0]?.[0] as {
      userVisibleOnly: boolean;
      applicationServerKey: Uint8Array;
    };
    expect(opts.userVisibleOnly).toBe(true);
    expect(Array.from(opts.applicationServerKey)).toEqual([1, 2, 3]);
  });

  it('handles url-safe characters and re-pads the key', async () => {
    const { subscribe } = installBrowser();
    stubApi({ key: '-_8' }); // '-'→'+', '_'→'/', pad '=' → bytes 0xFB,0xFF
    expect(await enablePush()).toBe(true);
    const opts = subscribe.mock.calls[0]?.[0] as { applicationServerKey: Uint8Array };
    expect(Array.from(opts.applicationServerKey)).toEqual([251, 255]);
  });

  it('returns false when the VAPID endpoint errors', async () => {
    const { subscribe } = installBrowser();
    stubApi({ vapidStatus: 503 });
    expect(await enablePush()).toBe(false);
    expect(subscribe).not.toHaveBeenCalled();
  });

  it('returns false when the VAPID fetch throws', async () => {
    installBrowser();
    stubApi({ vapidRejects: true });
    expect(await enablePush()).toBe(false);
  });

  it('returns false when the server has no key', async () => {
    const { subscribe } = installBrowser();
    stubApi({ key: null });
    expect(await enablePush()).toBe(false);
    expect(subscribe).not.toHaveBeenCalled();
  });

  it('returns false when service-worker registration fails', async () => {
    installBrowser({ registerError: new Error('sw blocked') });
    const fetchMock = stubApi();
    expect(await enablePush()).toBe(false);
    expect(subscribePost(fetchMock)).toBeUndefined();
  });

  it('returns false when the subscription is missing its endpoint', async () => {
    installBrowser({
      subscription: { toJSON: () => ({ keys: { p256dh: 'P', auth: 'A' } }) },
    });
    const fetchMock = stubApi();
    expect(await enablePush()).toBe(false);
    expect(subscribePost(fetchMock)).toBeUndefined();
  });

  it('returns false when the subscription is missing a crypto key', async () => {
    installBrowser({
      subscription: { toJSON: () => ({ endpoint: 'https://push.test/ep', keys: { p256dh: 'P' } }) },
    });
    const fetchMock = stubApi();
    expect(await enablePush()).toBe(false);
    expect(subscribePost(fetchMock)).toBeUndefined();
  });

  it('returns false when the server rejects the subscription', async () => {
    installBrowser();
    stubApi({ subscribeStatus: 500 });
    expect(await enablePush()).toBe(false);
  });
});

// ---- notification assets (Android silhouette regression) --------------------
//
// Symptom this pins against: on Android the push notification showed an empty
// square inside a circle instead of the VoxTranslate mark.
//
// Root cause: `badge` pointed at /icon.png, a 512x512 PNG with NO alpha channel
// (colour type 2 = RGB). Android does not draw the badge as an image — it uses
// the ALPHA channel as a stencil and tints it. A fully opaque image is a stencil
// covering the whole canvas, so the system renders a solid square inside its
// circular chrome. A badge MUST therefore be a small, transparent, monochrome
// glyph. `icon` (the big colour image) is unaffected by this rule.

describe('notification assets', () => {
  /** IHDR colour type: 4 (gray+alpha) and 6 (RGBA) are the ones carrying alpha. */
  function pngColourType(publicPath: string): number {
    const buf = readFileSync(new URL(`../../public${publicPath}`, import.meta.url));
    expect(buf.subarray(12, 16).toString('ascii')).toBe('IHDR'); // sanity: real PNG
    return buf.readUInt8(25);
  }

  function pngSize(publicPath: string): { w: number; h: number } {
    const buf = readFileSync(new URL(`../../public${publicPath}`, import.meta.url));
    return { w: buf.readUInt32BE(16), h: buf.readUInt32BE(20) };
  }

  const sw = readFileSync(new URL('../../public/sw.js', import.meta.url), 'utf8');
  const badge = /badge:\s*'([^']+)'/.exec(sw)?.[1];
  const icon = /icon:\s*'([^']+)'/.exec(sw)?.[1];

  it('declares both a badge and an icon for the push notification', () => {
    expect(badge).toBeTruthy();
    expect(icon).toBeTruthy();
  });

  it('never uses the opaque app icon as the badge (the original bug)', () => {
    expect(badge).not.toBe('/icon.png');
    expect(badge).not.toBe(icon);
  });

  it('ships a badge with an alpha channel Android can use as a stencil', () => {
    expect([4, 6]).toContain(pngColourType(badge!));
  });

  it('keeps the badge small — Android renders it at ~24dp', () => {
    const { w, h } = pngSize(badge!);
    expect(w).toBe(h); // square, or the system crops it
    expect(w).toBeLessThanOrEqual(96);
  });

  // The notification icon is fetched by the phone at display time, often on a
  // flaky mobile connection: a heavyweight file silently fails to load and the
  // notification falls back to a generic glyph. The 512x512 app icon (238 KB) is
  // far too big for that job — Android renders this at ~64dp.
  it('serves a lightweight notification icon', () => {
    const bytes = readFileSync(new URL(`../../public${icon}`, import.meta.url)).byteLength;
    expect(bytes).toBeLessThan(60_000);
    expect(pngSize(icon!).w).toBeLessThanOrEqual(192);
  });
});
