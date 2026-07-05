// @vitest-environment jsdom
//
// Room glossary editor (spec 0011): concept table rendering, language columns,
// A↔B pair generation, save/import/delete flows and the home/badge entry
// points. The module wires listeners at import time, so the DOM is mounted
// first and the module re-imported per test. api/auth/i18n/icons are mocked.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { Glossary, GlossaryEntry } from './api';

const authState = vi.hoisted(() => ({ loggedIn: true }));
vi.mock('./auth', () => ({ isLoggedIn: () => authState.loggedIn }));

const api = vi.hoisted(() => ({
  fetchGlossary: vi.fn(),
  saveGlossary: vi.fn(),
  importGlossaryCsv: vi.fn(),
  deleteGlossary: vi.fn(),
}));
vi.mock('./api', () => api);

// A small deterministic language set (real SUPPORTED is 84 locales).
vi.mock('./i18n', () => ({
  t: (k: string) => k,
  ENDONYM: { en: 'English', it: 'Italiano', fr: 'Français', de: 'Deutsch' },
  SUPPORTED: ['en', 'it', 'fr', 'de'],
}));

vi.mock('./icons', () => ({ icon: () => '<svg data-icon></svg>' }));

const MARKUP = `
  <input id="room" />
  <button id="btn-glossary-home" hidden></button>
  <button id="glossary-badge" class="hidden"></button>
  <div id="glossary-modal" class="hidden">
    <button id="glossary-close"></button>
    <input id="glossary-name" />
    <div id="glossary-lang-bar"></div>
    <div id="glossary-table-head"></div>
    <div id="glossary-rows"></div>
    <button id="glossary-add-row"></button>
    <span id="glossary-count"></span>
    <textarea id="glossary-csv-text"></textarea>
    <button id="glossary-import"></button>
    <span id="glossary-status"></span>
    <button id="glossary-delete"></button>
    <button id="glossary-save"></button>
  </div>
`;

const el = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;

/** Drain the microtask queue (and 0ms timers under fake timers). */
async function settle(): Promise<void> {
  if (vi.isFakeTimers()) await vi.advanceTimersByTimeAsync(0);
  else await new Promise<void>((r) => setTimeout(r, 0));
}

const pair = (sl: string, tl: string, st: string, tt: string): GlossaryEntry => ({
  source_lang: sl,
  target_lang: tl,
  source_term: st,
  target_term: tt,
});

const emptyGlossary = (over: Partial<Glossary> = {}): Glossary => ({
  name: null,
  entries: [],
  max_entries: 200,
  ...over,
});

type GlossaryModule = typeof import('./glossary');
let mod: GlossaryModule;

async function load(): Promise<void> {
  mod = await import('./glossary');
}

/** Open the editor for `room` through the home 📖 button. */
async function openEditor(room = 'abc', g: Glossary | null = emptyGlossary()): Promise<void> {
  api.fetchGlossary.mockResolvedValueOnce(g);
  el<HTMLInputElement>('room').value = room;
  el('btn-glossary-home').click();
  await settle();
}

const rows = (): HTMLElement[] =>
  Array.from(document.querySelectorAll<HTMLElement>('#glossary-rows .glossary-row'));
const rowValues = (i: number): string[] =>
  Array.from(rows()[i].querySelectorAll<HTMLInputElement>('input[data-lang]')).map((x) => x.value);
const headLabels = (): (string | null)[] =>
  Array.from(el('glossary-table-head').querySelectorAll('span')).map((s) => s.textContent);
const setCell = (row: number, lang: string, value: string): void => {
  rows()[row].querySelector<HTMLInputElement>(`input[data-lang="${lang}"]`)!.value = value;
};

beforeEach(() => {
  vi.resetModules();
  authState.loggedIn = true;
  api.fetchGlossary.mockReset();
  api.saveGlossary.mockReset();
  api.importGlossaryCsv.mockReset();
  api.deleteGlossary.mockReset();
  document.body.innerHTML = MARKUP;
});

afterEach(() => {
  vi.useRealTimers();
});

