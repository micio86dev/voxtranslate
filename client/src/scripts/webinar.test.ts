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
  archiveWebinar,
  buildPublishConstraints,
  buildCardTimeline,
  buildDetailInfo,
  cancelWebinar,
  canHostWebinar,
  clampReminder,
  createWebinar,
  formatScheduledStart,
  formatWebinarClock,
  fromDatetimeLocalValue,
  getPublicWebinar,
  getWebinar,
  goLive,
  isWebinarLive,
  isWebinarRestorable,
  listPublicWebinars,
  listWebinars,
  patchWebinar,
  publishStarted,
  publishStopped,
  qrDownloadFilename,
  resetEndIfStartPassed,
  showVoiceCloneToggle,
  showWebinarCloneAction,
  toDatetimeLocalValue,
  transcriptRowsToSrt,
  transcriptRowsToTxt,
  unarchiveWebinar,
  validateSchedule,
  type PublicWebinar,
  type PublicWebinarListItem,
  type TranscriptRow,
  type WebinarSessionStats,
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
  visibility: 'private',
  project_id: null,
  scheduled_start: null,
  scheduled_end: null,
  actual_start: null,
  actual_end: null,
  record_video: false,
  record_transcript: true,
  voice_clone: false,
  chat_enabled: false,
  join_url: 'https://voxtranslate.app/w/ab12cd',
  playback_url: null,
  created_at: '2026-07-11T00:00:00Z',
  archived_at: null,
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

  it('carries chat_enabled in the POST body when the create-form toggle is on', async () => {
    fetchMock.mockResolvedValue(okJson(webinar({ chat_enabled: true })));
    const out = await createWebinar({
      org_id: 'o1',
      title: 'Launch',
      source_language: 'en',
      chat_enabled: true,
    });
    expect(out.chat_enabled).toBe(true);
    const [, init] = fetchMock.mock.calls[0];
    expect(JSON.parse(init?.body as string)).toEqual({
      org_id: 'o1',
      title: 'Launch',
      source_language: 'en',
      chat_enabled: true,
    });
  });

  it('carries visibility in the POST body when the create-form toggle is on', async () => {
    fetchMock.mockResolvedValue(okJson(webinar({ visibility: 'public' })));
    const out = await createWebinar({
      org_id: 'o1',
      title: 'Launch',
      source_language: 'en',
      visibility: 'public',
    });
    expect(out.visibility).toBe('public');
    const [, init] = fetchMock.mock.calls[0];
    expect(JSON.parse(init?.body as string)).toEqual({
      org_id: 'o1',
      title: 'Launch',
      source_language: 'en',
      visibility: 'public',
    });
  });

  it('carries an optional project_id in the POST body when one is chosen', async () => {
    fetchMock.mockResolvedValue(okJson(webinar({ project_id: 'p1' })));
    const out = await createWebinar({
      org_id: 'o1',
      title: 'Launch',
      source_language: 'en',
      project_id: 'p1',
      tier: 'enhanced',
    });
    expect(out.project_id).toBe('p1');
    const [, init] = fetchMock.mock.calls[0];
    expect(JSON.parse(init?.body as string)).toEqual({
      org_id: 'o1',
      title: 'Launch',
      source_language: 'en',
      project_id: 'p1',
      tier: 'enhanced',
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
  it('GETs the active list by default (archived=false) with an encoded org_id and auth headers', async () => {
    fetchMock.mockResolvedValue(okJson([webinar()]));
    const out = await listWebinars('org 1');
    expect(out).toEqual([webinar()]);
    expect(fetchMock).toHaveBeenCalledWith(
      'http://test/api/webinars?org_id=org%201&archived=false',
      { headers: { Authorization: 'Bearer tok' } },
    );
  });

  it('requests the archived list when archived is true', async () => {
    const archived = webinar({ archived_at: '2026-07-12T00:00:00Z', status: 'ended' });
    fetchMock.mockResolvedValue(okJson([archived]));
    const out = await listWebinars('o1', true);
    expect(out).toEqual([archived]);
    expect(fetchMock).toHaveBeenCalledWith(
      'http://test/api/webinars?org_id=o1&archived=true',
      { headers: { Authorization: 'Bearer tok' } },
    );
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

describe('archiveWebinar', () => {
  it('POSTs to the archive endpoint and returns the webinar with archived_at set', async () => {
    fetchMock.mockResolvedValue(okJson(webinar({ archived_at: '2026-07-12T00:00:00Z' })));
    const out = await archiveWebinar('w1');
    expect(out.archived_at).toBe('2026-07-12T00:00:00Z');
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://test/api/webinars/w1/archive');
    expect(init?.method).toBe('POST');
    expect(init?.headers).toEqual({ Authorization: 'Bearer tok' });
    expect(init?.body).toBeUndefined();
  });

  it('encodes the id in the path', async () => {
    fetchMock.mockResolvedValue(okJson(webinar()));
    await archiveWebinar('w 1/x');
    expect(fetchMock.mock.calls[0][0]).toBe('http://test/api/webinars/w%201%2Fx/archive');
  });

  it('throws on error and on network failure', async () => {
    fetchMock.mockResolvedValueOnce(okJson({ error: 'not found' }, 404));
    await expect(archiveWebinar('w1')).rejects.toMatchObject({ status: 404 });
    fetchMock.mockRejectedValueOnce(new Error('net'));
    await expect(archiveWebinar('w1')).rejects.toMatchObject({ name: 'WebinarError', status: 0 });
  });
});

describe('unarchiveWebinar', () => {
  it('POSTs to the unarchive endpoint and returns the restored webinar (archived_at null)', async () => {
    fetchMock.mockResolvedValue(okJson(webinar({ archived_at: null })));
    const out = await unarchiveWebinar('w1');
    expect(out.archived_at).toBeNull();
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://test/api/webinars/w1/unarchive');
    expect(init?.method).toBe('POST');
    expect(init?.headers).toEqual({ Authorization: 'Bearer tok' });
    expect(init?.body).toBeUndefined();
  });

  it('encodes the id in the path', async () => {
    fetchMock.mockResolvedValue(okJson(webinar()));
    await unarchiveWebinar('w 1/x');
    expect(fetchMock.mock.calls[0][0]).toBe('http://test/api/webinars/w%201%2Fx/unarchive');
  });

  it('throws on error and on network failure', async () => {
    fetchMock.mockResolvedValueOnce(okJson({ error: 'not found' }, 404));
    await expect(unarchiveWebinar('w1')).rejects.toMatchObject({ status: 404 });
    fetchMock.mockRejectedValueOnce(new Error('net'));
    await expect(unarchiveWebinar('w1')).rejects.toMatchObject({ name: 'WebinarError', status: 0 });
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

  it('GETs the public endpoint by code, forwarding auth for host detection', async () => {
    fetchMock.mockResolvedValue(okJson(pub()));
    const out = await getPublicWebinar('ab12cd');
    expect(out).toEqual(pub());
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://test/api/w/ab12cd');
    // Optional auth: a logged-in caller forwards the Bearer so the server can flag
    // is_host (a host opening their own /w link is sent to the studio). authHeaders()
    // is empty for guests, who then send no Authorization at all.
    expect(init?.headers).toEqual({ Authorization: 'Bearer tok' });
  });

  it('surfaces the host-only is_host + id fields when the server returns them', async () => {
    fetchMock.mockResolvedValue(okJson(pub({ is_host: true, id: 'w-123' })));
    const out = await getPublicWebinar('ab12cd');
    expect(out.is_host).toBe(true);
    expect(out.id).toBe('w-123');
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

describe('listPublicWebinars', () => {
  const listItem = (over: Partial<PublicWebinarListItem> = {}): PublicWebinarListItem => ({
    code: 'ab12cd',
    title: 'Launch',
    status: 'live',
    source_language: 'en',
    tier: 'enhanced',
    scheduled_start: null,
    join_url: 'https://voxtranslate.app/w/ab12cd',
    viewers: 12,
    ...over,
  });

  it('GETs the public endpoint with auth and unwraps the { webinars } envelope', async () => {
    const items = [listItem(), listItem({ code: 'ef34gh', status: 'scheduled', viewers: 0 })];
    fetchMock.mockResolvedValue(okJson({ webinars: items }));
    const out = await listPublicWebinars();
    expect(out).toEqual(items);
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://test/api/webinars/public');
    expect((init as RequestInit)?.headers).toEqual({ Authorization: 'Bearer tok' });
  });

  it('returns [] when the envelope has no webinars array', async () => {
    fetchMock.mockResolvedValue(okJson({}));
    expect(await listPublicWebinars()).toEqual([]);
  });

  it('returns [] on a non-2xx response (best-effort)', async () => {
    fetchMock.mockResolvedValue(okJson({ error: 'nope' }, 500));
    expect(await listPublicWebinars()).toEqual([]);
  });

  it('returns [] when the body is not JSON (parse throws)', async () => {
    // ok:true response whose json() rejects — exercises the try/catch around parsing.
    fetchMock.mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => {
        throw new Error('not json');
      },
    } as unknown as Response);
    expect(await listPublicWebinars()).toEqual([]);
  });

  it('returns [] when fetch rejects (offline)', async () => {
    fetchMock.mockRejectedValue(new Error('net'));
    expect(await listPublicWebinars()).toEqual([]);
  });
});

describe('isWebinarLive', () => {
  it('is true only for the live status', () => {
    expect(isWebinarLive({ status: 'live' })).toBe(true);
    expect(isWebinarLive({ status: 'scheduled' })).toBe(false);
    expect(isWebinarLive({ status: 'ended' })).toBe(false);
  });
});

describe('formatScheduledStart', () => {
  it('returns "" for null/undefined/empty (an immediate webinar)', () => {
    expect(formatScheduledStart(null)).toBe('');
    expect(formatScheduledStart(undefined)).toBe('');
    expect(formatScheduledStart('')).toBe('');
  });

  it('returns "" for an unparseable value', () => {
    expect(formatScheduledStart('not-a-date')).toBe('');
  });

  it('formats a valid ISO timestamp into a non-empty short date-time string', () => {
    const out = formatScheduledStart('2026-07-12T15:00:00Z');
    expect(out).not.toBe('');
    expect(typeof out).toBe('string');
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
      video: { width: { ideal: 1280 }, height: { ideal: 720 } },
    });
    expect(
      buildPublishConstraints({ withCamera: true, videoDeviceId: 'cam-2' }),
    ).toEqual({
      audio: true,
      video: {
        deviceId: { exact: 'cam-2' },
        width: { ideal: 1280 },
        height: { ideal: 720 },
      },
    });
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
      video: {
        deviceId: { exact: 'cam-1' },
        width: { ideal: 1280 },
        height: { ideal: 720 },
      },
    });
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

describe('validateSchedule', () => {
  const NOW = Date.UTC(2030, 0, 1, 12, 0, 0); // fixed reference "now"
  const iso = (ms: number) => new Date(NOW + ms).toISOString();
  const MIN = 60 * 1000;
  const HOUR = 60 * MIN;

  it('accepts an empty (immediate) schedule', () => {
    expect(validateSchedule(null, null, NOW)).toBe('ok');
  });

  it('accepts a future start', () => {
    expect(validateSchedule(iso(HOUR), null, NOW)).toBe('ok');
  });

  it('rejects a clearly-past start', () => {
    expect(validateSchedule(iso(-HOUR), null, NOW)).toBe('startPast');
  });

  it('tolerates a start within the clock-skew window', () => {
    // 1 minute in the past — inside the 2-minute tolerance.
    expect(validateSchedule(iso(-1 * MIN), null, NOW)).toBe('ok');
  });

  it('rejects an end at or before the start', () => {
    expect(validateSchedule(iso(HOUR), iso(HOUR), NOW)).toBe('endBeforeStart');
    expect(validateSchedule(iso(2 * HOUR), iso(HOUR), NOW)).toBe(
      'endBeforeStart',
    );
  });

  it('accepts an end after the start', () => {
    expect(validateSchedule(iso(HOUR), iso(2 * HOUR), NOW)).toBe('ok');
  });

  it('reports the past start first when both rules fail', () => {
    // Past start AND end-before-start → startPast wins (checked first).
    expect(validateSchedule(iso(-2 * HOUR), iso(-3 * HOUR), NOW)).toBe(
      'startPast',
    );
  });

  it('skips the end check when there is no start to compare', () => {
    expect(validateSchedule(null, iso(HOUR), NOW)).toBe('ok');
  });
});

describe('formatWebinarClock', () => {
  it('formats zero seconds as 00:00', () => {
    expect(formatWebinarClock(0)).toBe('00:00');
  });

  it('rounds sub-minute seconds down to the current whole minute', () => {
    expect(formatWebinarClock(65)).toBe('00:01');
  });

  it('formats exactly one hour as 01:00', () => {
    expect(formatWebinarClock(3600)).toBe('01:00');
  });

  it('formats an hour-plus span as zero-padded HH:MM', () => {
    expect(formatWebinarClock(3725)).toBe('01:02'); // 1h 2m 5s → 01:02
  });

  it('clamps negatives and non-finite input to 00:00', () => {
    expect(formatWebinarClock(-5)).toBe('00:00');
    expect(formatWebinarClock(NaN)).toBe('00:00');
    expect(formatWebinarClock(Infinity)).toBe('00:00');
  });

  it('pads hours beyond 9 without truncation', () => {
    expect(formatWebinarClock(10 * 3600 + 7 * 60)).toBe('10:07');
  });
});

// ---------------------------------------------------------------------------
// clampReminder (D6 — PR2)
// ---------------------------------------------------------------------------
describe('clampReminder', () => {
  it('returns the reminder value unchanged when it fits within the time to start', () => {
    // 10 minutes reminder, 30 minutes to start → 10 is fine
    expect(clampReminder(10, 30)).toBe(10);
  });

  it('clamps down to minutesUntilStart when the reminder exceeds it', () => {
    // 60 minutes reminder, 30 minutes to start → clamped to 30
    expect(clampReminder(60, 30)).toBe(30);
  });

  it('returns 0 when minutesUntilStart is 0 (webinar starts right now)', () => {
    expect(clampReminder(10, 0)).toBe(0);
  });

  it('clamps reminder to 0 when minutesUntilStart is negative (already past)', () => {
    // Past start → clamp both to 0
    expect(clampReminder(10, -5)).toBe(0);
  });

  it('returns 0 for a reminder of 0 regardless of time to start', () => {
    expect(clampReminder(0, 120)).toBe(0);
  });

  it('returns the exact minutesUntilStart when reminder equals it', () => {
    expect(clampReminder(45, 45)).toBe(45);
  });

  it('collapses a non-finite reminder (empty field → NaN) to 0 instead of propagating NaN', () => {
    expect(clampReminder(Number.NaN, 30)).toBe(0);
  });

  it('falls back to the reminder value when minutesUntilStart is non-finite', () => {
    expect(clampReminder(10, Number.NaN)).toBe(10);
  });
});

// ---------------------------------------------------------------------------
// resetEndIfStartPassed (D6 — PR2)
// ---------------------------------------------------------------------------
describe('resetEndIfStartPassed', () => {
  const iso = (ms: number) => new Date(ms).toISOString();
  const T1 = 1_700_000_000_000; // arbitrary past reference
  const HOUR = 3_600_000;

  it('returns false when end is strictly after the new start (no reset needed)', () => {
    // start = T1, end = T1 + 1h → end > start → keep
    expect(resetEndIfStartPassed(iso(T1), iso(T1 + HOUR))).toBe(false);
  });

  it('returns true when end equals start (end must be strictly after)', () => {
    expect(resetEndIfStartPassed(iso(T1), iso(T1))).toBe(true);
  });

  it('returns true when end is before start (start moved forward past end)', () => {
    // start = T1 + 2h, end = T1 + 1h → end <= start → reset
    expect(resetEndIfStartPassed(iso(T1 + 2 * HOUR), iso(T1 + HOUR))).toBe(true);
  });

  it('returns false when end is null (no end to reset)', () => {
    expect(resetEndIfStartPassed(iso(T1), null)).toBe(false);
  });

  it('returns false when start is null (nothing to compare)', () => {
    expect(resetEndIfStartPassed(null, iso(T1))).toBe(false);
  });

  it('returns false when both are null', () => {
    expect(resetEndIfStartPassed(null, null)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// buildCardTimeline (D7 — PR3)
// Returns a compact time string for the card summary line.
// ---------------------------------------------------------------------------
describe('buildCardTimeline', () => {
  const ISO_START = '2026-08-01T14:00:00Z';
  const ISO_END   = '2026-08-01T15:30:00Z';
  const ISO_ACT_START = '2026-08-01T14:05:00Z';
  const ISO_ACT_END   = '2026-08-01T15:35:00Z';

  it('returns empty string when all time fields are null (immediate/no-time webinar)', () => {
    expect(buildCardTimeline(webinar())).toBe('');
  });

  it('returns scheduled_start formatted when only scheduled_start is set', () => {
    const w = webinar({ scheduled_start: ISO_START });
    const result = buildCardTimeline(w);
    // Must be non-empty and include the formatted start
    expect(result).not.toBe('');
    expect(result).toBe(formatScheduledStart(ISO_START));
  });

  it('appends scheduled_end when both scheduled_start and scheduled_end are set', () => {
    const w = webinar({ scheduled_start: ISO_START, scheduled_end: ISO_END });
    const result = buildCardTimeline(w);
    expect(result).toContain(formatScheduledStart(ISO_START));
    expect(result).toContain('–'); // em-dash separator between start and end
  });

  it('uses actual_start when actual_end is also present (ended webinar)', () => {
    const w = webinar({
      status: 'ended',
      actual_start: ISO_ACT_START,
      actual_end: ISO_ACT_END,
    });
    const result = buildCardTimeline(w);
    expect(result).toContain(formatScheduledStart(ISO_ACT_START));
    expect(result).toContain('–');
  });

  it('returns scheduled_start for a live webinar that has not yet set actual fields', () => {
    const w = webinar({ status: 'live', scheduled_start: ISO_START });
    const result = buildCardTimeline(w);
    expect(result).toBe(formatScheduledStart(ISO_START));
  });

  it('shows actual_start alone for a live webinar mid-broadcast (actual_start set, no end)', () => {
    // actual_start takes priority over scheduled_start; with no end it stands alone
    // (the previously-untested live-with-actual_start branch).
    const w = webinar({ status: 'live', actual_start: ISO_ACT_START, scheduled_start: ISO_START });
    expect(buildCardTimeline(w)).toBe(formatScheduledStart(ISO_ACT_START));
  });
});

// ---------------------------------------------------------------------------
// buildDetailInfo (D5/D7 — PR3)
// Returns structured data for populating the detail modal.
// ---------------------------------------------------------------------------
describe('buildDetailInfo', () => {
  const ISO_START = '2026-08-15T10:00:00Z';
  const ISO_END   = '2026-08-15T11:00:00Z';

  it('returns the webinar id, title, code, status, tier, visibility, and source_language', () => {
    const w = webinar({
      status: 'scheduled',
      source_language: 'fr',
      tier: 'standard',
      visibility: 'public',
    });
    const info = buildDetailInfo(w);
    expect(info.id).toBe('w1');
    expect(info.title).toBe('Launch');
    expect(info.code).toBe('ab12cd');
    expect(info.status).toBe('scheduled');
    expect(info.tier).toBe('standard');
    expect(info.visibility).toBe('public');
    expect(info.source_language).toBe('fr');
  });

  it('includes description, project_id, join_url, and created_at', () => {
    const w = webinar({ description: 'A great talk', project_id: 'p42' });
    const info = buildDetailInfo(w);
    expect(info.description).toBe('A great talk');
    expect(info.project_id).toBe('p42');
    expect(info.join_url).toBe('https://voxtranslate.app/w/ab12cd');
    expect(info.created_at).toBe('2026-07-11T00:00:00Z');
  });

  it('includes scheduling fields when present', () => {
    const w = webinar({ scheduled_start: ISO_START, scheduled_end: ISO_END });
    const info = buildDetailInfo(w);
    expect(info.scheduled_start).toBe(ISO_START);
    expect(info.scheduled_end).toBe(ISO_END);
  });

  it('includes actual_start and actual_end for an ended webinar', () => {
    const w = webinar({ status: 'ended', actual_start: ISO_START, actual_end: ISO_END });
    const info = buildDetailInfo(w);
    expect(info.actual_start).toBe(ISO_START);
    expect(info.actual_end).toBe(ISO_END);
  });

  it('includes record_video, record_transcript, chat_enabled flags', () => {
    const w = webinar({ record_video: true, record_transcript: true, chat_enabled: true });
    const info = buildDetailInfo(w);
    expect(info.record_video).toBe(true);
    expect(info.record_transcript).toBe(true);
    expect(info.chat_enabled).toBe(true);
  });

  it('includes notify_friends and reminder_minutes_before when present', () => {
    const w = webinar({ notify_friends: false, reminder_minutes_before: 30 });
    const info = buildDetailInfo(w);
    expect(info.notify_friends).toBe(false);
    expect(info.reminder_minutes_before).toBe(30);
  });

  it('returns null description and project_id when webinar has none', () => {
    const w = webinar(); // defaults: description: null, project_id: null
    const info = buildDetailInfo(w);
    expect(info.description).toBeNull();
    expect(info.project_id).toBeNull();
  });
});

// isWebinarRestorable (Area E — PR4)
// Mirrors the server's unarchive time-guard (D3): effective time =
// scheduled_end ?? scheduled_start ?? actual_end; restorable iff absent OR in future.
describe('isWebinarRestorable', () => {
  const FUTURE = new Date(Date.now() + 60 * 60 * 1000).toISOString(); // +1 h
  const PAST   = new Date(Date.now() - 60 * 60 * 1000).toISOString(); // -1 h

  it('returns true when all time fields are null (never scheduled/aired — always restorable)', () => {
    const w = webinar({ scheduled_start: null, scheduled_end: null, actual_end: null });
    expect(isWebinarRestorable(w, Date.now())).toBe(true);
  });

  it('returns true when scheduled_end is in the future (primary key)', () => {
    const w = webinar({ scheduled_end: FUTURE });
    expect(isWebinarRestorable(w, Date.now())).toBe(true);
  });

  it('returns false when scheduled_end is in the past (primary key — blocks restore)', () => {
    const w = webinar({ scheduled_end: PAST });
    expect(isWebinarRestorable(w, Date.now())).toBe(false);
  });

  it('uses scheduled_start as fallback when scheduled_end is null — future → true', () => {
    const w = webinar({ scheduled_end: null, scheduled_start: FUTURE });
    expect(isWebinarRestorable(w, Date.now())).toBe(true);
  });

  it('uses scheduled_start as fallback when scheduled_end is null — past → false', () => {
    const w = webinar({ scheduled_end: null, scheduled_start: PAST });
    expect(isWebinarRestorable(w, Date.now())).toBe(false);
  });

  it('uses actual_end as last fallback when both scheduled fields are null — future → true', () => {
    const w = webinar({ scheduled_end: null, scheduled_start: null, actual_end: FUTURE });
    expect(isWebinarRestorable(w, Date.now())).toBe(true);
  });

  it('uses actual_end as last fallback when both scheduled fields are null — past → false', () => {
    const w = webinar({ scheduled_end: null, scheduled_start: null, actual_end: PAST });
    expect(isWebinarRestorable(w, Date.now())).toBe(false);
  });

  it('scheduled_end takes priority over scheduled_start even if start is in the future', () => {
    // end is past → not restorable, even though start is future
    const w = webinar({ scheduled_end: PAST, scheduled_start: FUTURE });
    expect(isWebinarRestorable(w, Date.now())).toBe(false);
  });

  it('scheduled_start takes priority over actual_end even if actual_end is in the future', () => {
    // scheduled_start is past → not restorable, even though actual_end is future
    const w = webinar({ scheduled_end: null, scheduled_start: PAST, actual_end: FUTURE });
    expect(isWebinarRestorable(w, Date.now())).toBe(false);
  });

  it('accepts a custom nowMs so the helper is fully deterministic (no Date.now() inside)', () => {
    const fixedNow = new Date('2026-08-01T00:00:00Z').getTime();
    const futureFromFixed = new Date('2026-08-02T00:00:00Z').toISOString();
    const pastFromFixed   = new Date('2026-07-31T00:00:00Z').toISOString();
    expect(isWebinarRestorable(webinar({ scheduled_end: futureFromFixed }), fixedNow)).toBe(true);
    expect(isWebinarRestorable(webinar({ scheduled_end: pastFromFixed }),   fixedNow)).toBe(false);
  });

  it('returns true for a live webinar with no effective end time (status=live, no end set)', () => {
    const w = webinar({ status: 'live', scheduled_end: null, scheduled_start: null, actual_end: null });
    expect(isWebinarRestorable(w, Date.now())).toBe(true);
  });
});

// ---- PR6: Transcript download format helpers ---------------------------------

/** A minimal TranscriptRow fixture for format tests. */
function row(over: Partial<TranscriptRow> = {}): TranscriptRow {
  return {
    original_text: 'Hello world',
    original_lang: 'en',
    translations: {},
    spoken_at: '2026-07-16T10:00:00.000Z',
    ...over,
  };
}

describe('transcriptRowsToTxt', () => {
  it('returns an empty string when rows is empty', () => {
    expect(transcriptRowsToTxt([], 'en')).toBe('');
  });

  it('formats a single row with its original text when no translation for lang', () => {
    const result = transcriptRowsToTxt([row()], 'en');
    expect(result).toContain('Hello world');
    expect(result).toContain('[');   // timestamp present
  });

  it('uses the translation for lang when available', () => {
    const r = row({ translations: { fr: 'Bonjour le monde' } });
    const result = transcriptRowsToTxt([r], 'fr');
    expect(result).toContain('Bonjour le monde');
    expect(result).not.toContain('Hello world');
  });

  it('falls back to original_text when lang translation is missing', () => {
    const r = row({ translations: { fr: 'Bonjour' } });
    expect(transcriptRowsToTxt([r], 'de')).toContain('Hello world');
  });

  it('separates multiple rows with newlines', () => {
    const rows = [
      row({ original_text: 'First' }),
      row({ original_text: 'Second', spoken_at: '2026-07-16T10:01:00.000Z' }),
    ];
    const result = transcriptRowsToTxt(rows, 'en');
    expect(result).toContain('First');
    expect(result).toContain('Second');
    const lines = result.split('\n').filter(Boolean);
    expect(lines.length).toBeGreaterThanOrEqual(2);
  });
});

describe('transcriptRowsToSrt', () => {
  it('returns an empty string when rows is empty', () => {
    expect(transcriptRowsToSrt([], 'en')).toBe('');
  });

  it('formats a single row as SRT block (sequence + timestamps + text)', () => {
    const result = transcriptRowsToSrt([row()], 'en');
    // SRT: sequence number
    expect(result).toMatch(/^1\n/);
    // SRT: timestamp line with --> separator
    expect(result).toContain('-->');
    // SRT: text
    expect(result).toContain('Hello world');
  });

  it('uses the translation for lang when available', () => {
    const r = row({ translations: { fr: 'Bonjour' } });
    expect(transcriptRowsToSrt([r], 'fr')).toContain('Bonjour');
  });

  it('falls back to original_text when translation is missing', () => {
    expect(transcriptRowsToSrt([row()], 'de')).toContain('Hello world');
  });

  it('numbers multiple rows sequentially and uses blank-line separators', () => {
    const rows = [
      row({ original_text: 'One' }),
      row({ original_text: 'Two', spoken_at: '2026-07-16T10:00:05.000Z' }),
    ];
    const result = transcriptRowsToSrt(rows, 'en');
    expect(result).toContain('1\n');
    expect(result).toContain('2\n');
    expect(result).toContain('One');
    expect(result).toContain('Two');
    // Blank-line separator between blocks
    expect(result).toContain('\n\n');
  });
});

describe('WebinarSessionStats type', () => {
  it('is structurally compatible with the expected shape', () => {
    const stats: WebinarSessionStats = {
      peak_viewers: 42,
      unique_attendees: 100,
      duration_seconds: 3600,
    };
    expect(stats.peak_viewers).toBe(42);
    expect(stats.unique_attendees).toBe(100);
    expect(stats.duration_seconds).toBe(3600);
  });
});
