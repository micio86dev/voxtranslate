// Live Connect — staff page of the main app (spec 0121, B2B desks). The thin layer between the
// DOM in `pages/live/desks.astro` and the desk poller / engine: the desks a member serves, who
// is waiting at each, and answering one customer.
//
// A member answers as THEMSELVES (account JWT) and the ORGANISATION pays, so there is no credit
// line. The answer call returns a live token for the staff seat; from there it is the same
// conversation screen as everybody else's. Polling pauses with the page and stops for good
// when the conversation starts (this tab is on a call now) or the sign-in expires.
//
// What a hidden tab does NOT get: polling is paused there on purpose, so a backgrounded tab
// shows no title badge until it is visible again. There is no push channel to do better.

import { billingEnabled, getUser, isLoggedIn } from '../auth';
import { t } from '../i18n';
import { pcmPlayback } from '../pcm-playback';
import { answerSession, getSession, type StaffDesk, type WaitingCustomer } from './api';
import { browserSupported } from './conversation';
import { DeskPoller, type DeskSnapshot } from './desk-poll';
import { defaultLanguage, mountLangPicker, pickableLanguages, type LangPicker } from './lang-picker';
import { staffErrorInfo } from './messages';
import { byId, initPage, setHidden } from './page-common';
import { mountRoom } from './room';
import {
  clearStoredSession,
  loadStaffLang,
  loadStoredSession,
  saveStaffLang,
  saveStoredSession,
  type StoredSession,
} from './session-store';
import { fill, langLabel } from './view';

interface Target {
  desk: StaffDesk;
  customer: WaitingCustomer;
}

let poller: DeskPoller | null = null;
let picker: LangPicker | null = null;
let uiLang = 'en';
let baseTitle = '';
let snapshot: DeskSnapshot | null = null;
let seen = new Set<string>();
let baselineTaken = false;
let newDeskName: string | null = null;
let target: Target | null = null;
let answering = false;
/** Which message `#ls-error` holds: a poll problem clears itself on recovery, a notice does not. */
let errorKind: 'poll' | 'notice' | null = null;

// --- small DOM helpers ----------------------------------------------------------------------

/** Set text only when it changed: a live region re-announces whatever is rewritten. */
function setText(el: Element | null, text: string): void {
  if (el && el.textContent !== text) el.textContent = text;
}

function child<K extends keyof HTMLElementTagNameMap>(
  parent: HTMLElement,
  tag: K,
  cls: string,
): HTMLElementTagNameMap[K] {
  const e = parent.ownerDocument.createElement(tag);
  e.className = cls;
  parent.append(e);
  return e;
}

/** Make `parent`'s children exactly `wanted`, in order, moving nothing that is already placed. */
function syncChildren(parent: HTMLElement, wanted: HTMLElement[]): void {
  for (const c of [...parent.children]) if (!wanted.includes(c as HTMLElement)) c.remove();
  wanted.forEach((node, i) => {
    if (parent.children[i] !== node) parent.insertBefore(node, parent.children[i] ?? null);
  });
}

function showError(key: string, kind: 'poll' | 'notice'): void {
  const el = byId('ls-error');
  if (!el) return;
  setText(el, t(key));
  el.hidden = false;
  errorKind = kind;
}

function clearError(onlyKind?: 'poll' | 'notice'): void {
  if (onlyKind && errorKind !== onlyKind) return;
  setHidden('ls-error', true);
  errorKind = null;
}

function showSignIn(expired: boolean): void {
  poller?.stop();
  closePanel();
  setText(byId('ls-signin-text'), t(expired ? 'liveStaffErrAuth' : 'liveStaffSignIn'));
  setHidden('ls-list', true);
  setHidden('ls-session', true);
  setHidden('ls-signin', false);
  document.title = baseTitle;
}

// --- the desk list --------------------------------------------------------------------------

function buildCard(doc: Document, deskId: string): HTMLElement {
  const card = doc.createElement('article');
  card.className = 'card lc-desk-card';
  card.dataset.desk = deskId;
  child(card, 'h2', 'lc-desk-name');
  child(card, 'p', 'lc-note lc-desk-meta');
  child(card, 'p', 'lc-desk-count');
  child(card, 'ul', 'lc-queue');
  child(card, 'p', 'lc-note lc-queue-empty');
  return card;
}

function buildRow(doc: Document, sessionId: string): HTMLElement {
  const row = doc.createElement('li');
  row.className = 'lc-queue-row';
  row.dataset.session = sessionId;
  const who = child(row, 'div', 'lc-queue-who');
  child(who, 'p', 'lc-queue-name');
  child(who, 'p', 'lc-queue-meta');
  const btn = child(row, 'button', 'btn-primary lc-answer-btn');
  btn.type = 'button';
  btn.dataset.answer = sessionId;
  return row;
}