describe('home entry point', () => {
  it('shows the 📖 button only for signed-in users', async () => {
    await load();
    mod.initGlossary();
    expect(el<HTMLButtonElement>('btn-glossary-home').hidden).toBe(false);
    authState.loggedIn = false;
    mod.refreshGlossaryHome();
    expect(el<HTMLButtonElement>('btn-glossary-home').hidden).toBe(true);
  });

  it('focuses the room field instead of opening without a room code', async () => {
    await load();
    el<HTMLInputElement>('room').value = '   ';
    el('btn-glossary-home').click();
    await settle();
    expect(document.activeElement).toBe(el('room'));
    expect(api.fetchGlossary).not.toHaveBeenCalled();
    expect(el('glossary-modal').classList.contains('hidden')).toBe(true);
  });

  it('opens the editor for the normalized room code', async () => {
    await load();
    await openEditor(' AbC ');
    expect(api.fetchGlossary).toHaveBeenCalledWith('abc');
    expect(el('glossary-modal').classList.contains('hidden')).toBe(false);
  });

  it('uses the injected show() from app.ts (focus trap)', async () => {
    await load();
    const showSpy = vi.fn((target: HTMLElement, v: boolean) =>
      target.classList.toggle('hidden', !v),
    );
    mod.initGlossary({ show: showSpy });
    await openEditor();
    expect(showSpy).toHaveBeenCalledWith(el('glossary-modal'), true);
  });

  it('shows a load error when the glossary fetch fails', async () => {
    await load();
    await openEditor('abc', null);
    expect(el('glossary-status').textContent).toBe('glossaryLoadFailed');
    expect(el('glossary-status').classList.contains('error')).toBe(true);
  });

  it('ignores a stale fetch after the editor was retargeted', async () => {
    await load();
    let resolveA!: (g: Glossary | null) => void;
    api.fetchGlossary.mockImplementationOnce(
      () => new Promise<Glossary | null>((r) => (resolveA = r)),
    );
    el<HTMLInputElement>('room').value = 'aaa';
    el('btn-glossary-home').click(); // pending fetch for aaa
    await openEditor('bbb', emptyGlossary({ name: 'B name' }));
    expect(el<HTMLInputElement>('glossary-name').value).toBe('B name');
    resolveA(emptyGlossary({ name: 'A name' })); // stale — must not clobber
    await settle();
    expect(el<HTMLInputElement>('glossary-name').value).toBe('B name');
  });

  it('closes via the ✕ button (default show)', async () => {
    await load();
    await openEditor();
    el('glossary-close').click();
    expect(el('glossary-modal').classList.contains('hidden')).toBe(true);
  });
});

describe('in-call badge', () => {
  it('appears only inside an active room with entries', async () => {
    await load();
    const badge = el<HTMLButtonElement>('glossary-badge');
    mod.onGlossaryActive('Legal', 3); // no active room yet
    expect(badge.classList.contains('hidden')).toBe(true);

    mod.setGlossaryRoom('abc');
    mod.onGlossaryActive('Legal', 3);
    expect(badge.textContent).toBe('📖 Legal (3)');
    expect(badge.disabled).toBe(false);
    expect(badge.classList.contains('hidden')).toBe(false);

    mod.onGlossaryActive(null, 2); // unnamed glossary
    expect(badge.textContent).toBe('📖 (2)');

    mod.onGlossaryActive('Legal', 0); // emptied → badge retracts
    expect(badge.classList.contains('hidden')).toBe(true);
  });

  it('is visible but disabled for guests', async () => {
    await load();
    authState.loggedIn = false;
    mod.setGlossaryRoom('abc');
    mod.onGlossaryActive('Legal', 3);
    const badge = el<HTMLButtonElement>('glossary-badge');
    expect(badge.disabled).toBe(true);
    expect(badge.classList.contains('hidden')).toBe(false);
    badge.click(); // disabled → no editor
    await settle();
    expect(api.fetchGlossary).not.toHaveBeenCalled();
  });

  it('opens the editor for the active room when clicked', async () => {
    await load();
    mod.setGlossaryRoom('abc');
    mod.onGlossaryActive('Legal', 3);
    api.fetchGlossary.mockResolvedValueOnce(emptyGlossary());
    el('glossary-badge').click();
    await settle();
    expect(api.fetchGlossary).toHaveBeenCalledWith('abc');
    expect(el('glossary-modal').classList.contains('hidden')).toBe(false);
  });

  it('leaving the room hides the badge and closes an open editor', async () => {
    await load();
    mod.setGlossaryRoom('abc');
    mod.onGlossaryActive('Legal', 3);
    await openEditor('abc');
    mod.setGlossaryRoom(null);
    expect(el('glossary-badge').classList.contains('hidden')).toBe(true);
    expect(el('glossary-modal').classList.contains('hidden')).toBe(true);
  });
});

