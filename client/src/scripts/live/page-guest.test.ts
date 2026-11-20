// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import en from '../i18n/en.json';

const EN = en as Record<string, string>;
const CODE = 'S3CR3T-INVITE-CODE';

function pageMarkup(): string {
  const src = readFileSync(resolve(process.cwd(), 'src/pages/lc/[code].astro'), 'utf8');
  const main = /<main[\s\S]*?<\/main>/.exec(src)?.[0] ?? '';
  return main
    .replace('data-code={code}', `data-code="${CODE}"`)
    .replace(/=\{T\('([A-Za-z]+)'\)\}/g, (_m, k: string) => `="${EN[k]}"`)
    .replace(/\{T\('([A-Za-z]+)'\)\}/g, (_m, k: string) => EN[k]);
}

const api = { joinSession: vi.fn(), getSession: vi.fn() };
vi.mock('./api', () => ({
  joinSession: (...a: unknown[]) => api.joinSession(...a),
  getSession: (...a: unknown[]) => api.getSession(...a),
}));
let supported = true;
vi.mock('./conversation', () => ({ browserSupported: () => supported }));
const unlock = vi.fn();
vi.mock('../pcm-playback', () => ({ pcmPlayback: { unlock: () => unlock() } }));
const rooms: Array<{ role: string; session: Record<string, unknown> }> = [];
vi.mock('./room', () => ({
  mountRoom: (o: { role: string; session: Record<string, unknown> }) => {
    rooms.push(o);
    return { els: {}, destroy: vi.fn() };
  },
}));

import { mountLiveGuest } from './page-guest';
import { STORAGE_KEY } from './session-store';

const JOINED = { session_id: 's1', peer_id: 'peer-g', token: 'LIVE', ws_path: '/ws/live', host: { name: 'Anna', lang: 'it' } };
const $ = (id: string) => document.getElementById(id) as HTMLElement;
const flush = () => new Promise((r) => setTimeout(r, 0));
const consoleSpies = (['log', 'info', 'warn', 'error', 'debug'] as const).map((m) => vi.spyOn(console, m).mockImplementation(() => {}));

function setNavigatorLanguage(l: string) {
  Object.defineProperty(navigator, 'language', { value: l, configurable: true });
  Object.defineProperty(navigator, 'languages', { value: [l], configurable: true });
}

beforeEach(() => {
  document.head.innerHTML = '';
  document.body.innerHTML = pageMarkup();
  sessionStorage.clear();
  rooms.length = 0;
  supported = true;
  api.joinSession.mockReset();
  api.getSession.mockReset();
  unlock.mockReset();
  consoleSpies.forEach((s) => s.mockClear());
  setNavigatorLanguage('en-US');
});
afterEach(() => vi.clearAllTimers());

describe('join form', () => {
  it('defaults the language from the browser (regional tag mapped to the base language), name empty, consent unticked', async () => {
    setNavigatorLanguage('it-IT');
    await mountLiveGuest();
    expect($('lg-lang').textContent).toContain('Italiano');
    expect(($('lg-name') as HTMLInputElement).value).toBe('');
    expect(($('lg-consent') as HTMLInputElement).checked).toBe(false);
    expect($('lg-session').hidden).toBe(true);
  });

  it('shows the privacy notice next to the consent checkbox and makes the checkbox reference it', async () => {
    await mountLiveGuest();
    expect($('lg-notice').textContent).toBe(EN.liveJoinNotice);
    expect($('lg-consent').getAttribute('aria-describedby')).toBe('lg-notice');
  });

  it('refuses to join without consent, tells the guest why, and sends nothing', async () => {
    await mountLiveGuest();
    $('lg-join-btn').click();
    await flush();
    expect(api.joinSession).not.toHaveBeenCalled();
    expect($('lg-status').textContent).toBe(EN.liveConsentRequired);
    expect($('lg-status').hidden).toBe(false);
  });

  it('joins with the code from the route, the chosen language, a default name, and consent', async () => {
    api.joinSession.mockResolvedValue({ ok: true, data: JOINED });
    await mountLiveGuest();
    ($('lg-consent') as HTMLInputElement).checked = true;
    $('lg-join-btn').click();
    expect(unlock).toHaveBeenCalledTimes(1);
    await flush();
    expect(api.joinSession).toHaveBeenCalledWith({ code: CODE, name: 'Guest', lang: 'en', consent: true });
  });

  it('sends the typed name, trimmed', async () => {
    api.joinSession.mockResolvedValue({ ok: true, data: JOINED });
    await mountLiveGuest();
    ($('lg-name') as HTMLInputElement).value = '  Marco  ';
    ($('lg-consent') as HTMLInputElement).checked = true;
    $('lg-join-btn').click();
    await flush();
    expect(api.joinSession.mock.calls[0][0].name).toBe('Marco');
  });

  it('on success hides the form, starts the conversation as a guest and stores the live token (never the code)', async () => {
    api.joinSession.mockResolvedValue({ ok: true, data: JOINED });
    await mountLiveGuest();
    ($('lg-consent') as HTMLInputElement).checked = true;
    $('lg-join-btn').click();
    await flush();
    expect($('lg-join').hidden).toBe(true);
    expect($('lg-hero').hidden).toBe(true);
    expect($('lg-session').hidden).toBe(false);
    expect(rooms[0].role).toBe('guest');
    expect(rooms[0].session).toEqual({ sessionId: 's1', token: 'LIVE', role: 'guest', myLang: 'en', myPeerId: 'peer-g', wsPath: '/ws/live' });
    expect(sessionStorage.getItem(STORAGE_KEY)).not.toContain(CODE);
  });

  it('ignores a double tap while joining', async () => {
    let finish: (v: unknown) => void = () => {};
    api.joinSession.mockReturnValue(new Promise((r) => (finish = r)));
    await mountLiveGuest();
    ($('lg-consent') as HTMLInputElement).checked = true;
    $('lg-join-btn').click();
    $('lg-join-btn').click();
    expect(api.joinSession).toHaveBeenCalledTimes(1);
    expect(($('lg-join-btn') as HTMLButtonElement).disabled).toBe(true);
    expect($('lg-join-btn').textContent).toBe('Joining…');
    finish({ ok: false, status: 429, code: 'rate_limited' });
    await flush();
    expect(($('lg-join-btn') as HTMLButtonElement).disabled).toBe(false);
    expect($('lg-join-btn').textContent).toBe('Join');
  });

  it('says the browser is unsupported and does not offer to join', async () => {
    supported = false;
    await mountLiveGuest();
    expect($('lg-status').textContent).toBe(EN.talkErrorUnsupported);
    expect(($('lg-join-btn') as HTMLButtonElement).disabled).toBe(true);
  });
});

