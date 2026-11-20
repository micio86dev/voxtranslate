// Live Connect — customer page of a business desk (spec 0121, B2B). The thin layer between the
// DOM in `pages/lc/d/[token].astro` and the engine: language, optional name, consent, scan,
// wait for the staff, and what to say when nobody comes.
//
// A desk QR is permanent and never stands for a conversation: every scan, and every "Try
// again", is a NEW `POST /api/live/desks/scan` and a NEW session. Nothing here resumes an ended
// one — the only thing that rejoins an old session is a reload of a still-open one.
//
// The QR path segment (`<public_id>.<mac>`) is a bearer secret. It is read once from a data
// attribute and sent to the scan request, nowhere else: not logged, not stored (only the public
// handle before the dot is, to tell one desk from another), not given to the room.

import { t } from '../i18n';
import { pcmPlayback } from '../pcm-playback';
import { getSession, scanDesk } from './api';
import { browserSupported } from './conversation';
import { defaultLanguage, mountLangPicker, pickableLanguages, type LangPicker } from './lang-picker';
import { errorInfo } from './messages';
import { byId, initPage, setHidden } from './page-common';
import { mountRoom, type RoomHandle } from './room';
import type { EndReason, LiveContext } from './session-machine';
import {
  clearDeskInfo,
  clearStoredSession,
  loadDeskInfo,
  loadStoredSession,
  saveDeskInfo,
  saveStoredSession,
  type DeskInfo,
  type StoredSession,
} from './session-store';
import { fill } from './view';

const VIEWS = ['ld-hero', 'ld-join', 'ld-session', 'ld-nobody', 'ld-ended'] as const;
type View = (typeof VIEWS)[number];
/** Past the countdown, how often (in ticks of one second) to ask the server if it gave up. */
const PROBE_EVERY_TICKS = 3;

/** What ends a desk session nobody answered: it expired, or its QR was revoked / replaced. */
const NOBODY_REASONS: ReadonlySet<EndReason> = new Set<EndReason>(['expired', 'desk_revoked', 'desk_regenerated']);

let picker: LangPicker | null = null;
let scanning = false;
let segment = '';
let handle = '';
/** The inputs of the last scan made in THIS page life: "Try again" reuses them (consent included). */
let scannedOnce = false;
let room: RoomHandle | null = null;
let hadPartner = false;
let countdown: ReturnType<typeof setInterval> | null = null;

function view(...visible: View[]): void {
  for (const id of VIEWS) setHidden(id, !visible.includes(id));
}

function showStatus(key: string, action: 'retry' | 'home' | 'none'): void {
  const status = byId('ld-status');
  if (status) {
    status.textContent = t(key);
    status.hidden = false;
  }
  setHidden('ld-retry', action !== 'retry');
  setHidden('ld-home', action !== 'home');
  setHidden('ld-actions', action === 'none');
}

function clearStatus(): void {
  setHidden('ld-status', true);
  setHidden('ld-actions', true);
}

function stopCountdown(): void {
  if (countdown !== null) {
    clearInterval(countdown);
    countdown = null;
  }
}

/** Paint the time left; false once it has run out. */
function paintCountdown(until: number): boolean {
  const el = byId('ld-countdown');
  const secs = Math.ceil((until - Date.now()) / 1000);
  if (el) {
    el.hidden = secs <= 0;
    if (secs > 0) el.textContent = fill(t('liveDeskWaitHint'), { secs: String(secs) });
  }
  return secs > 0;
}

function startCountdown(session: StoredSession, until: number): void {
  stopCountdown();
  paintCountdown(until);
  let overdue = 0;
  countdown = setInterval(() => {
    if (paintCountdown(until)) return;
    // The server decides when a desk request expires, and tells us over the socket. If that
    // frame never comes (a dropped socket, a late sweep) the page must not wait forever.
    overdue += 1;
    if (overdue % PROBE_EVERY_TICKS === 0) void probe(session);
  }, 1000);
}

async function probe(session: StoredSession): Promise<void> {
  const res = await getSession(session.sessionId, session.token);
  if (countdown === null || hadPartner) return; // answered or already handled meanwhile
  if (res.ok && (res.data.session.status === 'expired' || res.data.session.status === 'ended')) {
    showNobody('expired');
  }
}

/** Nobody came (or the QR stopped being valid): free everything and say so kindly. */
function showNobody(reason: EndReason): void {
  stopCountdown();
  room?.destroy();
  room = null;
  clearStoredSession();
  clearDeskInfo();
  const invalid = reason !== 'expired';
  const text = byId('ld-nobody-text');
  // A replaced or revoked code will not work again, so offering to retry it would be a trap.
  if (text) text.textContent = t(invalid ? 'liveDeskErrInvalid' : 'liveDeskNobody');
  setHidden('ld-try-again', invalid);
  view('ld-nobody');
}

function onState(ctx: LiveContext): void {
  if (ctx.partners.length) {
    hadPartner = true;
    stopCountdown();
    setHidden('ld-wait', true);
  }
  if (ctx.phase === 'ended' && !hadPartner && ctx.endReason && NOBODY_REASONS.has(ctx.endReason)) {
    showNobody(ctx.endReason);
  }
}

function enterSession(session: StoredSession, desk: DeskInfo | null): void {
  stopCountdown();
  hadPartner = false;
  view('ld-session');
  const wait = byId('ld-wait');
  if (wait) wait.hidden = !desk;
  if (desk) {
    const name = byId('ld-desk-name');
    if (name) name.textContent = desk.name;
    const loc = byId('ld-desk-location');
    if (loc) {
      loc.textContent = desk.location ?? '';
      loc.hidden = !desk.location;
    }
    startCountdown(session, desk.until);
  }
  const container = byId('ld-room');
  if (!container) return;
  container.textContent = '';
  room = mountRoom({ container, role: 'guest', session, waitingKey: 'liveDeskWaiting', onState });
}