describe('rendering entries as concepts', () => {
  it('groups flat pairs into concept rows with derived language columns', async () => {
    await load();
    await openEditor(
      'abc',
      emptyGlossary({
        name: 'Team terms',
        entries: [
          pair('en', 'it', 'cat', 'gatto'),
          pair('it', 'en', 'gatto', 'cat'),
          pair('en', 'fr', 'cat', 'chat'),
          pair('fr', 'en', 'chat', 'cat'),
          pair('en', 'it', 'dog', 'cane'),
          pair('it', 'en', 'cane', 'dog'),
        ],
      }),
    );
    expect(el<HTMLInputElement>('glossary-name').value).toBe('Team terms');
    expect(headLabels()).toEqual(['English', 'Italiano', 'Français', '']); // + delete spacer
    expect(rows()).toHaveLength(2);
    expect(rowValues(0)).toEqual(['cat', 'gatto', 'chat']);
    expect(rowValues(1)).toEqual(['dog', 'cane', '']); // fr missing for dog
    expect(el('glossary-count').textContent).toBe('2 / 200');

    // 3 languages → every chip is removable; only 'de' is left to add.
    const chips = document.querySelectorAll('#glossary-lang-bar .glossary-lang-chip');
    expect(chips).toHaveLength(3);
    chips.forEach((c) => expect(c.querySelector('.glossary-lang-rm')).toBeTruthy());
    const sel = document.querySelector<HTMLSelectElement>('.glossary-lang-add')!;
    expect(Array.from(sel.options).map((o) => o.value)).toEqual(['', 'de']);
  });

  it('renders a single empty row for a fresh room, with fixed 2-lang columns', async () => {
    await load();
    await openEditor();
    expect(rows()).toHaveLength(1);
    expect(rowValues(0)).toEqual(['', '']);
    expect(headLabels()).toEqual(['English', 'Italiano', '']);
    // 2 languages is the minimum → no remove buttons.
    expect(document.querySelector('.glossary-lang-rm')).toBeNull();
  });

  it('caps the add-row button at max_entries', async () => {
    await load();
    await openEditor('abc', emptyGlossary({ max_entries: 1 }));
    expect(el('glossary-count').textContent).toBe('1 / 1');
    expect(el<HTMLButtonElement>('glossary-add-row').disabled).toBe(true);
  });
});

describe('language columns', () => {
  it('adding a language preserves typed terms', async () => {
    await load();
    await openEditor();
    setCell(0, 'en', 'cat');
    setCell(0, 'it', 'gatto');
    const sel = document.querySelector<HTMLSelectElement>('.glossary-lang-add')!;
    sel.value = 'fr';
    sel.dispatchEvent(new Event('change'));
    expect(headLabels()).toEqual(['English', 'Italiano', 'Français', '']);
    expect(rowValues(0)).toEqual(['cat', 'gatto', '']);
  });

  it('ignores a change event without a selection', async () => {
    await load();
    await openEditor();
    const sel = document.querySelector<HTMLSelectElement>('.glossary-lang-add')!;
    sel.value = '';
    sel.dispatchEvent(new Event('change'));
    expect(headLabels()).toEqual(['English', 'Italiano', '']); // unchanged
  });

  it('removing a language drops its column but keeps the others', async () => {
    await load();
    await openEditor();
    setCell(0, 'en', 'cat');
    setCell(0, 'it', 'gatto');
    const sel = document.querySelector<HTMLSelectElement>('.glossary-lang-add')!;
    sel.value = 'fr';
    sel.dispatchEvent(new Event('change'));
    // Remove the third chip (Français).
    const rms = document.querySelectorAll<HTMLButtonElement>('.glossary-lang-rm');
    rms[rms.length - 1].click();
    expect(headLabels()).toEqual(['English', 'Italiano', '']);
    expect(rowValues(0)).toEqual(['cat', 'gatto']);
    expect(document.querySelector('.glossary-lang-rm')).toBeNull(); // back at the 2-lang floor
  });

  it('rebuilds an empty table with one blank row after a language change', async () => {
    await load();
    await openEditor();
    rows()[0].querySelector<HTMLButtonElement>('.glossary-remove')!.click();
    expect(rows()).toHaveLength(0);
    const sel = document.querySelector<HTMLSelectElement>('.glossary-lang-add')!;
    sel.value = 'fr';
    sel.dispatchEvent(new Event('change'));
    expect(rows()).toHaveLength(1);
    expect(rowValues(0)).toEqual(['', '', '']);
  });
});

describe('row management', () => {
  it('adds a row, focuses its first input and updates the count', async () => {
    await load();
    await openEditor();
    el('glossary-add-row').click();
    expect(rows()).toHaveLength(2);
    expect(el('glossary-count').textContent).toBe('2 / 200');
    expect(document.activeElement).toBe(rows()[1].querySelector('input'));
  });

  it('removes a row via its ✕ button', async () => {
    await load();
    await openEditor();
    el('glossary-add-row').click();
    setCell(0, 'en', 'keep');
    rows()[1].querySelector<HTMLButtonElement>('.glossary-remove')!.click();
    expect(rows()).toHaveLength(1);
    expect(el('glossary-count').textContent).toBe('1 / 200');
  });
});

