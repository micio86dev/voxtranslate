import { afterEach, describe, expect, it, vi } from 'vitest';

// voip.ts only needs `authHeaders` + `HTTP_BASE` from auth.ts, which itself reads
// `location` and `localStorage` at import time — mock the module so this suite stays
// independent of that plumbing and the request/URL assertions are exact (style of
// api.test.ts).
vi.mock('./auth', () => ({
  authHeaders: () => ({ Authorization: 'Bearer tk' }),
  HTTP_BASE: 'http://api.test',
}));

import * as voip from './voip';

const BASE = 'http://api.test';
const AUTH = { Authorization: 'Bearer tk' };
const ORG = 'org-1';

function jsonRes(body: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
  } as Response;
}

function noBodyRes(status: number) {
  return { ok: status >= 200 && status < 300, status, json: async () => null } as Response;
}

/** Stub fetch with a fixed sequence of responses (an Error rejects that call). */
function stubFetch(...responses: Array<Response | Error>) {
  const mock = vi.fn();
  for (const r of responses) {
    if (r instanceof Error) mock.mockRejectedValueOnce(r);
    else mock.mockResolvedValueOnce(r);
  }
  vi.stubGlobal('fetch', mock);
  return mock;
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('errorCode', () => {
  it('reads the {error} shape the server sends on a refusal', () => {
    expect(voip.errorCode({ error: 'insufficient_credits' })).toBe('insufficient_credits');
  });

  it('is null for a successful body, null data, or a body with no error field', () => {
    expect(voip.errorCode({ call_id: 'c1' })).toBeNull();
    expect(voip.errorCode(null)).toBeNull();
    expect(voip.errorCode({})).toBeNull();
  });
});

describe('quoteVoipCall', () => {
  it('POSTs the destination to the org-scoped quote endpoint with auth', async () => {
    const quote = {
      destination: '+39••••4567',
      country: 'IT',
      price_per_minute: '0.05',
      currency: 'USD',
      reserve_credits: 50,
      estimated_minutes: 10,
      balance_credits: 500,
      engine_id: 'standard',
      recording: false,
      transcription: false,
      consent_policy: 'notice_only',
      disclosure_language: null,
    };
    const mock = stubFetch(jsonRes(quote, 200));
    const r = await voip.quoteVoipCall(ORG, { destination: '+393201234567' });
    expect(r.ok).toBe(true);
    expect(r.status).toBe(200);
    expect(r.data).toEqual(quote);
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/business/organizations/${ORG}/voip/quote`);
    const init = mock.mock.calls[0][1] as RequestInit;
    expect(init.method).toBe('POST');
    expect(init.headers).toEqual({ ...AUTH, 'Content-Type': 'application/json' });
    expect(JSON.parse(init.body as string)).toEqual({ destination: '+393201234567' });
  });

  it('surfaces a refusal code from a non-2xx body', async () => {
    stubFetch(jsonRes({ error: 'destination_too_expensive' }, 402));
    const r = await voip.quoteVoipCall(ORG, { destination: '+393201234567' });
    expect(r.ok).toBe(false);
    expect(r.status).toBe(402);
    expect(voip.errorCode(r.data)).toBe('destination_too_expensive');
  });

  it('never throws on a network failure', async () => {
    stubFetch(new Error('net'));
    const r = await voip.quoteVoipCall(ORG, { destination: '+393201234567' });
    expect(r).toEqual({ ok: false, status: 0, data: null });
  });
});

describe('dialVoipCall', () => {
  it('POSTs the full dial body and returns the created call on 201', async () => {
    const created = {
      call_id: 'c1',
      session_id: 's1',
      room: 'ph-abc123',
      status: 'created',
      reserved_credits: 50,
      price_per_minute: '0.05',
    };
    const mock = stubFetch(jsonRes(created, 201));
    const body: voip.VoipDialRequest = {
      destination: '+393201234567',
      source_language: 'en',
      target_language: 'it',
    };
    const r = await voip.dialVoipCall(ORG, body);
    expect(r.ok).toBe(true);
    expect(r.status).toBe(201);
    expect(r.data).toEqual(created);
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/business/organizations/${ORG}/voip/calls`);
    const init = mock.mock.calls[0][1] as RequestInit;
    expect(init.method).toBe('POST');
    expect(JSON.parse(init.body as string)).toEqual(body);
  });

  it('surfaces a refusal code without placing the call (money-safety)', async () => {
    stubFetch(jsonRes({ error: 'caller_id_unverified' }, 403));
    const r = await voip.dialVoipCall(ORG, {
      destination: '+393201234567',
      source_language: 'en',
      target_language: 'it',
    });
    expect(r.ok).toBe(false);
    expect(voip.errorCode(r.data)).toBe('caller_id_unverified');
  });
});

