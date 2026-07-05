import { describe, it, expect, vi, afterEach } from 'vitest';

// api.ts only needs `authHeaders` + `HTTP_BASE` from auth.ts, which itself reads
// `location` and `localStorage` at import time — mock the module so this suite
// stays independent of that plumbing and the request/URL assertions are exact.
vi.mock('./auth', () => ({
  authHeaders: () => ({ Authorization: 'Bearer tk' }),
  HTTP_BASE: 'http://api.test',
}));

import * as api from './api';

const BASE = 'http://api.test';
const AUTH = { Authorization: 'Bearer tk' };

function okJson(body: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
    text: async () => JSON.stringify(body),
  } as Response;
}

function textRes(text: string, status: number) {
  return {
    ok: status >= 200 && status < 300,
    status,
    text: async () => text,
    json: async () => JSON.parse(text),
  } as Response;
}

/** A response whose body readers reject (malformed payload). */
function brokenBody(status: number) {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => {
      throw new Error('bad json');
    },
    text: async () => {
      throw new Error('bad body');
    },
  } as unknown as Response;
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
  vi.useRealTimers();
});

// ---- Enhanced (Cartesia) session + voice cloning ----------------------------

describe('enhanced session + voice cloning', () => {
  it('fetchEnhancedSession POSTs with auth and returns the session', async () => {
    const session = {
      token: 'ct',
      expires_at: 123,
      cartesia_version: '2025-04-16',
      stt: { endpoint: 'wss://stt', model: 'ink-whisper' },
      tts: { endpoint: 'wss://tts', model: 'sonic-3.5' },
      voice_cloning_enabled: true,
    };
    const mock = stubFetch(okJson(session));
    const r = await api.fetchEnhancedSession();
    expect(r?.token).toBe('ct');
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/sessions/enhanced/session`);
    const init = mock.mock.calls[0][1] as RequestInit;
    expect(init.method).toBe('POST');
    expect(init.headers).toEqual(AUTH);
  });

  it('fetchEnhancedSession returns null on 401 and on network error', async () => {
    stubFetch(okJson('no', 401), new Error('net'));
    expect(await api.fetchEnhancedSession()).toBeNull();
    expect(await api.fetchEnhancedSession()).toBeNull();
  });

  it('cloneVoice uploads the clip (+ optional language) and returns the voice id', async () => {
    const mock = stubFetch(okJson({ voice_id: 'v1' }));
    const r = await api.cloneVoice(new Blob(['a']), 'it');
    expect(r).toEqual({ voice_id: 'v1' });
    const form = (mock.mock.calls[0][1] as RequestInit).body as FormData;
    expect(form.get('language')).toBe('it');
    expect(form.get('clip')).toBeInstanceOf(Blob);
  });

  it('cloneVoice resolves to the fallback shape on rejection and network error', async () => {
    const mock = stubFetch(okJson('payment required', 402), new Error('net'));
    expect(await api.cloneVoice(new Blob(['a']))).toEqual({ voice_id: null, fallback: true });
    expect(await api.cloneVoice(new Blob(['a']))).toEqual({ voice_id: null, fallback: true });
    // No language argument → the field is absent from the form.
    const form = (mock.mock.calls[0][1] as RequestInit).body as FormData;
    expect(form.get('language')).toBeNull();
  });
});

// ---- Transcript + bookmarks --------------------------------------------------

describe('transcript + bookmarks', () => {
  it('fetchTranscript returns the doc; null on 403 / network error', async () => {
    const doc = {
      session: { id: 's1', room_name: 'r', started_at: 'x', duration_seconds: 1, participants: [] },
      events: [],
      bookmarks: [],
      exported_at: 'y',
    };
    const mock = stubFetch(okJson(doc), okJson('no', 403), new Error('net'));
    expect((await api.fetchTranscript('s 1'))?.session.id).toBe('s1');
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/sessions/s%201/transcript.json`);
    expect((mock.mock.calls[0][1] as RequestInit).headers).toEqual(AUTH);
    expect(await api.fetchTranscript('s1')).toBeNull();
    expect(await api.fetchTranscript('s1')).toBeNull();
  });

  it('fetchBookmarks lists all bookmarks; null on failure', async () => {
    const rows = [{ id: 'b1', ts: 't', by: 'Al', mine: true }];
    const mock = stubFetch(okJson(rows), okJson('no', 404), new Error('net'));
    expect(await api.fetchBookmarks('s1')).toEqual(rows);
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/sessions/s1/bookmarks`);
    expect(await api.fetchBookmarks('s1')).toBeNull();
    expect(await api.fetchBookmarks('s1')).toBeNull();
  });

  it('addBookmark POSTs label + ts and returns the pin; null on failure', async () => {
    const pin = { id: 'b2', ts: 'T', label: 'key point', by: 'Al', mine: true };
    const mock = stubFetch(okJson(pin), okJson('no', 401), new Error('net'));
    expect(await api.addBookmark('s1', { label: 'key point', ts: 'T' })).toEqual(pin);
    const init = mock.mock.calls[0][1] as RequestInit;
    expect(init.method).toBe('POST');
    expect(JSON.parse(init.body as string)).toEqual({ label: 'key point', ts: 'T' });
    // Default opts → server stamps "now".
    expect(await api.addBookmark('s1')).toBeNull();
    expect(JSON.parse((mock.mock.calls[1][1] as RequestInit).body as string)).toEqual({});
    expect(await api.addBookmark('s1')).toBeNull();
  });

  it('updateBookmarkLabel PATCHes the label; false on rejection / network', async () => {
    const mock = stubFetch(okJson({}, 200), okJson('no', 403), new Error('net'));
    expect(await api.updateBookmarkLabel('s1', 'b/1', 'new label')).toBe(true);
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/sessions/s1/bookmarks/b%2F1`);
    expect((mock.mock.calls[0][1] as RequestInit).method).toBe('PATCH');
    expect(await api.updateBookmarkLabel('s1', 'b1', '')).toBe(false);
    expect(await api.updateBookmarkLabel('s1', 'b1', 'x')).toBe(false);
  });

  it('deleteBookmark DELETEs; false on network error', async () => {
    const mock = stubFetch(okJson({}, 200), new Error('net'));
    expect(await api.deleteBookmark('s1', 'b1')).toBe(true);
    expect((mock.mock.calls[0][1] as RequestInit).method).toBe('DELETE');
    expect(await api.deleteBookmark('s1', 'b1')).toBe(false);
  });
});