describe('join errors: every code gets its own message and a sensible action', () => {
  const cases: Array<[string, string, 'retry' | 'home' | null]> = [
    ['invite_not_found', 'liveErrInviteNotFound', 'home'],
    ['invite_expired', 'liveErrInviteExpired', 'home'],
    ['session_full', 'liveErrSessionFull', 'home'],
    ['session_ended', 'liveErrSessionEnded', 'home'],
    ['consent_required', 'liveConsentRequired', null],
    ['bad_lang', 'liveErrBadLang', null],
    ['insufficient_balance', 'liveErrOwnerCredits', 'home'],
    ['live_busy', 'liveErrBusy', 'retry'],
    ['rate_limited', 'liveErrRateLimited', 'retry'],
    ['network', 'liveErrNetworkRequest', 'retry'],
    ['live_disabled', 'liveUnavailable', 'home'],
    ['wrong_session', 'liveErrWrongSession', 'home'],
  ];
  it.each(cases)('%s', async (code, key, action) => {
    api.joinSession.mockResolvedValue({ ok: false, status: 400, code });
    await mountLiveGuest();
    ($('lg-consent') as HTMLInputElement).checked = true;
    $('lg-join-btn').click();
    await flush();
    expect($('lg-status').textContent).toBe(EN[key]);
    expect($('lg-retry').hidden).toBe(action !== 'retry');
    expect($('lg-home').hidden).toBe(action !== 'home');
    expect($('lg-actions').hidden).toBe(action === null);
    expect($('lg-session').hidden).toBe(true);
  });

  it('retry joins again with the same inputs', async () => {
    api.joinSession.mockResolvedValueOnce({ ok: false, status: 503, code: 'live_busy' });
    api.joinSession.mockResolvedValueOnce({ ok: true, data: JOINED });
    await mountLiveGuest();
    ($('lg-consent') as HTMLInputElement).checked = true;
    $('lg-join-btn').click();
    await flush();
    $('lg-retry').click();
    await flush();
    expect(api.joinSession).toHaveBeenCalledTimes(2);
    expect(rooms).toHaveLength(1);
    expect($('lg-status').hidden).toBe(true);
  });
});

describe('reload', () => {
  const STORED = { sessionId: 's9', token: 'T9', role: 'guest', myLang: 'es', myPeerId: 'pg', wsPath: '/ws/live' };

  it('rejoins a stored, still-open guest session without asking for the invite again', async () => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify(STORED));
    api.getSession.mockResolvedValue({ ok: true, data: { session: { status: 'active' }, you: { role: 'guest' } } });
    await mountLiveGuest();
    expect(api.getSession).toHaveBeenCalledWith('s9', 'T9');
    expect($('lg-join').hidden).toBe(true);
    expect(rooms[0].session).toMatchObject({ sessionId: 's9', role: 'guest', myLang: 'es' });
  });

  it('falls back to the join form when the stored session is over', async () => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify(STORED));
    api.getSession.mockResolvedValue({ ok: false, status: 410, code: 'session_ended' });
    await mountLiveGuest();
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
    expect($('lg-join').hidden).toBe(false);
    expect(rooms).toHaveLength(0);
  });

  it('ignores a stored HOST session on the guest page', async () => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify({ ...STORED, role: 'host' }));
    await mountLiveGuest();
    expect(api.getSession).not.toHaveBeenCalled();
    expect($('lg-join').hidden).toBe(false);
  });
});

describe('the invitation code is a secret', () => {
  it('is never logged, whatever happens', async () => {
    api.joinSession.mockResolvedValueOnce({ ok: false, status: 410, code: 'invite_expired' });
    api.joinSession.mockResolvedValueOnce({ ok: true, data: JOINED });
    await mountLiveGuest();
    ($('lg-consent') as HTMLInputElement).checked = true;
    $('lg-join-btn').click();
    await flush();
    $('lg-join-btn').click();
    await flush();
    for (const spy of consoleSpies) expect(spy).not.toHaveBeenCalled();
  });

  it('is sent only to the join call and is absent from storage', async () => {
    api.joinSession.mockResolvedValue({ ok: true, data: JOINED });
    await mountLiveGuest();
    ($('lg-consent') as HTMLInputElement).checked = true;
    $('lg-join-btn').click();
    await flush();
    expect(JSON.stringify(api.joinSession.mock.calls)).toContain(CODE);
    expect(JSON.stringify(Object.entries(sessionStorage))).not.toContain(CODE);
    expect(JSON.stringify(Object.entries(localStorage))).not.toContain(CODE);
    expect(JSON.stringify(rooms)).not.toContain(CODE);
  });
});
