// Room glossary editor (spec 0011). Term pairs saved here are injected into
// the Groq translation prompt server-side ("MANDATORY TERMINOLOGY"), so the
// room's jargon translates verbatim. Entry points: a 📖 button next to the
// room code on home, and an in-call header badge that appears (via the
// `glossary_active` WS message) whenever the room has entries. Auth-only:
// guests see the badge but can't open the editor.
//
// Each row is a "concept": one cluster of equivalent terms across languages.
// The user picks which languages appear as columns (2–8); saving generates all
// A↔B pairs automatically so the server receives flat GlossaryEntry rows.

import {
  deleteGlossary,
  fetchGlossary,
  importGlossaryCsv,
  saveGlossary,
  type Glossary,
  type GlossaryEntry,
} from './api';
import { isLoggedIn } from './auth';
import { ENDONYM, SUPPORTED, t } from './i18n';
import { icon } from './icons';

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;

const homeBtn = $<HTMLButtonElement>('btn-glossary-home');
const badge = $<HTMLButtonElement>('glossary-badge');
const modal = $('glossary-modal');
const nameInput = $<HTMLInputElement>('glossary-name');
const langBarEl = $('glossary-lang-bar');
const tableHeadEl = $('glossary-table-head');
const rowsEl = $('glossary-rows');
const addRowBtn = $<HTMLButtonElement>('glossary-add-row');
const countEl = $('glossary-count');
const csvText = $<HTMLTextAreaElement>('glossary-csv-text');
const importBtn = $<HTMLButtonElement>('glossary-import');
const statusEl = $('glossary-status');
const deleteBtn = $<HTMLButtonElement>('glossary-delete');
const saveBtn = $<HTMLButtonElement>('glossary-save');

const MAX_LANGS = 8;

/** app.ts's show(): modal-overlay visibility + focus trap/restore. */
let show: (el: HTMLElement, visible: boolean) => void = (el, v) =>
  el.classList.toggle('hidden', !v);
/** Room of the call we're currently in (badge target); null outside calls. */
let activeRoom: string | null = null;
/** Room the open editor is bound to (home can edit before joining). */
let editingRoom: string | null = null;
let maxEntries = 200;
let deleteArmTimer: number | null = null;

/** Active language columns shown in the concept table. */
let selectedLangs: string[] = ['en', 'it'];

/** One row in the concept table: lang → term text. */
type Concept = Record<string, string>;

// ---- Public API --------------------------------------------------------------

/** Wire events; app.ts passes its `show` so the modal gets the focus trap. */
export function initGlossary(opts: { show?: typeof show } = {}): void {
  if (opts.show) show = opts.show;
  refreshGlossaryHome();
}

/** Home 📖 button is auth-only; re-checked when the home screen is entered. */
export function refreshGlossaryHome(): void {
  homeBtn.hidden = !isLoggedIn();
}

/** Called on room join (room code) and on leave (null). */
export function setGlossaryRoom(room: string | null): void {
  activeRoom = room;
  badge.classList.add('hidden');
  if (!room && !modal.classList.contains('hidden')) show(modal, false);
}

/** `glossary_active` WS frame: badge on join + live re-broadcast after edits. */
export function onGlossaryActive(name: string | null, entries: number): void {
  if (!activeRoom || entries === 0) {
    badge.classList.add('hidden');
    return;
  }
  badge.textContent = `📖 ${name ? `${name} ` : ''}(${entries})`;
  badge.disabled = !isLoggedIn(); // guests see it but can't open the editor
  badge.classList.remove('hidden');
}

// ---- Open / close ------------------------------------------------------------

homeBtn.addEventListener('click', () => {
  const room = $<HTMLInputElement>('room').value.trim().toLowerCase();
  if (!room) {
    $<HTMLInputElement>('room').focus();
    return;
  }
  void openEditor(room);
});

badge.addEventListener('click', () => {
  if (activeRoom && isLoggedIn()) void openEditor(activeRoom);
});

$('glossary-close').addEventListener('click', () => show(modal, false));

async function openEditor(room: string): Promise<void> {
  editingRoom = room;
  setStatus('', false);
  csvText.value = '';
  selectedLangs = ['en', 'it'];
  renderEditor({ name: null, entries: [], max_entries: maxEntries });
  show(modal, true);
  const g = await fetchGlossary(room);
  if (editingRoom !== room) return; // closed/retargeted while loading
  if (g) renderEditor(g);
  else setStatus(t('glossaryLoadFailed'), true);
}

function setStatus(text: string, isError: boolean): void {
  statusEl.textContent = text;
  statusEl.classList.toggle('error', isError);
  statusEl.classList.remove('ok');
}

/**
 * Confirm a successful save: a green pill (with a ✓) + a brief green flash on the
 * Save button, both clearing after a moment so they read as a confirmation, not a
 * permanent label. The editor stays open so more terms can be added.
 */