function paintCard(card: HTMLElement, d: StaffDesk, queue: WaitingCustomer[]): void {
  const doc = card.ownerDocument;
  setText(card.querySelector('.lc-desk-name'), d.name);
  setText(card.querySelector('.lc-desk-meta'), d.location_label ? `${d.org_name} · ${d.location_label}` : d.org_name);
  setText(card.querySelector('.lc-desk-count'), fill(t('liveStaffWaitingCount'), { n: String(queue.length) }));
  const list = card.querySelector<HTMLElement>('.lc-queue');
  const empty = card.querySelector<HTMLElement>('.lc-queue-empty');
  if (empty) {
    setText(empty, t('liveStaffNobody'));
    empty.hidden = queue.length > 0;
  }
  if (!list) return;
  const existing = new Map<string, HTMLElement>();
  for (const r of list.children) existing.set((r as HTMLElement).dataset.session ?? '', r as HTMLElement);
  const rowsWanted = queue.map((c) => {
    const row = existing.get(c.session_id) ?? buildRow(doc, c.session_id);
    const label = langLabel(c.guest_lang);
    setText(row.querySelector('.lc-queue-name'), c.guest_name);
    setText(
      row.querySelector('.lc-queue-meta'),
      `${label.flag} ${label.native} · ${fill(t('liveStaffWaited'), { secs: String(c.waited_secs) })}`,
    );
    const btn = row.querySelector('button');
    setText(btn, t('liveStaffAnswer'));
    btn?.setAttribute('aria-label', fill(t('liveStaffAnswerFor'), { name: c.guest_name }));
    return row;
  });
  syncChildren(list, rowsWanted);
}

function renderDesks(s: DeskSnapshot): void {
  const host = byId('ls-desks');
  if (!host) return;
  setHidden('ls-empty', s.desks.length > 0);
  const existing = new Map<string, HTMLElement>();
  for (const c of host.children) existing.set((c as HTMLElement).dataset.desk ?? '', c as HTMLElement);
  const wanted = s.desks.map((d) => {
    const card = existing.get(d.id) ?? buildCard(host.ownerDocument, d.id);
    paintCard(card, d, s.waiting[d.id] ?? []);
    return card;
  });
  syncChildren(host, wanted);
}

/** The cue: a title badge with the count, and a visible polite message when somebody NEW waits. */
function paintCue(s: DeskSnapshot): void {
  const all: Array<{ id: string; desk: StaffDesk }> = [];
  for (const d of s.desks) for (const c of s.waiting[d.id] ?? []) all.push({ id: c.session_id, desk: d });
  // Customers already waiting when the page opened are on the list in front of the member:
  // announcing them as "new" would be a lie.
  if (baselineTaken) {
    const fresh = all.find((x) => !seen.has(x.id));
    if (fresh) newDeskName = fresh.desk.name;
  }
  baselineTaken = true;
  seen = new Set(all.map((x) => x.id));
  if (all.length === 0) newDeskName = null;
  document.title = all.length > 0 ? `(${all.length}) ${baseTitle}` : baseTitle;
  const banner = byId('ls-banner');
  if (banner) {
    banner.hidden = newDeskName === null;
    if (newDeskName !== null) setText(banner, fill(t('liveStaffNewCustomer'), { desk: newDeskName }));
  }
}

function onSnapshot(s: DeskSnapshot): void {
  snapshot = s;
  clearError('poll');
  renderDesks(s);
  paintCue(s);
  // The customer the member was about to answer can leave while the choice is open.
  if (target && !(s.waiting[target.desk.id] ?? []).some((c) => c.session_id === target?.customer.session_id)) {
    closePanel();
    showError('liveStaffErrGone', 'notice');
  }
}

// --- answering ------------------------------------------------------------------------------

function closePanel(): void {
  target = null;
  setHidden('ls-answer', true);
  setHidden('ls-answer-status', true);
}

function openPanel(desk: StaffDesk, customer: WaitingCustomer): void {
  clearError('notice');
  target = { desk, customer };
  setText(byId('ls-answer-title'), fill(t('liveStaffAnswerFor'), { name: customer.guest_name }));
  const codes = pickableLanguages().map((l) => l.code);
  // The language this member last spoke at THIS desk, else what the desk expects, else theirs.
  picker?.set(defaultLanguage(codes, loadStaffLang(desk.id), desk.default_lang, getUser()?.language, uiLang));
  setHidden('ls-answer-status', true);
  setHidden('ls-answer', false);
  const panel = byId('ls-answer');
  panel?.scrollIntoView?.({ block: 'nearest' });
  byId('ls-lang')?.querySelector('button')?.focus();
}

function panelError(key: string): void {
  const el = byId('ls-answer-status');
  if (!el) return;
  setText(el, t(key));
  el.hidden = false;
}

