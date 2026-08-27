// "Talk to the World" — public conversation discovery (/world).
//
// A deliberately lean page: it never imports app.ts, so opening the discovery
// experience does not pay for the whole call client. Joining hands the room back
// to the SPA as a full navigation to `/?room=CODE` — the same deep link invites
// and scheduled meetings already use — so there is exactly one join path.

import { initAnalytics, track } from './analytics';
import { fillAvatar } from './avatar';
import { HTTP_BASE } from './auth';
import { DISCOVERY_LIMIT, MAX_ROOM, pick, roomLanguages, type DiscoveryRoom } from './discovery';
import { applyI18n, ENDONYM, FLAG, loadLocale, resolveStoredLang, setUiLang, t } from './i18n';
import { icon } from './icons';
import { buildInviteLink } from './invite';

/** Matches the lobby's cadence in app.ts — occupancy is polled, not pushed. */
const POLL_MS = 3000;

const $ = (id: string): HTMLElement => document.getElementById(id)!;

/** Latest snapshot of every live public room. */
let latest: DiscoveryRoom[] = [];
/** Room codes currently on screen, in display order. */
let shown: string[] = [];
/** Card elements by room code, so a poll updates in place instead of repainting. */
const cards = new Map<string, HTMLButtonElement>();
let pollTimer: number | null = null;

/** Fetch the live public rooms. Returns null on failure so the caller keeps the last render. */
async function fetchRooms(): Promise<DiscoveryRoom[] | null> {
  try {
    const res = await fetch(`${HTTP_BASE}/rooms`, { cache: 'no-store' });
    if (!res.ok) return null;
    const data = (await res.json()) as { rooms?: DiscoveryRoom[] };
    return data.rooms ?? [];
  } catch {
    return null;
  }
}

/** The rooms behind the codes currently on screen, in order, dropping any that vanished. */
function displayed(): DiscoveryRoom[] {
  const byCode = new Map(latest.map((r) => [r.room, r]));
  return shown.map((code) => byCode.get(code)).filter((r): r is DiscoveryRoom => !!r);
}

function peopleLabel(count: number): string {
  return count === 1
    ? t('worldOnePersonTalking')
    : t('worldPeopleTalking').replace('{n}', String(count));
}

function buildCard(room: DiscoveryRoom): HTMLButtonElement {
  const el = document.createElement('button');
  el.type = 'button';
  el.className = 'wr-card';
  el.dataset.room = room.room;

  const head = document.createElement('div');
  head.className = 'wr-head';
  const people = document.createElement('span');
  people.className = 'wr-people';
  const count = document.createElement('span');
  count.className = 'wr-count';
  head.append(people, count);

  const avatars = document.createElement('div');
  avatars.className = 'wr-avatars';

  const langs = document.createElement('div');
  langs.className = 'wr-langs';

  const cta = document.createElement('span');
  cta.className = 'wr-cta';

  el.append(head, avatars, langs, cta);
  el.addEventListener('click', () => join(room.room, cards.get(room.room)?.dataset.count));
  return el;
}

/** Write a room's live state into its card. Called on first paint and on every poll. */
function paintCard(el: HTMLButtonElement, room: DiscoveryRoom): void {
  const full = room.count >= MAX_ROOM;
  el.dataset.count = String(room.count);
  // A room that filled up while you were looking at it stays visible — it just stops
  // being an invitation. Removing it would make the page jump under the pointer.
  el.disabled = full;
  el.classList.toggle('wr-full', full);

  el.querySelector('.wr-people')!.textContent = peopleLabel(room.count);
  // Occupancy is stated in words AND as a ratio: never colour alone.
  el.querySelector('.wr-count')!.innerHTML = `${icon('users', 13)} ${room.count} / ${MAX_ROOM}`;

  const avatars = el.querySelector<HTMLElement>('.wr-avatars')!;
  avatars.textContent = '';
  for (const m of room.participants) {
    const av = document.createElement('span');
    av.className = 'wr-av';
    av.title = m.name;
    fillAvatar(av, m.name, m.avatar, 56, 1);
    avatars.appendChild(av);
  }

  const langs = el.querySelector<HTMLElement>('.wr-langs')!;
  langs.textContent = '';
  for (const code of roomLanguages(room)) {
    const chip = document.createElement('span');
    chip.className = 'chip';
    chip.textContent = `${FLAG[code] ?? ''} ${ENDONYM[code] ?? code}`.trim();
    langs.appendChild(chip);
  }

  el.querySelector('.wr-cta')!.textContent = full ? t('roomFull') : t('worldJoin');
}

