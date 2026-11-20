// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import en from '../i18n/en.json';

const EN = en as Record<string, string>;
const HANDLE = 'pub123';
const SEGMENT = `${HANDLE}.SECRETMAC`;

function pageMarkup(): string {
  const src = readFileSync(resolve(process.cwd(), 'src/pages/lc/d/[token].astro'), 'utf8');
  const main = /<main[\s\S]*?<\/main>/.exec(src)?.[0] ?? '';
  return main
    .replace('data-segment={token}', `data-segment="${SEGMENT}"`)
    .replace(/=\{T\('([A-Za-z]+)'\)\}/g, (_m, k: string) => `="${EN[k]}"`)
    .replace(/\{T\('([A-Za-z]+)'\)\}/g, (_m, k: string) => EN[k]);
}

const api = { scanDesk: vi.fn(), getSession: vi.fn() };
vi.mock('./api', () => ({
  scanDesk: (...a: unknown[]) => api.scanDesk(...a),
  getSession: (...a: unknown[]) => api.getSession(...a),
}));
let supported = true;
vi.mock('./conversation', () => ({ browserSupported: () => supported }));
const unlock = vi.fn();
vi.mock('../pcm-playback', () => ({ pcmPlayback: { unlock: () => unlock() } }));
interface RoomCall {
  role: string;
  waitingKey?: string;
  session: Record<string, unknown>;
  onState?: (ctx: unknown) => void;
  destroy: ReturnType<typeof vi.fn>;
}
const rooms: RoomCall[] = [];
vi.mock('./room', () => ({
  mountRoom: (o: Omit<RoomCall, 'destroy'>) => {
    const call: RoomCall = { ...o, destroy: vi.fn() };
    rooms.push(call);
    return { els: {}, destroy: call.destroy };
  },
}));

import { mountLiveDesk } from './page-desk';
import { DESK_KEY, STORAGE_KEY } from './session-store';

const T0 = new Date('2026-11-19T10:00:00Z').getTime();
const SCANNED = {
  session_id: 's1', peer_id: 'peer-g', token: 'LIVE-1', ws_path: '/ws/live',
  desk: { name: 'Front desk', location_label: 'Terminal 2' }, wait_secs: 60,
};
const $ = (id: string) => document.getElementById(id) as HTMLElement;
const flush = () => vi.advanceTimersByTimeAsync(0);
const consoleSpies = (['log', 'info', 'warn', 'error', 'debug'] as const).map((m) => vi.spyOn(console, m).mockImplementation(() => {}));

function setNavigatorLanguage(l: string) {
  Object.defineProperty(navigator, 'language', { value: l, configurable: true });
  Object.defineProperty(navigator, 'languages', { value: [l], configurable: true });
}
const ctx = (phase: string, over: Record<string, unknown> = {}) => ({ phase, partners: [], endReason: null, failure: null, ...over });
const staffPartner = { peerId: 'st1', name: 'Giulia', lang: 'it', muted: false };

async function submit(consent = true) {
  ($('ld-consent') as HTMLInputElement).checked = consent;
  $('ld-join-btn').click();
  await flush();
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(T0);
  document.head.innerHTML = '';
  document.body.innerHTML = pageMarkup();
  sessionStorage.clear();
  rooms.length = 0;
  supported = true;
  api.scanDesk.mockReset();
  api.getSession.mockReset();
  unlock.mockReset();
  consoleSpies.forEach((s) => s.mockClear());
  setNavigatorLanguage('en-US');
});
afterEach(() => vi.useRealTimers());

describe('join screen', () => {
  it('shows the BUSINESS notice (a text transcript is kept), not the personal-invite one, next to the consent box', async () => {
    await mountLiveDesk();
    expect($('ld-notice').textContent).toBe(EN.liveDeskNotice);
    expect($('ld-notice').textContent).toContain('transcript');
    expect($('ld-notice').textContent).not.toBe(EN.liveJoinNotice);
    expect($('ld-consent').getAttribute('aria-describedby')).toBe('ld-notice');
    expect(($('ld-consent') as HTMLInputElement).checked).toBe(false);
  });

  it('defaults the language from the browser, with an empty optional name', async () => {
    setNavigatorLanguage('it-IT');
    await mountLiveDesk();
    expect($('ld-lang').textContent).toContain('Italiano');
    expect(($('ld-name') as HTMLInputElement).value).toBe('');
    expect($('ld-session').hidden).toBe(true);
  });

  it('refuses to scan without consent, says why in words, and sends nothing', async () => {
    await mountLiveDesk();
    await submit(false);
    expect(api.scanDesk).not.toHaveBeenCalled();
    expect($('ld-status').textContent).toBe(EN.liveConsentRequired);
  });

  it('says the browser is unsupported and does not offer to scan', async () => {
    supported = false;
    await mountLiveDesk();
    expect($('ld-status').textContent).toBe(EN.talkErrorUnsupported);
    expect(($('ld-join-btn') as HTMLButtonElement).disabled).toBe(true);
  });
});

