// VoxTranslate for Business helpers (spec 0106): org/project listing, room
// binding, cloud-recording upload (presign → PUT → complete) and project voice
// notes. Pure fetch/XHR glue — everything is mocked, no network.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('./auth', () => ({
  authHeaders: () => ({ Authorization: 'Bearer tok' }),
  HTTP_BASE: 'http://test',
}));

import {
  DASHBOARD_URL,
  bindRoom,
  canCloudRecord,
  getRoomBinding,
  listMyOrgs,
  listProjects,
  listProjectVoiceMessages,
  uploadProjectVoiceMessage,
  uploadRecording,
  voiceMessageAudioUrl,
  type BusinessOrg,
} from './business';

function okJson(body: unknown, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
    text: async () => JSON.stringify(body),
  } as unknown as Response;
}

const org = (subscription_status: string): BusinessOrg => ({
  id: 'o1',
  name: 'Acme',
  role: 'owner',
  plan: 'business',
  subscription_status,
});

const fetchMock = vi.fn<(url: string, init?: RequestInit) => Promise<Response>>();

type ProgressPayload = { lengthComputable: boolean; loaded: number; total: number };

/** Minimal XHR stand-in: records open/headers/body, lets tests fire callbacks. */
class FakeXHR {
  static instances: FakeXHR[] = [];
  method = '';
  url = '';
  headers: Record<string, string> = {};
  status = 0;
  responseText = '';
  body: FormData | null = null;
  upload = { onprogress: null as ((e: ProgressPayload) => void) | null };
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onabort: (() => void) | null = null;
  constructor() {
    FakeXHR.instances.push(this);
  }
  open(method: string, url: string): void {
    this.method = method;
    this.url = url;
  }
  setRequestHeader(k: string, v: string): void {
    this.headers[k] = v;
  }
  send(body: FormData): void {
    this.body = body;
  }
}