// ---- Room glossary ------------------------------------------------------------

describe('glossary', () => {
  const entry = { source_lang: 'en', target_lang: 'it', source_term: 'foo', target_term: 'bar' };

  it('fetchGlossary returns the glossary, an empty one on 404, null otherwise', async () => {
    const g = { name: 'legal', entries: [entry], max_entries: 200 };
    const mock = stubFetch(okJson(g), okJson('none', 404), okJson('no', 401), new Error('net'));
    expect(await api.fetchGlossary('my room')).toEqual(g);
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/rooms/my%20room/glossary`);
    expect(await api.fetchGlossary('r')).toEqual({ name: null, entries: [], max_entries: 200 });
    expect(await api.fetchGlossary('r')).toBeNull();
    expect(await api.fetchGlossary('r')).toBeNull();
  });

  it('saveGlossary returns the saved glossary, surfaces 400 text, empty error on network', async () => {
    const g = { name: null, entries: [entry], max_entries: 200 };
    const mock = stubFetch(okJson(g), textRes('too many entries', 400), new Error('net'));
    expect(await api.saveGlossary('r', null, [entry])).toEqual({ glossary: g, error: '' });
    expect(JSON.parse((mock.mock.calls[0][1] as RequestInit).body as string)).toEqual({
      name: null,
      entries: [entry],
    });
    expect(await api.saveGlossary('r', 'n', [])).toEqual({
      glossary: null,
      error: 'too many entries',
    });
    expect(await api.saveGlossary('r', 'n', [])).toEqual({ glossary: null, error: '' });
  });

  it('importGlossaryCsv POSTs the CSV to /import', async () => {
    const g = { name: null, entries: [entry], max_entries: 200 };
    const mock = stubFetch(okJson(g));
    expect(await api.importGlossaryCsv('r', 'en,it,foo,bar')).toEqual({ glossary: g, error: '' });
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/rooms/r/glossary/import`);
    expect(JSON.parse((mock.mock.calls[0][1] as RequestInit).body as string)).toEqual({
      csv: 'en,it,foo,bar',
    });
  });

  it('deleteGlossary reports ok; false on network error', async () => {
    const mock = stubFetch(okJson({}, 200), new Error('net'));
    expect(await api.deleteGlossary('r')).toBe(true);
    expect((mock.mock.calls[0][1] as RequestInit).method).toBe('DELETE');
    expect(await api.deleteGlossary('r')).toBe(false);
  });
});

// ---- Vox Voices prefs + upload config -----------------------------------------

