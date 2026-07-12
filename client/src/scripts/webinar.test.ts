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
  addToCalendar,
  buildPublishConstraints,
  cancelWebinar,
  canHostWebinar,
  createWebinar,
  fromDatetimeLocalValue,
  getPublicWebinar,
  getWebinar,
  goLive,
  listWebinars,
  patchWebinar,
  publishStarted,
  publishStopped,
  qrDownloadFilename,
  showVoiceCloneToggle,
  showWebinarCloneAction,
  toDatetimeLocalValue,
  webinarPresenceStub,
  type PublicWebinar,
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

describe('goLive', () => {
  it('POSTs to the go-live endpoint and returns the tokenized publish URL', async () => {
    fetchMock.mockResolvedValue(
      okJson({ publish_url: 'https://ingest/webinar/ab12/whip?token=t', expires_in: 120 }),
    );
    const out = await goLive('w1');
    expect(out).toEqual({ publish_url: 'https://ingest/webinar/ab12/whip?token=t', expires_in: 120 });
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://test/api/webinars/w1/go-live');
    expect(init?.method).toBe('POST');
    expect(init?.headers).toEqual({ Authorization: 'Bearer tok' });
  });

  it('throws on error and on network failure', async () => {
    fetchMock.mockResolvedValueOnce(okJson({ error: 'not scheduled' }, 409));
    await expect(goLive('w1')).rejects.toMatchObject({ status: 409, message: 'not scheduled' });
    fetchMock.mockRejectedValueOnce(new Error('net'));
    await expect(goLive('w1')).rejects.toMatchObject({ status: 0 });
  });
});

describe('publishStarted / publishStopped', () => {
  it('POSTs publish-started and returns the live webinar', async () => {
    fetchMock.mockResolvedValue(okJson(webinar({ status: 'live' })));
    const out = await publishStarted('w1');
    expect(out.status).toBe('live');
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://test/api/webinars/w1/publish-started');
    expect(init?.method).toBe('POST');
    expect(init?.headers).toEqual({ Authorization: 'Bearer tok' });
  });

  it('POSTs publish-stopped and returns the ended webinar', async () => {
    fetchMock.mockResolvedValue(okJson(webinar({ status: 'ended' })));
    const out = await publishStopped('w1');
    expect(out.status).toBe('ended');
    expect(fetchMock.mock.calls[0][0]).toBe('http://test/api/webinars/w1/publish-stopped');
  });

  it('throw a typed error / network error', async () => {
    fetchMock.mockResolvedValueOnce(okJson({}, 401));
    await expect(publishStarted('w1')).rejects.toMatchObject({ status: 401 });
    fetchMock.mockRejectedValueOnce(new Error('net'));
    await expect(publishStopped('w1')).rejects.toMatchObject({ status: 0 });
  });
});