describe('saving', () => {
  it('emits all A↔B pairs, trims cells and flashes confirmation', async () => {
    vi.useFakeTimers();
    await load();
    await openEditor();
    setCell(0, 'en', ' Hello ');
    setCell(0, 'it', 'Ciao');
    el<HTMLInputElement>('glossary-name').value = ' Team ';
    api.saveGlossary.mockResolvedValueOnce({
      glossary: emptyGlossary({
        name: 'Team',
        entries: [pair('en', 'it', 'Hello', 'Ciao'), pair('it', 'en', 'Ciao', 'Hello')],
      }),
      error: '',
    });
    el('glossary-save').click();
    await settle();
    expect(api.saveGlossary).toHaveBeenCalledWith('abc', 'Team', [
      pair('en', 'it', 'Hello', 'Ciao'),
      pair('it', 'en', 'Ciao', 'Hello'),
    ]);
    // Server response re-renders the editor…
    expect(el<HTMLInputElement>('glossary-name').value).toBe('Team');
    expect(rowValues(0)).toEqual(['Hello', 'Ciao']);
    // …and the green confirmation clears after a moment.
    expect(el('glossary-status').textContent).toBe('glossarySaved');
    expect(el('glossary-status').classList.contains('ok')).toBe(true);
    expect(el('glossary-save').classList.contains('saved')).toBe(true);
    await vi.advanceTimersByTimeAsync(2400);
    expect(el('glossary-status').textContent).toBe('');
    expect(el('glossary-status').classList.contains('ok')).toBe(false);
    expect(el('glossary-save').classList.contains('saved')).toBe(false);
    expect(el<HTMLButtonElement>('glossary-save').disabled).toBe(false);
  });

  it('skips all-empty rows and passes null for an empty name', async () => {
    await load();
    await openEditor();
    el('glossary-add-row').click();
    setCell(1, 'en', 'dog');
    setCell(1, 'it', 'cane');
    api.saveGlossary.mockResolvedValueOnce({ glossary: emptyGlossary(), error: '' });
    el('glossary-save').click();
    await settle();
    expect(api.saveGlossary).toHaveBeenCalledWith('abc', null, [
      pair('en', 'it', 'dog', 'cane'),
      pair('it', 'en', 'cane', 'dog'),
    ]);
  });

  it('rejects a row with a single filled cell', async () => {
    await load();
    await openEditor();
    setCell(0, 'en', 'orphan');
    el('glossary-save').click();
    await settle();
    expect(api.saveGlossary).not.toHaveBeenCalled();
    expect(el('glossary-status').textContent).toBe('glossaryRowInvalid');
    expect(el('glossary-status').classList.contains('error')).toBe(true);
  });

  it('shows the server 400 text and re-enables the button', async () => {
    await load();
    await openEditor();
    setCell(0, 'en', 'a');
    setCell(0, 'it', 'b');
    api.saveGlossary.mockResolvedValueOnce({ glossary: null, error: 'duplicate terms' });
    el('glossary-save').click();
    await settle();
    expect(el('glossary-status').textContent).toBe('duplicate terms');
    expect(el('glossary-status').classList.contains('error')).toBe(true);
    expect(el<HTMLButtonElement>('glossary-save').disabled).toBe(false);
  });

  it('falls back to a generic error when the server sends none', async () => {
    await load();
    await openEditor();
    setCell(0, 'en', 'a');
    setCell(0, 'it', 'b');
    api.saveGlossary.mockResolvedValueOnce({ glossary: null, error: '' });
    el('glossary-save').click();
    await settle();
    expect(el('glossary-status').textContent).toBe('glossaryLoadFailed');
  });

  it('does nothing before an editor is opened', async () => {
    await load();
    el('glossary-save').click();
    await settle();
    expect(api.saveGlossary).not.toHaveBeenCalled();
  });
});

