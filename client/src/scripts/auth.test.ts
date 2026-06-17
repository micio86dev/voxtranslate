import { describe, it, expect, vi, beforeEach } from 'vitest';

// auth.ts reads `location` + `localStorage` at module load. Provide node stubs
// that persist across `vi.resetModules()` (the backing map lives in this file,
// which isn't reset), so we can test rehydration from storage.
const backing = new Map<string, string>();
vi.stubGlobal('location', { protocol: 'http:', host: 'localhost:4321' });
vi.stubGlobal('localStorage', {
  getItem: (k: string) => (backing.has(k) ? backing.get(k)! : null),
  setItem: (k: string, v: string) => void backing.set(k, String(v)),
  removeItem: (k: string) => void backing.delete(k),
  clear: () => backing.clear(),
});

// Fresh module per test so the cached token/user/billing state resets.
async function fresh() {
  vi.resetModules();
  return import('./auth');
}

function okJson(body: unknown, status = 200) {
  return { ok: status >= 200 && status < 300, status, json: async () => body } as Response;
}

beforeEach(() => {
  backing.clear();
});

describe('session', () => {
  it('saves, reads, and clears the session (persisted)', async () => {
    const auth = await fresh();
    expect(auth.isLoggedIn()).toBe(false);
    const user = { id: 'u1', email: 'a@b.com', name: 'Al', balance: 2 };
    auth.saveSession('tok', user);
    expect(auth.isLoggedIn()).toBe(true);
    expect(auth.getToken()).toBe('tok');
    expect(auth.getUser()?.name).toBe('Al');

    // A new module instance rehydrates from storage.
    const auth2 = await fresh();
    expect(auth2.getToken()).toBe('tok');
    expect(auth2.getUser()?.balance).toBe(2);

    auth2.clearSession();
    expect(auth2.isLoggedIn()).toBe(false);
    expect(localStorage.getItem('vox.token')).toBeNull();
  });

  it('patches the cached balance', async () => {
    const auth = await fresh();
    auth.saveSession('t', { id: 'u', email: 'e', name: 'n', balance: 5 });
    auth.setBalance(3.5);
    expect(auth.getUser()?.balance).toBe(3.5);
    // setBalance with no user is a no-op.
    auth.clearSession();
    auth.setBalance(9);
    expect(auth.getUser()).toBeNull();
  });

  it('ignores corrupt stored user', async () => {
    backing.set('vox.token', 't');
    backing.set('vox.user', '{not json');
    const auth = await fresh();
    expect(auth.getUser()).toBeNull();
  });
});

describe('headers + ws url', () => {
  it('attaches the bearer token only when logged in', async () => {
    const auth = await fresh();
    expect(auth.authHeaders()).toEqual({});
    let url = auth.buildWsUrl(new URLSearchParams({ room: 'r', lang: 'en' }));
    expect(url).not.toContain('token=');

    auth.saveSession('abc', { id: 'u', email: 'e', name: 'n', balance: 1 });
    expect(auth.authHeaders()).toEqual({ Authorization: 'Bearer abc' });
    url = auth.buildWsUrl(new URLSearchParams({ room: 'r', lang: 'en' }));
    expect(url).toContain('token=abc');
  });
});

describe('billingEnabled', () => {
  it('returns true on 200, captures the client id, and caches', async () => {
    const auth = await fresh();
    const fetchMock = vi.fn().mockResolvedValue(okJson({ google_client_id: 'gid.apps' }, 200));
    vi.stubGlobal('fetch', fetchMock);
    expect(await auth.billingEnabled()).toBe(true);
    expect(auth.getGoogleClientId()).toBe('gid.apps');
    expect(await auth.billingEnabled()).toBe(true);
    expect(fetchMock).toHaveBeenCalledTimes(1); // cached
  });

  it('returns false on 503', async () => {
    const auth = await fresh();
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('off', 503)));
    expect(await auth.billingEnabled()).toBe(false);
  });

  it('returns false when the probe throws', async () => {
    const auth = await fresh();
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new Error('net')));
    expect(await auth.billingEnabled()).toBe(false);
  });
});

