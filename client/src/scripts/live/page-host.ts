// Live Connect — host page wiring (spec 0121). The thin layer between the DOM in
// `pages/live.astro` and the engine: gates, setup form, invite panel. No protocol logic and
// no audio logic live here — those are in `conversation.ts` and are tested without a browser.

import { billingEnabled, getUser, isLoggedIn } from '../auth';
import { t } from '../i18n';
import { pcmPlayback } from '../pcm-playback';
import { toast } from '../toast';
import { createSession, getSession } from './api';
import { defaultLanguage, mountLangPicker, pickableLanguages, type LangPicker } from './lang-picker';
import { errorInfo, type ErrorAction } from './messages';
import { byId, initPage, setHidden } from './page-common';
import { mountRoom } from './room';
import { initialContext, isTerminal, type LiveContext } from './session-machine';
import { fill } from './view';
import { clearStoredSession, loadStoredSession, saveStoredSession, type StoredSession } from './session-store';

let picker: LangPicker | null = null;
let creating = false;

/** The seats of THIS conversation: how many guests it admits, and which peers took one. A
 *  seat stays taken when its guest drops, so it is counted from everybody ever seen. */
const seated = new Set<string>();
let maxGuests = 1;
/** The invitation panel was opened by hand while a guest is connected. */
let inviteOpen = false;
let hasInvite = false;
/** The room as the engine last reported it. */
let roster: LiveContext = initialContext();

const MAX_GUESTS = 3;

/** The number the host picked, always 1-3. */
function chosenGuests(): number {
  const picked = document.querySelector<HTMLInputElement>('input[name="lh-guests"]:checked');
  const n = Number(picked?.value);
  return Number.isInteger(n) && n >= 1 && n <= MAX_GUESTS ? n : 1;
}

function paintGuestsHint(): void {
  const hint = byId('lh-guests-hint');
  const n = chosenGuests();
  if (hint) hint.textContent = n === 1 ? t('liveGuestsHintOne') : fill(t('liveGuestsHint'), { max: String(n) });
}

function showError(code: string): void {
  const info = errorInfo(code, 'host');
  const status = byId('lh-status');
  if (status) {
    status.textContent = t(info.key);
    status.hidden = false;
  }
  const links: Record<string, ErrorAction> = { 'lh-buy': 'buy', 'lh-signin-link': 'signin', 'lh-home': 'home' };
  for (const [id, action] of Object.entries(links)) setHidden(id, info.action !== action);
  setHidden('lh-actions', !Object.values(links).includes(info.action));
}

function clearError(): void {
  setHidden('lh-status', true);
  setHidden('lh-actions', true);
}

/** Draw the QR lazily: the `qrcode` chunk is only fetched when an invite is actually shown. */
async function paintQr(text: string): Promise<void> {
  const img = byId<HTMLImageElement>('lh-qr');
  if (!img) return;
  img.alt = t('liveQrAlt');
  try {
    const { qrDataUrl } = await import('../qr');
    const src = await qrDataUrl(text, '');
    if (src) {
      img.src = src;
      img.hidden = false;
    } else {
      img.hidden = true;
    }
  } catch {
    // The copyable link still works without the QR.
    img.hidden = true;
  }
}

function wireInvite(url: string): void {
  const link = byId<HTMLInputElement>('lh-link');
  if (link) link.value = url;

  byId('lh-copy')?.addEventListener('click', () => {
    const write = navigator.clipboard?.writeText(url);
    void Promise.resolve(write)
      .then(() => {
        if (!write) throw new Error('no clipboard');
        toast(t('copied'), 'ok');
      })
      .catch(() => {
        // Leave the text selected so a long-press copy still works.
        link?.select();
        toast(t('copyFailed'), 'err');
      });
  });

  const share = byId('lh-share');
  if (share && typeof navigator.share === 'function') {
    share.hidden = false;
    share.addEventListener('click', () => {
      void navigator.share({ url, title: t('liveConnect') }).catch(() => {
        // Dismissing the share sheet rejects with AbortError; nothing to report.
      });
    });
  }
}

/** What the invitation panel shows: everything, a toggle, or nothing. */
function paintInvite(): void {
  const ctx = roster;
  for (const p of ctx.partners) seated.add(p.peerId);
  const here = ctx.partners.length;
  const several = maxGuests > 1;
  const full = seated.size >= maxGuests;
  const over = isTerminal(ctx);

  const seats = byId('lh-seats');
  if (seats) {
    seats.textContent = fill(t('liveSeats'), { n: String(Math.min(seated.size, maxGuests)), max: String(maxGuests) });
    seats.hidden = !several || over;
  }

  // Nobody here: the invitation is the whole point. A guest connected and seats left: it folds
  // away behind a toggle. Every seat taken (or the conversation over): nothing to offer.
  const hidden = !hasInvite || over || full;
  setHidden('lh-invite', hidden);
  const folded = !hidden && here > 0 && !inviteOpen;
  setHidden('lh-invite-body', folded);
  const toggle = byId<HTMLButtonElement>('lh-invite-toggle');
  if (toggle) {
    toggle.hidden = hidden || here === 0;
    toggle.setAttribute('aria-expanded', String(!folded));
    const key = folded ? 'liveInviteMore' : 'liveInviteHide';
    toggle.textContent = t(key);
    toggle.dataset.i18n = key;
  }
}

