// @vitest-environment jsdom
//
// In-call bookmarks (spec 0013/0039): the module wires its DOM at import time,
// so every test resets the module registry, rebuilds the scaffold, and imports
// fresh. All API calls are mocked; timers are faked (auto-dismiss windows).
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { Bookmark } from './api';

const api = vi.hoisted(() => ({
  addBookmark: vi.fn(),
  deleteBookmark: vi.fn(),
  fetchBookmarks: vi.fn(),
  updateBookmarkLabel: vi.fn(),
}));
vi.mock('./api', () => api);

const auth = vi.hoisted(() => ({ isLoggedIn: vi.fn() }));
vi.mock('./auth', () => auth);

vi.mock('./i18n', () => ({ t: (key: string) => key }));
vi.mock('./icons', () => ({
  icon: (name: string) => `<i data-icon="${name}"></i>`,
}));

const SCAFFOLD = `
  <button id="btn-bookmark" hidden></button>
  <div id="bookmark-pop" class="hidden">
    <span id="bookmark-pop-title"></span>
    <button id="bookmark-pop-close"></button>
    <input id="bookmark-label-input" />
    <button id="bookmark-label-save"></button>
    <button id="bookmark-show-all"></button>
  </div>
  <aside id="bookmarks-panel" class="closed">
    <button id="bookmarks-close"></button>
    <div id="bookmarks-list"></div>
  </aside>
`;

const $ = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;
const btn = () => $<HTMLButtonElement>('btn-bookmark');
const pop = () => $('bookmark-pop');
const title = () => $('bookmark-pop-title');
const input = () => $<HTMLInputElement>('bookmark-label-input');
const save = () => $<HTMLButtonElement>('bookmark-label-save');
const showAll = () => $<HTMLButtonElement>('bookmark-show-all');
const panel = () => $('bookmarks-panel');
const list = () => $('bookmarks-list');

/** Flush pending microtasks (mocked promises resolve without real timers). */
const flush = async (): Promise<void> => {
  for (let i = 0; i < 10; i++) await Promise.resolve();
};

function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

const MINE: Bookmark = {
  id: 'b1',
  ts: '2026-01-01T10:05:00.000Z',
  label: 'Old',
  by: 'Me',
  mine: true,
};
const THEIRS: Bookmark = {
  id: 'b2',
  ts: '2026-01-01T10:06:00.000Z',
  label: null,
  by: 'Ann',
  mine: false,
};

async function load() {
  return import('./bookmarks');
}

const keydown = (el: HTMLElement, key: string) =>
  el.dispatchEvent(new KeyboardEvent('keydown', { key, cancelable: true }));

beforeEach(() => {
  vi.resetModules();
  vi.clearAllMocks();
  vi.useFakeTimers();
  document.body.innerHTML = SCAFFOLD;
  auth.isLoggedIn.mockReturnValue(true);
  api.addBookmark.mockResolvedValue(null);
  api.deleteBookmark.mockResolvedValue(false);
  api.fetchBookmarks.mockResolvedValue([]);
  api.updateBookmarkLabel.mockResolvedValue(false);
});

afterEach(() => {
  vi.useRealTimers();
});

describe('wiring + session gating', () => {
  it('renders the close icon into the popover X at import time', async () => {
    await load();
    expect($('bookmark-pop-close').innerHTML).toContain('data-icon="close"');
  });

  it('shows the 🔖 button only for a logged-in user with a session', async () => {
    const mod = await load();
    mod.setBookmarkSession('s1');
    expect(btn().hidden).toBe(false);
    mod.setBookmarkSession(null);
    expect(btn().hidden).toBe(true);
    auth.isLoggedIn.mockReturnValue(false);
    mod.setBookmarkSession('s1'); // guest: session id is discarded
    expect(btn().hidden).toBe(true);
  });

  it('clicking 🔖 without a session does nothing', async () => {
    const mod = await load();
    mod.setBookmarkSession(null);
    btn().click();
    expect(pop().classList.contains('hidden')).toBe(true);
  });

  it('a session change closes an open panel and clears the pins', async () => {
    api.fetchBookmarks.mockResolvedValue([MINE, THEIRS]);
    const mod = await load();
    mod.setBookmarkSession('s1');
    showAll().click();
    await flush();
    expect(panel().classList.contains('open')).toBe(true);
    expect(list().querySelectorAll('.bm-item')).toHaveLength(2);
    mod.setBookmarkSession('s2');
    expect(panel().classList.contains('open')).toBe(false);
    expect(panel().classList.contains('closed')).toBe(true);
    expect(list().querySelector('.bm-empty')?.textContent).toBe('bookmarksEmpty');
  });
});