/** Reconcile the DOM with `shown` — add, remove and update, but never blank the list. */
function paint(): void {
  const list = $('wr-list');
  const rooms = displayed();
  const any = rooms.length > 0;

  // The empty state carries its own "start one" CTA, so the nudge and the
  // show-me-more control stand down rather than stacking three CTAs on a bare page.
  list.hidden = !any;
  $('wr-empty').hidden = any;
  $('wr-nudge').hidden = !any;
  $('wr-more').hidden = !any;

  for (const [code, el] of cards) {
    if (!shown.includes(code)) {
      el.remove();
      cards.delete(code);
    }
  }

  rooms.forEach((room, i) => {
    let el = cards.get(room.room);
    if (!el) {
      el = buildCard(room);
      cards.set(room.room, el);
    }
    paintCard(el, room);
    // Keep DOM order in step with display order without rebuilding the list.
    if (list.children[i] !== el) list.insertBefore(el, list.children[i] ?? null);
  });
}

function setLoading(on: boolean): void {
  // Skeletons stand in for content we do not have yet — only on the first load.
  // Once cards exist, a later batch shows a spinner on the button and leaves the
  // current conversations on screen, so the page never blanks under the reader.
  $('wr-skeleton').hidden = !on || cards.size > 0;
  $('wr-list').setAttribute('aria-busy', String(on));
  const more = $('wr-more') as HTMLButtonElement;
  more.classList.toggle('btn-loading', on);
  more.disabled = on;
}

/**
 * Draw a new batch. `fresh` excludes what is already on screen so "show me other
 * conversations" actually shows other conversations; the first load has nothing
 * to exclude.
 */
async function drawBatch(fresh: boolean): Promise<void> {
  setLoading(true);
  const rooms = await fetchRooms();
  setLoading(false);
  if (rooms) latest = rooms;
  const exclude = fresh ? new Set(shown) : new Set<string>();
  shown = pick(latest, { limit: DISCOVERY_LIMIT, exclude }).map((r) => r.room);
  paint();
}

/**
 * Keep the current batch honest between refreshes: update occupancy in place, drop
 * rooms that ended, and top the batch back up so the page does not slowly empty out.
 */
async function poll(): Promise<void> {
  const rooms = await fetchRooms();
  if (!rooms) return;
  latest = rooms;
  const live = new Set(rooms.map((r) => r.room));
  const kept = shown.filter((code) => live.has(code));
  if (kept.length < DISCOVERY_LIMIT) {
    const top = pick(latest, {
      limit: DISCOVERY_LIMIT - kept.length,
      exclude: new Set(kept),
    });
    kept.push(...top.map((r) => r.room));
  }
  shown = kept;
  paint();
}

/** Hand the room to the SPA, which owns pre-join, the guest sign-in gate and the call. */
function join(room: string, occupancy?: string): void {
  track('public_room_join_clicked', { occupancy: Number(occupancy) || 0 });
  location.href = buildInviteLink(location.origin, room);
}

function startPublic(): void {
  track('public_room_created_from_world');
  location.href = '/?public=1';
}

export function mountWorld(): void {
  const lang = resolveStoredLang();
  setUiLang(lang);
  applyI18n();
  void loadLocale(lang).then(applyI18n);

  initAnalytics();
  track('world_page_viewed');

  $('wr-more').addEventListener('click', () => {
    track('public_room_discovery_refresh');
    void drawBatch(true);
  });
  for (const el of document.querySelectorAll('[data-world-create]')) {
    el.addEventListener('click', startPublic);
  }

  void drawBatch(false);
  pollTimer = window.setInterval(() => void poll(), POLL_MS);
  // Stop polling while the tab is hidden — nobody is reading a background page.
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) {
      if (pollTimer) clearInterval(pollTimer);
      pollTimer = null;
    } else if (!pollTimer) {
      void poll();
      pollTimer = window.setInterval(() => void poll(), POLL_MS);
    }
  });
}
