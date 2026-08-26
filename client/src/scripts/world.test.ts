// @vitest-environment jsdom
// /world discovery controller. The network and the locale glue are mocked, so these
// tests cover the wiring: which rooms get painted, how a batch is refreshed, what a
// full room looks like, and where a join navigates.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const track = vi.fn();
vi.mock('./analytics', () => ({
  initAnalytics: vi.fn(),
  track: (...args: unknown[]) => track(...args),
}));

vi.mock('./auth', () => ({
  HTTP_BASE: 'http://api.test',
  // fillAvatar() asks auth for a sized avatar URL; no picture → gradient + initials.
  avatarUrl: () => null,
}));

vi.mock('./icons', () => ({ icon: () => '' }));

// `t` returns the key so assertions read as keys — EXCEPT the one string carrying a
// placeholder, which returns its real English value so the `{n}` substitution is
// actually exercised rather than trivially passing.
vi.mock('./i18n', async (importOriginal) => ({
  ...(await importOriginal<typeof import('./i18n')>()),
  t: (k: string) => (k === 'worldPeopleTalking' ? '{n} people are talking' : k),
  applyI18n: vi.fn(),
  setUiLang: vi.fn(),
  loadLocale: vi.fn(async () => {}),
  resolveStoredLang: () => 'en',
}));

import { mountWorld } from './world';

type Seed = { room: string; count: number; langs?: string[] };

const roomsPayload = (seeds: Seed[]) => ({
  rooms: seeds.map((s) => ({
    room: s.room,
    count: s.count,
    participants: Array.from({ length: s.count }, (_, i) => ({
      name: `${s.room}-${i}`,
      lang: (s.langs ?? ['it'])[i % (s.langs ?? ['it']).length],
      avatar: null,
    })),
  })),
});

let respond: () => unknown;
const fetchMock = vi.fn(async () => ({ ok: true, json: async () => respond() }));

function buildDom(): void {
  document.body.innerHTML = `
    <main class="world">
      <div id="wr-skeleton"></div>
      <div id="wr-list" hidden></div>
      <button id="wr-more" hidden></button>
      <section id="wr-empty" hidden>
        <button data-world-create></button>
      </section>
      <section id="wr-nudge" hidden>
        <button data-world-create></button>
      </section>
    </main>`;
}

const el = (id: string) => document.getElementById(id)!;
const cards = () => [...document.querySelectorAll<HTMLButtonElement>('.wr-card')];
const codes = () => cards().map((c) => c.dataset.room);

/** Let the mocked fetch promise chain settle. */
const settle = () => new Promise((r) => setTimeout(r, 0));

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  track.mockClear();
  fetchMock.mockClear();
  respond = () => roomsPayload([]);
  vi.stubGlobal('fetch', fetchMock);
  buildDom();
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  document.body.innerHTML = '';
});