describe('getVoipCall', () => {
  it('GETs the call detail by id', async () => {
    const detail = {
      id: 'c1',
      session_id: 's1',
      room: null,
      status: 'completed',
      failure_reason: null,
      direction: 'outbound',
      recipient_e164: '+393201234567',
      recipient_country: 'IT',
      source_language: 'en',
      target_language: 'it',
      engine_id: 'standard',
      started_at: '2026-09-17T00:00:00Z',
      ended_at: '2026-09-17T00:05:00Z',
      duration_seconds: 300,
      credits_consumed: 25,
      quoted_price_per_min: '0.05',
      cost_status: 'final',
      recording_status: 'none',
      recording_available: false,
      transcription_status: 'none',
      ai_analysis_requested: false,
      consent_status: 'not_required',
      project_id: null,
      contact_id: null,
      contact_name: null,
    };
    const mock = stubFetch(jsonRes(detail));
    const r = await voip.getVoipCall(ORG, 'c1');
    expect(r.data).toEqual(detail);
    expect(mock.mock.calls[0][0]).toBe(
      `${BASE}/api/business/organizations/${ORG}/voip/calls/c1`,
    );
    const init = mock.mock.calls[0][1] as RequestInit;
    expect(init.headers).toEqual(AUTH);
    expect(init.method ?? 'GET').toBe('GET');
  });

  it('returns 404/data null for a call from another org (tenancy boundary)', async () => {
    stubFetch(noBodyRes(404));
    const r = await voip.getVoipCall(ORG, 'not-mine');
    expect(r.ok).toBe(false);
    expect(r.status).toBe(404);
  });
});

describe('hangUpVoipCall', () => {
  it('POSTs to the hangup endpoint and reports the server ack', async () => {
    const mock = stubFetch(jsonRes({ requested: true }));
    const r = await voip.hangUpVoipCall(ORG, 'c1');
    expect(r.ok).toBe(true);
    expect(r.data).toEqual({ requested: true });
    expect(mock.mock.calls[0][0]).toBe(
      `${BASE}/api/business/organizations/${ORG}/voip/calls/c1/hangup`,
    );
    const init = mock.mock.calls[0][1] as RequestInit;
    expect(init.method).toBe('POST');
    expect(init.headers).toEqual(AUTH);
  });

  it('never throws on a network failure — every exit must still resolve (money-safety)', async () => {
    stubFetch(new Error('net'));
    const r = await voip.hangUpVoipCall(ORG, 'c1');
    expect(r).toEqual({ ok: false, status: 0, data: null });
  });

  it('omits keepalive by default, so an ordinary in-tab hangup is unaffected', async () => {
    const mock = stubFetch(jsonRes({ requested: true }));
    await voip.hangUpVoipCall(ORG, 'c1');
    const init = mock.mock.calls[0][1] as RequestInit;
    expect(init.keepalive).toBeUndefined();
  });

  it('passes keepalive:true through to fetch when asked (spec R5: the pagehide exit — the tab is unloading, so the request must be allowed to outlive the document, unlike sendBeacon it can still carry Authorization)', async () => {
    const mock = stubFetch(jsonRes({ requested: true }));
    await voip.hangUpVoipCall(ORG, 'c1', { keepalive: true });
    const init = mock.mock.calls[0][1] as RequestInit;
    expect(init.keepalive).toBe(true);
  });
});

describe('listVoipContacts', () => {
  it('GETs with only the provided query params encoded', async () => {
    const page = { contacts: [{ id: 'ct1', name: 'Ada', company: null, role: null, notes: null, tags: [], email: null }], page: 1, limit: 50 };
    const mock = stubFetch(jsonRes(page));
    const r = await voip.listVoipContacts(ORG, { q: 'ada' });
    expect(r.data).toEqual(page);
    expect(mock.mock.calls[0][0]).toBe(
      `${BASE}/api/business/organizations/${ORG}/voip/contacts?q=ada`,
    );
  });

  it('omits the query string entirely when no filters are given', async () => {
    const mock = stubFetch(jsonRes({ contacts: [], page: 1, limit: 50 }));
    await voip.listVoipContacts(ORG);
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/business/organizations/${ORG}/voip/contacts`);
  });

  it('encodes every supported filter', async () => {
    const mock = stubFetch(jsonRes({ contacts: [], page: 2, limit: 10 }));
    await voip.listVoipContacts(ORG, {
      q: 'ada',
      projectId: 'p1',
      tag: 'vip',
      language: 'it',
      page: 2,
      limit: 10,
    });
    const url = new URL(mock.mock.calls[0][0] as string);
    expect(url.searchParams.get('q')).toBe('ada');
    expect(url.searchParams.get('project_id')).toBe('p1');
    expect(url.searchParams.get('tag')).toBe('vip');
    expect(url.searchParams.get('language')).toBe('it');
    expect(url.searchParams.get('page')).toBe('2');
    expect(url.searchParams.get('limit')).toBe('10');
  });
});

describe('getVoipContact', () => {
  it('GETs one contact with its numbers and linked projects', async () => {
    const detail = {
      id: 'ct1',
      name: 'Ada',
      company: null,
      role: null,
      notes: null,
      tags: [],
      email: null,
      numbers: [{ id: 'n1', e164: '+393201234567', label: 'mobile', language: 'it', is_primary: true }],
      projects: [{ id: 'p1', name: 'Acme' }],
    };
    const mock = stubFetch(jsonRes(detail));
    const r = await voip.getVoipContact(ORG, 'ct1');
    expect(r.data).toEqual(detail);
    expect(mock.mock.calls[0][0]).toBe(
      `${BASE}/api/business/organizations/${ORG}/voip/contacts/ct1`,
    );
  });

  it('returns not-ok for a contact that does not exist', async () => {
    stubFetch(noBodyRes(404));
    const r = await voip.getVoipContact(ORG, 'nope');
    expect(r.ok).toBe(false);
    expect(r.status).toBe(404);
  });
});