describe('label-first pin prompt (spec 0039)', () => {
  it('opens the required-label prompt and auto-dismisses when left empty', async () => {
    const mod = await load();
    mod.setBookmarkSession('s1');
    btn().click();
    expect(pop().classList.contains('hidden')).toBe(false);
    expect(title().textContent).toBe('bookmarkLabelPrompt');
    expect(input().hidden).toBe(false);
    expect(input().value).toBe('');
    expect(document.activeElement).toBe(input());
    await vi.advanceTimersByTimeAsync(6000);
    expect(pop().classList.contains('hidden')).toBe(true);
  });

  it('a half-typed label disarms the auto-close', async () => {
    const mod = await load();
    mod.setBookmarkSession('s1');
    btn().click();
    input().value = 'draft';
    input().dispatchEvent(new Event('input'));
    await vi.advanceTimersByTimeAsync(10000);
    expect(pop().classList.contains('hidden')).toBe(false);
  });

  it('refuses to save an empty label: flags the prompt and keeps it open', async () => {
    const mod = await load();
    mod.setBookmarkSession('s1');
    btn().click();
    input().value = '   ';
    keydown(input(), 'Enter');
    await flush();
    expect(api.addBookmark).not.toHaveBeenCalled();
    expect(pop().classList.contains('bookmark-pop-error')).toBe(true);
    expect(pop().classList.contains('hidden')).toBe(false);
  });

  it('saves a labelled pin, renders it, confirms, then auto-dismisses', async () => {
    const d = deferred<Bookmark | null>();
    api.addBookmark.mockReturnValueOnce(d.promise);
    const mod = await load();
    mod.setBookmarkSession('s1');
    btn().click();
    input().value = ' Deal ';
    keydown(input(), 'Enter');
    await flush();
    expect(save().disabled).toBe(true); // in flight
    expect(api.addBookmark).toHaveBeenCalledWith('s1', {
      label: 'Deal',
      ts: expect.any(String),
    });
    d.resolve({ ...MINE, id: 'b9', label: 'Deal' });
    await flush();
    expect(save().disabled).toBe(false);
    expect(title().textContent).toBe('bookmarkAdded');
    expect(pop().classList.contains('bookmark-pop-error')).toBe(false);
    expect(input().hidden).toBe(true);
    expect(showAll().hidden).toBe(false);
    expect(list().querySelectorAll('.bm-item')).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(2000);
    expect(pop().classList.contains('hidden')).toBe(true);
  });

  it('reports a failed save (red, no show-all) and keeps the list unchanged', async () => {
    api.addBookmark.mockResolvedValue(null);
    const mod = await load();
    mod.setBookmarkSession('s1');
    btn().click();
    input().value = 'Deal';
    save().click();
    await flush();
    expect(title().textContent).toBe('bookmarkFailed');
    expect(pop().classList.contains('bookmark-pop-error')).toBe(true);
    expect(showAll().hidden).toBe(true);
    expect(list().querySelector('.bm-empty')).not.toBeNull();
    await vi.advanceTimersByTimeAsync(4000);
    expect(pop().classList.contains('hidden')).toBe(true);
  });

  it('Escape abandons the prompt: nothing saved, focus back on 🔖', async () => {
    const mod = await load();
    mod.setBookmarkSession('s1');
    btn().click();
    input().value = 'Deal';
    keydown(input(), 'Escape');
    expect(pop().classList.contains('hidden')).toBe(true);
    expect(document.activeElement).toBe(btn());
    // The pending moment was discarded — a later Save cannot pin it.
    input().value = 'Deal';
    save().click();
    await flush();
    expect(api.addBookmark).not.toHaveBeenCalled();
  });

  it('the X close discards the pending pin like Escape', async () => {
    const mod = await load();
    mod.setBookmarkSession('s1');
    btn().click();
    $('bookmark-pop-close').click();
    expect(pop().classList.contains('hidden')).toBe(true);
    expect(document.activeElement).toBe(btn());
  });
});