async function scan(): Promise<void> {
  if (scanning) return;
  clearStatus();
  if (!browserSupported()) {
    showStatus('talkErrorUnsupported', 'none');
    return;
  }
  const consent = byId<HTMLInputElement>('ld-consent');
  if (!consent?.checked) {
    // Stated in words rather than by disabling the button: a disabled control gives a stranger
    // on a phone no way to learn what is missing.
    showStatus(errorInfo('consent_required', 'guest').key, 'none');
    consent?.focus();
    return;
  }
  const lang = picker?.get();
  if (!lang) return;

  scanning = true;
  // Unlock audio inside the tap: iOS will not start an AudioContext once the gesture is spent.
  pcmPlayback.unlock();
  const btn = byId<HTMLButtonElement>('ld-join-btn');
  if (btn) {
    btn.disabled = true;
    btn.textContent = t('liveJoining');
  }
  const name = byId<HTMLInputElement>('ld-name')?.value.trim() || t('liveGuestName');

  const res = await scanDesk({ segment, name, lang, consent: true });
  scanning = false;
  if (btn) {
    btn.disabled = false;
    btn.textContent = t('liveDeskStart');
  }
  scannedOnce = true;
  if (!res.ok) {
    const info = errorInfo(res.code, 'guest');
    showStatus(info.key, info.action === 'retry' || info.action === 'home' ? info.action : 'none');
    // Every scan of a code that is not valid gets the same answer: do not invite another.
    if (res.code === 'invalid_desk_qr' && btn) btn.disabled = true;
    return;
  }
  const d = res.data;
  const session: StoredSession = {
    sessionId: d.session_id,
    token: d.token,
    role: 'guest',
    myLang: lang,
    myPeerId: d.peer_id,
    wsPath: d.ws_path,
  };
  const desk: DeskInfo = {
    sessionId: d.session_id,
    handle,
    name: d.desk.name,
    location: d.desk.location_label,
    until: Date.now() + d.wait_secs * 1000,
  };
  saveStoredSession(session);
  saveDeskInfo(desk);
  enterSession(session, desk);
}

/** "Try again": a brand new scan. The old session is gone for good. */
function tryAgain(): void {
  stopCountdown();
  room?.destroy();
  room = null;
  clearStoredSession();
  clearDeskInfo();
  clearStatus();
  view('ld-hero', 'ld-join');
  // Consent was given on this page and the notice has not changed: do not make a person who
  // is already standing at a desk tick the same box again. After a reload there is no such
  // consent to reuse, so the form comes back instead.
  if (scannedOnce) void scan();
}

type Resumed =
  | { kind: 'open'; session: StoredSession; desk: DeskInfo }
  | { kind: 'gone'; neverAnswered: boolean }
  | null;

/** A stored DESK session of THIS desk that is still open on the server, so a reload rejoins it. */
async function resumable(): Promise<Resumed> {
  const stored = loadStoredSession();
  if (!stored || stored.role !== 'guest') return null;
  const desk = loadDeskInfo(stored.sessionId);
  if (!desk || desk.handle !== handle) return null;

  const res = await getSession(stored.sessionId, stored.token);
  if (res.ok) {
    const { status, participants } = res.data.session;
    if (status !== 'ended' && status !== 'expired') return { kind: 'open', session: stored, desk };
    clearStoredSession();
    clearDeskInfo();
    return { kind: 'gone', neverAnswered: status === 'expired' || participants.length < 2 };
  }
  // The server could not be asked: let the engine's reconnect logic find out.
  if (res.status === 0 || res.status >= 500) return { kind: 'open', session: stored, desk };
  clearStoredSession();
  clearDeskInfo();
  return res.status === 410 || res.code === 'session_ended' ? { kind: 'gone', neverAnswered: false } : null;
}

export async function mountLiveDesk(): Promise<void> {
  const uiLang = initPage();
  stopCountdown();
  room = null;
  scanning = false;
  scannedOnce = false;
  hadPartner = false;
  segment = byId('ld-root')?.dataset.segment ?? '';
  handle = segment.split('.')[0];

  const codes = pickableLanguages().map((l) => l.code);
  const lang$ = byId('ld-lang');
  if (lang$) {
    lang$.textContent = '';
    const browser = typeof navigator !== 'undefined' ? [...(navigator.languages ?? []), navigator.language] : [];
    picker = mountLangPicker(lang$, {
      idPrefix: 'ld',
      codes,
      initial: defaultLanguage(codes, ...browser, uiLang),
      labelKey: 'liveYourLang',
    });
  }

  byId('ld-join-btn')?.addEventListener('click', () => void scan());
  byId('ld-retry')?.addEventListener('click', () => void scan());
  byId('ld-try-again')?.addEventListener('click', () => tryAgain());

  const again = await resumable();
  if (again?.kind === 'open') {
    enterSession(again.session, again.desk);
    return;
  }
  if (again?.kind === 'gone') {
    if (again.neverAnswered) showNobody('expired');
    else view('ld-ended');
    return;
  }
  if (!browserSupported()) {
    showStatus('talkErrorUnsupported', 'none');
    const btn = byId<HTMLButtonElement>('ld-join-btn');
    if (btn) btn.disabled = true;
  }
}
