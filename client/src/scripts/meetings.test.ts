// @vitest-environment jsdom
//
// Personal scheduled meetings (spec: scheduled meetings, Phase 1e): the home
// card list, the create modal, the custom confirm dialog and the join-room
// bridge. The module keeps state (`wired`, `T`), so each test re-imports it
// fresh after building the DOM. fetch + auth + analytics are mocked.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { Meeting } from './meetings';

const authState = vi.hoisted(() => ({ loggedIn: true }));
vi.mock('./auth', () => ({
  authHeaders: () => ({ Authorization: 'Bearer tok' }),
  HTTP_BASE: 'http://test',
  isLoggedIn: () => authState.loggedIn,
}));

const analytics = vi.hoisted(() => ({ track: vi.fn() }));
vi.mock('./analytics', () => ({ track: analytics.track }));

function okJson(body: unknown, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
    text: async () => JSON.stringify(body),
  } as unknown as Response;
}

const settle = (): Promise<void> => new Promise((r) => setTimeout(r, 0));
const el = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;

const meeting = (over: Partial<Meeting> = {}): Meeting => ({
  id: 'm1',
  title: 'Standup',
  scheduled_at: '2026-07-03T10:00:00Z',
  end_at: '2026-07-03T10:30:00Z',
  join_url: 'http://test/j/abc',
  room_code: 'ab-cd',
  status: 'scheduled',
  ...over,
});

const FULL_MARKUP = `
  <div id="schedule-card" class="hidden">
    <button id="schedule-open"></button>
    <div id="schedule-list"></div>
  </div>
  <div id="schedule-modal" class="hidden">
    <p id="sm-err" class="hidden"></p>
    <input id="sm-title" />
    <input id="sm-start" />
    <input id="sm-duration" />
    <input id="sm-emails" />
    <select id="sm-repeat">
      <option value="" selected></option>
      <option value="weekly">weekly</option>
      <option value="daily">daily</option>
    </select>
    <div id="sm-count-wrap" class="hidden"><input id="sm-count" /></div>
    <button id="sm-create"></button>
    <button id="sm-cancel-btn"></button>
    <button id="sm-cancel-btn2"></button>
  </div>
  <div id="confirm-modal" class="hidden">
    <p id="confirm-text"></p>
    <button id="confirm-yes"></button>
    <button id="confirm-no"></button>
  </div>
  <input id="room" />
  <button id="enter"></button>
`;

const fetchMock = vi.fn<(url: string, init?: RequestInit) => Promise<Response>>();

type MeetingsModule = typeof import('./meetings');

/** Build the DOM, route GET /api/meetings to `meetings`, import + wire fresh. */
async function setup(markup = FULL_MARKUP, meetings: Meeting[] = []): Promise<MeetingsModule> {
  document.body.innerHTML = markup;
  fetchMock.mockImplementation(async () => okJson(meetings));
  const mod = await import('./meetings');
  mod.setupScheduling((k) => k);
  await settle();
  return mod;
}