describe('side panel', () => {
  it('show-all opens the panel, pulls everyone’s pins, and relayouts', async () => {
    api.fetchBookmarks.mockResolvedValue([MINE, THEIRS]);
    const layout = vi.fn();
    const mod = await load();
    mod.initBookmarks({ layout });
    mod.setBookmarkSession('s1');
    btn().click();
    showAll().click();
    await flush();
    expect(pop().classList.contains('hidden')).toBe(true);
    expect(panel().classList.contains('open')).toBe(true);
    expect(api.fetchBookmarks).toHaveBeenCalledWith('s1');
    const rows = list().querySelectorAll('.bm-item');
    expect(rows).toHaveLength(2);
    // Own pin gets edit + delete; someone else's gets neither.
    expect(rows[0].querySelectorAll('button.bm-action')).toHaveLength(2);
    expect(rows[1].querySelectorAll('button.bm-action')).toHaveLength(0);
    // Unlabelled pin renders the placeholder style.
    expect(rows[1].querySelector('.bm-label-empty')?.textContent).toBe('bookmarkNoLabel');
    expect(layout).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(320);
    expect(layout).toHaveBeenCalledTimes(1);
  });

  it('keeps the current pins when the refresh fails', async () => {
    api.fetchBookmarks.mockResolvedValueOnce([MINE]).mockResolvedValueOnce(null);
    const mod = await load();
    mod.setBookmarkSession('s1');
    showAll().click();
    await flush();
    expect(list().querySelectorAll('.bm-item')).toHaveLength(1);
    $('bookmarks-close').click();
    showAll().click(); // second refresh → null
    await flush();
    expect(panel().classList.contains('open')).toBe(true);
    expect(list().querySelectorAll('.bm-item')).toHaveLength(1); // unchanged
  });

  it('the close button shuts the panel (default no-op relayout)', async () => {
    const mod = await load();
    mod.setBookmarkSession('s1');
    showAll().click();
    await flush();
    $('bookmarks-close').click();
    expect(panel().classList.contains('open')).toBe(false);
    // The default relayout is a no-op — advancing past the 320ms hook must not throw.
    await vi.advanceTimersByTimeAsync(320);
    expect(panel().classList.contains('closed')).toBe(true);
  });

  it('deletes an own pin and empties the list; a failed delete re-enables', async () => {
    api.fetchBookmarks.mockResolvedValue([MINE]);
    api.deleteBookmark.mockResolvedValueOnce(false).mockResolvedValueOnce(true);
    const mod = await load();
    mod.setBookmarkSession('s1');
    showAll().click();
    await flush();
    const del = () =>
      list().querySelectorAll<HTMLButtonElement>('.bm-item button.bm-action')[1];
    del().click();
    await flush();
    expect(api.deleteBookmark).toHaveBeenCalledWith('s1', 'b1');
    expect(list().querySelectorAll('.bm-item')).toHaveLength(1); // failed → kept
    expect(del().disabled).toBe(false);
    del().click();
    await flush();
    expect(list().querySelectorAll('.bm-item')).toHaveLength(0);
    expect(list().querySelector('.bm-empty')).not.toBeNull();
  });

  it('deleting the freshly saved pin clears the last-pin reference', async () => {
    api.addBookmark.mockResolvedValue({ ...MINE, id: 'b9', label: 'Deal' });
    api.deleteBookmark.mockResolvedValue(true);
    const mod = await load();
    mod.setBookmarkSession('s1');
    btn().click();
    input().value = 'Deal';
    keydown(input(), 'Enter');
    await flush();
    const del = list().querySelectorAll<HTMLButtonElement>('.bm-item button.bm-action')[1];
    del.click();
    await flush();
    expect(api.deleteBookmark).toHaveBeenCalledWith('s1', 'b9');
    expect(list().querySelector('.bm-empty')).not.toBeNull();
  });
});

describe('inline label edit', () => {
  async function openWithMine() {
    api.fetchBookmarks.mockResolvedValue([{ ...MINE }]);
    const mod = await load();
    mod.setBookmarkSession('s1');
    showAll().click();
    await flush();
    const edit = list().querySelectorAll<HTMLButtonElement>('.bm-item button.bm-action')[0];
    edit.click();
    return list().querySelector<HTMLInputElement>('.bm-edit-input') as HTMLInputElement;
  }

  it('replaces the label with a prefilled input', async () => {
    const field = await openWithMine();
    expect(field).not.toBeNull();
    expect(field.value).toBe('Old');
    expect(document.activeElement).toBe(field);
  });

  it('Enter saves the trimmed label through the API', async () => {
    api.updateBookmarkLabel.mockResolvedValue(true);
    const field = await openWithMine();
    field.value = '  New  ';
    keydown(field, 'Enter');
    await flush();
    expect(api.updateBookmarkLabel).toHaveBeenCalledWith('s1', 'b1', 'New');
    expect(list().querySelector('.bm-label')?.textContent).toBe('New');
  });

  it('Escape reverts without saving', async () => {
    const field = await openWithMine();
    field.value = 'New';
    keydown(field, 'Escape');
    await flush();
    expect(api.updateBookmarkLabel).not.toHaveBeenCalled();
    expect(list().querySelector('.bm-label')?.textContent).toBe('Old');
  });

  it('an emptied label is never saved — it just reverts (spec 0039)', async () => {
    const field = await openWithMine();
    field.value = '   ';
    keydown(field, 'Enter');
    await flush();
    expect(api.updateBookmarkLabel).not.toHaveBeenCalled();
    expect(list().querySelector('.bm-label')?.textContent).toBe('Old');
  });

  it('blur saves once; the Enter→blur double-fire is guarded', async () => {
    api.updateBookmarkLabel.mockResolvedValue(true);
    const field = await openWithMine();
    field.value = 'Blur';
    keydown(field, 'Enter');
    field.dispatchEvent(new Event('blur'));
    await flush();
    expect(api.updateBookmarkLabel).toHaveBeenCalledTimes(1);
    expect(list().querySelector('.bm-label')?.textContent).toBe('Blur');
  });

  it('a failed update keeps the old label', async () => {
    api.updateBookmarkLabel.mockResolvedValue(false);
    const field = await openWithMine();
    field.value = 'New';
    field.dispatchEvent(new Event('blur'));
    await flush();
    expect(list().querySelector('.bm-label')?.textContent).toBe('Old');
  });
});
