import { describe, expect, it, vi } from 'vitest';

vi.mock('../auth', () => ({
  HTTP_BASE: 'http://api.test',
  authHeaders: () => ({ Authorization: 'Bearer ACCOUNT-JWT' }),
}));

import { createSession, endSession, getSession, joinSession, type ApiResult } from './api';

function res(status: number, body: unknown): Response {
  return new Response(typeof body === 'string' ? body : JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

function fetcher(r: Response | Error) {
  return vi.fn(async (_url: string, _init?: RequestInit) => {
    if (r instanceof Error) throw r;
    return r;
  });
}

const CREATED = {
  session_id: 's1',
  invite_url: 'https://x/lc/CODE',
  qr_payload: 'https://x/lc/CODE',
  expires_at: '2026-01-01T00:00:00Z',
  peer_id: 'p1',
  token: 'LIVE',
  ws_path: '/ws/live',
};

describe('createSession', () => {
  it('POSTs the account JWT and the snake_case body, and returns the data', async () => {
    const f = fetcher(res(200, CREATED));
    const out = await createSession({ lang: 'it', saveTranscript: true }, f as never);
    expect(out).toEqual({ ok: true, data: CREATED });
    const [url, init] = f.mock.calls[0];
    expect(url).toBe('http://api.test/api/live/sessions');
    expect(init?.method).toBe('POST');
    expect(init?.headers).toMatchObject({
      Authorization: 'Bearer ACCOUNT-JWT',
      'Content-Type': 'application/json',
    });
    expect(JSON.parse(String(init?.body))).toEqual({ lang: 'it', save_transcript: true });
  });

  it('never puts the account JWT in the URL', async () => {
    const f = fetcher(res(200, CREATED));
    await createSession({ lang: 'it', saveTranscript: false }, f as never);
    expect(String(f.mock.calls[0][0])).not.toContain('ACCOUNT-JWT');
  });
});

describe('joinSession', () => {
  it('sends no Authorization header (a guest has no account) and the consent flag', async () => {
    const f = fetcher(res(200, { session_id: 's', peer_id: 'g', token: 't', ws_path: '/ws/live', host: { name: 'A', lang: 'it' } }));
    const out = await joinSession({ code: 'CODE', name: 'Guest', lang: 'en', consent: true }, f as never);
    expect(out.ok).toBe(true);
    const [url, init] = f.mock.calls[0];
    expect(url).toBe('http://api.test/api/live/join');
    expect(JSON.stringify(init?.headers)).not.toMatch(/authorization/i);
    expect(JSON.parse(String(init?.body))).toEqual({ code: 'CODE', name: 'Guest', lang: 'en', consent: true });
  });
});

describe('getSession / endSession', () => {
  it('use the LIVE token as the bearer, never the account JWT', async () => {
    const f = fetcher(res(200, { session: { id: 's' }, you: { peer_id: 'p', role: 'host' } }));
    await getSession('s 1', 'LIVE-TOKEN', f as never);
    const [url, init] = f.mock.calls[0];
    expect(url).toBe('http://api.test/api/live/sessions/s%201');
    expect(init?.headers).toMatchObject({ Authorization: 'Bearer LIVE-TOKEN' });
    expect(JSON.stringify(init?.headers)).not.toContain('ACCOUNT-JWT');

    const g = fetcher(res(200, {}));
    await endSession('s1', 'LIVE-TOKEN', g as never);
    expect(g.mock.calls[0][0]).toBe('http://api.test/api/live/sessions/s1/end');
    expect(g.mock.calls[0][1]?.method).toBe('POST');
    expect(g.mock.calls[0][1]?.headers).toMatchObject({ Authorization: 'Bearer LIVE-TOKEN' });
  });
});

describe('error mapping (never throws)', () => {
  const cases: Array<[number, string]> = [
    [404, 'invite_not_found'],
    [410, 'invite_expired'],
    [409, 'session_full'],
    [410, 'session_ended'],
    [400, 'consent_required'],
    [400, 'bad_lang'],
    [429, 'too_many_sessions'],
    [402, 'insufficient_balance'],
    [402, 'insufficient_credits'],
    [403, 'subscription_required'],
    [400, 'project_required'],
    [404, 'project_not_found'],
    [503, 'live_busy'],
    [429, 'rate_limited'],
    [401, 'bad_token'],
    [401, 'token_expired'],
    [403, 'wrong_session'],
    [404, 'live_disabled'],
  ];
  it.each(cases)('HTTP %i %s keeps the server code', async (status, code) => {
    const out = (await joinSession({ code: 'c', name: 'n', lang: 'en', consent: true }, fetcher(res(status, { code, message: 'm' })) as never)) as ApiResult<unknown>;
    expect(out).toEqual({ ok: false, status, code });
  });

  it('maps an unreadable error body to a status-based code', async () => {
    const out = await getSession('s', 't', fetcher(res(502, '<html>bad gateway</html>')) as never);
    expect(out).toEqual({ ok: false, status: 502, code: 'server_error' });
    const out4 = await getSession('s', 't', fetcher(res(418, 'nope')) as never);
    expect(out4).toEqual({ ok: false, status: 418, code: 'http_error' });
  });

  it('maps a network failure to status 0 / code network instead of throwing', async () => {
    const out = await createSession({ lang: 'it', saveTranscript: false }, fetcher(new TypeError('offline')) as never);
    expect(out).toEqual({ ok: false, status: 0, code: 'network' });
  });

  it('treats a 2xx with an unparseable body as a server error', async () => {
    const out = await getSession('s', 't', fetcher(res(200, 'not json')) as never);
    expect(out).toEqual({ ok: false, status: 200, code: 'server_error' });
  });
});
