// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import en from '../i18n/en.json';

const EN = en as Record<string, string>;

/** The real page markup, so an id that drifts between the .astro file and the script fails here. */
function pageMarkup(): string {
  const src = readFileSync(resolve(process.cwd(), 'src/pages/live/desks.astro'), 'utf8');
  const main = /<main[\s\S]*?<\/main>/.exec(src)?.[0] ?? '';
  return main
    .replace(/=\{T\('([A-Za-z]+)'\)\}/g, (_m, k: string) => `="${EN[k]}"`)
    .replace(/\{T\('([A-Za-z]+)'\)\}/g, (_m, k: string) => EN[k]);
}

const auth = { billing: true, loggedIn: true, user: { language: 'en' } as { language?: string } | null };
vi.mock('../auth', () => ({
  billingEnabled: async () => auth.billing,
  isLoggedIn: () => auth.loggedIn,
  getUser: () => auth.user,
}));
const api = { myDesks: vi.fn(), deskWaiting: vi.fn(), answerSession: vi.fn(), getSession: vi.fn() };
vi.mock('./api', () => ({
  myDesks: (...a: unknown[]) => api.myDesks(...a),
  deskWaiting: (...a: unknown[]) => api.deskWaiting(...a),
  answerSession: (...a: unknown[]) => api.answerSession(...a),
  getSession: (...a: unknown[]) => api.getSession(...a),
}));
let supported = true;
vi.mock('./conversation', () => ({ browserSupported: () => supported }));
const unlock = vi.fn();
vi.mock('../pcm-playback', () => ({ pcmPlayback: { unlock: () => unlock() } }));
interface RoomCall {
  role: string;
  session: Record<string, unknown>;
  container: HTMLElement;
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

import { mountLiveStaff } from './page-staff';
import { STAFF_LANG_KEY, STORAGE_KEY } from './session-store';

const ok = <T>(data: T) => ({ ok: true as const, data });
const fail = (status: number, code = 'http_error') => ({ ok: false as const, status, code });
const desk = (over: Record<string, unknown> = {}) => ({
  id: 'd1', org_id: 'o1', org_name: 'Acme', name: 'Front desk', location_label: 'Terminal 2', default_lang: 'it', waiting: 0, ...over,
});
const cust = (id: string, name = 'Marco', lang = 'es', waited = 7) => ({ session_id: id, guest_name: name, guest_lang: lang, waited_secs: waited });
const ANSWERED = { session_id: 's1', peer_id: 'peer-st', token: 'STAFF-LIVE', ws_path: '/ws/live', guest: { name: 'Marco', lang: 'es' } };

/** A scripted backend: what my-desks and the queues return right now. */
const world: { desks: ReturnType<typeof desk>[]; queues: Record<string, ReturnType<typeof cust>[]> } = { desks: [], queues: {} };
function setWorld(desks: ReturnType<typeof desk>[], queues: Record<string, ReturnType<typeof cust>[]> = {}) {
  world.desks = desks.map((d) => ({ ...d, waiting: (queues[d.id] ?? []).length }));
  world.queues = queues;
}

const $ = (id: string) => document.getElementById(id) as HTMLElement;
const flush = () => vi.advanceTimersByTimeAsync(0);
const tick = (ms = 2000) => vi.advanceTimersByTimeAsync(ms);
const rows = () => [...document.querySelectorAll<HTMLElement>('#ls-desks li[data-session]')];
const answerBtn = (sessionId: string) => document.querySelector<HTMLButtonElement>(`#ls-desks li[data-session="${sessionId}"] button`)!;
const consoleSpies = (['log', 'info', 'warn', 'error', 'debug'] as const).map((m) => vi.spyOn(console, m).mockImplementation(() => {}));

async function mount() {
  await mountLiveStaff();
  await flush();
}

beforeEach(() => {
  vi.useFakeTimers();
  document.head.innerHTML = '';
  document.title = 'Desks — VoxTranslate';
  document.body.innerHTML = pageMarkup();
  sessionStorage.clear();
  localStorage.clear();
  rooms.length = 0;
  supported = true;
  auth.billing = true;
  auth.loggedIn = true;
  auth.user = { language: 'en' };
  setWorld([desk()]);
  for (const m of Object.values(api)) m.mockReset();
  api.myDesks.mockImplementation(async () => ok({ desks: world.desks }));
  api.deskWaiting.mockImplementation(async (id: string) => ok({ waiting: world.queues[id] ?? [] }));
  api.answerSession.mockResolvedValue(ok(ANSWERED));
  api.getSession.mockResolvedValue(ok({ session: { status: 'active' }, you: { role: 'staff' } }));
  unlock.mockReset();
  consoleSpies.forEach((s) => s.mockClear());
});
afterEach(() => vi.useRealTimers());

describe('gates', () => {
  it('billing off: the unavailable state, and no polling', async () => {
    auth.billing = false;
    await mount();
    expect($('ls-unavailable').hidden).toBe(false);
    expect($('ls-list').hidden).toBe(true);
    await tick(10_000);
    expect(api.myDesks).not.toHaveBeenCalled();
  });

  it('signed out: asks to sign in, and does not poll', async () => {
    auth.loggedIn = false;
    await mount();
    expect($('ls-signin').hidden).toBe(false);
    expect($('ls-signin-text').textContent).toBe(EN.liveStaffSignIn);
    expect($('ls-list').hidden).toBe(true);
    await tick(10_000);
    expect(api.myDesks).not.toHaveBeenCalled();
  });
});

describe('the desks', () => {
  it('lists each desk with its organisation, location and how many are waiting', async () => {
    setWorld([desk(), desk({ id: 'd2', name: 'Side desk', location_label: null, org_name: 'Beta' })], { d1: [cust('a'), cust('b', 'Ana')] });
    await mount();
    expect($('ls-list').hidden).toBe(false);
    const cards = [...document.querySelectorAll<HTMLElement>('#ls-desks article')];
    expect(cards.map((c) => c.dataset.desk)).toEqual(['d1', 'd2']);
    expect(cards[0].querySelector('.lc-desk-name')?.textContent).toBe('Front desk');
    expect(cards[0].querySelector('.lc-desk-meta')?.textContent).toBe('Acme · Terminal 2');
    expect(cards[1].querySelector('.lc-desk-meta')?.textContent).toBe('Beta');
    expect(cards[0].querySelector('.lc-desk-count')?.textContent).toBe('Waiting customers: 2');
    expect(cards[1].querySelector('.lc-queue-empty')?.hasAttribute('hidden')).toBe(false);
    expect(cards[1].querySelector('.lc-queue-empty')?.textContent).toBe(EN.liveStaffNobody);
  });

  it('shows each waiting customer with name, language and time, and an Answer button named after them', async () => {
    setWorld([desk()], { d1: [cust('a', 'Marco', 'es', 7)] });
    await mount();
    const [row] = rows();
    expect(row.querySelector('.lc-queue-name')?.textContent).toBe('Marco');
    expect(row.querySelector('.lc-queue-meta')?.textContent).toContain('Español');
    expect(row.querySelector('.lc-queue-meta')?.textContent).toContain('Waiting 7 s');
    expect(answerBtn('a').textContent).toBe('Answer');
    expect(answerBtn('a').getAttribute('aria-label')).toBe('Answer Marco');
  });

  it('a member of an organisation with no desk gets an explanation, not an empty page', async () => {
    setWorld([]);
    await mount();
    expect($('ls-empty').hidden).toBe(false);
    setWorld([desk()]);
    await tick();
    expect($('ls-empty').hidden).toBe(true);
  });

  it('customers are text, never markup', async () => {
    setWorld([desk({ name: '<img src=x>' })], { d1: [cust('a', '<b>Hi</b>')] });
    await mount();
    expect(document.querySelector('#ls-desks img')).toBeNull();
    expect(document.querySelector('#ls-desks b')).toBeNull();
    expect(rows()[0].querySelector('.lc-queue-name')?.textContent).toBe('<b>Hi</b>');
  });

  it('updates in place: the Answer button a colleague is about to press is the same element after a poll', async () => {
    setWorld([desk()], { d1: [cust('a', 'Marco', 'es', 7)] });
    await mount();
    const before = answerBtn('a');
    before.focus();
    setWorld([desk()], { d1: [cust('a', 'Marco', 'es', 9), cust('b', 'Ana')] });
    await tick();
    expect(answerBtn('a')).toBe(before);
    expect(document.activeElement).toBe(before);
    expect(rows()[0].querySelector('.lc-queue-meta')?.textContent).toContain('Waiting 9 s');
    expect(rows().map((r) => r.dataset.session)).toEqual(['a', 'b']);
  });

  it('drops a customer who left, and a desk that disappeared', async () => {
    setWorld([desk(), desk({ id: 'd2', name: 'Other' })], { d1: [cust('a')] });
    await mount();
    setWorld([desk()], {});
    await tick();
    expect(rows()).toHaveLength(0);
    expect(document.querySelectorAll('#ls-desks article')).toHaveLength(1);
  });
});

describe('the cue for a new customer', () => {
  it('has a polite live region and uses no sound', () => {
    document.body.innerHTML = pageMarkup();
    const banner = document.getElementById('ls-banner') as HTMLElement;
    expect(banner.getAttribute('role')).toBe('status');
    expect(banner.getAttribute('aria-live')).toBe('polite');
    expect(document.querySelector('audio')).toBeNull();
  });

  it('badges the title with how many are waiting, but does not announce customers already there when the page opened', async () => {
    setWorld([desk()], { d1: [cust('a')] });
    await mount();
    expect(document.title).toBe('(1) Desks — VoxTranslate');
    expect($('ls-banner').hidden).toBe(true);
  });

  it('announces a customer who starts waiting, names the desk, and counts them in the title', async () => {
    await mount();
    expect(document.title).toBe('Desks — VoxTranslate');
    setWorld([desk()], { d1: [cust('a')] });
    await tick();
    expect($('ls-banner').hidden).toBe(false);
    expect($('ls-banner').textContent).toBe('A customer is waiting at Front desk.');
    expect(document.title).toBe('(1) Desks — VoxTranslate');
    setWorld([desk()], { d1: [cust('a'), cust('b')] });
    await tick();
    expect(document.title).toBe('(2) Desks — VoxTranslate');
  });

  it('clears both when nobody is waiting any more', async () => {
    await mount();
    setWorld([desk()], { d1: [cust('a')] });
    await tick();
    setWorld([desk()], {});
    await tick();
    expect($('ls-banner').hidden).toBe(true);
    expect(document.title).toBe('Desks — VoxTranslate');
  });

  it('respects reduced motion: the cue is text and a title badge, and its only animation is declared for "no-preference"', () => {
    const css = readFileSync(resolve(process.cwd(), 'src/scripts/live/live.css'), 'utf8');
    const outside = css.replace(/@media \(prefers-reduced-motion: no-preference\)\s*\{[\s\S]*?\n\}/g, '');
    expect(css).toMatch(/@media \(prefers-reduced-motion: no-preference\)[\s\S]*?\.lc-alert[\s\S]*?animation/);
    // Nothing that styles .lc-alert outside that block may animate.
    expect(/\.lc-alert[^{]*\{[^}]*animation/.test(outside)).toBe(false);
  });
});

describe('answering', () => {
  async function openPanel(sessionId = 'a') {
    answerBtn(sessionId).click();
    await flush();
  }

  beforeEach(() => {
    setWorld([desk()], { d1: [cust('a', 'Marco', 'es')] });
  });

  it('opens a language choice titled with the customer, defaulting to the desk\'s language', async () => {
    await mount();
    await openPanel();
    expect($('ls-answer').hidden).toBe(false);
    expect($('ls-answer-title').textContent).toBe('Answer Marco');
    expect($('ls-lang').textContent).toContain('Italiano');
    expect(api.answerSession).not.toHaveBeenCalled();
  });

  it('prefers the language this member last spoke at this desk', async () => {
    localStorage.setItem(STAFF_LANG_KEY, JSON.stringify({ d1: 'fr' }));
    await mount();
    await openPanel();
    expect($('ls-lang').textContent).toContain('Français');
  });

  it('falls back to the UI language when the desk language is not one Live Connect offers', async () => {
    setWorld([desk({ default_lang: 'xx-unknown' })], { d1: [cust('a')] });
    await mount();
    await openPanel();
    expect($('ls-lang').textContent).toContain('English');
  });

  it('Cancel closes the choice and answers nobody', async () => {
    await mount();
    await openPanel();
    $('ls-answer-cancel').click();
    expect($('ls-answer').hidden).toBe(true);
    expect(api.answerSession).not.toHaveBeenCalled();
  });

  it('answers the chosen customer in the chosen language and opens the conversation as the staff seat', async () => {
    await mount();
    await openPanel();
    $('ls-answer-go').click();
    expect(unlock).toHaveBeenCalledTimes(1);
    await flush();
    expect(api.answerSession).toHaveBeenCalledWith('a', 'it');
    expect(rooms).toHaveLength(1);
    expect(rooms[0].role).toBe('staff');
    expect(rooms[0].session).toEqual({ sessionId: 's1', token: 'STAFF-LIVE', role: 'staff', myLang: 'it', myPeerId: 'peer-st', wsPath: '/ws/live' });
    expect($('ls-list').hidden).toBe(true);
    expect($('ls-hero').hidden).toBe(true);
    expect($('ls-session').hidden).toBe(false);
    expect($('ls-customer').textContent).toContain('Marco');
    expect($('ls-customer').textContent).toContain('Español');
    expect(JSON.parse(sessionStorage.getItem(STORAGE_KEY) ?? '{}')).toMatchObject({ role: 'staff', token: 'STAFF-LIVE' });
  });

  it('remembers the language per desk for next time', async () => {
    await mount();
    await openPanel();
    $('ls-answer-go').click();
    await flush();
    expect(JSON.parse(localStorage.getItem(STAFF_LANG_KEY) ?? '{}')).toEqual({ d1: 'it' });
  });

  it('stops polling once the conversation starts, restores the title, and leaves no timer behind', async () => {
    await mount();
    setWorld([desk()], { d1: [cust('a'), cust('b')] });
    await tick();
    await openPanel();
    $('ls-answer-go').click();
    await flush();
    const calls = api.myDesks.mock.calls.length;
    await tick(20_000);
    expect(api.myDesks.mock.calls.length).toBe(calls);
    expect(document.title).toBe('Desks — VoxTranslate');
    expect(vi.getTimerCount()).toBe(0);
  });

  it('ignores a double tap on Answer', async () => {
    let finish: (v: unknown) => void = () => {};
    api.answerSession.mockReturnValue(new Promise((r) => (finish = r)));
    await mount();
    await openPanel();
    $('ls-answer-go').click();
    $('ls-answer-go').click();
    expect(api.answerSession).toHaveBeenCalledTimes(1);
    expect(($('ls-answer-go') as HTMLButtonElement).disabled).toBe(true);
    finish(ok(ANSWERED));
    await flush();
    expect(rooms).toHaveLength(1);
  });

  it('a browser that cannot capture audio is told so, and nobody is answered', async () => {
    supported = false;
    await mount();
    await openPanel();
    $('ls-answer-go').click();
    await flush();
    expect(api.answerSession).not.toHaveBeenCalled();
    expect($('ls-answer-status').textContent).toBe(EN.talkErrorUnsupported);
  });

  it('closes the choice when the customer leaves while it is open, and says so', async () => {
    await mount();
    await openPanel();
    setWorld([desk()], {});
    await tick();
    expect($('ls-answer').hidden).toBe(true);
    expect($('ls-error').textContent).toBe(EN.liveStaffErrGone);
  });
});

describe('answer failures: each code has its own message and the right next step', () => {
  beforeEach(() => {
    setWorld([desk()], { d1: [cust('a', 'Marco')] });
  });
  async function tryAnswer(res: unknown) {
    api.answerSession.mockResolvedValue(res);
    await mount();
    answerBtn('a').click();
    await flush();
    $('ls-answer-go').click();
    await flush();
  }

  it('409 already_answered: a colleague got there first. The choice closes and the list refreshes', async () => {
    api.myDesks.mockClear();
    await tryAnswer(fail(409, 'already_answered'));
    expect($('ls-error').textContent).toBe(EN.liveStaffErrTaken);
    expect($('ls-answer').hidden).toBe(true);
    expect(api.myDesks.mock.calls.length).toBeGreaterThan(1);
    expect(rooms).toHaveLength(0);
  });

  it.each(['session_expired', 'session_ended'])('410 %s: the customer left. The choice closes and the list refreshes', async (code) => {
    await tryAnswer(fail(410, code));
    expect($('ls-error').textContent).toBe(EN.liveStaffErrGone);
    expect($('ls-answer').hidden).toBe(true);
  });

  it.each(['insufficient_credits', 'subscription_required'])('402 %s: the organisation cannot take calls; the member cannot fix that', async (code) => {
    await tryAnswer(fail(402, code));
    expect($('ls-answer-status').textContent).toBe(EN.liveStaffErrOrg);
    expect($('ls-answer').hidden).toBe(false);
    expect(rooms).toHaveLength(0);
  });

  it('503 live_busy: lines are busy; the choice stays so it can be tried again', async () => {
    await tryAnswer(fail(503, 'live_busy'));
    expect($('ls-answer-status').textContent).toBe(EN.liveErrBusy);
    expect($('ls-answer').hidden).toBe(false);
    expect(($('ls-answer-go') as HTMLButtonElement).disabled).toBe(false);
    api.answerSession.mockResolvedValue(ok(ANSWERED));
    $('ls-answer-go').click();
    await flush();
    expect(rooms).toHaveLength(1);
  });

  it('404: not available to you', async () => {
    await tryAnswer(fail(404, 'session_not_found'));
    expect($('ls-answer-status').textContent).toBe(EN.liveStaffErrNotFound);
  });

  it('401: the sign-in expired. Polling stops and the sign-in prompt comes back', async () => {
    await tryAnswer(fail(401));
    expect($('ls-signin').hidden).toBe(false);
    expect($('ls-signin-text').textContent).toBe(EN.liveStaffErrAuth);
    expect($('ls-list').hidden).toBe(true);
    const calls = api.myDesks.mock.calls.length;
    await tick(20_000);
    expect(api.myDesks.mock.calls.length).toBe(calls);
  });

  it('a dead network can be retried', async () => {
    await tryAnswer(fail(0, 'network'));
    expect($('ls-answer-status').textContent).toBe(EN.liveErrNetworkRequest);
    expect($('ls-answer').hidden).toBe(false);
  });

  it('a failed answer does not remember a language that was never used', async () => {
    await tryAnswer(fail(409, 'already_answered'));
    expect(localStorage.getItem(STAFF_LANG_KEY)).toBeNull();
  });
});

describe('polling failures', () => {
  it('shows a quiet error while the server is unreachable, keeps the last list, and clears it on recovery', async () => {
    setWorld([desk()], { d1: [cust('a')] });
    await mount();
    api.myDesks.mockResolvedValueOnce(fail(503, 'server_error'));
    await tick();
    expect($('ls-error').hidden).toBe(false);
    expect($('ls-error').textContent).toBe(EN.talkErrorGeneric);
    expect(rows()).toHaveLength(1);
    await tick(30_000);
    expect($('ls-error').hidden).toBe(true);
  });

  it('a dead network reads as a connection problem', async () => {
    await mount();
    api.myDesks.mockResolvedValue(fail(0, 'network'));
    await tick();
    expect($('ls-error').textContent).toBe(EN.liveErrNetworkRequest);
  });

  it('a 401 while polling asks to sign in again and stops asking', async () => {
    await mount();
    api.myDesks.mockResolvedValue(fail(401));
    await tick();
    expect($('ls-signin').hidden).toBe(false);
    expect($('ls-signin-text').textContent).toBe(EN.liveStaffErrAuth);
    expect($('ls-list').hidden).toBe(true);
    const calls = api.myDesks.mock.calls.length;
    await tick(60_000);
    expect(api.myDesks.mock.calls.length).toBe(calls);
  });
});

describe('reload', () => {
  const STORED = { sessionId: 's9', token: 'T9', role: 'staff', myLang: 'it', myPeerId: 'ps', wsPath: '/ws/live' };

  it('rejoins a stored, still-open staff seat with the stored token and does not poll', async () => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify(STORED));
    await mount();
    expect(api.getSession).toHaveBeenCalledWith('s9', 'T9');
    expect(rooms).toHaveLength(1);
    expect(rooms[0].role).toBe('staff');
    expect(rooms[0].session).toMatchObject({ sessionId: 's9', token: 'T9', role: 'staff' });
    expect($('ls-list').hidden).toBe(true);
    expect($('ls-customer').hidden).toBe(true);
    await tick(10_000);
    expect(api.myDesks).not.toHaveBeenCalled();
  });

  it('a conversation that is over clears the seat and shows the desks', async () => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify(STORED));
    api.getSession.mockResolvedValue(ok({ session: { status: 'ended' }, you: { role: 'staff' } }));
    await mount();
    expect(rooms).toHaveLength(0);
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
    expect($('ls-list').hidden).toBe(false);
  });

  it('a refused token is the same: back to the desks', async () => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify(STORED));
    api.getSession.mockResolvedValue(fail(410, 'session_ended'));
    await mount();
    expect(rooms).toHaveLength(0);
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it('ignores a stored guest or host session', async () => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify({ ...STORED, role: 'guest' }));
    await mount();
    expect(api.getSession).not.toHaveBeenCalled();
    expect($('ls-list').hidden).toBe(false);
  });
});