describe('tts prefs + file-upload config', () => {
  it('saveTtsPrefs POSTs the prefs; false on rejection / network', async () => {
    const mock = stubFetch(okJson({}, 200), okJson('no', 401), new Error('net'));
    expect(await api.saveTtsPrefs({ tts_engine_pref: 'vox', tts_voice_id: 'af_bella' })).toBe(true);
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/user/tts-prefs`);
    expect(JSON.parse((mock.mock.calls[0][1] as RequestInit).body as string)).toEqual({
      tts_engine_pref: 'vox',
      tts_voice_id: 'af_bella',
    });
    expect(await api.saveTtsPrefs({})).toBe(false);
    expect(await api.saveTtsPrefs({})).toBe(false);
  });

  it('fileUploadEnabled is true only for enabled === true', async () => {
    stubFetch(
      okJson({ enabled: true }),
      okJson({ enabled: 'yes' }),
      okJson({}),
      okJson('no', 503),
      new Error('net'),
    );
    expect(await api.fileUploadEnabled()).toBe(true);
    expect(await api.fileUploadEnabled()).toBe(false);
    expect(await api.fileUploadEnabled()).toBe(false);
    expect(await api.fileUploadEnabled()).toBe(false);
    expect(await api.fileUploadEnabled()).toBe(false);
  });
});

// ---- Chat file upload (pre-check + XHR) ----------------------------------------

describe('checkUploadFile', () => {
  const file = (name: string, size: number) => new File([new Uint8Array(size)], name);

  it('accepts supported documents (case-insensitive extension)', () => {
    expect(api.checkUploadFile(file('notes.txt', 10))).toBeNull();
    expect(api.checkUploadFile(file('REPORT.PDF', 1024))).toBeNull();
    expect(api.checkUploadFile(file('deck.pptx', api.UPLOAD_MAX_BYTES))).toBeNull();
  });

  it('rejects unsupported types and bad sizes', () => {
    expect(api.checkUploadFile(file('evil.exe', 10))).toBe('type');
    expect(api.checkUploadFile(file('Makefile', 10))).toBe('type');
    expect(api.checkUploadFile(file('big.pdf', api.UPLOAD_MAX_BYTES + 1))).toBe('size');
    expect(api.checkUploadFile(file('empty.md', 0))).toBe('size');
  });

  it('exposes picker metadata pairing extensions with MIME types', () => {
    expect(api.UPLOAD_ACCEPT).toContain('.pdf');
    expect(api.UPLOAD_ACCEPT).toContain('application/pdf');
    expect(api.UPLOAD_EXTS).toContain('docx');
  });
});

describe('uploadChatFile', () => {
  class FakeXHR {
    static instances: FakeXHR[] = [];
    method = '';
    url = '';
    headers: Record<string, string> = {};
    status = 0;
    responseText = '';
    upload = {
      onprogress: null as
        | ((e: { lengthComputable: boolean; loaded: number; total: number }) => void)
        | null,
    };
    onload: (() => void) | null = null;
    onerror: (() => void) | null = null;
    onabort: (() => void) | null = null;
    sent: FormData | null = null;
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
      this.sent = body;
    }
  }

  function stubXhr() {
    FakeXHR.instances = [];
    vi.stubGlobal('XMLHttpRequest', FakeXHR as unknown as typeof XMLHttpRequest);
    return FakeXHR;
  }

  it('uploads with auth, reports progress, and parses translate_blocked', async () => {
    const X = stubXhr();
    const progress: number[] = [];
    const p = api.uploadChatFile('my room', 'p1', new File(['hello'], 'notes.txt'), (f) =>
      progress.push(f),
    );
    const xhr = X.instances[0];
    expect(xhr.method).toBe('POST');
    expect(xhr.url).toBe(`${BASE}/api/rooms/my%20room/files`);
    expect(xhr.headers).toEqual(AUTH);
    expect(xhr.sent?.get('peer_id')).toBe('p1');
    expect(xhr.sent?.get('file')).toBeInstanceOf(Blob);

    xhr.upload.onprogress?.({ lengthComputable: true, loaded: 1, total: 4 });
    xhr.upload.onprogress?.({ lengthComputable: false, loaded: 9, total: 9 }); // ignored
    expect(progress).toEqual([0.25]);

    xhr.status = 200;
    xhr.responseText = JSON.stringify({ translate_blocked: 'credits' });
    xhr.onload?.();
    expect(await p).toEqual({ ok: true, status: 200, translateBlocked: 'credits' });
  });

  it('ignores non-JSON / unexpected bodies and reports HTTP failures', async () => {
    const X = stubXhr();
    // No onProgress → no progress handler is installed.
    const p1 = api.uploadChatFile('r', 'p2', new File(['x'], 'a.pdf'));
    const x1 = X.instances[0];
    expect(x1.upload.onprogress).toBeNull();
    x1.status = 500;
    x1.responseText = 'internal error'; // non-JSON — swallowed
    x1.onload?.();
    expect(await p1).toEqual({ ok: false, status: 500, translateBlocked: undefined });

    const p2 = api.uploadChatFile('r', 'p2', new File(['x'], 'a.pdf'));
    const x2 = X.instances[1];
    x2.status = 201;
    x2.responseText = JSON.stringify({ translate_blocked: 'other' }); // unknown value
    x2.onload?.();
    expect(await p2).toEqual({ ok: true, status: 201, translateBlocked: undefined });
  });

  it('resolves { ok:false, status:0 } on network error and abort', async () => {
    const X = stubXhr();
    const p1 = api.uploadChatFile('r', 'p', new File(['x'], 'a.txt'));
    X.instances[0].onerror?.();
    expect(await p1).toEqual({ ok: false, status: 0 });

    const p2 = api.uploadChatFile('r', 'p', new File(['x'], 'a.txt'));
    X.instances[1].onabort?.();
    expect(await p2).toEqual({ ok: false, status: 0 });
  });
});

// ---- AI pricing (page-lifetime cache) -------------------------------------------

describe('fetchAiPricing', () => {
  it('returns null on failure, then fetches once and caches', async () => {
    const pricing = {
      report: { base: 1, per_minute: 0.1 },
      sentiment: { base: 1, per_participant: 0.2, per_minute: 0.1 },
      email: { draft: 0.5 },
      suggestions: { per_minute: 0.05, interval_seconds: 60 },
      email_enabled: true,
    };
    const mock = stubFetch(okJson('no', 503), new Error('net'), okJson(pricing));
    expect(await api.fetchAiPricing()).toBeNull(); // 503 → not cached
    expect(await api.fetchAiPricing()).toBeNull(); // network → not cached
    expect(await api.fetchAiPricing()).toEqual(pricing);
    expect(await api.fetchAiPricing()).toEqual(pricing); // served from cache
    expect(mock).toHaveBeenCalledTimes(3);
  });
});

// ---- AI transcript correction -----------------------------------------------------

describe('correction', () => {
  it('fetchCorrectionStatus omits lang for original mode; null on failure', async () => {
    const mock = stubFetch(
      okJson({ cached: true, cost: 0 }),
      okJson({ cached: false, cost: 1.2 }),
      okJson('no', 403),
      new Error('net'),
    );
    expect(await api.fetchCorrectionStatus('s1', 'original', 'it')).toEqual({
      cached: true,
      cost: 0,
    });
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/sessions/s1/correction?mode=original`);
    expect(await api.fetchCorrectionStatus('s1', 'translated', 'it')).toEqual({
      cached: false,
      cost: 1.2,
    });
    expect(mock.mock.calls[1][0]).toBe(`${BASE}/api/sessions/s1/correction?mode=translated&lang=it`);
    expect(await api.fetchCorrectionStatus('s1', 'both', 'it')).toBeNull();
    expect(await api.fetchCorrectionStatus('s1', 'both', 'it')).toBeNull();
  });

  it('ensureCorrection handles cache hits, 402 (incl. unreadable), 429, and errors', async () => {
    stubFetch(
      okJson({ cached: true, cost: 0.4 }),
      okJson({ required: 2, available: 0.5 }, 402),
      brokenBody(402),
      okJson('slow down', 429),
      okJson('boom', 500),
      new Error('net'),
    );
    expect(await api.ensureCorrection('s1', 'original', 'en')).toEqual({
      ok: true,
      cached: true,
      cost: 0.4,
    });
    expect(await api.ensureCorrection('s1', 'original', 'en')).toEqual({
      ok: false,
      cached: false,
      insufficient: { required: 2, available: 0.5 },
    });
    expect(await api.ensureCorrection('s1', 'original', 'en')).toEqual({
      ok: false,
      cached: false,
      insufficient: {},
    });
    expect(await api.ensureCorrection('s1', 'original', 'en')).toEqual({
      ok: false,
      cached: false,
      rateLimited: true,
    });
    expect(await api.ensureCorrection('s1', 'original', 'en')).toEqual({ ok: false, cached: false });
    expect(await api.ensureCorrection('s1', 'original', 'en')).toEqual({ ok: false, cached: false });
  });

  it('ensureCorrection polls a 202 job to done', async () => {
    vi.useFakeTimers();
    stubFetch(
      okJson({ job_id: 'j1' }, 202),
      okJson({ status: 'done', result: { cached: false, charged: true, cost: 1, balance: 9 } }),
    );
    const p = api.ensureCorrection('s1', 'both', 'it');
    await vi.advanceTimersByTimeAsync(1500);
    expect(await p).toEqual({ ok: true, cached: false, charged: true, cost: 1, balance: 9 });
  });

  it('ensureCorrection surfaces job-level insufficient credits and generic job failures', async () => {
    vi.useFakeTimers();
    stubFetch(
      okJson({ job_id: 'j1' }, 202),
      okJson({
        status: 'failed',
        error: 'insufficient_credits',
        result: { required: 3, available: 1 },
      }),
    );
    let p = api.ensureCorrection('s1', 'original', 'en');
    await vi.advanceTimersByTimeAsync(1500);
    expect(await p).toEqual({
      ok: false,
      cached: false,
      insufficient: { required: 3, available: 1 },
    });

    // insufficient_credits with no payload → empty insufficient shape.
    stubFetch(
      okJson({ job_id: 'j2' }, 202),
      okJson({ status: 'failed', error: 'insufficient_credits', result: null }),
    );
    p = api.ensureCorrection('s1', 'original', 'en');
    await vi.advanceTimersByTimeAsync(1500);
    expect(await p).toEqual({ ok: false, cached: false, insufficient: {} });

    // Any other failure → generic.
    stubFetch(okJson({ job_id: 'j3' }, 202), okJson({ status: 'failed', error: 'llm_error' }));
    p = api.ensureCorrection('s1', 'original', 'en');
    await vi.advanceTimersByTimeAsync(1500);
    expect(await p).toEqual({ ok: false, cached: false });
  });
});

