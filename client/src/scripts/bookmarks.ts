// In-call bookmarks (spec 0013). The 🔖 control-bar button pins the current
// moment instantly (the server stamps the time — no client clock skew), then a
// small popover offers an optional label for ~3s before auto-dismissing.
// Pins are reviewed/edited in a side panel that follows the chat/participants
// panel pattern. Auth-only: the button stays hidden for guests.

import {
  addBookmark,
  deleteBookmark,
  fetchBookmarks,
  updateBookmarkLabel,
  type Bookmark,
} from './api';
import { isLoggedIn } from './auth';
import { t } from './i18n';
import { icon } from './icons';

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;

const btn = $<HTMLButtonElement>('btn-bookmark');
const pop = $('bookmark-pop');
const popTitle = $('bookmark-pop-title');
const popInput = $<HTMLInputElement>('bookmark-label-input');
const popSave = $<HTMLButtonElement>('bookmark-label-save');
const popShowAll = $<HTMLButtonElement>('bookmark-show-all');
const panel = $('bookmarks-panel');
const list = $('bookmarks-list');

let sessionId: string | null = null;
let pins: Bookmark[] = [];
/** ISO moment captured when 🔖 was pressed; the pending (not-yet-saved) pin. */
let pendingTs: string | null = null;
/** Most-recent saved pin (the delete handler clears it when that pin is removed). */
let lastPin: Bookmark | null = null;
let dismissTimer: number | null = null;
let layout: () => void = () => {};

/** Wire the post-toggle relayout callback (app.ts passes layoutVideos). */
export function initBookmarks(opts: { layout?: () => void } = {}): void {
  if (opts.layout) layout = opts.layout;
}

/**
 * Called on room join (with `room_joined.session_id`) and on leave (null).
 * Guests never see the button — bookmark APIs are auth-gated server-side.
 */
export function setBookmarkSession(id: string | null): void {
  sessionId = id && isLoggedIn() ? id : null;
  pins = [];
  lastPin = null;
  btn.hidden = !sessionId;
  hidePop();
  if (panel.classList.contains('open')) togglePanel(false);
  renderList();
}

// ---- Pin: label-first (spec 0039) --------------------------------------------
// A saved bookmark must ALWAYS carry a label, so 🔖 no longer pins instantly. It
// captures the moment (client clock, at press) and opens a required-label prompt;
// the pin is POSTed — with that label + timestamp — only once a non-empty label is
// confirmed. Abandoning the prompt (Escape / walk away) saves nothing.

btn.addEventListener('click', () => {
  if (!sessionId || btn.disabled) return;
  pendingTs = new Date().toISOString();
  openPrompt();
});

/** Open the required-label prompt for a fresh pin. */
function openPrompt(): void {
  popTitle.textContent = t('bookmarkLabelPrompt');
  pop.classList.remove('bookmark-pop-error');
  popInput.hidden = false;
  popSave.hidden = false;
  popShowAll.hidden = false;
  popInput.value = '';
  pop.classList.remove('hidden');
  popInput.focus();
  armDismiss();
}

/** Transient result after a save attempt; auto-dismisses. */
function showResult(failed: boolean): void {
  popTitle.textContent = failed ? t('bookmarkFailed') : t('bookmarkAdded');
  pop.classList.toggle('bookmark-pop-error', failed);
  popInput.hidden = true;
  popSave.hidden = true;
  popShowAll.hidden = failed;
  pop.classList.remove('hidden');
  if (dismissTimer) clearTimeout(dismissTimer);
  dismissTimer = window.setTimeout(hidePop, failed ? 4000 : 2000);
}

function hidePop(): void {
  if (dismissTimer) clearTimeout(dismissTimer);
  dismissTimer = null;
  pendingTs = null; // a closed prompt discards the pending moment — nothing is saved
  pop.classList.add('hidden');
}

/** Auto-close only an EMPTY prompt (nothing to lose); a half-typed label stays put. */
function armDismiss(): void {
  if (dismissTimer) clearTimeout(dismissTimer);
  dismissTimer = null;
  if (!popInput.hidden && !popInput.value.trim()) {
    dismissTimer = window.setTimeout(hidePop, 6000);
  }
}

popInput.addEventListener('input', armDismiss);
popInput.addEventListener('focus', armDismiss);
popInput.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    void commitPin();
  } else if (e.key === 'Escape') {
    e.stopPropagation();
    hidePop();
    btn.focus();
  }
});
popSave.addEventListener('click', () => void commitPin());
popShowAll.addEventListener('click', () => {
  hidePop();
  togglePanel(true);
});

