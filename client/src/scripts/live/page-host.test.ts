// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import en from '../i18n/en.json';
import type { LiveContext } from './session-machine';
import { initialContext } from './session-machine';

const EN = en as Record<string, string>;

/** The real page markup, so an id that drifts between the .astro file and the script fails here. */
function pageMarkup(): string {
  const src = readFileSync(resolve(process.cwd(), 'src/pages/live.astro'), 'utf8');
  const main = /<main[\s\S]*?<\/main>/.exec(src)?.[0] ?? '';
  return main
    .replace(/=\{T\('([A-Za-z]+)'\)\}/g, (_m, k: string) => `="${EN[k]}"`)
    .replace(/\{T\('([A-Za-z]+)'\)\}/g, (_m, k: string) => EN[k]);
}

const auth = {
  billing: true,
  loggedIn: true,
  user: { language: 'it', balance: 9 } as { language?: string; balance: number } | null,
};
vi.mock('../auth', () => ({
  billingEnabled: async () => auth.billing,
  isLoggedIn: () => auth.loggedIn,
  getUser: () => auth.user,
  setBalance: vi.fn(),
}));
const api = {
  createSession: vi.fn(),
  getSession: vi.fn(),
};
vi.mock('./api', () => ({
  createSession: (...a: unknown[]) => api.createSession(...a),
  getSession: (...a: unknown[]) => api.getSession(...a),
}));
const unlock = vi.fn();
vi.mock('../pcm-playback', () => ({ pcmPlayback: { unlock: () => unlock() } }));
const toast = vi.fn();
vi.mock('../toast', () => ({ toast: (...a: unknown[]) => toast(...a) }));
const qrDataUrl = vi.fn(async (_t: string, _f: string) => 'data:image/png;base64,QR');
vi.mock('../qr', () => ({ qrDataUrl: (t: string, f: string) => qrDataUrl(t, f) }));

interface RoomCall {
  role: string;
  session: Record<string, unknown>;
  onState: (c: LiveContext) => void;
}
const rooms: RoomCall[] = [];
vi.mock('./room', () => ({
  mountRoom: (o: RoomCall & { container: HTMLElement }) => {
    rooms.push(o);
    return { els: {}, destroy: vi.fn() };
  },
}));

import { mountLiveHost } from './page-host';
import { STORAGE_KEY } from './session-store';

const CREATED = {
  session_id: 's1',
  invite_url: 'https://app.test/lc/SECRETCODE',
  qr_payload: 'https://app.test/lc/SECRETCODE',
  expires_at: '2030-01-01T00:00:00Z',
  peer_id: 'peer-a',
  token: 'LIVE',
  ws_path: '/ws/live',
};
const $ = (id: string) => document.getElementById(id) as HTMLElement;
const flush = () => new Promise((r) => setTimeout(r, 0));

beforeEach(() => {
  document.head.innerHTML = '';
  document.body.innerHTML = pageMarkup();
  sessionStorage.clear();
  rooms.length = 0;
  Object.assign(auth, { billing: true, loggedIn: true, user: { language: 'it', balance: 9 } });
  api.createSession.mockReset();
  api.getSession.mockReset();
  unlock.mockReset();
  toast.mockReset();
  qrDataUrl.mockClear();
  delete (navigator as { share?: unknown }).share;
});

describe('gates', () => {
  it('shows the unavailable state when billing is disabled, and nothing else', async () => {
    auth.billing = false;
    await mountLiveHost();
    expect($('lh-unavailable').hidden).toBe(false);
    expect($('lh-setup').hidden).toBe(true);
    expect($('lh-signin').hidden).toBe(true);
  });

  it('offers sign-in to a signed-out visitor instead of the setup form', async () => {
    auth.loggedIn = false;
    await mountLiveHost();
    expect($('lh-signin').hidden).toBe(false);
    expect($('lh-setup').hidden).toBe(true);
  });
});