// ---- Quiz history + AI quiz --------------------------------------------------------

describe('quizzes', () => {
  const question = { prompt: 'Q?', options: ['a', 'b'], correct_index: 1 };
  const result = { peer_id: 'p1', display_name: 'Al', score: 1, total: 1 };

  it('saveQuizHistory POSTs the summary; false on failure', async () => {
    const summary = { title: 'T', questions: [question], results: [result] };
    const mock = stubFetch(okJson({}, 201), okJson('no', 401), new Error('net'));
    expect(await api.saveQuizHistory('s1', summary)).toBe(true);
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/sessions/s1/quizzes`);
    expect(JSON.parse((mock.mock.calls[0][1] as RequestInit).body as string)).toEqual(summary);
    expect(await api.saveQuizHistory('s1', summary)).toBe(false);
    expect(await api.saveQuizHistory('s1', summary)).toBe(false);
  });

  it('fetchSessionQuizzes returns rows; [] on failure', async () => {
    const rows = [{ id: 'q1', title: null, questions: [question], created_at: 'x', results: [] }];
    stubFetch(okJson(rows), okJson('no', 403), new Error('net'));
    expect(await api.fetchSessionQuizzes('s1')).toEqual(rows);
    expect(await api.fetchSessionQuizzes('s1')).toEqual([]);
    expect(await api.fetchSessionQuizzes('s1')).toEqual([]);
  });

  it('generateAiQuiz returns the quiz, typed 402s, and generic failures', async () => {
    const quiz = { questions: [{ q: { en: 'Q?' }, options: { en: ['a', 'b'] }, answer: 0 }], cost: 1 };
    const mock = stubFetch(
      okJson(quiz),
      okJson({ required: 2, available: 1 }, 402),
      brokenBody(402),
      okJson('no', 500),
      new Error('net'),
    );
    expect(await api.generateAiQuiz('history', 5, 'en', ['en', 'it'])).toEqual({ ok: true, quiz });
    expect(JSON.parse((mock.mock.calls[0][1] as RequestInit).body as string)).toEqual({
      prompt: 'history',
      count: 5,
      lang: 'en',
      langs: ['en', 'it'],
    });
    expect(await api.generateAiQuiz('p', 1, 'en', [])).toEqual({
      ok: false,
      reason: 'insufficient_credits',
      required: 2,
      available: 1,
    });
    expect(await api.generateAiQuiz('p', 1, 'en', [])).toEqual({
      ok: false,
      reason: 'insufficient_credits',
    });
    expect(await api.generateAiQuiz('p', 1, 'en', [])).toEqual({ ok: false, reason: 'failed' });
    expect(await api.generateAiQuiz('p', 1, 'en', [])).toEqual({ ok: false, reason: 'failed' });
  });
});

// ---- AI report -----------------------------------------------------------------------

describe('AI report', () => {
  const report = { format: 'exec', lang: 'en', markdown: '# R', model: 'm', cost: 1 };

  it('fetchLatestReport returns the report; null on 404 / network', async () => {
    stubFetch(okJson(report), okJson('no', 404), new Error('net'));
    expect(await api.fetchLatestReport('s1')).toEqual(report);
    expect(await api.fetchLatestReport('s1')).toBeNull();
    expect(await api.fetchLatestReport('s1')).toBeNull();
  });

  it('generateReport nulls empty guidelines and handles sync 200 / error text / network', async () => {
    const mock = stubFetch(okJson(report), textRes('transcript too short', 422), new Error('net'));
    expect(await api.generateReport('s1', { format: 'exec', lang: 'en', guidelines: '   ' })).toEqual(
      { report, insufficient: null, error: '' },
    );
    expect(JSON.parse((mock.mock.calls[0][1] as RequestInit).body as string)).toEqual({
      format: 'exec',
      lang: 'en',
      guidelines: null,
    });
    expect(
      await api.generateReport('s1', { format: 'exec', lang: 'en', guidelines: 'be brief' }),
    ).toEqual({ report: null, insufficient: null, error: 'transcript too short' });
    expect(JSON.parse((mock.mock.calls[1][1] as RequestInit).body as string).guidelines).toBe(
      'be brief',
    );
    expect(
      await api.generateReport('s1', { format: 'exec', lang: 'en', guidelines: '' }),
    ).toEqual({ report: null, insufficient: null, error: '' });
  });

  it('generateReport surfaces the standard 402 body', async () => {
    const body = { error: 'insufficient_credits', required: 5, available: 2, feature: 'report' };
    stubFetch(okJson(body, 402));
    expect(await api.generateReport('s1', { format: 'exec', lang: 'en', guidelines: '' })).toEqual({
      report: null,
      insufficient: body,
      error: '',
    });
  });

  it('generateReport polls a 202 job through 404s, pending, and network blips to done', async () => {
    vi.useFakeTimers();
    stubFetch(
      okJson({ job_id: 'j1' }, 202),
      okJson('not yet visible', 404), // ignored — keeps polling
      okJson({ status: 'pending' }), // not terminal — keeps polling (backoff grows)
      new Error('net blip'), // ignored — keeps polling
      okJson({ status: 'done', result: report }),
    );
    const p = api.generateReport('s1', { format: 'exec', lang: 'en', guidelines: '' });
    await vi.advanceTimersByTimeAsync(1500 + 2000 + 2500 + 3000);
    expect(await p).toEqual({ report, insufficient: null, error: '' });
  });

  it('generateReport maps job failures to insufficient / generic errors', async () => {
    vi.useFakeTimers();
    const body = { error: 'insufficient_credits', required: 5, available: 2, feature: 'report' };
    stubFetch(
      okJson({ job_id: 'j1' }, 202),
      okJson({ status: 'failed', error: 'insufficient_credits', result: body }),
    );
    let p = api.generateReport('s1', { format: 'exec', lang: 'en', guidelines: '' });
    await vi.advanceTimersByTimeAsync(1500);
    expect(await p).toEqual({ report: null, insufficient: body, error: '' });

    // A malformed 402 payload narrows to null (asInsufficient guard).
    stubFetch(
      okJson({ job_id: 'j2' }, 202),
      okJson({ status: 'failed', error: 'insufficient_credits', result: 'nope' }),
    );
    p = api.generateReport('s1', { format: 'exec', lang: 'en', guidelines: '' });
    await vi.advanceTimersByTimeAsync(1500);
    expect(await p).toEqual({ report: null, insufficient: null, error: '' });

    stubFetch(okJson({ job_id: 'j3' }, 202), okJson({ status: 'failed', error: 'llm_error' }));
    p = api.generateReport('s1', { format: 'exec', lang: 'en', guidelines: '' });
    await vi.advanceTimersByTimeAsync(1500);
    expect(await p).toEqual({ report: null, insufficient: null, error: '' });
  });
});

// ---- Sentiment -------------------------------------------------------------------------

describe('sentiment', () => {
  const sentiment = {
    result: { overall: { score: 0.5, mood: 'positive' }, timeline: [], speakers: [], key_moments: [], window_secs: 60 },
    model: 'm',
    cost: 1,
    cached: false,
  };

  it('fetchSentiment returns the analysis; null on failure', async () => {
    const mock = stubFetch(okJson(sentiment), okJson('no', 404), new Error('net'));
    expect(await api.fetchSentiment('s1')).toEqual(sentiment);
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/sessions/s1/sentiment`);
    expect(await api.fetchSentiment('s1')).toBeNull();
    expect(await api.fetchSentiment('s1')).toBeNull();
  });

  it('generateSentiment handles 200, malformed 402, error text, and network', async () => {
    stubFetch(
      okJson(sentiment),
      okJson({ error: 'something_else' }, 402), // not the standard shape → null insufficient
      textRes('too short', 422),
      new Error('net'),
    );
    expect(await api.generateSentiment('s1')).toEqual({
      sentiment,
      insufficient: null,
      error: '',
    });
    expect(await api.generateSentiment('s1')).toEqual({
      sentiment: null,
      insufficient: null,
      error: '',
    });
    expect(await api.generateSentiment('s1')).toEqual({
      sentiment: null,
      insufficient: null,
      error: 'too short',
    });
    expect(await api.generateSentiment('s1')).toEqual({
      sentiment: null,
      insufficient: null,
      error: '',
    });
  });

  it('generateSentiment surfaces a valid 402 and job-level insufficient credits', async () => {
    vi.useFakeTimers();
    const body = { error: 'insufficient_credits', required: 3, available: 0, feature: 'sentiment' };
    stubFetch(okJson(body, 402));
    expect(await api.generateSentiment('s1')).toEqual({
      sentiment: null,
      insufficient: body,
      error: '',
    });

    stubFetch(
      okJson({ job_id: 'j1' }, 202),
      okJson({ status: 'failed', error: 'insufficient_credits', result: body }),
    );
    const p = api.generateSentiment('s1');
    await vi.advanceTimersByTimeAsync(1500);
    expect(await p).toEqual({ sentiment: null, insufficient: body, error: '' });
  });

  it('generateSentiment polls a 202 job to done, and times out to a generic failure', async () => {
    vi.useFakeTimers();
    stubFetch(okJson({ job_id: 'j1' }, 202), okJson({ status: 'done', result: sentiment }));
    let p = api.generateSentiment('s1');
    await vi.advanceTimersByTimeAsync(1500);
    expect(await p).toEqual({ sentiment, insufficient: null, error: '' });

    // A job that never terminates → timeout after the 10-minute deadline.
    const mock = vi.fn().mockResolvedValue(okJson({ status: 'pending' }));
    mock.mockResolvedValueOnce(okJson({ job_id: 'j2' }, 202));
    vi.stubGlobal('fetch', mock);
    p = api.generateSentiment('s1');
    await vi.advanceTimersByTimeAsync(10 * 60 * 1000 + 15_000);
    expect(await p).toEqual({ sentiment: null, insufficient: null, error: '' });

    // Job failure without an error code → generic failure too.
    stubFetch(okJson({ job_id: 'j3' }, 202), okJson({ status: 'failed', error: 'llm_error' }));
    p = api.generateSentiment('s1');
    await vi.advanceTimersByTimeAsync(1500);
    expect(await p).toEqual({ sentiment: null, insufficient: null, error: '' });
  });
});

