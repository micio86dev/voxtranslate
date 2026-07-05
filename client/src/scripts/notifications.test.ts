// Unit tests for the notification-preferences + in-app-center REST helpers.
// Every wrapper fails soft (null / false / []) so the settings UI never throws.
import { afterEach, describe, expect, it, vi } from 'vitest';

// notifications.ts only needs the API base + auth header; mocking auth.ts keeps
// this node-env test free of auth's module-eval side effects (location/localStorage).
vi.mock('./auth', () => ({
  HTTP_BASE: 'http://api.test',
  authHeaders: () => ({ Authorization: 'Bearer tok' }),
}));

import {
  NOTIF_CHANNELS,
  NOTIF_EVENTS,
  fetchPreferences,
  fetchUnread,
  markRead,
  savePreferences,
  type InAppNotification,
  type Prefs,
} from './notifications';

const PREFS: Prefs = {
  preferences: [{ type: 'friend_request', channel: 'push', enabled: true }],
  quiet_hours_start: 22,
  quiet_hours_end: 7,
  timezone: 'Europe/Rome',
};

const NOTIF: InAppNotification = {
  id: 'n1',
  type: 'friend_request',
  title: 'New friend request',
  body: 'Anna wants to connect',
  data: {},
  read_at: null,
  created_at: '2026-07-01T10:00:00Z',
};

function jsonRes(body: unknown, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
  } as unknown as Response;
}

function stubFetch(result: Response): ReturnType<typeof vi.fn> {
  const mock = vi.fn().mockResolvedValue(result);
  vi.stubGlobal('fetch', mock);
  return mock;
}

function stubFetchError(): void {
  vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new TypeError('network down')));
}

afterEach(() => vi.unstubAllGlobals());

describe('notification constants', () => {
  it('lists the friend events first (panel ordering)', () => {
    expect(NOTIF_EVENTS.slice(0, 2)).toEqual(['friend_request', 'friend_accepted']);
    expect(NOTIF_EVENTS).toHaveLength(8);
  });

  it('ships exactly the three delivery channels', () => {
    expect([...NOTIF_CHANNELS]).toEqual(['push', 'email', 'in_app']);
  });
});

describe('fetchPreferences', () => {
  it('returns the parsed preferences and sends the auth header', async () => {
    const mock = stubFetch(jsonRes(PREFS));
    expect(await fetchPreferences()).toEqual(PREFS);
    expect(mock).toHaveBeenCalledWith('http://api.test/api/notifications/preferences', {
      headers: { Authorization: 'Bearer tok' },
    });
  });

  it('returns null on a non-ok response', async () => {
    stubFetch(jsonRes('nope', 401));
    expect(await fetchPreferences()).toBeNull();
  });

  it('returns null when fetch throws (offline)', async () => {
    stubFetchError();
    expect(await fetchPreferences()).toBeNull();
  });
});

describe('savePreferences', () => {
  it('PATCHes the rows as JSON and returns true on ok', async () => {
    const mock = stubFetch(jsonRes(null));
    expect(await savePreferences(PREFS.preferences)).toBe(true);
    const [url, init] = mock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe('http://api.test/api/notifications/preferences');
    expect(init.method).toBe('PATCH');
    expect(init.headers).toEqual({
      Authorization: 'Bearer tok',
      'Content-Type': 'application/json',
    });
    expect(JSON.parse(String(init.body))).toEqual({ preferences: PREFS.preferences });
  });

  it('returns false on a non-ok response', async () => {
    stubFetch(jsonRes('nope', 500));
    expect(await savePreferences([])).toBe(false);
  });

  it('returns false when fetch throws', async () => {
    stubFetchError();
    expect(await savePreferences([])).toBe(false);
  });
});

describe('fetchUnread', () => {
  it('fetches unread with the default limit of 20', async () => {
    const mock = stubFetch(jsonRes({ notifications: [NOTIF] }));
    expect(await fetchUnread()).toEqual([NOTIF]);
    expect(mock).toHaveBeenCalledWith('http://api.test/api/notifications?unread=true&limit=20', {
      headers: { Authorization: 'Bearer tok' },
    });
  });

  it('honors a custom limit', async () => {
    const mock = stubFetch(jsonRes({ notifications: [] }));
    expect(await fetchUnread(5)).toEqual([]);
    expect(String(mock.mock.calls[0]?.[0])).toContain('limit=5');
  });

  it('returns [] when the payload has no notifications field', async () => {
    stubFetch(jsonRes({}));
    expect(await fetchUnread()).toEqual([]);
  });

  it('returns [] on a non-ok response', async () => {
    stubFetch(jsonRes('nope', 503));
    expect(await fetchUnread()).toEqual([]);
  });

  it('returns [] when fetch throws', async () => {
    stubFetchError();
    expect(await fetchUnread()).toEqual([]);
  });
});

describe('markRead', () => {
  it('POSTs to the encoded read endpoint and returns true on ok', async () => {
    const mock = stubFetch(jsonRes(null));
    expect(await markRead('n/1')).toBe(true);
    expect(mock).toHaveBeenCalledWith('http://api.test/api/notifications/n%2F1/read', {
      method: 'POST',
      headers: { Authorization: 'Bearer tok' },
    });
  });

  it('returns false on a non-ok response', async () => {
    stubFetch(jsonRes('nope', 404));
    expect(await markRead('n1')).toBe(false);
  });

  it('returns false when fetch throws', async () => {
    stubFetchError();
    expect(await markRead('n1')).toBe(false);
  });
});