describe('login + me', () => {
  it('exchanges a credential for a session', async () => {
    const auth = await fresh();
    const user = { id: 'u1', email: 'a@b.com', name: 'Al', balance: 2 };
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson({ token: 'jwt', user })));
    const u = await auth.loginWithGoogle('cred');
    expect(u.name).toBe('Al');
    expect(auth.getToken()).toBe('jwt');
  });

  it('throws on login failure', async () => {
    const auth = await fresh();
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('bad', 401)));
    await expect(auth.loginWithGoogle('bad')).rejects.toThrow();
  });

  it('refreshMe updates balance, and clears on 401', async () => {
    const auth = await fresh();
    auth.saveSession('t', { id: 'u', email: 'e', name: 'n', balance: 1 });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson({ id: 'u', email: 'e', name: 'n', balance: 7 })));
    const u = await auth.refreshMe();
    expect(u?.balance).toBe(7);

    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('no', 401)));
    expect(await auth.refreshMe()).toBeNull();
    expect(auth.isLoggedIn()).toBe(false);

    // refreshMe with no token returns null without fetching.
    expect(await auth.refreshMe()).toBeNull();
  });
});

describe('billing data + checkout', () => {
  it('fetches packages, history, usage', async () => {
    const auth = await fresh();
    auth.saveSession('t', { id: 'u', email: 'e', name: 'n', balance: 1 });
    vi.stubGlobal(
      'fetch',
      vi.fn()
        .mockResolvedValueOnce(okJson([{ id: 'starter', name: 'S', price_usd: 5, credits_usd: 5 }]))
        .mockResolvedValueOnce(okJson([{ amount: 2, kind: 'free_credit', balance_after: 2, created_at: 'x' }]))
        .mockResolvedValueOnce(okJson([{ room: 'r', speaking_seconds: 9, cost: 0.1, started_at: 'x' }])),
    );
    expect((await auth.fetchPackages())[0].id).toBe('starter');
    expect((await auth.fetchHistory())[0].kind).toBe('free_credit');
    expect((await auth.fetchUsage())[0].room).toBe('r');
  });

  it('returns [] on non-ok billing endpoints', async () => {
    const auth = await fresh();
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('no', 500)));
    expect(await auth.fetchPackages()).toEqual([]);
    expect(await auth.fetchHistory()).toEqual([]);
    expect(await auth.fetchUsage()).toEqual([]);
  });

  it('startCheckout returns the redirect url, throws on failure', async () => {
    const auth = await fresh();
    auth.saveSession('t', { id: 'u', email: 'e', name: 'n', balance: 1 });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson({ url: 'https://stripe/x' })));
    expect(await auth.startCheckout('starter')).toBe('https://stripe/x');

    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('no', 400)));
    await expect(auth.startCheckout('bad')).rejects.toThrow();
  });
});

describe('safety + gdpr', () => {
  it('submitConsent posts and flips consent locally on success', async () => {
    const auth = await fresh();
    auth.saveSession('t', { id: 'u', email: 'e', name: 'n', balance: 1, consent_given: false });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson({ consent_given: true })));
    expect(auth.consentGiven()).toBe(false);
    expect(await auth.submitConsent(true)).toBe(true);
    expect(auth.consentGiven()).toBe(true);
    expect(auth.getUser()?.consent_given).toBe(true);
  });

  it('submitConsent returns false on rejection', async () => {
    const auth = await fresh();
    auth.saveSession('t', { id: 'u', email: 'e', name: 'n', balance: 1 });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('no', 403)));
    expect(await auth.submitConsent(false)).toBe(false);
    expect(auth.consentGiven()).toBe(false);
  });

  it('reportUser posts and reports ok/ko', async () => {
    const auth = await fresh();
    auth.saveSession('t', { id: 'u', email: 'e', name: 'n', balance: 1 });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('reported', 201)));
    expect(await auth.reportUser({ room: 'r', reason: 'harassment' })).toBe(true);
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('no', 500)));
    expect(await auth.reportUser({ room: 'r', reason: 'x' })).toBe(false);
  });

  it('exportData returns the document or null', async () => {
    const auth = await fresh();
    auth.saveSession('t', { id: 'u', email: 'e', name: 'n', balance: 1 });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson({ profile: { email: 'e' } })));
    expect((await auth.exportData() as any).profile.email).toBe('e');
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('no', 500)));
    expect(await auth.exportData()).toBeNull();
  });

  it('deleteAccount clears the session on success', async () => {
    const auth = await fresh();
    auth.saveSession('t', { id: 'u', email: 'e', name: 'n', balance: 1 });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('deleted', 200)));
    expect(await auth.deleteAccount()).toBe(true);
    expect(auth.isLoggedIn()).toBe(false);
  });
});

