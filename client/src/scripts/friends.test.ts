// Unit tests for the /api/friends REST helpers. Every wrapper fails soft (empty
// list / false / short error string) so the panel UI never throws on a blip.
import { afterEach, describe, expect, it, vi } from 'vitest';

// friends.ts only needs the API base + auth header; mocking auth.ts keeps this
// node-env test free of auth's module-eval side effects (location/localStorage).
vi.mock('./auth', () => ({
  HTTP_BASE: 'http://api.test',
  authHeaders: () => ({ Authorization: 'Bearer tok' }),
}));

import {
  acceptFriend,
  fetchFriendRequests,
  fetchFriends,
  inviteFriendToCall,
  removeFriend,
  sendFriendRequest,
  type Friend,
  type FriendRequests,
} from './friends';

const ANNA: Friend = { id: 'u1', name: 'Anna', email: 'anna@x.co', avatar_url: null };

function jsonRes(body: unknown, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
    text: async () => (typeof body === 'string' ? body : JSON.stringify(body)),
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

describe('fetchFriends', () => {
  it('returns the parsed list and sends the auth header', async () => {
    const mock = stubFetch(jsonRes([ANNA]));
    expect(await fetchFriends()).toEqual([ANNA]);
    expect(mock).toHaveBeenCalledWith('http://api.test/api/friends', {
      headers: { Authorization: 'Bearer tok' },
    });
  });

  it('returns [] on a non-ok response', async () => {
    stubFetch(jsonRes('nope', 500));
    expect(await fetchFriends()).toEqual([]);
  });

  it('returns [] when fetch throws (offline)', async () => {
    stubFetchError();
    expect(await fetchFriends()).toEqual([]);
  });
});

describe('fetchFriendRequests', () => {
  it('returns the parsed incoming/outgoing sets', async () => {
    const reqs: FriendRequests = { incoming: [ANNA], outgoing: [] };
    const mock = stubFetch(jsonRes(reqs));
    expect(await fetchFriendRequests()).toEqual(reqs);
    expect(mock).toHaveBeenCalledWith('http://api.test/api/friends/requests', {
      headers: { Authorization: 'Bearer tok' },
    });
  });

  it('returns empty sets on a non-ok response', async () => {
    stubFetch(jsonRes('nope', 401));
    expect(await fetchFriendRequests()).toEqual({ incoming: [], outgoing: [] });
  });

  it('returns empty sets when fetch throws', async () => {
    stubFetchError();
    expect(await fetchFriendRequests()).toEqual({ incoming: [], outgoing: [] });
  });
});

describe('sendFriendRequest', () => {
  it('POSTs the typed email and resolves null on success', async () => {
    const mock = stubFetch(jsonRes(null));
    expect(await sendFriendRequest({ email: 'a@x.co' })).toBeNull();
    const [url, init] = mock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe('http://api.test/api/friends/request');
    expect(init.method).toBe('POST');
    expect(init.headers).toEqual({
      Authorization: 'Bearer tok',
      'Content-Type': 'application/json',
    });
    expect(init.body).toBe('{"email":"a@x.co"}');
  });

  it('POSTs the user id for in-call add-friend buttons', async () => {
    const mock = stubFetch(jsonRes(null));
    expect(await sendFriendRequest({ userId: 'u2' })).toBeNull();
    const [, init] = mock.mock.calls[0] as [string, RequestInit];
    expect(init.body).toBe('{"user_id":"u2"}');
  });

  it('returns the server message on failure', async () => {
    stubFetch(jsonRes('already_friends', 409));
    expect(await sendFriendRequest({ email: 'a@x.co' })).toBe('already_friends');
  });

  it('falls back to the status code when the body is empty', async () => {
    stubFetch(jsonRes('', 500));
    expect(await sendFriendRequest({ email: 'a@x.co' })).toBe('500');
  });

  it('falls back to the status code when the body is unreadable', async () => {
    stubFetch({
      ok: false,
      status: 418,
      text: () => Promise.reject(new Error('unreadable')),
    } as unknown as Response);
    expect(await sendFriendRequest({ email: 'a@x.co' })).toBe('418');
  });

  it("returns 'network' when fetch throws", async () => {
    stubFetchError();
    expect(await sendFriendRequest({ email: 'a@x.co' })).toBe('network');
  });
});

describe('acceptFriend', () => {
  it('POSTs to the encoded accept endpoint and returns true on ok', async () => {
    const mock = stubFetch(jsonRes(null));
    expect(await acceptFriend('u/1')).toBe(true);
    expect(mock).toHaveBeenCalledWith('http://api.test/api/friends/u%2F1/accept', {
      method: 'POST',
      headers: { Authorization: 'Bearer tok' },
    });
  });

  it('returns false on a non-ok response', async () => {
    stubFetch(jsonRes('nope', 404));
    expect(await acceptFriend('u1')).toBe(false);
  });

  it('returns false when fetch throws', async () => {
    stubFetchError();
    expect(await acceptFriend('u1')).toBe(false);
  });
});

describe('removeFriend', () => {
  it('DELETEs the encoded relationship and returns true on ok', async () => {
    const mock = stubFetch(jsonRes(null));
    expect(await removeFriend('u 2')).toBe(true);
    expect(mock).toHaveBeenCalledWith('http://api.test/api/friends/u%202', {
      method: 'DELETE',
      headers: { Authorization: 'Bearer tok' },
    });
  });

  it('returns false on a non-ok response', async () => {
    stubFetch(jsonRes('nope', 403));
    expect(await removeFriend('u1')).toBe(false);
  });

  it('returns false when fetch throws', async () => {
    stubFetchError();
    expect(await removeFriend('u1')).toBe(false);
  });
});

describe('inviteFriendToCall', () => {
  it('POSTs the room and returns true on ok', async () => {
    const mock = stubFetch(jsonRes(null));
    expect(await inviteFriendToCall('u1', 'blue-fox-42')).toBe(true);
    const [url, init] = mock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe('http://api.test/api/friends/u1/invite');
    expect(init.method).toBe('POST');
    expect(init.body).toBe('{"room":"blue-fox-42"}');
  });

  it('returns false on a non-ok response', async () => {
    stubFetch(jsonRes('nope', 400));
    expect(await inviteFriendToCall('u1', 'r')).toBe(false);
  });

  it('returns false when fetch throws', async () => {
    stubFetchError();
    expect(await inviteFriendToCall('u1', 'r')).toBe(false);
  });
});