/** Save the pending pin — only ever with a non-empty label. */
async function commitPin(): Promise<void> {
  const label = popInput.value.trim();
  if (!label) {
    // Never save unlabelled: flag the prompt (red) and keep it open.
    pop.classList.add('bookmark-pop-error');
    popInput.focus();
    armDismiss();
    return;
  }
  if (!sessionId || !pendingTs) return;
  popSave.disabled = true;
  const bm = await addBookmark(sessionId, { label, ts: pendingTs });
  popSave.disabled = false;
  pendingTs = null;
  if (bm) {
    pins.push(bm);
    lastPin = bm;
    renderList();
  }
  showResult(bm === null);
}

// ---- Side panel ----------------------------------------------------------------

function togglePanel(open: boolean): void {
  panel.classList.toggle('open', open);
  panel.classList.toggle('closed', !open);
  if (open) void refreshList();
  setTimeout(layout, 320);
}

$('bookmarks-close').addEventListener('click', () => togglePanel(false));

/** Re-pull from the server so other participants' pins appear too. */
async function refreshList(): Promise<void> {
  if (!sessionId) return;
  const want = sessionId;
  const rows = await fetchBookmarks(want);
  if (rows && sessionId === want) {
    pins = rows;
    renderList();
  }
}

function renderList(): void {
  list.innerHTML = '';
  if (!pins.length) {
    const empty = document.createElement('p');
    empty.className = 'bm-empty';
    empty.textContent = t('bookmarksEmpty');
    list.appendChild(empty);
    return;
  }
  for (const bm of pins) {
    list.appendChild(renderRow(bm));
  }
}

function renderRow(bm: Bookmark): HTMLElement {
  const row = document.createElement('div');
  row.className = 'bm-item';

  const time = document.createElement('span');
  time.className = 'bm-time mono';
  time.textContent = new Date(bm.ts).toLocaleTimeString([], {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  });

  const body = document.createElement('div');
  body.className = 'bm-body';
  const by = document.createElement('span');
  by.className = 'bm-by';
  by.textContent = bm.by;
  const label = document.createElement('span');
  label.className = bm.label ? 'bm-label' : 'bm-label bm-label-empty';
  label.textContent = bm.label || t('bookmarkNoLabel');
  body.append(by, label);
  row.append(time, body);

  if (bm.mine) {
    const edit = ghostIconBtn('pencil', t('bookmarkEdit'));
    edit.addEventListener('click', () => startEdit(bm, body, label));
    const del = ghostIconBtn('trash', t('bookmarkDelete'));
    del.addEventListener('click', async () => {
      if (!sessionId) return;
      del.disabled = true;
      if (await deleteBookmark(sessionId, bm.id)) {
        pins = pins.filter((p) => p.id !== bm.id);
        if (lastPin?.id === bm.id) lastPin = null;
        renderList();
      } else {
        del.disabled = false;
      }
    });
    row.append(edit, del);
  }
  return row;
}

function ghostIconBtn(name: string, title: string): HTMLButtonElement {
  const b = document.createElement('button');
  b.type = 'button';
  b.className = 'btn-ghost icon-btn bm-action';
  b.innerHTML = icon(name, 16);
  b.title = title;
  b.setAttribute('aria-label', title);
  return b;
}

/** Inline label edit: Enter saves, Escape cancels. An emptied label is NOT saved
 *  (spec 0039: a bookmark always keeps a label) — it just reverts. */
function startEdit(bm: Bookmark, body: HTMLElement, label: HTMLElement): void {
  const input = document.createElement('input');
  input.type = 'text';
  input.maxLength = 200;
  input.className = 'bm-edit-input';
  input.value = bm.label ?? '';
  input.setAttribute('aria-label', t('bookmarkEdit'));
  body.replaceChild(input, label);
  input.focus();
  input.select();

  let done = false;
  const finish = async (save: boolean) => {
    if (done) return;
    done = true;
    const text = input.value.trim();
    if (save && text && sessionId && (await updateBookmarkLabel(sessionId, bm.id, text))) {
      bm.label = text;
    }
    renderList();
  };
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      void finish(true);
    } else if (e.key === 'Escape') {
      e.stopPropagation();
      void finish(false);
    }
  });
  input.addEventListener('blur', () => void finish(true));
}