describe('setup', () => {
  it('shows the form with my account language preselected and the transcript switch OFF', async () => {
    await mountLiveHost();
    expect($('lh-setup').hidden).toBe(false);
    expect($('lh-lang').textContent).toContain('Italiano');
    expect(($('lh-transcript') as HTMLInputElement).checked).toBe(false);
  });

  describe('guests control', () => {
    const radios = () => Array.from(document.querySelectorAll<HTMLInputElement>('input[name="lh-guests"]'));
    const choose = (n: number) => {
      const r = radios().find((x) => x.value === String(n)) as HTMLInputElement;
      r.checked = true;
      r.dispatchEvent(new Event('change', { bubbles: true }));
    };

    it('offers 1, 2 or 3 guests as a labelled group, defaulting to 1, with the one-line explanation', async () => {
      await mountLiveHost();
      expect(radios().map((r) => r.value)).toEqual(['1', '2', '3']);
      expect(radios().filter((r) => r.checked).map((r) => r.value)).toEqual(['1']);
      const group = $('lh-guests-field');
      expect(group.tagName).toBe('FIELDSET');
      expect(group.querySelector('legend')?.textContent).toBe(EN.liveGuests);
      expect($('lh-guests-hint').textContent).toBe(EN.liveGuestsHintOne);
    });

    it('updates the explanation with the chosen number', async () => {
      await mountLiveHost();
      choose(3);
      expect($('lh-guests-hint').textContent).toBe(EN.liveGuestsHint.replace('{max}', '3'));
    });

    it('sends the chosen number and remembers it with the stored session', async () => {
      api.createSession.mockResolvedValue({ ok: true, data: { ...CREATED, max_guests: 3 } });
      await mountLiveHost();
      choose(3);
      $('lh-create').click();
      await flush();
      expect(api.createSession).toHaveBeenCalledWith({ lang: 'it', saveTranscript: false, maxGuests: 3 });
      expect(rooms[0].session).toMatchObject({ maxGuests: 3 });
      expect(JSON.parse(sessionStorage.getItem(STORAGE_KEY) ?? '{}').maxGuests).toBe(3);
    });

    it('treats a missing or odd value as 1', async () => {
      api.createSession.mockResolvedValue({ ok: true, data: CREATED });
      await mountLiveHost();
      radios().forEach((r) => (r.checked = false));
      $('lh-create').click();
      await flush();
      expect(api.createSession).toHaveBeenCalledWith({ lang: 'it', saveTranscript: false, maxGuests: 1 });
    });

    it('shows a friendly message for bad_max_guests', async () => {
      api.createSession.mockResolvedValue({ ok: false, status: 400, code: 'bad_max_guests' });
      await mountLiveHost();
      $('lh-create').click();
      await flush();
      expect($('lh-status').hidden).toBe(false);
      expect($('lh-status').textContent).toBe(EN.liveErrBadGuests);
    });
  });

  it('creates a session with the chosen language and the transcript flag, then shows the invite', async () => {
    api.createSession.mockResolvedValue({ ok: true, data: CREATED });
    await mountLiveHost();
    ($('lh-transcript') as HTMLInputElement).checked = true;
    $('lh-create').click();
    expect(unlock).toHaveBeenCalledTimes(1); // inside the click gesture, before the request
    await flush();
    expect(api.createSession).toHaveBeenCalledWith({ lang: 'it', saveTranscript: true, maxGuests: 1 });
    expect($('lh-setup').hidden).toBe(true);
    expect($('lh-hero').hidden).toBe(true);
    expect($('lh-session').hidden).toBe(false);
    expect(($('lh-link') as HTMLInputElement).value).toBe(CREATED.invite_url);
    expect(($('lh-qr') as HTMLImageElement).getAttribute('src')).toBe('data:image/png;base64,QR');
    expect(qrDataUrl).toHaveBeenCalledWith(CREATED.qr_payload, '');
    expect(($('lh-qr') as HTMLImageElement).alt).toBe(EN.liveQrAlt);
    expect($('lh-invite').hidden).toBe(false);
  });

  it('hands the room the live credentials, as a host, with the invite URL', async () => {
    api.createSession.mockResolvedValue({ ok: true, data: CREATED });
    await mountLiveHost();
    $('lh-create').click();
    await flush();
    expect(rooms).toHaveLength(1);
    expect(rooms[0].role).toBe('host');
    expect(rooms[0].session).toEqual({
      sessionId: 's1',
      token: 'LIVE',
      role: 'host',
      myLang: 'it',
      myPeerId: 'peer-a',
      wsPath: '/ws/live',
      inviteUrl: CREATED.invite_url,
      maxGuests: 1,
    });
  });

  it('shows the invite only while nobody has joined', async () => {
    api.createSession.mockResolvedValue({ ok: true, data: CREATED });
    await mountLiveHost();
    $('lh-create').click();
    await flush();
    const ctx = (over: Partial<LiveContext>): LiveContext => ({ ...initialContext(), ...over });
    rooms[0].onState(ctx({ phase: 'waiting_peer' }));
    expect($('lh-invite').hidden).toBe(false);
    rooms[0].onState(ctx({ phase: 'live', partners: [{ peerId: 'b', name: 'B', lang: 'es', muted: false }] }));
    expect($('lh-invite').hidden).toBe(true);
    // A pair has one seat and the guest took it: the link has nothing left to offer, even if they drop.
    rooms[0].onState(ctx({ phase: 'waiting_peer' }));
    expect($('lh-invite').hidden).toBe(true);
    rooms[0].onState(ctx({ phase: 'ended', endReason: 'ended_by_me' }));
    expect($('lh-invite').hidden).toBe(true);
  });

  it('disables the button while creating, and re-enables it after a failure', async () => {
    let finish: (v: unknown) => void = () => {};
    api.createSession.mockReturnValue(new Promise((r) => (finish = r)));
    await mountLiveHost();
    const btn = $('lh-create') as HTMLButtonElement;
    btn.click();
    expect(btn.disabled).toBe(true);
    expect(btn.textContent).toBe('Creating…');
    btn.click(); // a double tap must not create two sessions
    expect(api.createSession).toHaveBeenCalledTimes(1);
    finish({ ok: false, status: 429, code: 'rate_limited' });
    await flush();
    expect(btn.disabled).toBe(false);
    expect(btn.textContent).toBe('Create conversation');
  });

  it.each([
    ['insufficient_balance', 'liveErrInsufficient', 'lh-buy'],
    ['too_many_sessions', 'liveErrTooMany', null],
    ['live_busy', 'liveErrBusy', null],
    ['rate_limited', 'liveErrRateLimited', null],
    ['network', 'liveErrNetworkRequest', null],
    ['unauthorized', 'liveErrAuth', 'lh-signin-link'],
    ['live_disabled', 'liveUnavailable', 'lh-home'],
  ])('maps %s to its own message and next step', async (code, key, link) => {
    api.createSession.mockResolvedValue({ ok: false, status: 400, code });
    await mountLiveHost();
    $('lh-create').click();
    await flush();
    expect($('lh-status').textContent).toBe(EN[key]);
    expect($('lh-status').hidden).toBe(false);
    expect($('lh-session').hidden).toBe(true);
    for (const id of ['lh-buy', 'lh-signin-link', 'lh-home']) {
      expect($(id).hidden, `${code}: ${id}`).toBe(id !== link);
    }
    expect($('lh-actions').hidden).toBe(link === null);
  });

  it('clears the previous error on the next attempt', async () => {
    api.createSession.mockResolvedValueOnce({ ok: false, status: 429, code: 'rate_limited' });
    api.createSession.mockResolvedValueOnce({ ok: true, data: CREATED });
    await mountLiveHost();
    $('lh-create').click();
    await flush();
    expect($('lh-status').hidden).toBe(false);
    $('lh-create').click();
    await flush();
    expect($('lh-status').hidden).toBe(true);
  });
});