// ---- Follow-up email ----------------------------------------------------------------------

describe('follow-up email', () => {
  const email = {
    id: 'e1',
    status: 'draft',
    subject: 'S',
    body_text: 'B',
    recipients: [{ kind: 'participant', name: 'Al', cc: false }],
  };
  const draftOpts = {
    recipients: [{ kind: 'participant', peer_id: 'p1' } as const],
    tone: 'formal',
    guidelines: '  ',
    lang: 'en',
    includeSummary: true,
  };

  it('fetchLatestEmail returns the draft; null on failure', async () => {
    const mock = stubFetch(okJson(email), okJson('no', 404), new Error('net'));
    expect(await api.fetchLatestEmail('s1')).toEqual(email);
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/sessions/s1/email`);
    expect(await api.fetchLatestEmail('s1')).toBeNull();
    expect(await api.fetchLatestEmail('s1')).toBeNull();
  });

  it('generateEmailDraft maps opts to the wire shape and handles sync outcomes', async () => {
    const body402 = { error: 'insufficient_credits', required: 1, available: 0, feature: 'email' };
    const mock = stubFetch(
      okJson(email),
      okJson(body402, 402),
      textRes('no recipients', 400),
      new Error('net'),
    );
    expect(await api.generateEmailDraft('s1', draftOpts)).toEqual({
      email,
      insufficient: null,
      error: '',
    });
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/sessions/s1/email-draft`);
    expect(JSON.parse((mock.mock.calls[0][1] as RequestInit).body as string)).toEqual({
      recipients: [{ kind: 'participant', peer_id: 'p1' }],
      tone: 'formal',
      guidelines: null, // trimmed-empty → null
      lang: 'en',
      include_summary: true,
    });
    expect(await api.generateEmailDraft('s1', draftOpts)).toEqual({
      email: null,
      insufficient: body402,
      error: '',
    });
    expect(await api.generateEmailDraft('s1', draftOpts)).toEqual({
      email: null,
      insufficient: null,
      error: 'no recipients',
    });
    expect(await api.generateEmailDraft('s1', draftOpts)).toEqual({
      email: null,
      insufficient: null,
      error: '',
    });
  });

  it('generateEmailDraft polls a 202 job: done, insufficient, and generic failure', async () => {
    vi.useFakeTimers();
    stubFetch(okJson({ job_id: 'j1' }, 202), okJson({ status: 'done', result: email }));
    let p = api.generateEmailDraft('s1', draftOpts);
    await vi.advanceTimersByTimeAsync(1500);
    expect(await p).toEqual({ email, insufficient: null, error: '' });

    const body402 = { error: 'insufficient_credits', required: 1, available: 0, feature: 'email' };
    stubFetch(
      okJson({ job_id: 'j2' }, 202),
      okJson({ status: 'failed', error: 'insufficient_credits', result: body402 }),
    );
    p = api.generateEmailDraft('s1', draftOpts);
    await vi.advanceTimersByTimeAsync(1500);
    expect(await p).toEqual({ email: null, insufficient: body402, error: '' });

    stubFetch(okJson({ job_id: 'j3' }, 202), okJson({ status: 'failed', error: 'llm_error' }));
    p = api.generateEmailDraft('s1', draftOpts);
    await vi.advanceTimersByTimeAsync(1500);
    expect(await p).toEqual({ email: null, insufficient: null, error: '' });
  });

  it('sendEmail sends edits and surfaces server / network errors', async () => {
    const sent = { id: 'e1', status: 'sent', resend_id: 'r1', sent_at: 'now' };
    const mock = stubFetch(okJson(sent), textRes('already sent', 409), new Error('net'));
    expect(await api.sendEmail('s1', 'e1', { subject: 'Edited' })).toEqual({ sent, error: '' });
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/sessions/s1/email-send`);
    expect(JSON.parse((mock.mock.calls[0][1] as RequestInit).body as string)).toEqual({
      email_id: 'e1',
      subject: 'Edited',
    });
    expect(await api.sendEmail('s1', 'e1')).toEqual({ sent: null, error: 'already sent' });
    expect(await api.sendEmail('s1', 'e1')).toEqual({ sent: null, error: '' });
  });
});

// ---- Shared 402 parsing --------------------------------------------------------------------

describe('parseInsufficient', () => {
  it('parses only a standard 402 body', async () => {
    const body = { error: 'insufficient_credits', required: 2, available: 1, feature: 'report' };
    expect(await api.parseInsufficient(okJson(body, 402))).toEqual(body);
    expect(await api.parseInsufficient(okJson(body, 200))).toBeNull(); // not a 402
    expect(await api.parseInsufficient(okJson({ error: 'other' }, 402))).toBeNull();
    expect(await api.parseInsufficient(brokenBody(402))).toBeNull(); // unreadable body
  });
});

// ---- Bug report + invites --------------------------------------------------------------------

describe('bug report + invites', () => {
  it('postBugReport POSTs message + page url; false on failure', async () => {
    const mock = stubFetch(okJson({}, 201), okJson('no', 500), new Error('net'));
    expect(await api.postBugReport('it broke', 'https://app/call')).toBe(true);
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/bug-report`);
    expect(JSON.parse((mock.mock.calls[0][1] as RequestInit).body as string)).toEqual({
      message: 'it broke',
      page_url: 'https://app/call',
    });
    expect(await api.postBugReport('x', 'y')).toBe(false);
    expect(await api.postBugReport('x', 'y')).toBe(false);
  });

  it('sendInvites returns counts, defaults, and typed failures', async () => {
    const mock = stubFetch(
      okJson({ sent: 2, failed: 1 }),
      okJson({}), // counts default to 0
      textRes('invalid email', 400),
      brokenBody(500), // unreadable error text → status fallback
      new Error('net'),
    );
    expect(await api.sendInvites('my room', ['a@b.c', 'd@e.f'], 'it')).toEqual({
      sent: 2,
      failed: 1,
    });
    expect(mock.mock.calls[0][0]).toBe(`${BASE}/api/rooms/my%20room/invite`);
    expect(JSON.parse((mock.mock.calls[0][1] as RequestInit).body as string)).toEqual({
      emails: ['a@b.c', 'd@e.f'],
      lang: 'it',
    });
    expect(await api.sendInvites('r', ['a@b.c'], 'en')).toEqual({ sent: 0, failed: 0 });
    expect(await api.sendInvites('r', ['a@b.c'], 'en')).toEqual({
      sent: 0,
      failed: 1,
      error: 'invalid email',
    });
    expect(await api.sendInvites('r', ['a@b.c'], 'en')).toEqual({
      sent: 0,
      failed: 1,
      error: 'error 500',
    });
    expect(await api.sendInvites('r', ['a@b.c', 'x@y.z'], 'en')).toEqual({
      sent: 0,
      failed: 2,
      error: 'network',
    });
  });
});