describe('secrets', () => {
  it('nothing is ever logged', async () => {
    setWorld([desk()], { d1: [cust('a')] });
    await mount();
    answerBtn('a').click();
    await flush();
    $('ls-answer-go').click();
    await flush();
    for (const spy of consoleSpies) expect(spy).not.toHaveBeenCalled();
  });

  it('the account token is not involved here, and the live token goes only to the room and the stored session', async () => {
    setWorld([desk()], { d1: [cust('a')] });
    await mount();
    answerBtn('a').click();
    await flush();
    $('ls-answer-go').click();
    await flush();
    expect(JSON.stringify(Object.entries(localStorage))).not.toContain('STAFF-LIVE');
    expect(document.body.innerHTML).not.toContain('STAFF-LIVE');
    expect(location.href).not.toContain('STAFF-LIVE');
  });
});

describe('the ?answer= deep link (from the dashboard Desk console)', () => {
  const ID = '11111111-2222-3333-4444-555555555555';
  const arrive = (q: string) => window.history.replaceState({}, '', `/live/desks${q}`);
  afterEach(() => window.history.replaceState({}, '', '/'));

  it('opens the language choice for exactly that customer, without answering for the member', async () => {
    setWorld([desk()], { d1: [cust('other', 'Zoe'), cust(ID, 'Marco', 'es')] });
    arrive(`?answer=${ID}`);
    await mount();
    expect($('ls-answer').hidden).toBe(false);
    expect($('ls-answer-title').textContent).toBe('Answer Marco');
    expect(api.answerSession).not.toHaveBeenCalled();
  });

  it('removes the parameter, so a reload does not repeat it and the id does not stay in the history', async () => {
    setWorld([desk()], { d1: [cust(ID)] });
    arrive(`?answer=${ID}&keep=1`);
    await mount();
    expect(window.location.search).toBe('?keep=1');
  });

  it('says the customer left when they are not waiting any more, and opens nothing', async () => {
    setWorld([desk()], { d1: [cust('someone-else')] });
    arrive(`?answer=${ID}`);
    await mount();
    expect($('ls-answer').hidden).toBe(true);
    expect($('ls-error').textContent).toBe(EN.liveStaffErrGone);
  });

  it('ignores a value that is not a session id — no panel, no error, still stripped', async () => {
    setWorld([desk()], { d1: [cust('a')] });
    for (const bad of ['a', '../../x', '<script>', '1111']) {
      document.body.innerHTML = pageMarkup();
      arrive(`?answer=${encodeURIComponent(bad)}`);
      await mount();
      expect($('ls-answer').hidden, bad).toBe(true);
      expect($('ls-error').hidden, bad).toBe(true);
      expect(window.location.search, bad).toBe('');
    }
  });

  it('opens once: after the member cancels, the next poll does not open it again', async () => {
    setWorld([desk()], { d1: [cust(ID)] });
    arrive(`?answer=${ID}`);
    await mount();
    ($('ls-answer-cancel') as HTMLButtonElement).click();
    await tick();
    expect($('ls-answer').hidden).toBe(true);
  });
});
