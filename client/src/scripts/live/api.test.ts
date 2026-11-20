import { describe, expect, it, vi } from 'vitest';

vi.mock('../auth', () => ({
  HTTP_BASE: 'http://api.test',
  authHeaders: () => ({ Authorization: 'Bearer ACCOUNT-JWT' }),
}));

import { answerSession, createSession, deskWaiting, endSession, getSession, joinSession, myDesks, scanDesk, type ApiResult } from './api';

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

describe('scanDesk', () => {
  const SCANNED = {
    session_id: 's1', peer_id: 'g1', token: 'LIVE', ws_path: '/ws/live',
    desk: { name: 'Front desk', location_label: 'Terminal 2' }, wait_secs: 60,
  };

  it('POSTs the QR segment, the language, the name and the consent, with no Authorization header', async () => {
    const f = fetcher(res(200, SCANNED));
    const out = await scanDesk({ segment: 'abc.def', name: 'Marco', lang: 'it', consent: true }, f as never);
    expect(out).toEqual({ ok: true, data: SCANNED });
    const [url, init] = f.mock.calls[0];
    expect(url).toBe('http://api.test/api/live/desks/scan');
    expect(init?.method).toBe('POST');
    expect(init?.headers).not.toHaveProperty('Authorization');
    expect(JSON.parse(String(init?.body))).toEqual({ segment: 'abc.def', name: 'Marco', lang: 'it', consent: true });
  });

  it('never puts the segment in the URL (it is a bearer secret)', async () => {
    const f = fetcher(res(200, SCANNED));
    await scanDesk({ segment: 'abc.def', name: '', lang: 'it', consent: true }, f as never);
    expect(String(f.mock.calls[0][0])).not.toContain('abc');
  });

  it.each([
    [404, 'invalid_desk_qr'], [503, 'desk_unavailable'], [429, 'desk_busy'], [429, 'rate_limited'],
    [503, 'live_busy'], [400, 'consent_required'], [400, 'bad_lang'],
  ])('keeps the code of a %s %s refusal', async (status, code) => {
    const out = await scanDesk({ segment: 's', name: '', lang: 'it', consent: true }, fetcher(res(status, { code, message: 'x' })) as never);
    expect(out).toEqual({ ok: false, status, code });
  });

  it('a dead network is its own code', async () => {
    expect(await scanDesk({ segment: 's', name: '', lang: 'it', consent: true }, fetcher(new TypeError('offline')) as never)).toEqual({ ok: false, status: 0, code: 'network' });
  });
});

describe('staff calls (account JWT)', () => {
  it('myDesks GETs /api/live/my-desks as the signed-in member', async () => {
    const data = { desks: [{ id: 'd1', org_id: 'o1', org_name: 'Acme', name: 'Desk', location_label: null, default_lang: 'en', waiting: 2 }] };
    const f = fetcher(res(200, data));
    expect(await myDesks(f as never)).toEqual({ ok: true, data });
    const [url, init] = f.mock.calls[0];
    expect(url).toBe('http://api.test/api/live/my-desks');
    expect(init?.method).toBe('GET');
    expect(init?.headers).toMatchObject({ Authorization: 'Bearer ACCOUNT-JWT' });
  });

  it('deskWaiting GETs the queue of one desk, with the id encoded', async () => {
    const data = { waiting: [{ session_id: 's1', guest_name: 'Marco', guest_lang: 'it', waited_secs: 4 }] };
    const f = fetcher(res(200, data));
    expect(await deskWaiting('d/1', f as never)).toEqual({ ok: true, data });
    expect(f.mock.calls[0][0]).toBe('http://api.test/api/live/desks/d%2F1/waiting');
    expect(f.mock.calls[0][1]?.headers).toMatchObject({ Authorization: 'Bearer ACCOUNT-JWT' });
  });

  it('answerSession POSTs the staff language to the session', async () => {
    const data = { session_id: 's1', peer_id: 'st1', token: 'LIVE', ws_path: '/ws/live', guest: { name: 'Marco', lang: 'it' } };
    const f = fetcher(res(200, data));
    expect(await answerSession('s1', 'en', f as never)).toEqual({ ok: true, data });
    const [url, init] = f.mock.calls[0];
    expect(url).toBe('http://api.test/api/live/sessions/s1/answer');
    expect(init?.method).toBe('POST');
    expect(init?.headers).toMatchObject({ Authorization: 'Bearer ACCOUNT-JWT', 'Content-Type': 'application/json' });
    expect(JSON.parse(String(init?.body))).toEqual({ lang: 'en' });
  });

  it.each([
    [409, 'already_answered'], [410, 'session_expired'], [410, 'session_ended'], [402, 'insufficient_credits'],
    [402, 'subscription_required'], [503, 'live_busy'],
  ])('answerSession keeps the code of a %s %s refusal', async (status, code) => {
    expect(await answerSession('s1', 'en', fetcher(res(status, { code, message: 'x' })) as never)).toEqual({ ok: false, status, code });
  });

  it('a plain-text refusal (the org routes answer without a body code) keeps its status', async () => {
    const out = await answerSession('s1', 'en', fetcher(new Response('insufficient role', { status: 403 })) as never);
    expect(out).toEqual({ ok: false, status: 403, code: 'http_error' });
  });
});