describe('CSV import', () => {
  it('focuses the textarea when the CSV is empty', async () => {
    await load();
    await openEditor();
    el('glossary-import').click();
    await settle();
    expect(document.activeElement).toBe(el('glossary-csv-text'));
    expect(api.importGlossaryCsv).not.toHaveBeenCalled();
  });

  it('saves the table silently, then merges the CSV and re-renders', async () => {
    await load();
    await openEditor();
    api.saveGlossary.mockResolvedValueOnce({ glossary: emptyGlossary(), error: '' });
    api.importGlossaryCsv.mockResolvedValueOnce({
      glossary: emptyGlossary({
        entries: [pair('en', 'it', 'cat', 'gatto'), pair('it', 'en', 'gatto', 'cat')],
      }),
      error: '',
    });
    el<HTMLTextAreaElement>('glossary-csv-text').value = 'en,it,cat,gatto';
    el('glossary-import').click();
    await settle();
    expect(api.saveGlossary).toHaveBeenCalledWith('abc', null, []); // pre-save (empty table)
    expect(api.importGlossaryCsv).toHaveBeenCalledWith('abc', 'en,it,cat,gatto');
    expect(rowValues(0)).toEqual(['cat', 'gatto']);
    expect(el<HTMLTextAreaElement>('glossary-csv-text').value).toBe('');
    expect(el('glossary-status').textContent).toBe('glossarySaved');
    expect(el<HTMLButtonElement>('glossary-import').disabled).toBe(false);
  });

  it('surfaces the CSV validation error and keeps the text for fixing', async () => {
    await load();
    await openEditor();
    api.saveGlossary.mockResolvedValueOnce({ glossary: emptyGlossary(), error: '' });
    api.importGlossaryCsv.mockResolvedValueOnce({ glossary: null, error: 'bad csv line 2' });
    el<HTMLTextAreaElement>('glossary-csv-text').value = 'en,it,cat';
    el('glossary-import').click();
    await settle();
    expect(el('glossary-status').textContent).toBe('bad csv line 2');
    expect(el('glossary-status').classList.contains('error')).toBe(true);
    expect(el<HTMLTextAreaElement>('glossary-csv-text').value).toBe('en,it,cat');
    expect(el<HTMLButtonElement>('glossary-import').disabled).toBe(false);
  });

  it('does not import when the pre-save fails validation', async () => {
    await load();
    await openEditor();
    setCell(0, 'en', 'orphan'); // single-cell row → invalid
    el<HTMLTextAreaElement>('glossary-csv-text').value = 'en,it,cat,gatto';
    el('glossary-import').click();
    await settle();
    expect(api.importGlossaryCsv).not.toHaveBeenCalled();
    expect(el('glossary-status').textContent).toBe('glossaryRowInvalid');
    expect(el<HTMLButtonElement>('glossary-import').disabled).toBe(false);
  });
});

describe('two-click delete', () => {
  it('arms on the first click and deletes on the second', async () => {
    await load();
    await openEditor(
      'abc',
      emptyGlossary({ name: 'X', entries: [pair('en', 'it', 'cat', 'gatto')] }),
    );
    api.deleteGlossary.mockResolvedValueOnce(true);
    el('glossary-delete').click();
    expect(el('glossary-delete').textContent).toBe('glossaryDeleteSure');
    expect(api.deleteGlossary).not.toHaveBeenCalled();
    el('glossary-delete').click();
    await settle();
    expect(api.deleteGlossary).toHaveBeenCalledWith('abc');
    expect(el('glossary-delete').textContent).toBe('glossaryDeleteAll');
    expect(el('glossary-status').textContent).toBe('glossaryDeleted');
    expect(el('glossary-status').classList.contains('error')).toBe(false);
    expect(rows()).toHaveLength(1); // editor reset to one blank row
    expect(rowValues(0)).toEqual(['', '']);
    expect(el<HTMLInputElement>('glossary-name').value).toBe('');
  });

  it('shows an error when the delete fails', async () => {
    await load();
    await openEditor();
    api.deleteGlossary.mockResolvedValueOnce(false);
    el('glossary-delete').click();
    el('glossary-delete').click();
    await settle();
    expect(el('glossary-status').textContent).toBe('glossaryLoadFailed');
    expect(el('glossary-status').classList.contains('error')).toBe(true);
  });

  it('disarms after 4s so a late click only re-arms', async () => {
    vi.useFakeTimers();
    await load();
    await openEditor();
    el('glossary-delete').click();
    expect(el('glossary-delete').textContent).toBe('glossaryDeleteSure');
    await vi.advanceTimersByTimeAsync(4000);
    expect(el('glossary-delete').textContent).toBe('glossaryDeleteAll');
    el('glossary-delete').click(); // arms again, must NOT delete
    await settle();
    expect(api.deleteGlossary).not.toHaveBeenCalled();
    expect(el('glossary-delete').textContent).toBe('glossaryDeleteSure');
  });

  it('ignores clicks before an editor is opened', async () => {
    await load();
    el('glossary-delete').click();
    await settle();
    expect(api.deleteGlossary).not.toHaveBeenCalled();
    expect(el('glossary-delete').textContent).toBe('');
  });
});