describe('mountWorld', () => {
  it('reports the page view once', async () => {
    mountWorld();
    await settle();
    expect(track).toHaveBeenCalledWith('world_page_viewed');
  });

  it('shows the invitation, not a bare list, when nothing is open', async () => {
    mountWorld();
    await settle();
    expect(el('wr-empty').hidden).toBe(false);
    expect(el('wr-list').hidden).toBe(true);
    // No competing CTAs while the empty state carries its own.
    expect(el('wr-more').hidden).toBe(true);
    expect(el('wr-nudge').hidden).toBe(true);
  });

  it('paints at most five conversations and hides the empty state', async () => {
    respond = () =>
      roomsPayload(Array.from({ length: 9 }, (_, i) => ({ room: `r${i}`, count: (i % 3) + 1 })));
    mountWorld();
    await settle();
    expect(cards()).toHaveLength(5);
    expect(el('wr-empty').hidden).toBe(true);
    expect(el('wr-list').hidden).toBe(false);
    expect(el('wr-more').hidden).toBe(false);
    expect(el('wr-nudge').hidden).toBe(false);
  });

  it('never lists an empty or a full room', async () => {
    respond = () =>
      roomsPayload([
        { room: 'ghost', count: 0 },
        { room: 'full', count: 4 },
        { room: 'live', count: 2 },
      ]);
    mountWorld();
    await settle();
    expect(codes()).toEqual(['live']);
  });

  it('describes a room in people, not in room records', async () => {
    respond = () => roomsPayload([{ room: 'solo', count: 1 }, { room: 'trio', count: 3 }]);
    mountWorld();
    await settle();
    const labels = cards().map((c) => c.querySelector('.wr-people')!.textContent);
    expect(labels).toContain('worldOnePersonTalking');
    expect(labels).toContain('3 people are talking');
    // The room code is never rendered as copy.
    expect(document.body.textContent).not.toContain('trio');
  });

  it('lists the distinct languages in the room and skips pending detection', async () => {
    respond = () => roomsPayload([{ room: 'mixed', count: 3, langs: ['it', 'it', 'auto'] }]);
    mountWorld();
    await settle();
    const chips = [...document.querySelectorAll('.wr-langs .chip')].map((c) => c.textContent);
    expect(chips).toHaveLength(1);
    expect(chips[0]).toContain('Italiano');
  });

  it('keeps a room that filled up on screen but stops offering it', async () => {
    respond = () => roomsPayload([{ room: 'busy', count: 2 }]);
    mountWorld();
    await settle();
    expect(cards()[0].disabled).toBe(false);

    respond = () => roomsPayload([{ room: 'busy', count: 4 }]);
    await vi.advanceTimersByTimeAsync(3000);
    await settle();

    // Still visible — removing it would make the page jump — but no longer joinable.
    expect(codes()).toEqual(['busy']);
    expect(cards()[0].disabled).toBe(true);
    expect(cards()[0].querySelector('.wr-cta')!.textContent).toBe('roomFull');
  });

  it('replaces a room that ended, keeping the batch populated', async () => {
    respond = () =>
      roomsPayload([
        { room: 'a', count: 1 },
        { room: 'b', count: 1 },
      ]);
    mountWorld();
    await settle();
    expect(codes().sort()).toEqual(['a', 'b']);

    respond = () =>
      roomsPayload([
        { room: 'b', count: 1 },
        { room: 'c', count: 2 },
      ]);
    await vi.advanceTimersByTimeAsync(3000);
    await settle();
    expect(codes().sort()).toEqual(['b', 'c']);
  });

  it('keeps the last render when the network fails', async () => {
    respond = () => roomsPayload([{ room: 'a', count: 2 }]);
    mountWorld();
    await settle();
    expect(codes()).toEqual(['a']);

    fetchMock.mockImplementationOnce(async () => {
      throw new Error('offline');
    });
    await vi.advanceTimersByTimeAsync(3000);
    await settle();
    expect(codes()).toEqual(['a']);
  });

  it('asks for a different batch on "show me other conversations"', async () => {
    respond = () =>
      roomsPayload(Array.from({ length: 10 }, (_, i) => ({ room: `r${i}`, count: 2 })));
    mountWorld();
    await settle();
    const first = codes();

    el('wr-more').click();
    await settle();

    expect(track).toHaveBeenCalledWith('public_room_discovery_refresh');
    expect(codes()).toHaveLength(5);
    // Fresh rooms exist, so none of the previous batch comes back.
    expect(codes().some((c) => first.includes(c))).toBe(false);
  });

  it('hands a join to the SPA deep link', async () => {
    respond = () => roomsPayload([{ room: 'plaza', count: 2 }]);
    mountWorld();
    await settle();

    const assign = vi.fn();
    Object.defineProperty(window, 'location', {
      configurable: true,
      value: { origin: 'https://app.test', set href(v: string) { assign(v); } },
    });

    cards()[0].click();
    expect(track).toHaveBeenCalledWith('public_room_join_clicked', { occupancy: 2 });
    expect(assign).toHaveBeenCalledWith('https://app.test/?room=plaza');
  });

  it('routes "start a public conversation" through the existing create flow', async () => {
    mountWorld();
    await settle();

    const assign = vi.fn();
    Object.defineProperty(window, 'location', {
      configurable: true,
      value: { origin: 'https://app.test', set href(v: string) { assign(v); } },
    });

    el('wr-empty').querySelector<HTMLButtonElement>('[data-world-create]')!.click();
    expect(track).toHaveBeenCalledWith('public_room_created_from_world');
    expect(assign).toHaveBeenCalledWith('/?public=1');
  });

  it('stops polling while the tab is hidden', async () => {
    respond = () => roomsPayload([{ room: 'a', count: 2 }]);
    mountWorld();
    await settle();
    const afterMount = fetchMock.mock.calls.length;

    Object.defineProperty(document, 'hidden', { configurable: true, value: true });
    document.dispatchEvent(new Event('visibilitychange'));
    await vi.advanceTimersByTimeAsync(9000);
    expect(fetchMock.mock.calls.length).toBe(afterMount);

    Object.defineProperty(document, 'hidden', { configurable: true, value: false });
    document.dispatchEvent(new Event('visibilitychange'));
    await settle();
    expect(fetchMock.mock.calls.length).toBeGreaterThan(afterMount);
  });
});