describe('scan', () => {
  it('sends the QR segment from the route, the language, a default name and the consent, unlocking audio in the tap', async () => {
    api.scanDesk.mockResolvedValue({ ok: true, data: SCANNED });
    await mountLiveDesk();
    ($('ld-consent') as HTMLInputElement).checked = true;
    $('ld-join-btn').click();
    expect(unlock).toHaveBeenCalledTimes(1);
    await flush();
    expect(api.scanDesk).toHaveBeenCalledWith({ segment: SEGMENT, name: 'Guest', lang: 'en', consent: true });
  });

  it('sends the typed name, trimmed', async () => {
    api.scanDesk.mockResolvedValue({ ok: true, data: SCANNED });
    await mountLiveDesk();
    ($('ld-name') as HTMLInputElement).value = '  Marco ';
    await submit();
    expect(api.scanDesk.mock.calls[0][0].name).toBe('Marco');
  });

  it('opens the conversation as the guest with the live token, waiting for the staff, and stores the session', async () => {
    api.scanDesk.mockResolvedValue({ ok: true, data: SCANNED });
    await mountLiveDesk();
    await submit();
    expect($('ld-join').hidden).toBe(true);
    expect($('ld-hero').hidden).toBe(true);
    expect($('ld-session').hidden).toBe(false);
    expect(rooms).toHaveLength(1);
    expect(rooms[0].role).toBe('guest');
    expect(rooms[0].waitingKey).toBe('liveDeskWaiting');
    expect(rooms[0].session).toEqual({ sessionId: 's1', token: 'LIVE-1', role: 'guest', myLang: 'en', myPeerId: 'peer-g', wsPath: '/ws/live' });
    expect(JSON.parse(sessionStorage.getItem(STORAGE_KEY) ?? '{}').token).toBe('LIVE-1');
  });

  it('shows the desk name and location and a countdown of wait_secs', async () => {
    api.scanDesk.mockResolvedValue({ ok: true, data: SCANNED });
    await mountLiveDesk();
    await submit();
    expect($('ld-wait').hidden).toBe(false);
    expect($('ld-desk-name').textContent).toBe('Front desk');
    expect($('ld-desk-location').textContent).toBe('Terminal 2');
    expect($('ld-desk-location').hidden).toBe(false);
    expect($('ld-countdown').textContent).toBe('A member of staff will join shortly. This request expires in 60 s.');
    await vi.advanceTimersByTimeAsync(5000);
    expect($('ld-countdown').textContent).toContain('expires in 55 s');
  });

  it('hides the location line for a desk without one', async () => {
    api.scanDesk.mockResolvedValue({ ok: true, data: { ...SCANNED, desk: { name: 'Desk', location_label: null } } });
    await mountLiveDesk();
    await submit();
    expect($('ld-desk-location').hidden).toBe(true);
  });

  it('ignores a double tap while scanning, and restores the button afterwards', async () => {
    let finish: (v: unknown) => void = () => {};
    api.scanDesk.mockReturnValue(new Promise((r) => (finish = r)));
    await mountLiveDesk();
    ($('ld-consent') as HTMLInputElement).checked = true;
    $('ld-join-btn').click();
    $('ld-join-btn').click();
    expect(api.scanDesk).toHaveBeenCalledTimes(1);
    expect(($('ld-join-btn') as HTMLButtonElement).disabled).toBe(true);
    finish({ ok: false, status: 429, code: 'rate_limited' });
    await flush();
    expect(($('ld-join-btn') as HTMLButtonElement).disabled).toBe(false);
    expect($('ld-join-btn').textContent).toBe(EN.liveDeskStart);
  });
});