describe('transcripts', () => {
  // downloadBlob touches `document` + `URL` object-url APIs — stub both and
  // hand back the anchor so tests can assert href/download/click.
  function stubDownloadDom() {
    const anchor = { href: '', download: '', click: vi.fn() };
    vi.stubGlobal('document', { createElement: vi.fn(() => anchor) });
    // Subclass so `new URL(...)` keeps working (vite-node's module loader
    // needs it) while the object-url statics become assertable mocks.
    class StubURL extends URL {
      static override createObjectURL = vi.fn(() => 'blob:mock');
      static override revokeObjectURL = vi.fn();
    }
    vi.stubGlobal('URL', StubURL);
    return anchor;
  }

  function blobResponse(contentDisposition: string | null) {
    return {
      ok: true,
      status: 200,
      blob: async () => new Blob(['x']),
      headers: { get: (k: string) => (k === 'content-disposition' ? contentDisposition : null) },
    } as unknown as Response;
  }

  it('fetchSessions lists recorded calls with the auth header, [] on failure', async () => {
    const auth = await fresh();
    auth.saveSession('t', { id: 'u', email: 'e', name: 'n', balance: 1 });
    const fetchMock = vi
      .fn()
      .mockResolvedValue(okJson([{ id: 's1', room: 'r', started_at: 'x', event_count: 3 }]));
    vi.stubGlobal('fetch', fetchMock);
    expect((await auth.fetchSessions())[0].event_count).toBe(3);
    expect(fetchMock.mock.calls[0][0]).toContain('/api/sessions');
    expect(fetchMock.mock.calls[0][1].headers).toEqual({ Authorization: 'Bearer t' });

    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('no', 401)));
    expect(await auth.fetchSessions()).toEqual([]);
  });

  it('downloadBlob clicks a temporary object-url anchor and revokes it', async () => {
    const auth = await fresh();
    const anchor = stubDownloadDom();
    auth.downloadBlob(new Blob(['x']), 'f.json');
    expect(anchor.href).toBe('blob:mock');
    expect(anchor.download).toBe('f.json');
    expect(anchor.click).toHaveBeenCalledOnce();
    expect((URL.revokeObjectURL as ReturnType<typeof vi.fn>)).toHaveBeenCalledWith('blob:mock');
  });

  it('downloadTranscript(pdf) sends tz + lang and uses the server filename', async () => {
    const auth = await fresh();
    auth.saveSession('t', { id: 'u', email: 'e', name: 'n', balance: 1 });
    // Deterministic browser timezone (keep the rest of Intl intact).
    vi.stubGlobal('Intl', {
      ...Intl,
      DateTimeFormat: function () {
        return { resolvedOptions: () => ({ timeZone: 'Europe/Rome' }) };
      },
    });
    const anchor = stubDownloadDom();
    const fetchMock = vi
      .fn()
      .mockResolvedValue(blobResponse('attachment; filename="voxtranslate-room-abc12345.pdf"'));
    vi.stubGlobal('fetch', fetchMock);

    expect((await auth.downloadTranscript('s1', 'pdf', 'it')).ok).toBe(true);
    const url = fetchMock.mock.calls[0][0] as string;
    expect(url).toContain('/api/sessions/s1/transcript.pdf');
    expect(url).toContain('tz=Europe%2FRome');
    expect(url).toContain('lang=it');
    expect(fetchMock.mock.calls[0][1].headers).toEqual({ Authorization: 'Bearer t' });
    expect(anchor.download).toBe('voxtranslate-room-abc12345.pdf');
  });

  it('downloadTranscript(json) has no query and falls back to a default name', async () => {
    const auth = await fresh();
    const anchor = stubDownloadDom();
    const fetchMock = vi.fn().mockResolvedValue(blobResponse(null));
    vi.stubGlobal('fetch', fetchMock);

    expect((await auth.downloadTranscript('s2', 'json')).ok).toBe(true);
    const url = fetchMock.mock.calls[0][0] as string;
    expect(url).toContain('/api/sessions/s2/transcript.json');
    expect(url).not.toContain('?');
    expect(anchor.download).toBe('voxtranslate-transcript.json');
  });

  it('downloadTranscript(srt/vtt) sends the mode and target params', async () => {
    const auth = await fresh();
    stubDownloadDom();
    const fetchMock = vi.fn().mockResolvedValue(blobResponse(null));
    vi.stubGlobal('fetch', fetchMock);

    expect((await auth.downloadTranscript('s4', 'srt', 'it', 'both')).ok).toBe(true);
    expect(fetchMock.mock.calls[0][0]).toContain('/api/sessions/s4/transcript.srt?lang=both&target=it');
    // Default subtitle mode is "translated".
    expect((await auth.downloadTranscript('s5', 'vtt', 'fr')).ok).toBe(true);
    expect(fetchMock.mock.calls[1][0]).toContain('/api/sessions/s5/transcript.vtt?lang=translated&target=fr');
  });

  it('downloadTranscript returns false on failure without downloading', async () => {
    const auth = await fresh();
    const anchor = stubDownloadDom();
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('forbidden', 403)));
    const r = await auth.downloadTranscript('s3', 'json');
    expect(r.ok).toBe(false);
    expect(r.status).toBe(403); // surfaced so callers can special-case 429 (#222)
    expect(anchor.click).not.toHaveBeenCalled();
  });
});