beforeEach(() => {
  vi.resetModules();
  authState.loggedIn = true;
  analytics.track.mockReset();
  fetchMock.mockReset();
  vi.stubGlobal('fetch', fetchMock);
  sessionStorage.clear();
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('setupScheduling gating', () => {
  it('no-ops when the card markup is absent', async () => {
    document.body.innerHTML = '<div></div>';
    const mod = await import('./meetings');
    mod.setupScheduling((k) => k);
    await settle();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('hides the card for guests and skips the list fetch', async () => {
    document.body.innerHTML = FULL_MARKUP;
    el('schedule-card').classList.remove('hidden');
    authState.loggedIn = false;
    const mod = await import('./meetings');
    mod.setupScheduling((k) => k);
    await settle();
    expect(el('schedule-card').classList.contains('hidden')).toBe(true);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('tolerates a card without a list element', async () => {
    document.body.innerHTML = '<div id="schedule-card" class="hidden"></div>';
    const mod = await import('./meetings');
    mod.setupScheduling((k) => k);
    await settle();
    expect(el('schedule-card').classList.contains('hidden')).toBe(false);
    expect(fetchMock).not.toHaveBeenCalled();
  });
});

describe('meeting list', () => {
  it('renders upcoming meetings, filtering cancelled ones', async () => {
    await setup(FULL_MARKUP, [meeting(), meeting({ id: 'm2', status: 'cancelled' })]);
    expect(fetchMock).toHaveBeenCalledWith('http://test/api/meetings', {
      headers: { Authorization: 'Bearer tok' },
    });
    const items = document.querySelectorAll('.schedule-item');
    expect(items).toHaveLength(1);
    expect(items[0].querySelector('.schedule-item-title')?.textContent).toBe('Standup');
    expect(items[0].querySelector('.schedule-item-when')?.textContent).toBe(
      new Date('2026-07-03T10:00:00Z').toLocaleString(),
    );
  });

  it('escapes HTML in meeting titles', async () => {
    await setup(FULL_MARKUP, [meeting({ title: '<img src=x>&"' })]);
    // jsdom re-serializes &quot; as a literal " inside text nodes; the tag and
    // ampersand escapes are what block markup injection.
    expect(el('schedule-list').innerHTML).toContain('&lt;img src=x&gt;&amp;');
    expect(el('schedule-list').innerHTML).not.toContain('<img');
    expect(document.querySelector('.schedule-item-title')?.textContent).toBe('<img src=x>&"');
  });

  it('shows the empty state when there are no meetings', async () => {
    await setup(FULL_MARKUP, []);
    expect(document.querySelector('.schedule-empty')?.textContent).toBe('scheduleEmpty');
  });

  it('degrades to the empty state on API errors and offline', async () => {
    document.body.innerHTML = FULL_MARKUP;
    fetchMock.mockResolvedValue(okJson('x', 500));
    let mod = await import('./meetings');
    mod.setupScheduling((k) => k);
    await settle();
    expect(document.querySelector('.schedule-empty')).toBeTruthy();

    vi.resetModules();
    document.body.innerHTML = FULL_MARKUP;
    fetchMock.mockRejectedValue(new Error('net'));
    mod = await import('./meetings');
    mod.setupScheduling((k) => k);
    await settle();
    expect(document.querySelector('.schedule-empty')).toBeTruthy();
  });
});

describe('joining a meeting', () => {
  it('prefills the room field, tags the join source and clicks connect', async () => {
    await setup(FULL_MARKUP, [meeting({ room_code: 'xy-z9' })]);
    const enterClicked = vi.fn();
    el('enter').addEventListener('click', enterClicked);
    document.querySelector<HTMLButtonElement>('.join-meet')!.click();
    expect(sessionStorage.getItem('vox_join_src')).toBe('meeting');
    expect(el<HTMLInputElement>('room').value).toBe('xy-z9');
    expect(enterClicked).toHaveBeenCalledTimes(1);
  });

  it('still joins when sessionStorage is blocked (private mode)', async () => {
    await setup(FULL_MARKUP, [meeting({ room_code: 'xy-z9' })]);
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    document.querySelector<HTMLButtonElement>('.join-meet')!.click();
    expect(el<HTMLInputElement>('room').value).toBe('xy-z9');
  });

  it('falls back to a full navigation when the home controls are absent', async () => {
    const markup = FULL_MARKUP.replace('<input id="room" />', '').replace(
      '<button id="enter"></button>',
      '',
    );
    await setup(markup, [meeting({ room_code: 'xy-z9' })]);
    // jsdom can't actually navigate; the click must not throw and the join
    // source must still be tagged for analytics.
    document.querySelector<HTMLButtonElement>('.join-meet')!.click();
    expect(sessionStorage.getItem('vox_join_src')).toBe('meeting');
  });
});

describe('cancelling a meeting', () => {
  it('asks via the custom dialog and cancels + re-renders on confirm', async () => {
    await setup(FULL_MARKUP, [meeting()]);
    fetchMock.mockImplementation(async (url, init) => {
      if (init?.method === 'POST') {
        expect(url).toBe('http://test/api/meetings/m1/cancel');
        return okJson({});
      }
      return okJson([]);
    });
    document.querySelector<HTMLButtonElement>('.sm-cancel')!.click();
    await settle();
    expect(el('confirm-modal').classList.contains('hidden')).toBe(false);
    expect(el('confirm-text').textContent).toBe('scheduleCancelConfirm');
    expect(el('confirm-yes').textContent).toBe('scheduleCancelMeeting');
    expect(el('confirm-no').textContent).toBe('scheduleKeep');
    el('confirm-yes').click();
    await settle();
    expect(el('confirm-modal').classList.contains('hidden')).toBe(true);
    const posts = fetchMock.mock.calls.filter(([, init]) => init?.method === 'POST');
    expect(posts).toHaveLength(1);
    expect(document.querySelector('.schedule-empty')).toBeTruthy(); // list re-rendered
  });

  it('keeps the meeting when the dialog is declined', async () => {
    await setup(FULL_MARKUP, [meeting()]);
    document.querySelector<HTMLButtonElement>('.sm-cancel')!.click();
    await settle();
    el('confirm-no').click();
    await settle();
    const posts = fetchMock.mock.calls.filter(([, init]) => init?.method === 'POST');
    expect(posts).toHaveLength(0);
    expect(document.querySelector('.schedule-item')).toBeTruthy();
  });

  it('falls back to window.confirm when the dialog markup is missing', async () => {
    const markup = FULL_MARKUP.replace(/<div id="confirm-modal"[\s\S]*?<\/div>/, '');
    await setup(markup, [meeting()]);
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    fetchMock.mockImplementation(async (_url, init) =>
      init?.method === 'POST' ? okJson({}) : okJson([]),
    );
    document.querySelector<HTMLButtonElement>('.sm-cancel')!.click();
    await settle();
    expect(window.confirm).toHaveBeenCalledWith('scheduleCancelConfirm');
    const posts = fetchMock.mock.calls.filter(([, init]) => init?.method === 'POST');
    expect(posts).toHaveLength(1);
  });

  it('does not re-render when the cancel call fails', async () => {
    await setup(FULL_MARKUP, [meeting()]);
    fetchMock.mockImplementation(async (_url, init) => {
      if (init?.method === 'POST') throw new Error('net');
      return okJson([]);
    });
    document.querySelector<HTMLButtonElement>('.sm-cancel')!.click();
    await settle();
    el('confirm-yes').click();
    await settle();
    expect(document.querySelector('.schedule-item')).toBeTruthy(); // still listed
  });
});

describe('create modal', () => {
  it('opens, toggles the occurrences field with the repeat select, and closes', async () => {
    await setup();
    el('schedule-open').click();
    expect(el('schedule-modal').classList.contains('hidden')).toBe(false);

    const sel = el<HTMLSelectElement>('sm-repeat');
    sel.value = 'weekly';
    sel.dispatchEvent(new Event('change'));
    expect(el('sm-count-wrap').classList.contains('hidden')).toBe(false);
    sel.value = '';
    sel.dispatchEvent(new Event('change'));
    expect(el('sm-count-wrap').classList.contains('hidden')).toBe(true);

    el('sm-cancel-btn').click();
    expect(el('schedule-modal').classList.contains('hidden')).toBe(true);
    expect(el('sm-err').classList.contains('hidden')).toBe(true);

    el('schedule-open').click();
    el('sm-cancel-btn2').click();
    expect(el('schedule-modal').classList.contains('hidden')).toBe(true);
  });

  it('requires a title and a start time', async () => {
    await setup();
    el<HTMLInputElement>('sm-title').value = '';
    el<HTMLInputElement>('sm-start').value = '2026-07-10T10:00';
    el('sm-create').click();
    await settle();
    expect(el('sm-err').textContent).toBe('scheduleErrRequired');
    expect(el('sm-err').classList.contains('hidden')).toBe(false);

    el<HTMLInputElement>('sm-title').value = 'Sync';
    el<HTMLInputElement>('sm-start').value = '';
    el('sm-create').click();
    await settle();
    expect(el('sm-err').textContent).toBe('scheduleErrRequired');
    const posts = fetchMock.mock.calls.filter(([, init]) => init?.method === 'POST');
    expect(posts).toHaveLength(0);
  });

  it('creates a recurring meeting with invitees and resets the form', async () => {
    await setup();
    fetchMock.mockImplementation(async (_url, init) =>
      init?.method === 'POST' ? okJson({ ...meeting(), invitees: [] }, 201) : okJson([]),
    );
    el<HTMLInputElement>('sm-title').value = ' Sync ';
    el<HTMLInputElement>('sm-start').value = '2026-07-10T10:00';
    el<HTMLInputElement>('sm-duration').value = '45';
    el<HTMLInputElement>('sm-emails').value = 'a@x.com, ,b@y.com';
    el<HTMLSelectElement>('sm-repeat').value = 'weekly';
    el<HTMLInputElement>('sm-count').value = '3';
    el('schedule-open').click();
    el('sm-create').click();
    await settle();

    const post = fetchMock.mock.calls.find(([, init]) => init?.method === 'POST');
    expect(post).toBeTruthy();
    expect(post![0]).toBe('http://test/api/meetings');
    expect(post![1]?.headers).toEqual({
      Authorization: 'Bearer tok',
      'Content-Type': 'application/json',
    });
    const body = JSON.parse(post![1]?.body as string) as Record<string, unknown>;
    expect(body.title).toBe('Sync');
    expect(body.scheduled_at).toBe(new Date('2026-07-10T10:00').toISOString());
    expect(body.duration_minutes).toBe(45);
    expect(typeof body.timezone).toBe('string');
    expect((body.timezone as string).length).toBeGreaterThan(0);
    expect(body.invitee_emails).toEqual(['a@x.com', 'b@y.com']);
    expect(body.recurrence).toEqual({ freq: 'weekly', count: 3 });

    expect(analytics.track).toHaveBeenCalledWith('schedule_meeting', { recurring: true });
    expect(el('schedule-modal').classList.contains('hidden')).toBe(true);
    expect(el<HTMLInputElement>('sm-title').value).toBe('');
    expect(el<HTMLInputElement>('sm-emails').value).toBe('');
    expect(el<HTMLSelectElement>('sm-repeat').value).toBe('');
    expect(el('sm-count-wrap').classList.contains('hidden')).toBe(true);
  });

  it('defaults the duration to 30, omits count<=0 and recurrence when one-off', async () => {
    await setup();
    fetchMock.mockImplementation(async (_url, init) =>
      init?.method === 'POST' ? okJson({ ...meeting(), invitees: [] }, 201) : okJson([]),
    );
    // One-off, no explicit duration → 30 min, no recurrence.
    el<HTMLInputElement>('sm-title').value = 'One-off';
    el<HTMLInputElement>('sm-start').value = '2026-07-10T10:00';
    el<HTMLInputElement>('sm-duration').value = '';
    el<HTMLInputElement>('sm-emails').value = '';
    el('sm-create').click();
    await settle();
    let body = JSON.parse(
      fetchMock.mock.calls.find(([, init]) => init?.method === 'POST')![1]?.body as string,
    ) as Record<string, unknown>;
    expect(body.duration_minutes).toBe(30);
    expect(body.invitee_emails).toEqual([]);
    expect(body.recurrence).toBeUndefined();
    expect(analytics.track).toHaveBeenCalledWith('schedule_meeting', { recurring: false });

    // Recurring without an occurrence count → recurrence carries only freq.
    fetchMock.mockClear();
    el<HTMLInputElement>('sm-title').value = 'Daily';
    el<HTMLInputElement>('sm-start').value = '2026-07-10T10:00';
    el<HTMLSelectElement>('sm-repeat').value = 'daily';
    el<HTMLInputElement>('sm-count').value = '';
    el('sm-create').click();
    await settle();
    body = JSON.parse(
      fetchMock.mock.calls.find(([, init]) => init?.method === 'POST')![1]?.body as string,
    ) as Record<string, unknown>;
    expect(body.recurrence).toEqual({ freq: 'daily' });
  });

  it('maps failures: 409 → connect calendar, other statuses and offline → save error', async () => {
    await setup();
    el<HTMLInputElement>('sm-title').value = 'Sync';
    el<HTMLInputElement>('sm-start').value = '2026-07-10T10:00';

    fetchMock.mockImplementation(async (_url, init) =>
      init?.method === 'POST' ? okJson({}, 409) : okJson([]),
    );
    el('sm-create').click();
    await settle();
    expect(el('sm-err').textContent).toBe('scheduleConnectCalendar');
    expect(el('sm-err').classList.contains('hidden')).toBe(false);

    fetchMock.mockImplementation(async (_url, init) =>
      init?.method === 'POST' ? okJson({}, 500) : okJson([]),
    );
    el('sm-create').click();
    await settle();
    expect(el('sm-err').textContent).toBe('scheduleErrSave');

    fetchMock.mockImplementation(async (_url, init) => {
      if (init?.method === 'POST') throw new Error('net');
      return okJson([]);
    });
    el('sm-create').click();
    await settle();
    expect(el('sm-err').textContent).toBe('scheduleErrSave');
    expect(analytics.track).not.toHaveBeenCalled();
    expect(el('schedule-modal').classList.contains('hidden')).toBe(true); // never opened
  });

  it('wires listeners only once across repeated setup calls', async () => {
    const mod = await setup();
    mod.setupScheduling((k) => k); // second home entry
    await settle();
    fetchMock.mockImplementation(async (_url, init) =>
      init?.method === 'POST' ? okJson({ ...meeting(), invitees: [] }, 201) : okJson([]),
    );
    el<HTMLInputElement>('sm-title').value = 'Sync';
    el<HTMLInputElement>('sm-start').value = '2026-07-10T10:00';
    el('sm-create').click();
    await settle();
    const posts = fetchMock.mock.calls.filter(([, init]) => init?.method === 'POST');
    expect(posts).toHaveLength(1); // duplicate listeners would double-POST
  });
});
