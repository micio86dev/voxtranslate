// Webinar host helpers (webinar phase 0): create/list/get/patch/cancel a webinar
// for a B2B org plus the `canHostWebinar` predicate. Pure fetch glue — everything
// is mocked, no network.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('./auth', () => ({
  authHeaders: () => ({ Authorization: 'Bearer tok' }),
  HTTP_BASE: 'http://test',
}));

import {
  WebinarError,
  cancelWebinar,
  canHostWebinar,
  createWebinar,
  getWebinar,
  listWebinars,
  patchWebinar,
  type WebinarView,
} from './webinar';
import type { BusinessOrg } from './business';

function okJson(body: unknown, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
    text: async () => JSON.stringify(body),
  } as unknown as Response;
}

/** A non-JSON error response (json() rejects), to exercise the fallback message. */
function badBody(status: number): Response {
  return {
    ok: false,
    status,
    json: async () => {
      throw new Error('not json');
    },
  } as unknown as Response;
}

const org = (subscription_status: string): BusinessOrg => ({
  id: 'o1',
  name: 'Acme',
  role: 'owner',
  plan: 'business',
  subscription_status,
});

const webinar = (over: Partial<WebinarView> = {}): WebinarView => ({
  id: 'w1',
  org_id: 'o1',
  code: 'ab12cd',
  title: 'Launch',
  description: null,
  source_language: 'en',
  tier: 'enhanced',
  status: 'scheduled',
  scheduled_start: null,
  scheduled_end: null,
  actual_start: null,
  actual_end: null,
  record_video: false,
  record_transcript: true,
  voice_clone: false,
  join_url: 'https://voxtranslate.app/w/ab12cd',
  playback_url: null,
  created_at: '2026-07-11T00:00:00Z',
  ...over,
});

const fetchMock = vi.fn<(url: string, init?: RequestInit) => Promise<Response>>();