beforeEach(() => {
  fetchMock.mockReset();
  vi.stubGlobal('fetch', fetchMock);
  FakeXHR.instances = [];
  vi.stubGlobal('XMLHttpRequest', FakeXHR);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('canCloudRecord', () => {
  it('allows only orgs with an active subscription', () => {
    expect(canCloudRecord(org('active'))).toBe(true);
    expect(canCloudRecord(org('none'))).toBe(false);
    expect(canCloudRecord(org('past_due'))).toBe(false);
    expect(canCloudRecord(org('canceled'))).toBe(false);
    expect(canCloudRecord(undefined)).toBe(false);
  });
});

describe('DASHBOARD_URL', () => {
  it('points at the standalone dashboard origin', () => {
    expect(DASHBOARD_URL).toBe('https://dashboard.voxtranslate.app');
  });
});

describe('listMyOrgs', () => {
  it('returns the org list with auth headers', async () => {
    fetchMock.mockResolvedValue(okJson([org('active')]));
    const orgs = await listMyOrgs();
    expect(orgs).toEqual([org('active')]);
    expect(fetchMock).toHaveBeenCalledWith('http://test/api/business/organizations', {
      headers: { Authorization: 'Bearer tok' },
    });
  });

  it('returns [] on a non-ok response', async () => {
    fetchMock.mockResolvedValue(okJson({ error: 'nope' }, 403));
    expect(await listMyOrgs()).toEqual([]);
  });

  it('returns [] when fetch throws (offline)', async () => {
    fetchMock.mockRejectedValue(new Error('net'));
    expect(await listMyOrgs()).toEqual([]);
  });
});

describe('listProjects', () => {
  it('returns the projects of the org', async () => {
    fetchMock.mockResolvedValue(okJson([{ id: 'p1', name: 'Alpha' }]));
    expect(await listProjects('o1')).toEqual([{ id: 'p1', name: 'Alpha' }]);
    expect(fetchMock).toHaveBeenCalledWith(
      'http://test/api/business/organizations/o1/projects',
      { headers: { Authorization: 'Bearer tok' } },
    );
  });

  it('returns [] on error responses and network failures', async () => {
    fetchMock.mockResolvedValueOnce(okJson('x', 500));
    expect(await listProjects('o1')).toEqual([]);
    fetchMock.mockRejectedValueOnce(new Error('net'));
    expect(await listProjects('o1')).toEqual([]);
  });
});

describe('bindRoom', () => {
  it('PATCHes the binding with an encoded room code', async () => {
    fetchMock.mockResolvedValue(okJson({}));
    const ok = await bindRoom('sala grande', {
      org_id: 'o1',
      project_id: 'p1',
      cloud_recording_enabled: true,
    });
    expect(ok).toBe(true);
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://test/api/rooms/sala%20grande/business');
    expect(init?.method).toBe('PATCH');
    expect(init?.headers).toEqual({
      Authorization: 'Bearer tok',
      'Content-Type': 'application/json',
    });
    expect(JSON.parse(init?.body as string)).toEqual({
      org_id: 'o1',
      project_id: 'p1',
      cloud_recording_enabled: true,
    });
  });

  it('returns false on a non-ok response', async () => {
    fetchMock.mockResolvedValue(okJson({}, 403));
    expect(await bindRoom('r', { org_id: 'o1' })).toBe(false);
  });

  it('returns false when fetch throws', async () => {
    fetchMock.mockRejectedValue(new Error('net'));
    expect(await bindRoom('r', { org_id: 'o1' })).toBe(false);
  });
});

describe('uploadRecording', () => {
  const blob = new Blob([new Uint8Array(8)], { type: 'video/webm' });

  it('presigns, PUTs to storage, then completes', async () => {
    fetchMock
      .mockResolvedValueOnce(okJson({ upload_url: 'http://signed/put', object_path: 'p/x.webm' }))
      .mockResolvedValueOnce(okJson({}))
      .mockResolvedValueOnce(okJson({}));
    expect(await uploadRecording('sess 1', blob, 12)).toBe(true);
    expect(fetchMock).toHaveBeenCalledTimes(3);

    const [presignUrl, presignInit] = fetchMock.mock.calls[0];
    expect(presignUrl).toBe('http://test/api/business/rooms/sess%201/recording/upload-url');
    expect(presignInit?.method).toBe('POST');
    expect(presignInit?.headers).toEqual({ Authorization: 'Bearer tok' });

    // The PUT goes to the signed URL with NO auth header (the URL is signed).
    const [putUrl, putInit] = fetchMock.mock.calls[1];
    expect(putUrl).toBe('http://signed/put');
    expect(putInit?.method).toBe('PUT');
    expect(putInit?.headers).toEqual({ 'Content-Type': 'video/webm', 'x-upsert': 'true' });
    expect(putInit?.body).toBe(blob);

    const [doneUrl, doneInit] = fetchMock.mock.calls[2];
    expect(doneUrl).toBe('http://test/api/business/rooms/sess%201/recording/complete');
    expect(JSON.parse(doneInit?.body as string)).toEqual({
      object_path: 'p/x.webm',
      duration_seconds: 12,
    });
  });

  it('stops after a failed presign', async () => {
    fetchMock.mockResolvedValueOnce(okJson({}, 402));
    expect(await uploadRecording('s', blob, 5)).toBe(false);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('stops after a failed storage PUT', async () => {
    fetchMock
      .mockResolvedValueOnce(okJson({ upload_url: 'http://signed/put', object_path: 'p' }))
      .mockResolvedValueOnce(okJson({}, 500));
    expect(await uploadRecording('s', blob, 5)).toBe(false);
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it('reports failure when the complete step fails', async () => {
    fetchMock
      .mockResolvedValueOnce(okJson({ upload_url: 'http://signed/put', object_path: 'p' }))
      .mockResolvedValueOnce(okJson({}))
      .mockResolvedValueOnce(okJson({}, 500));
    expect(await uploadRecording('s', blob, 5)).toBe(false);
  });

  it('returns false when any step throws', async () => {
    fetchMock.mockRejectedValue(new Error('net'));
    expect(await uploadRecording('s', blob, 5)).toBe(false);
  });
});

describe('uploadProjectVoiceMessage', () => {
  const file = new File([new Uint8Array([1, 2, 3])], 'note.webm', { type: 'audio/webm' });

  it('POSTs the form via XHR with auth headers and rounded duration', async () => {
    const onProgress = vi.fn();
    const promise = uploadProjectVoiceMessage('org 1', 'p/1', file, 89.6, onProgress);
    const xhr = FakeXHR.instances[0];
    expect(xhr.method).toBe('POST');
    expect(xhr.url).toBe(
      'http://test/api/business/organizations/org%201/projects/p%2F1/voice-messages',
    );
    expect(xhr.headers).toEqual({ Authorization: 'Bearer tok' });
    const form = xhr.body!;
    expect((form.get('file') as File).name).toBe('note.webm');
    expect(form.get('duration_seconds')).toBe('90');

    xhr.upload.onprogress!({ lengthComputable: true, loaded: 1, total: 4 });
    expect(onProgress).toHaveBeenCalledWith(0.25);
    xhr.upload.onprogress!({ lengthComputable: false, loaded: 9, total: 0 });
    expect(onProgress).toHaveBeenCalledTimes(1); // non-computable events are ignored

    xhr.status = 201;
    xhr.responseText = JSON.stringify({ translate_blocked: 'credits' });
    xhr.onload!();
    await expect(promise).resolves.toEqual({ ok: true, status: 201, translateBlocked: 'credits' });
  });

  it('omits the duration field and progress hook when absent', async () => {
    const promise = uploadProjectVoiceMessage('o', 'p', file, null);
    const xhr = FakeXHR.instances[0];
    expect(xhr.body!.get('duration_seconds')).toBeNull();
    expect(xhr.upload.onprogress).toBeNull();
    xhr.status = 200;
    xhr.responseText = '{}';
    xhr.onload!();
    await expect(promise).resolves.toEqual({ ok: true, status: 200, translateBlocked: undefined });
  });

  it("surfaces the 'error' translate-blocked reason", async () => {
    const promise = uploadProjectVoiceMessage('o', 'p', file, 3);
    const xhr = FakeXHR.instances[0];
    xhr.status = 200;
    xhr.responseText = JSON.stringify({ translate_blocked: 'error' });
    xhr.onload!();
    await expect(promise).resolves.toEqual({ ok: true, status: 200, translateBlocked: 'error' });
  });

  it('ignores unknown translate_blocked values and non-JSON bodies', async () => {
    const p1 = uploadProjectVoiceMessage('o', 'p', file, 3);
    const x1 = FakeXHR.instances[0];
    x1.status = 200;
    x1.responseText = JSON.stringify({ translate_blocked: 'later' });
    x1.onload!();
    await expect(p1).resolves.toEqual({ ok: true, status: 200, translateBlocked: undefined });

    const p2 = uploadProjectVoiceMessage('o', 'p', file, 3);
    const x2 = FakeXHR.instances[1];
    x2.status = 200;
    x2.responseText = '<!doctype html>';
    x2.onload!();
    await expect(p2).resolves.toEqual({ ok: true, status: 200, translateBlocked: undefined });
  });

  it('reports a non-2xx status as failure', async () => {
    const promise = uploadProjectVoiceMessage('o', 'p', file, 3);
    const xhr = FakeXHR.instances[0];
    xhr.status = 422;
    xhr.responseText = JSON.stringify({ error: 'too long' });
    xhr.onload!();
    await expect(promise).resolves.toEqual({ ok: false, status: 422, translateBlocked: undefined });
  });

  it('resolves ok:false on network error and abort', async () => {
    const p1 = uploadProjectVoiceMessage('o', 'p', file, 3);
    FakeXHR.instances[0].onerror!();
    await expect(p1).resolves.toEqual({ ok: false, status: 0 });

    const p2 = uploadProjectVoiceMessage('o', 'p', file, 3);
    FakeXHR.instances[1].onabort!();
    await expect(p2).resolves.toEqual({ ok: false, status: 0 });
  });
});

describe('listProjectVoiceMessages', () => {
  it('returns the voice_messages array', async () => {
    fetchMock.mockResolvedValue(okJson({ voice_messages: [{ id: 'vm1' }] }));
    const list = await listProjectVoiceMessages('o1', 'p1');
    expect(list.map((m) => m.id)).toEqual(['vm1']);
    expect(fetchMock).toHaveBeenCalledWith(
      'http://test/api/business/organizations/o1/projects/p1/voice-messages',
      { headers: { Authorization: 'Bearer tok' } },
    );
  });

  it('returns [] when the field is missing, on error, or offline', async () => {
    fetchMock.mockResolvedValueOnce(okJson({}));
    expect(await listProjectVoiceMessages('o', 'p')).toEqual([]);
    fetchMock.mockResolvedValueOnce(okJson('x', 500));
    expect(await listProjectVoiceMessages('o', 'p')).toEqual([]);
    fetchMock.mockRejectedValueOnce(new Error('net'));
    expect(await listProjectVoiceMessages('o', 'p')).toEqual([]);
  });
});

describe('voiceMessageAudioUrl', () => {
  it('returns the signed playback URL', async () => {
    fetchMock.mockResolvedValue(okJson({ url: 'http://signed/audio' }));
    expect(await voiceMessageAudioUrl('o1', 'p1', 'vm1')).toBe('http://signed/audio');
    expect(fetchMock).toHaveBeenCalledWith(
      'http://test/api/business/organizations/o1/projects/p1/voice-messages/vm1/audio-url',
      { headers: { Authorization: 'Bearer tok' } },
    );
  });

  it('returns null when the url is missing, on error, or offline', async () => {
    fetchMock.mockResolvedValueOnce(okJson({}));
    expect(await voiceMessageAudioUrl('o', 'p', 'v')).toBeNull();
    fetchMock.mockResolvedValueOnce(okJson('x', 404));
    expect(await voiceMessageAudioUrl('o', 'p', 'v')).toBeNull();
    fetchMock.mockRejectedValueOnce(new Error('net'));
    expect(await voiceMessageAudioUrl('o', 'p', 'v')).toBeNull();
  });
});

describe('getRoomBinding', () => {
  it('returns the binding for a bound room', async () => {
    fetchMock.mockResolvedValue(okJson({ org_id: 'o1', project_id: 'p1' }));
    expect(await getRoomBinding('sala grande')).toEqual({ org_id: 'o1', project_id: 'p1' });
    expect(fetchMock).toHaveBeenCalledWith('http://test/api/rooms/sala%20grande/business', {
      headers: { Authorization: 'Bearer tok' },
    });
  });

  it('treats a 200 with a null org_id as unbound (no console-error 404s)', async () => {
    fetchMock.mockResolvedValue(okJson({ org_id: null, project_id: null }));
    expect(await getRoomBinding('r')).toBeNull();
  });

  it('returns null on error responses and network failures', async () => {
    fetchMock.mockResolvedValueOnce(okJson('x', 401));
    expect(await getRoomBinding('r')).toBeNull();
    fetchMock.mockRejectedValueOnce(new Error('net'));
    expect(await getRoomBinding('r')).toBeNull();
  });
});