/** Wording for one guest vs several, set once the size is known. */
function paintInviteCopy(): void {
  const several = maxGuests > 1;
  for (const [id, one, many] of [
    ['lh-invite-title', 'liveInviteTitle', 'liveInviteTitleMulti'],
    ['lh-invite-expires', 'liveInviteExpires', 'liveInviteExpiresMulti'],
  ] as const) {
    const node = byId(id);
    if (!node) continue;
    const key = several ? many : one;
    node.dataset.i18n = key;
    node.textContent = t(key);
  }
}

function enterSession(session: StoredSession, qrText?: string): void {
  // The page title and blurb are for the setup; the conversation needs the room.
  setHidden('lh-hero', true);
  setHidden('lh-setup', true);
  setHidden('lh-session', false);
  maxGuests = session.maxGuests ?? 1;
  inviteOpen = false;
  roster = { ...initialContext(), phase: 'waiting_peer' };
  paintInviteCopy();
  const invite = session.inviteUrl;
  hasInvite = !!invite;
  if (invite) {
    wireInvite(invite);
    void paintQr(qrText ?? invite);
  }
  paintInvite();
  byId('lh-invite-toggle')?.addEventListener('click', () => {
    // Only shown while a guest is connected, where "folded" is the resting state.
    inviteOpen = !inviteOpen;
    paintInvite();
  });
  const container = byId('lh-room');
  if (!container) return;
  mountRoom({
    container,
    role: 'host',
    session,
    onState: (ctx: LiveContext) => {
      roster = ctx;
      // The panel follows the room: with nobody here it is simply open again.
      if (ctx.partners.length === 0) inviteOpen = false;
      paintInvite();
    },
  });
}

async function create(): Promise<void> {
  const lang = picker?.get();
  if (!lang || creating) return;
  creating = true;
  // Unlock audio inside the tap: iOS will not start an AudioContext once the gesture is
  // spent, and the request below outlives it.
  pcmPlayback.unlock();
  const btn = byId<HTMLButtonElement>('lh-create');
  if (btn) {
    btn.disabled = true;
    btn.textContent = t('liveCreating');
  }
  clearError();
  const guests = chosenGuests();
  const res = await createSession({
    lang,
    saveTranscript: !!byId<HTMLInputElement>('lh-transcript')?.checked,
    maxGuests: guests,
  });
  creating = false;
  if (btn) {
    btn.disabled = false;
    btn.textContent = t('liveCreate');
  }
  if (!res.ok) {
    showError(res.code);
    return;
  }
  const d = res.data;
  const session: StoredSession = {
    sessionId: d.session_id,
    token: d.token,
    role: 'host',
    myLang: lang,
    myPeerId: d.peer_id,
    wsPath: d.ws_path,
    inviteUrl: d.invite_url,
    // What the server confirmed, falling back to what was asked for.
    maxGuests: d.max_guests ?? guests,
  };
  seated.clear();
  saveStoredSession(session);
  enterSession(session, d.qr_payload);
}

/** A stored host session that is still open on the server, so a reload rejoins it. */
async function resumable(): Promise<StoredSession | null> {
  const stored = loadStoredSession();
  if (!stored || stored.role !== 'host') return null;
  seated.clear();
  const res = await getSession(stored.sessionId, stored.token);
  if (res.ok) {
    const info = res.data.session;
    if (info.status !== 'ended' && info.status !== 'expired') {
      // The server knows the size and who already took a seat; the stored copy may predate it.
      for (const p of info.participants ?? []) if (p.role === 'guest') seated.add(p.peer_id);
      const max = info.max_guests ?? stored.maxGuests;
      return max ? { ...stored, maxGuests: max } : stored;
    }
  } else if (res.status === 0 || res.status >= 500) {
    // The server could not be asked. The session may well be alive: let the engine's own
    // reconnect logic find out, rather than throwing the credentials away.
    return stored;
  }
  clearStoredSession();
  return null;
}

export async function mountLiveHost(): Promise<void> {
  const uiLang = initPage();

  if (!(await billingEnabled())) {
    setHidden('lh-unavailable', false);
    return;
  }
  if (!isLoggedIn()) {
    setHidden('lh-signin', false);
    return;
  }

  const codes = pickableLanguages().map((l) => l.code);
  const lang$ = byId('lh-lang');
  if (lang$) {
    picker = mountLangPicker(lang$, {
      idPrefix: 'lh',
      codes,
      initial: defaultLanguage(codes, getUser()?.language, uiLang),
      labelKey: 'liveYourLang',
    });
  }
  byId('lh-create')?.addEventListener('click', () => void create());
  paintGuestsHint();
  document.querySelectorAll('input[name="lh-guests"]').forEach((r) => r.addEventListener('change', paintGuestsHint));

  const again = await resumable();
  if (again) {
    enterSession(again);
    return;
  }
  setHidden('lh-setup', false);
}