describe('scan errors: every code gets its own message and a sensible action', () => {
  const cases: Array<[number, string, string, 'retry' | 'home' | null]> = [
    [404, 'invalid_desk_qr', 'liveDeskErrInvalid', null],
    [503, 'desk_unavailable', 'liveDeskErrUnavailable', 'retry'],
    [429, 'desk_busy', 'liveDeskErrBusy', 'retry'],
    [429, 'rate_limited', 'liveErrRateLimited', 'retry'],
    [503, 'live_busy', 'liveErrBusy', 'retry'],
    [400, 'consent_required', 'liveConsentRequired', null],
    [400, 'bad_lang', 'liveErrBadLang', null],
    [0, 'network', 'liveErrNetworkRequest', 'retry'],
    [500, 'server_error', 'talkErrorGeneric', 'retry'],
  ];
  it.each(cases)('%s %s', async (status, code, key, action) => {
    api.scanDesk.mockResolvedValue({ ok: false, status, code });
    await mountLiveDesk();
    await submit();
    expect($('ld-status').textContent).toBe(EN[key]);
    expect($('ld-status').hidden).toBe(false);
    expect($('ld-retry').hidden).toBe(action !== 'retry');
    expect($('ld-actions').hidden).toBe(action === null);
    expect($('ld-session').hidden).toBe(true);
    expect(rooms).toHaveLength(0);
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it('a QR that is not valid cannot be scanned again: the button is off, the message asks for staff help', async () => {
    api.scanDesk.mockResolvedValue({ ok: false, status: 404, code: 'invalid_desk_qr' });
    await mountLiveDesk();
    await submit();
    expect(($('ld-join-btn') as HTMLButtonElement).disabled).toBe(true);
    expect(EN.liveDeskErrInvalid).toContain('staff');
  });

  it('retry scans again with the same inputs and clears the message', async () => {
    api.scanDesk.mockResolvedValueOnce({ ok: false, status: 503, code: 'desk_unavailable' });
    api.scanDesk.mockResolvedValueOnce({ ok: true, data: SCANNED });
    await mountLiveDesk();
    ($('ld-name') as HTMLInputElement).value = 'Ana';
    await submit();
    $('ld-retry').click();
    await flush();
    expect(api.scanDesk).toHaveBeenCalledTimes(2);
    expect(api.scanDesk.mock.calls[1][0]).toEqual(api.scanDesk.mock.calls[0][0]);
    expect(rooms).toHaveLength(1);
    expect($('ld-status').hidden).toBe(true);
  });
});

describe('nobody answers', () => {
  async function waiting() {
    api.scanDesk.mockResolvedValue({ ok: true, data: SCANNED });
    await mountLiveDesk();
    await submit();
    return rooms[0];
  }

  it.each(['expired'])('server verdict %s shows a friendly "nobody was available" screen and frees the room', async (reason) => {
    const room = await waiting();
    room.onState?.(ctx('ended', { endReason: reason }));
    expect($('ld-nobody').hidden).toBe(false);
    expect($('ld-nobody-text').textContent).toBe(EN.liveDeskNobody);
    expect($('ld-try-again').hidden).toBe(false);
    expect($('ld-session').hidden).toBe(true);
    expect(room.destroy).toHaveBeenCalledTimes(1);
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
    expect(sessionStorage.getItem(DESK_KEY)).toBeNull();
  });

  it.each(['desk_revoked', 'desk_regenerated'])('%s says the code is no longer valid and offers no pointless retry', async (reason) => {
    const room = await waiting();
    room.onState?.(ctx('ended', { endReason: reason }));
    expect($('ld-nobody-text').textContent).toBe(EN.liveDeskErrInvalid);
    expect($('ld-try-again').hidden).toBe(true);
  });

  it('"Try again" rescans with the same segment and the same inputs and mounts a NEW session, never the old one', async () => {
    const first = await waiting();
    first.onState?.(ctx('ended', { endReason: 'expired' }));
    api.scanDesk.mockResolvedValueOnce({
      ok: true,
      data: { ...SCANNED, session_id: 's2', peer_id: 'peer-g2', token: 'LIVE-2' },
    });
    $('ld-try-again').click();
    await flush();
    expect(api.scanDesk).toHaveBeenCalledTimes(2);
    expect(api.scanDesk.mock.calls[1][0]).toEqual(api.scanDesk.mock.calls[0][0]);
    expect(api.scanDesk.mock.calls[1][0].segment).toBe(SEGMENT);
    expect(rooms).toHaveLength(2);
    expect(rooms[1].session).toMatchObject({ sessionId: 's2', token: 'LIVE-2' });
    expect(JSON.stringify(sessionStorage.getItem(STORAGE_KEY))).not.toContain('LIVE-1');
    expect($('ld-nobody').hidden).toBe(true);
    expect($('ld-session').hidden).toBe(false);
    expect($('ld-countdown').textContent).toContain('60 s');
  });

  it('a failed rescan is reported on the form, with the retry action', async () => {
    const first = await waiting();
    first.onState?.(ctx('ended', { endReason: 'expired' }));
    api.scanDesk.mockResolvedValueOnce({ ok: false, status: 429, code: 'desk_busy' });
    $('ld-try-again').click();
    await flush();
    expect($('ld-join').hidden).toBe(false);
    expect($('ld-status').textContent).toBe(EN.liveDeskErrBusy);
    expect($('ld-retry').hidden).toBe(false);
  });

  it('an ending AFTER the staff joined is the conversation\'s own ended screen, not "nobody was available"', async () => {
    const room = await waiting();
    room.onState?.(ctx('live', { partners: [staffPartner] }));
    room.onState?.(ctx('ended', { endReason: 'ended_by_staff' }));
    expect($('ld-nobody').hidden).toBe(true);
    expect(room.destroy).not.toHaveBeenCalled();
    expect(sessionStorage.getItem(STORAGE_KEY)).not.toBeNull();
  });

  it('an expiry after the staff joined and left is also not "nobody was available"', async () => {
    const room = await waiting();
    room.onState?.(ctx('live', { partners: [staffPartner] }));
    room.onState?.(ctx('waiting_peer'));
    room.onState?.(ctx('ended', { endReason: 'expired' }));
    expect($('ld-nobody').hidden).toBe(true);
  });

  it('when the countdown runs out it asks the server, and shows the screen if the session is over', async () => {
    const room = await waiting();
    api.getSession.mockResolvedValue({ ok: true, data: { session: { status: 'expired', participants: [{ role: 'guest' }] }, you: { role: 'guest' } } });
    await vi.advanceTimersByTimeAsync(60_000);
    expect($('ld-countdown').hidden).toBe(true);
    expect(api.getSession).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(3000);
    expect(api.getSession).toHaveBeenCalledWith('s1', 'LIVE-1');
    expect($('ld-nobody').hidden).toBe(false);
    expect(room.destroy).toHaveBeenCalledTimes(1);
  });

  it('keeps waiting when the server says the session is still open past the countdown', async () => {
    await waiting();
    api.getSession.mockResolvedValue({ ok: true, data: { session: { status: 'waiting', participants: [] }, you: { role: 'guest' } } });
    await vi.advanceTimersByTimeAsync(70_000);
    expect($('ld-nobody').hidden).toBe(true);
    expect($('ld-session').hidden).toBe(false);
  });
});

describe('the staff arrives', () => {
  it('stops the countdown and hides the waiting card, and no timer is left behind', async () => {
    api.scanDesk.mockResolvedValue({ ok: true, data: SCANNED });
    await mountLiveDesk();
    await submit();
    rooms[0].onState?.(ctx('live', { partners: [staffPartner] }));
    expect($('ld-wait').hidden).toBe(true);
    expect(vi.getTimerCount()).toBe(0);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(api.getSession).not.toHaveBeenCalled();
  });
});

describe('reload', () => {
  const STORED = { sessionId: 's9', token: 'T9', role: 'guest', myLang: 'es', myPeerId: 'pg', wsPath: '/ws/live' };
  const DESK = { sessionId: 's9', handle: HANDLE, name: 'Side desk', location: null, until: T0 + 30_000 };
  const store = (stored: object = STORED, desk: object | null = DESK) => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify(stored));
    if (desk) sessionStorage.setItem(DESK_KEY, JSON.stringify(desk));
  };

  it('rejoins the SAME session with the stored token, restoring the desk name and what is left of the countdown, without scanning again', async () => {
    store();
    api.getSession.mockResolvedValue({ ok: true, data: { session: { status: 'waiting', participants: [{ role: 'guest' }] }, you: { role: 'guest' } } });
    await mountLiveDesk();
    expect(api.getSession).toHaveBeenCalledWith('s9', 'T9');
    expect(api.scanDesk).not.toHaveBeenCalled();
    expect(rooms).toHaveLength(1);
    expect(rooms[0].session).toMatchObject({ sessionId: 's9', token: 'T9', role: 'guest', myLang: 'es' });
    expect($('ld-join').hidden).toBe(true);
    expect($('ld-desk-name').textContent).toBe('Side desk');
    expect($('ld-countdown').textContent).toContain('expires in 30 s');
  });

  it('rejoins an answered conversation without a waiting card', async () => {
    store(STORED, { ...DESK, until: T0 - 1000 });
    api.getSession.mockResolvedValue({ ok: true, data: { session: { status: 'active', participants: [{ role: 'guest' }, { role: 'staff' }] }, you: { role: 'guest' } } });
    await mountLiveDesk();
    expect(rooms).toHaveLength(1);
    expect($('ld-countdown').hidden).toBe(true);
  });

  it('a session that ended with nobody answering shows the "nobody was available" screen and clears the stored credentials', async () => {
    store();
    api.getSession.mockResolvedValue({ ok: true, data: { session: { status: 'expired', participants: [{ role: 'guest' }] }, you: { role: 'guest' } } });
    await mountLiveDesk();
    expect(rooms).toHaveLength(0);
    expect($('ld-nobody').hidden).toBe(false);
    expect($('ld-join').hidden).toBe(true);
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
    expect(sessionStorage.getItem(DESK_KEY)).toBeNull();
  });

  it('"Try again" after such a reload brings back the form (the consent was not given in this page life)', async () => {
    store();
    api.getSession.mockResolvedValue({ ok: true, data: { session: { status: 'expired', participants: [] }, you: { role: 'guest' } } });
    await mountLiveDesk();
    $('ld-try-again').click();
    await flush();
    expect(api.scanDesk).not.toHaveBeenCalled();
    expect($('ld-join').hidden).toBe(false);
    expect($('ld-nobody').hidden).toBe(true);
    expect($('ld-hero').hidden).toBe(false);
  });

  it('a conversation that ended after the staff answered shows the ended screen', async () => {
    store();
    api.getSession.mockResolvedValue({ ok: true, data: { session: { status: 'ended', participants: [{ role: 'guest' }, { role: 'staff' }] }, you: { role: 'guest' } } });
    await mountLiveDesk();
    expect($('ld-ended').hidden).toBe(false);
    expect($('ld-nobody').hidden).toBe(true);
    expect(rooms).toHaveLength(0);
  });

  it('a 410 from the server is an ended conversation too', async () => {
    store();
    api.getSession.mockResolvedValue({ ok: false, status: 410, code: 'session_ended' });
    await mountLiveDesk();
    expect($('ld-ended').hidden).toBe(false);
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it('a refused token (401) is just a fresh visit: the form, nothing stored', async () => {
    store();
    api.getSession.mockResolvedValue({ ok: false, status: 401, code: 'bad_token' });
    await mountLiveDesk();
    expect($('ld-join').hidden).toBe(false);
    expect($('ld-ended').hidden).toBe(true);
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it('a server that cannot be asked keeps the session: the engine will reconnect', async () => {
    store();
    api.getSession.mockResolvedValue({ ok: false, status: 0, code: 'network' });
    await mountLiveDesk();
    expect(rooms).toHaveLength(1);
  });

  it('ignores a stored session that is not a desk session (a personal-invite guest, a host)', async () => {
    store(STORED, null);
    await mountLiveDesk();
    expect(api.getSession).not.toHaveBeenCalled();
    expect($('ld-join').hidden).toBe(false);
    sessionStorage.clear();
    store({ ...STORED, role: 'host' });
    await mountLiveDesk();
    expect(api.getSession).not.toHaveBeenCalled();
  });

  it('does not resume the session of ANOTHER desk: scanning a different QR in this tab starts from the form', async () => {
    store(STORED, { ...DESK, handle: 'otherdesk' });
    await mountLiveDesk();
    expect(api.getSession).not.toHaveBeenCalled();
    expect(rooms).toHaveLength(0);
    expect($('ld-join').hidden).toBe(false);
  });
});

describe('the QR segment is a secret', () => {
  it('is sent only to the scan call: never logged, stored, or handed to the room', async () => {
    api.scanDesk.mockResolvedValueOnce({ ok: false, status: 503, code: 'desk_unavailable' });
    api.scanDesk.mockResolvedValueOnce({ ok: true, data: SCANNED });
    await mountLiveDesk();
    await submit();
    $('ld-retry').click();
    await flush();
    expect(JSON.stringify(api.scanDesk.mock.calls)).toContain(SEGMENT);
    for (const spy of consoleSpies) expect(spy).not.toHaveBeenCalled();
    expect(JSON.stringify(Object.entries(sessionStorage))).not.toContain('SECRETMAC');
    expect(JSON.stringify(Object.entries(localStorage))).not.toContain('SECRETMAC');
    expect(JSON.stringify(rooms)).not.toContain('SECRETMAC');
    // The public handle alone is what is remembered, to tell desks apart.
    expect(JSON.parse(sessionStorage.getItem(DESK_KEY) ?? '{}').handle).toBe(HANDLE);
  });
});