beforeEach(() => {
  fetchMock.mockReset();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('canHostWebinar', () => {
  it('allows only orgs with an active subscription', () => {
    expect(canHostWebinar(org('active'))).toBe(true);
    expect(canHostWebinar(org('none'))).toBe(false);
    expect(canHostWebinar(org('past_due'))).toBe(false);
    expect(canHostWebinar(org('canceled'))).toBe(false);
    expect(canHostWebinar(undefined)).toBe(false);
  });
});

describe('createWebinar', () => {
  it('POSTs the body with auth + content-type headers and parses the result', async () => {
    fetchMock.mockResolvedValue(okJson(webinar()));
    const out = await createWebinar({
      org_id: 'o1',
      title: 'Launch',
      source_language: 'en',
      tier: 'enhanced',
      record_transcript: true,
    });
    expect(out).toEqual(webinar());
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://test/api/webinars');
    expect(init?.method).toBe('POST');
    expect(init?.headers).toEqual({
      Authorization: 'Bearer tok',
      'Content-Type': 'application/json',
    });
    expect(JSON.parse(init?.body as string)).toEqual({
      org_id: 'o1',
      title: 'Launch',
      source_language: 'en',
      tier: 'enhanced',
      record_transcript: true,
    });
  });

  it('throws a typed WebinarError with the server message on a non-2xx', async () => {
    fetchMock.mockResolvedValue(okJson({ error: 'subscription inactive' }, 402));
    await expect(
      createWebinar({ org_id: 'o1', title: 'x', source_language: 'en' }),
    ).rejects.toMatchObject({ name: 'WebinarError', status: 402, message: 'subscription inactive' });
  });

  it('falls back to a generic message when the error body is not JSON', async () => {
    fetchMock.mockResolvedValue(badBody(400));
    await expect(
      createWebinar({ org_id: 'o1', title: 'x', source_language: 'en' }),
    ).rejects.toMatchObject({ status: 400, message: 'request failed (400)' });
  });

  it('throws a status-0 WebinarError when fetch rejects (offline)', async () => {
    fetchMock.mockRejectedValue(new Error('net'));
    await expect(
      createWebinar({ org_id: 'o1', title: 'x', source_language: 'en' }),
    ).rejects.toMatchObject({ name: 'WebinarError', status: 0 });
  });
});

describe('listWebinars', () => {
  it('GETs the org list with an encoded org_id and auth headers', async () => {
    fetchMock.mockResolvedValue(okJson([webinar()]));
    const out = await listWebinars('org 1');
    expect(out).toEqual([webinar()]);
    expect(fetchMock).toHaveBeenCalledWith('http://test/api/webinars?org_id=org%201', {
      headers: { Authorization: 'Bearer tok' },
    });
  });

  it('throws on a non-ok response', async () => {
    fetchMock.mockResolvedValue(okJson({ error: 'no' }, 404));
    await expect(listWebinars('o1')).rejects.toMatchObject({ status: 404 });
  });

  it('throws a status-0 error when fetch rejects', async () => {
    fetchMock.mockRejectedValue(new Error('net'));
    await expect(listWebinars('o1')).rejects.toMatchObject({ status: 0 });
  });
});

describe('getWebinar', () => {
  it('GETs a single webinar by encoded id', async () => {
    fetchMock.mockResolvedValue(okJson(webinar()));
    expect(await getWebinar('w 1')).toEqual(webinar());
    expect(fetchMock).toHaveBeenCalledWith('http://test/api/webinars/w%201', {
      headers: { Authorization: 'Bearer tok' },
    });
  });

  it('throws on error and on network failure', async () => {
    fetchMock.mockResolvedValueOnce(okJson({}, 401));
    await expect(getWebinar('w1')).rejects.toMatchObject({ status: 401 });
    fetchMock.mockRejectedValueOnce(new Error('net'));
    await expect(getWebinar('w1')).rejects.toMatchObject({ status: 0 });
  });
});

describe('patchWebinar', () => {
  it('PATCHes the partial body with auth + content-type headers', async () => {
    fetchMock.mockResolvedValue(okJson(webinar({ title: 'New' })));
    const out = await patchWebinar('w1', { title: 'New', record_video: true });
    expect(out.title).toBe('New');
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://test/api/webinars/w1');
    expect(init?.method).toBe('PATCH');
    expect(init?.headers).toEqual({
      Authorization: 'Bearer tok',
      'Content-Type': 'application/json',
    });
    expect(JSON.parse(init?.body as string)).toEqual({ title: 'New', record_video: true });
  });

  it('throws 409 once the webinar is no longer scheduled', async () => {
    fetchMock.mockResolvedValue(okJson({ error: 'already started' }, 409));
    await expect(patchWebinar('w1', { title: 'x' })).rejects.toMatchObject({
      status: 409,
      message: 'already started',
    });
  });

  it('throws a status-0 error when fetch rejects', async () => {
    fetchMock.mockRejectedValue(new Error('net'));
    await expect(patchWebinar('w1', { title: 'x' })).rejects.toMatchObject({ status: 0 });
  });
});

describe('cancelWebinar', () => {
  it('POSTs to the cancel endpoint and returns the cancelled webinar', async () => {
    fetchMock.mockResolvedValue(okJson(webinar({ status: 'cancelled' })));
    const out = await cancelWebinar('w1');
    expect(out.status).toBe('cancelled');
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://test/api/webinars/w1/cancel');
    expect(init?.method).toBe('POST');
    expect(init?.headers).toEqual({ Authorization: 'Bearer tok' });
  });

  it('throws on error and on network failure', async () => {
    fetchMock.mockResolvedValueOnce(okJson({}, 404));
    await expect(cancelWebinar('w1')).rejects.toMatchObject({ status: 404 });
    fetchMock.mockRejectedValueOnce(new Error('net'));
    await expect(cancelWebinar('w1')).rejects.toMatchObject({ status: 0 });
  });
});

describe('WebinarError', () => {
  it('is an Error carrying the HTTP status', () => {
    const e = new WebinarError(402, 'nope');
    expect(e).toBeInstanceOf(Error);
    expect(e.name).toBe('WebinarError');
    expect(e.status).toBe(402);
    expect(e.message).toBe('nope');
  });
});