describe('invite panel', () => {
  async function created() {
    api.createSession.mockResolvedValue({ ok: true, data: CREATED });
    await mountLiveHost();
    $('lh-create').click();
    await flush();
  }

  it('copies the link and says so', async () => {
    const writeText = vi.fn(async () => {});
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    await created();
    $('lh-copy').click();
    await flush();
    expect(writeText).toHaveBeenCalledWith(CREATED.invite_url);
    expect(toast).toHaveBeenCalledWith('Copied', 'ok');
  });

  it('reports a copy failure instead of staying silent', async () => {
    Object.defineProperty(navigator, 'clipboard', { value: { writeText: async () => Promise.reject(new Error('x')) }, configurable: true });
    await created();
    $('lh-copy').click();
    await flush();
    expect(toast).toHaveBeenCalledWith('Copy failed', 'err');
  });

  it('offers Share only where the browser has it, and shares the URL', async () => {
    await created();
    expect($('lh-share').hidden).toBe(true);
    const share = vi.fn(async () => {});
    Object.defineProperty(navigator, 'share', { value: share, configurable: true });
    document.body.innerHTML = pageMarkup();
    sessionStorage.clear();
    api.createSession.mockResolvedValue({ ok: true, data: CREATED });
    await mountLiveHost();
    $('lh-create').click();
    await flush();
    expect($('lh-share').hidden).toBe(false);
    $('lh-share').click();
    await flush();
    expect(share).toHaveBeenCalledWith({ url: CREATED.invite_url, title: 'Live Connect' });
  });

  it('hides the QR when it cannot be drawn, leaving the copyable link', async () => {
    qrDataUrl.mockResolvedValueOnce('');
    await created();
    expect($('lh-qr').hidden).toBe(true);
    expect(($('lh-link') as HTMLInputElement).value).toBe(CREATED.invite_url);
  });
});