let savedTimer: number | undefined;
function flashSaved(): void {
  statusEl.textContent = t('glossarySaved');
  statusEl.classList.remove('error');
  statusEl.classList.add('ok');
  saveBtn.classList.add('saved');
  window.clearTimeout(savedTimer);
  savedTimer = window.setTimeout(() => {
    statusEl.classList.remove('ok');
    statusEl.textContent = '';
    saveBtn.classList.remove('saved');
  }, 2400);
}

// ---- Concept ↔ entry conversion ---------------------------------------------

/**
 * Derive `selectedLangs` from the entry set, then group entries into concept
 * rows (one row per distinct source_term in the primary language).
 */
function entriesToConcepts(entries: GlossaryEntry[]): Concept[] {
  if (!entries.length) return [{}];
  const langs = new Set<string>();
  entries.forEach((e) => {
    langs.add(e.source_lang);
    langs.add(e.target_lang);
  });
  selectedLangs = Array.from(langs);
  const primary = selectedLangs[0];
  const seen = new Set<string>();
  const concepts: Concept[] = [];
  for (const entry of entries) {
    if (entry.source_lang !== primary) continue;
    if (seen.has(entry.source_term)) continue;
    seen.add(entry.source_term);
    const concept: Concept = { [primary]: entry.source_term };
    for (const lang of selectedLangs.slice(1)) {
      const tr = entries.find(
        (e) =>
          e.source_lang === primary &&
          e.target_lang === lang &&
          e.source_term === entry.source_term,
      );
      if (tr) concept[lang] = tr.target_term;
    }
    concepts.push(concept);
  }
  return concepts.length ? concepts : [{}];
}

// ---- Render ------------------------------------------------------------------

function renderEditor(g: Glossary): void {
  maxEntries = g.max_entries;
  nameInput.value = g.name ?? '';
  const concepts = entriesToConcepts(g.entries);
  renderLangBar();
  renderTableHead();
  rowsEl.innerHTML = '';
  for (const c of concepts) addConceptRow(c);
  updateCount();
}

function renderLangBar(): void {
  langBarEl.innerHTML = '';

  for (const lang of selectedLangs) {
    const chip = document.createElement('span');
    chip.className = 'glossary-lang-chip';
    chip.textContent = ENDONYM[lang] ?? lang;

    if (selectedLangs.length > 2) {
      const rm = document.createElement('button');
      rm.type = 'button';
      rm.className = 'glossary-lang-rm';
      rm.textContent = '×';
      rm.title = `Remove ${ENDONYM[lang] ?? lang}`;
      rm.addEventListener('click', () => {
        selectedLangs = selectedLangs.filter((l) => l !== lang);
        const concepts = collectConceptsRaw();
        renderLangBar();
        renderTableHead();
        rebuildRows(concepts);
        updateCount();
      });
      chip.appendChild(rm);
    }
    langBarEl.appendChild(chip);
  }

  if (selectedLangs.length < MAX_LANGS) {
    const sel = document.createElement('select');
    sel.className = 'glossary-lang-add';
    const placeholder = document.createElement('option');
    placeholder.value = '';
    placeholder.textContent = t('glossaryAddLang');
    placeholder.disabled = true;
    placeholder.selected = true;
    sel.appendChild(placeholder);
    for (const code of SUPPORTED) {
      if (selectedLangs.includes(code)) continue;
      const opt = document.createElement('option');
      opt.value = code;
      opt.textContent = ENDONYM[code];
      sel.appendChild(opt);
    }
    sel.addEventListener('change', () => {
      if (!sel.value) return;
      selectedLangs.push(sel.value);
      const concepts = collectConceptsRaw();
      renderLangBar();
      renderTableHead();
      rebuildRows(concepts);
      updateCount();
    });
    langBarEl.appendChild(sel);
  }
}

function renderTableHead(): void {
  tableHeadEl.innerHTML = '';
  for (const lang of selectedLangs) {
    const span = document.createElement('span');
    span.textContent = ENDONYM[lang] ?? lang;
    tableHeadEl.appendChild(span);
  }
  tableHeadEl.appendChild(document.createElement('span')); // spacer for delete column
}

function addConceptRow(concept: Concept = {}): void {
  const row = document.createElement('div');
  row.className = 'glossary-row';
  for (const lang of selectedLangs) {
    const input = document.createElement('input');
    input.type = 'text';
    input.maxLength = 200;
    input.autocomplete = 'off';
    input.value = concept[lang] ?? '';
    input.dataset.lang = lang;
    input.placeholder = ENDONYM[lang] ?? lang;
    row.appendChild(input);
  }
  const remove = document.createElement('button');
  remove.type = 'button';
  remove.className = 'glossary-remove';
  remove.innerHTML = icon('close', 14);
  remove.title = t('glossaryRemove');
  remove.setAttribute('aria-label', t('glossaryRemove'));
  remove.addEventListener('click', () => {
    row.remove();
    updateCount();
  });
  row.appendChild(remove);
  rowsEl.appendChild(row);
}