describe('formatters', () => {
  it('formats credits as USD', async () => {
    const auth = await fresh();
    expect(auth.formatCredits(2)).toBe('$2.00');
    expect(auth.formatCredits(0.1)).toBe('$0.10');
    expect(auth.formatCredits(undefined as unknown as number)).toBe('$0.00');
  });

  it('resizes google avatar urls', async () => {
    const auth = await fresh();
    expect(auth.avatarUrl(null)).toBeNull();
    expect(auth.avatarUrl('https://lh3.googleusercontent.com/a/x=s96-c', 48)).toContain('=s48');
    expect(auth.avatarUrl('https://lh3.googleusercontent.com/a/x', 64)).toBe('https://lh3.googleusercontent.com/a/x=s64');
    expect(auth.avatarUrl('https://example.com/p.png', 64)).toBe('https://example.com/p.png');
  });
});

describe('acquisition source', () => {
  it('captures ?source first-touch and sends it on login', async () => {
    vi.stubGlobal('location', { protocol: 'http:', host: 'localhost:4321', search: '?source=reddit' });
    const auth = await fresh();
    auth.captureAcquisitionSource();
    expect(auth.getAcquisitionSource()).toBe('reddit');

    // First touch wins: a later visit with a different source doesn't overwrite.
    vi.stubGlobal('location', { protocol: 'http:', host: 'localhost:4321', search: '?utm_source=twitter' });
    auth.captureAcquisitionSource();
    expect(auth.getAcquisitionSource()).toBe('reddit');

    const fetchMock = vi.fn().mockResolvedValue(okJson({ token: 'jwt', user: { id: 'u', email: 'e', name: 'n', balance: 0 } }));
    vi.stubGlobal('fetch', fetchMock);
    await auth.loginWithGoogle('cred');
    const body = JSON.parse((fetchMock.mock.calls[0][1] as RequestInit).body as string);
    expect(body).toMatchObject({ credential: 'cred', source: 'reddit' });
  });

  it('falls back to utm_source / ref and omits source when absent', async () => {
    vi.stubGlobal('location', { protocol: 'http:', host: 'localhost:4321', search: '?utm_source=newsletter' });
    let auth = await fresh();
    auth.captureAcquisitionSource();
    expect(auth.getAcquisitionSource()).toBe('newsletter');

    backing.clear();
    vi.stubGlobal('location', { protocol: 'http:', host: 'localhost:4321', search: '' });
    auth = await fresh();
    auth.captureAcquisitionSource();
    expect(auth.getAcquisitionSource()).toBeNull();

    const fetchMock = vi.fn().mockResolvedValue(okJson({ token: 'jwt', user: { id: 'u', email: 'e', name: 'n', balance: 0 } }));
    vi.stubGlobal('fetch', fetchMock);
    await auth.loginWithGoogle('cred');
    const body = JSON.parse((fetchMock.mock.calls[0][1] as RequestInit).body as string);
    expect(body.source).toBeUndefined();
  });
});