describe('invite panel with several guests', () => {
  const ctx = (over: Partial<LiveContext>): LiveContext => ({ ...initialContext(), ...over });
  const guest = (id: string, name = id) => ({ peerId: id, name, lang: 'es', muted: false });
  async function created(max: number) {
    api.createSession.mockResolvedValue({ ok: true, data: { ...CREATED, max_guests: max } });
    await mountLiveHost();
    const r = Array.from(document.querySelectorAll<HTMLInputElement>('input[name="lh-guests"]')).find((x) => x.value === String(max))!;
    r.checked = true;
    r.dispatchEvent(new Event('change', { bubbles: true }));
    $('lh-create').click();
    await flush();
  }

  it('says how many guests joined, and uses the several-guest wording', async () => {
    await created(3);
    expect($('lh-seats').hidden).toBe(false);
    expect($('lh-seats').textContent).toBe('0 of 3 guests joined');
    expect($('lh-invite').querySelector('[data-i18n="liveInviteTitleMulti"]')).not.toBeNull();
    expect($('lh-invite').querySelector('[data-i18n="liveInviteExpiresMulti"]')).not.toBeNull();
    rooms[0].onState(ctx({ phase: 'live', partners: [guest('b')] }));
    expect($('lh-seats').textContent).toBe('1 of 3 guests joined');
    rooms[0].onState(ctx({ phase: 'live', partners: [guest('b'), guest('c')] }));
    expect($('lh-seats').textContent).toBe('2 of 3 guests joined');
  });

  it('a pair keeps today\'s look: no seats line, the original invite wording', async () => {
    await created(1);
    expect($('lh-seats').hidden).toBe(true);
    expect($('lh-invite').querySelector('[data-i18n="liveInviteExpires"]')).not.toBeNull();
  });

  it('is open while nobody is here, collapses to a toggle once a guest joined, and re-opens on demand', async () => {
    await created(3);
    rooms[0].onState(ctx({ phase: 'waiting_peer' }));
    expect($('lh-invite').hidden).toBe(false);
    expect($('lh-invite-body').hidden).toBe(false);
    expect($('lh-invite-toggle').hidden).toBe(true);

    rooms[0].onState(ctx({ phase: 'live', partners: [guest('b')] }));
    expect($('lh-invite').hidden).toBe(false);
    expect($('lh-invite-body').hidden).toBe(true);
    const toggle = $('lh-invite-toggle') as HTMLButtonElement;
    expect(toggle.hidden).toBe(false);
    expect(toggle.getAttribute('aria-expanded')).toBe('false');
    expect(toggle.getAttribute('aria-controls')).toBe('lh-invite-body');
    expect(toggle.textContent).toBe(EN.liveInviteMore);

    toggle.click();
    expect($('lh-invite-body').hidden).toBe(false);
    expect(toggle.getAttribute('aria-expanded')).toBe('true');
    expect(toggle.textContent).toBe(EN.liveInviteHide);
    // The link is still there to copy.
    expect(($('lh-link') as HTMLInputElement).value).toBe(CREATED.invite_url);

    toggle.click();
    expect($('lh-invite-body').hidden).toBe(true);
  });

  it('re-opens by itself when the last guest leaves while seats remain', async () => {
    await created(3);
    rooms[0].onState(ctx({ phase: 'live', partners: [guest('b')] }));
    expect($('lh-invite-body').hidden).toBe(true);
    rooms[0].onState(ctx({ phase: 'waiting_peer' }));
    expect($('lh-invite').hidden).toBe(false);
    expect($('lh-invite-body').hidden).toBe(false);
    expect($('lh-invite-toggle').hidden).toBe(true);
  });

  it('is hidden once every seat is taken, even when somebody drops, and "N of M" stays', async () => {
    await created(2);
    rooms[0].onState(ctx({ phase: 'live', partners: [guest('b'), guest('c')] }));
    expect($('lh-invite').hidden).toBe(true);
    rooms[0].onState(ctx({ phase: 'live', partners: [guest('c')] }));
    expect($('lh-invite').hidden).toBe(true);
    expect($('lh-seats').textContent).toBe('2 of 2 guests joined');
  });

  it('hides the whole panel and the count when the conversation is over', async () => {
    await created(3);
    rooms[0].onState(ctx({ phase: 'live', partners: [guest('b')] }));
    rooms[0].onState(ctx({ phase: 'ended', endReason: 'ended_by_me' }));
    expect($('lh-invite').hidden).toBe(true);
    expect($('lh-seats').hidden).toBe(true);
  });

  it('a reload restores the seats from the server: who already took one, and the size', async () => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify({ sessionId: 's9', token: 'T9', role: 'host', myLang: 'es', myPeerId: 'p9', wsPath: '/ws/live', inviteUrl: 'https://app.test/lc/X' }));
    api.getSession.mockResolvedValue({
      ok: true,
      data: {
        session: {
          status: 'active',
          max_guests: 3,
          participants: [
            { peer_id: 'p9', role: 'host' },
            { peer_id: 'g1', role: 'guest' },
            { peer_id: 'g2', role: 'guest' },
          ],
        },
        you: { role: 'host' },
      },
    });
    await mountLiveHost();
    expect(rooms[0].session).toMatchObject({ maxGuests: 3 });
    expect($('lh-seats').textContent).toBe('2 of 3 guests joined');
    rooms[0].onState(ctx({ phase: 'waiting_peer' }));
    expect($('lh-invite').hidden).toBe(false);
    rooms[0].onState(ctx({ phase: 'live', partners: [guest('g1'), guest('g2'), guest('g3')] }));
    expect($('lh-seats').textContent).toBe('3 of 3 guests joined');
    expect($('lh-invite').hidden).toBe(true);
  });

  it('keeps the copy button working while collapsed content is reopened', async () => {
    const writeText = vi.fn(async () => {});
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    await created(3);
    rooms[0].onState(ctx({ phase: 'live', partners: [guest('b')] }));
    $('lh-invite-toggle').click();
    $('lh-copy').click();
    await flush();
    expect(writeText).toHaveBeenCalledWith(CREATED.invite_url);
  });
});