/** Rebuild all rows after a lang change, preserving term text. */
function rebuildRows(concepts: Concept[]): void {
  rowsEl.innerHTML = '';
  for (const c of concepts) addConceptRow(c);
  if (!rowsEl.children.length) addConceptRow();
}

/** Read current row data without validation (used when the lang bar changes). */
function collectConceptsRaw(): Concept[] {
  const concepts: Concept[] = [];
  for (const row of rowsEl.querySelectorAll<HTMLElement>('.glossary-row')) {
    const concept: Concept = {};
    for (const input of row.querySelectorAll<HTMLInputElement>('input[data-lang]')) {
      concept[input.dataset.lang!] = input.value;
    }
    concepts.push(concept);
  }
  return concepts;
}

addRowBtn.addEventListener('click', () => {
  addConceptRow();
  rowsEl.querySelector<HTMLElement>('.glossary-row:last-child input')?.focus();
  updateCount();
});

function updateCount(): void {
  const n = rowsEl.querySelectorAll('.glossary-row').length;
  countEl.textContent = `${n} / ${maxEntries}`;
  addRowBtn.disabled = n >= maxEntries;
}

/**
 * Validate the concept table and return flat GlossaryEntry pairs.
 * Rows with all cells empty are skipped; a row with only one filled cell is
 * an error (need at least two languages to form a translation pair).
 */
function collectConcepts(): GlossaryEntry[] | null {
  const allEntries: GlossaryEntry[] = [];
  for (const row of rowsEl.querySelectorAll<HTMLElement>('.glossary-row')) {
    const concept: Concept = {};
    for (const input of row.querySelectorAll<HTMLInputElement>('input[data-lang]')) {
      const v = input.value.trim();
      if (v) concept[input.dataset.lang!] = v;
    }
    const filled = selectedLangs.filter((l) => concept[l]);
    if (filled.length === 0) continue;
    if (filled.length === 1) {
      setStatus(t('glossaryRowInvalid'), true);
      return null;
    }
    for (let i = 0; i < filled.length; i++) {
      for (let j = 0; j < filled.length; j++) {
        if (i === j) continue;
        allEntries.push({
          source_lang: filled[i],
          target_lang: filled[j],
          source_term: concept[filled[i]],
          target_term: concept[filled[j]],
        });
      }
    }
  }
  return allEntries;
}

// ---- Save / import / delete --------------------------------------------------

/** Save the table. Returns true on success (import chains on this). */
async function doSave(silent: boolean): Promise<boolean> {
  const room = editingRoom;
  const entries = collectConcepts();
  if (!room || !entries) return false;
  saveBtn.disabled = true;
  const res = await saveGlossary(room, nameInput.value.trim() || null, entries);
  saveBtn.disabled = false;
  if (!res.glossary) {
    setStatus(res.error || t('glossaryLoadFailed'), true);
    return false;
  }
  renderEditor(res.glossary);
  if (!silent) flashSaved();
  return true;
}

saveBtn.addEventListener('click', () => void doSave(false));

importBtn.addEventListener('click', async () => {
  const room = editingRoom;
  const csv = csvText.value.trim();
  if (!room || !csv) {
    csvText.focus();
    return;
  }
  // Save the table first so unsaved edits aren't clobbered by the re-render.
  importBtn.disabled = true;
  const saved = await doSave(true);
  if (saved) {
    const res = await importGlossaryCsv(room, csv);
    if (res.glossary) {
      renderEditor(res.glossary);
      csvText.value = '';
      flashSaved();
    } else {
      setStatus(res.error || t('glossaryLoadFailed'), true);
    }
  }
  importBtn.disabled = false;
});

/** Two-click delete: first click arms ("click again…"), second one deletes. */
deleteBtn.addEventListener('click', async () => {
  if (!editingRoom) return;
  if (deleteArmTimer === null) {
    deleteBtn.textContent = t('glossaryDeleteSure');
    deleteArmTimer = window.setTimeout(disarmDelete, 4000);
    return;
  }
  disarmDelete();
  if (await deleteGlossary(editingRoom)) {
    renderEditor({ name: null, entries: [], max_entries: maxEntries });
    setStatus(t('glossaryDeleted'), false);
  } else {
    setStatus(t('glossaryLoadFailed'), true);
  }
});

function disarmDelete(): void {
  if (deleteArmTimer !== null) clearTimeout(deleteArmTimer);
  deleteArmTimer = null;
  deleteBtn.textContent = t('glossaryDeleteAll');
}