describe('getPublicWebinar', () => {
  const pub = (over: Partial<PublicWebinar> = {}): PublicWebinar => ({
    code: 'ab12cd',
    title: 'Launch',
    status: 'live',
    source_language: 'en',
    tier: 'enhanced',
    join_url: 'https://voxtranslate.app/w/ab12cd',
    playback_url: 'https://hls/webinar/ab12cd/index.m3u8',
    guest_id: 'guest-1',
    ...over,
  });

  it('GETs the public endpoint by code with NO auth headers', async () => {
    fetchMock.mockResolvedValue(okJson(pub()));
    const out = await getPublicWebinar('ab12cd');
    expect(out).toEqual(pub());
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://test/api/w/ab12cd');
    // Public endpoint — no Authorization header (init is undefined → no headers).
    expect(init).toBeUndefined();
  });

  it('encodes the code and throws 404 for an unknown/cancelled webinar', async () => {
    fetchMock.mockResolvedValue(okJson({ error: 'not found' }, 404));
    await expect(getPublicWebinar('bad code')).rejects.toMatchObject({ status: 404 });
    expect(fetchMock.mock.calls[0][0]).toBe('http://test/api/w/bad%20code');
  });

  it('throws a status-0 error when fetch rejects', async () => {
    fetchMock.mockRejectedValue(new Error('net'));
    await expect(getPublicWebinar('ab12cd')).rejects.toMatchObject({ status: 0 });
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

describe('qrDownloadFilename', () => {
  it('builds a webinar-{code}.png name', () => {
    expect(qrDownloadFilename('ab12cd')).toBe('webinar-ab12cd.png');
  });

  it('keeps letters, digits, dashes and underscores', () => {
    expect(qrDownloadFilename('AB-12_cd')).toBe('webinar-AB-12_cd.png');
  });

  it('replaces unsafe characters and trims stray dashes', () => {
    expect(qrDownloadFilename('a b/c.d')).toBe('webinar-a-b-c-d.png');
    expect(qrDownloadFilename('..bad..')).toBe('webinar-bad.png');
  });

  it('falls back to "webinar" for an empty or all-unsafe code', () => {
    expect(qrDownloadFilename('')).toBe('webinar-webinar.png');
    expect(qrDownloadFilename('///')).toBe('webinar-webinar.png');
  });
});

describe('showVoiceCloneToggle', () => {
  it('shows only for Enhanced when the voice is not yet cloned', () => {
    expect(showVoiceCloneToggle('enhanced', false)).toBe(true);
  });

  it('hides when the voice is already cloned', () => {
    expect(showVoiceCloneToggle('enhanced', true)).toBe(false);
  });

  it('hides on the Standard tier regardless of clone state', () => {
    expect(showVoiceCloneToggle('standard', false)).toBe(false);
    expect(showVoiceCloneToggle('standard', true)).toBe(false);
  });
});

describe('showWebinarCloneAction', () => {
  it('offers the pre-live clone action only for Enhanced + not-yet-cloned', () => {
    expect(showWebinarCloneAction('enhanced', false)).toBe(true);
    expect(showWebinarCloneAction('enhanced', true)).toBe(false);
    expect(showWebinarCloneAction('standard', false)).toBe(false);
    expect(showWebinarCloneAction('standard', true)).toBe(false);
  });
});

describe('addToCalendar', () => {
  const evt = {
    google_event_id: 'gcal-1',
    html_link: 'https://calendar.google.com/event?eid=gcal-1',
    join_url: 'https://voxtranslate.app/w/ab12cd',
  };

  it('POSTs to the calendar endpoint with auth headers and returns the event', async () => {
    fetchMock.mockResolvedValue(okJson(evt));
    const out = await addToCalendar('w1');
    expect(out).toEqual(evt);
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://test/api/webinars/w1/calendar');
    expect(init?.method).toBe('POST');
    expect(init?.headers).toEqual({ Authorization: 'Bearer tok' });
    // No JSON body — the server derives the event from the webinar's schedule.
    expect(init?.body).toBeUndefined();
  });

  it('encodes the id in the path', async () => {
    fetchMock.mockResolvedValue(okJson(evt));
    await addToCalendar('w 1/x');
    expect(fetchMock.mock.calls[0][0]).toBe('http://test/api/webinars/w%201%2Fx/calendar');
  });

  it('throws 409 when Google Calendar is not connected', async () => {
    fetchMock.mockResolvedValue(okJson({ error: 'calendar not connected' }, 409));
    await expect(addToCalendar('w1')).rejects.toMatchObject({
      status: 409,
      message: 'calendar not connected',
    });
  });

  it('throws 400 when the webinar is not scheduled', async () => {
    fetchMock.mockResolvedValue(okJson({ error: 'webinar has no scheduled start' }, 400));
    await expect(addToCalendar('w1')).rejects.toMatchObject({ status: 400 });
  });

  it('throws a status-0 WebinarError when fetch rejects', async () => {
    fetchMock.mockRejectedValue(new Error('net'));
    await expect(addToCalendar('w1')).rejects.toMatchObject({ name: 'WebinarError', status: 0 });
  });
});

describe('buildPublishConstraints', () => {
  it('always requests the mic; camera off by default (mic-only)', () => {
    expect(buildPublishConstraints()).toEqual({ audio: true, video: false });
    expect(buildPublishConstraints({})).toEqual({ audio: true, video: false });
  });

  it('pins the chosen mic device with deviceId: { exact }', () => {
    expect(buildPublishConstraints({ audioDeviceId: 'mic-2' })).toEqual({
      audio: { deviceId: { exact: 'mic-2' } },
      video: false,
    });
  });

  it('requests the camera only when withCamera is set, honoring the device id', () => {
    expect(buildPublishConstraints({ withCamera: true })).toEqual({
      audio: true,
      video: true,
    });
    expect(
      buildPublishConstraints({ withCamera: true, videoDeviceId: 'cam-2' }),
    ).toEqual({ audio: true, video: { deviceId: { exact: 'cam-2' } } });
  });

  it('ignores the video device id when the camera is off', () => {
    expect(buildPublishConstraints({ videoDeviceId: 'cam-9' })).toEqual({
      audio: true,
      video: false,
    });
  });

  it('combines pinned mic + pinned camera', () => {
    expect(
      buildPublishConstraints({
        audioDeviceId: 'mic-1',
        videoDeviceId: 'cam-1',
        withCamera: true,
      }),
    ).toEqual({
      audio: { deviceId: { exact: 'mic-1' } },
      video: { deviceId: { exact: 'cam-1' } },
    });
  });
});

describe('webinarPresenceStub', () => {
  it('returns 0 until real presence wiring lands', () => {
    expect(webinarPresenceStub()).toBe(0);
  });
});

describe('toDatetimeLocalValue', () => {
  it('returns "" for null/undefined/invalid (immediate webinar)', () => {
    expect(toDatetimeLocalValue(null)).toBe('');
    expect(toDatetimeLocalValue(undefined)).toBe('');
    expect(toDatetimeLocalValue('not-a-date')).toBe('');
  });

  it('formats an ISO timestamp as local YYYY-MM-DDTHH:mm', () => {
    // Build from a local Date so the assertion is timezone-independent.
    const d = new Date(2026, 6, 11, 9, 5); // 2026-07-11 09:05 local
    expect(toDatetimeLocalValue(d.toISOString())).toBe('2026-07-11T09:05');
  });
});

describe('fromDatetimeLocalValue', () => {
  it('returns null for an empty/whitespace/invalid value', () => {
    expect(fromDatetimeLocalValue('')).toBeNull();
    expect(fromDatetimeLocalValue('   ')).toBeNull();
    expect(fromDatetimeLocalValue('nonsense')).toBeNull();
  });

  it('round-trips through toDatetimeLocalValue', () => {
    const local = '2026-07-11T09:05';
    const iso = fromDatetimeLocalValue(local);
    expect(iso).not.toBeNull();
    expect(toDatetimeLocalValue(iso)).toBe(local);
  });
});