describe('reload', () => {
  const STORED = { sessionId: 's9', token: 'T9', role: 'host', myLang: 'es', myPeerId: 'p9', wsPath: '/ws/live', inviteUrl: 'https://app.test/lc/X' };

  it('rejoins a stored, still-open session instead of showing the form', async () => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify(STORED));
    api.getSession.mockResolvedValue({ ok: true, data: { session: { status: 'active' }, you: { role: 'host' } } });
    await mountLiveHost();
    expect(api.getSession).toHaveBeenCalledWith('s9', 'T9');
    expect($('lh-setup').hidden).toBe(true);
    expect($('lh-session').hidden).toBe(false);
    expect(rooms[0].session).toMatchObject({ sessionId: 's9', myLang: 'es', inviteUrl: 'https://app.test/lc/X' });
    expect(($('lh-link') as HTMLInputElement).value).toBe('https://app.test/lc/X');
  });

  it('rejoins when the server cannot be asked (network error): the engine reconnect decides', async () => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify(STORED));
    api.getSession.mockResolvedValue({ ok: false, status: 0, code: 'network' });
    await mountLiveHost();
    expect(rooms).toHaveLength(1);
    expect(sessionStorage.getItem(STORAGE_KEY)).not.toBeNull();
  });

  it.each([
    ['an ended session', { ok: true, data: { session: { status: 'ended' }, you: { role: 'host' } } }],
    ['a refused token', { ok: false, status: 401, code: 'bad_token' }],
  ])('drops %s and shows the form', async (_n, result) => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify(STORED));
    api.getSession.mockResolvedValue(result);
    await mountLiveHost();
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
    expect($('lh-setup').hidden).toBe(false);
    expect(rooms).toHaveLength(0);
  });

  it('ignores a stored GUEST session on the host page', async () => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify({ ...STORED, role: 'guest' }));
    await mountLiveHost();
    expect(api.getSession).not.toHaveBeenCalled();
    expect($('lh-setup').hidden).toBe(false);
  });
});