async function confirmAnswer(): Promise<void> {
  const current = target;
  const lang = picker?.get();
  if (!current || !lang || answering) return;
  setHidden('ls-answer-status', true);
  if (!browserSupported()) {
    panelError('talkErrorUnsupported');
    return;
  }
  answering = true;
  // Unlock audio inside the tap: iOS will not start an AudioContext once the gesture is spent.
  pcmPlayback.unlock();
  const btn = byId<HTMLButtonElement>('ls-answer-go');
  if (btn) {
    btn.disabled = true;
    btn.textContent = t('liveJoining');
  }
  const res = await answerSession(current.customer.session_id, lang);
  answering = false;
  if (btn) {
    btn.disabled = false;
    btn.textContent = t('liveStaffAnswer');
  }
  if (!res.ok) {
    const info = staffErrorInfo(res);
    if (info.action === 'signin') return showSignIn(true);
    if (info.action === 'refresh') {
      // Somebody else took them, or they left: the list is stale, so fix the list.
      closePanel();
      showError(info.key, 'notice');
      poller?.pollNow();
      return;
    }
    panelError(info.key);
    return;
  }
  saveStaffLang(current.desk.id, lang);
  const d = res.data;
  const session: StoredSession = {
    sessionId: d.session_id,
    token: d.token,
    role: 'staff',
    myLang: lang,
    myPeerId: d.peer_id,
    wsPath: d.ws_path,
  };
  saveStoredSession(session);
  enterSession(session, d.guest);
}

function enterSession(session: StoredSession, guest?: { name: string; lang: string }): void {
  poller?.stop();
  closePanel();
  document.title = baseTitle;
  setHidden('ls-hero', true);
  setHidden('ls-list', true);
  setHidden('ls-signin', true);
  setHidden('ls-session', false);
  const who = byId('ls-customer');
  if (who) {
    who.hidden = !guest;
    if (guest) {
      const label = langLabel(guest.lang);
      who.textContent = `${guest.name} · ${label.flag} ${label.native}`;
    }
  }
  const container = byId('ls-room');
  if (!container) return;
  container.textContent = '';
  mountRoom({ container, role: 'staff', session });
}

/** A stored staff seat that is still open on the server, so a reload rejoins the conversation. */
async function resumable(): Promise<StoredSession | null> {
  const stored = loadStoredSession();
  if (!stored || stored.role !== 'staff') return null;
  const res = await getSession(stored.sessionId, stored.token);
  if (res.ok) {
    const status = res.data.session.status;
    if (status !== 'ended' && status !== 'expired') return stored;
  } else if (res.status === 0 || res.status >= 500) {
    // The server could not be asked: let the engine's reconnect logic find out.
    return stored;
  }
  clearStoredSession();
  return null;
}

export async function mountLiveStaff(): Promise<void> {
  uiLang = initPage();
  baseTitle = document.title;
  poller?.stop();
  snapshot = null;
  seen = new Set();
  baselineTaken = false;
  newDeskName = null;
  target = null;
  answering = false;
  errorKind = null;

  if (!(await billingEnabled())) {
    setHidden('ls-unavailable', false);
    return;
  }
  if (!isLoggedIn()) {
    setHidden('ls-signin', false);
    return;
  }

  const lang$ = byId('ls-lang');
  if (lang$) {
    lang$.textContent = '';
    const codes = pickableLanguages().map((l) => l.code);
    picker = mountLangPicker(lang$, {
      idPrefix: 'ls',
      codes,
      initial: defaultLanguage(codes, getUser()?.language, uiLang),
      labelKey: 'liveYourLang',
    });
  }

  byId('ls-desks')?.addEventListener('click', (ev) => {
    const btn = (ev.target as Element | null)?.closest<HTMLElement>('button[data-answer]');
    if (!btn || !snapshot) return;
    const sessionId = btn.dataset.answer;
    const deskId = btn.closest<HTMLElement>('article[data-desk]')?.dataset.desk;
    const d = snapshot.desks.find((x) => x.id === deskId);
    const c = d ? (snapshot.waiting[d.id] ?? []).find((x) => x.session_id === sessionId) : undefined;
    if (d && c) openPanel(d, c);
  });
  byId('ls-answer-go')?.addEventListener('click', () => void confirmAnswer());
  byId('ls-answer-cancel')?.addEventListener('click', () => {
    const back = target?.customer.session_id;
    closePanel();
    if (back) document.querySelector<HTMLElement>(`button[data-answer="${back}"]`)?.focus();
  });

  const again = await resumable();
  if (again) {
    enterSession(again);
    return;
  }

  setHidden('ls-list', false);
  poller = new DeskPoller({
    onSnapshot,
    onError: (e) => showError(staffErrorInfo({ status: e.status, code: e.code }).key, 'poll'),
    onAuthLost: () => showSignIn(true),
  });
  poller.start();
}
