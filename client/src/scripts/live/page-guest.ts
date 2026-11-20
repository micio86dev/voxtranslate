// Live Connect — guest page wiring (spec 0121). The thin layer between the DOM in
// `pages/lc/[code].astro` and the engine: language, optional name, consent, join.
//
// The guest has no account and must be able to start talking in two taps, so this page
// imports nothing heavy: no app.ts, no call modules, no `qrcode`.
//
// The invitation code is a secret. It is read once from a data attribute, sent to the join
// request and nowhere else: not logged, not stored, not put in the room's credentials.

import { t } from '../i18n';
import { pcmPlayback } from '../pcm-playback';
import { getSession, joinSession } from './api';
import { browserSupported } from './conversation';
import { defaultLanguage, mountLangPicker, pickableLanguages, type LangPicker } from './lang-picker';
import { errorInfo, failureKey } from './messages';
import { byId, initPage, setHidden } from './page-common';
import { mountRoom } from './room';
import { clearStoredSession, loadStoredSession, saveStoredSession, type StoredSession } from './session-store';

let picker: LangPicker | null = null;
let joining = false;
let code = '';

function showStatus(key: string, action: 'retry' | 'home' | 'none'): void {
  const status = byId('lg-status');
  if (status) {
    status.textContent = t(key);
    status.hidden = false;
  }
  setHidden('lg-retry', action !== 'retry');
  setHidden('lg-home', action !== 'home');
  setHidden('lg-actions', action === 'none');
}

function clearStatus(): void {
  setHidden('lg-status', true);
  setHidden('lg-actions', true);
}

function enterSession(session: StoredSession): void {
  setHidden('lg-hero', true);
  setHidden('lg-join', true);
  setHidden('lg-session', false);
  const container = byId('lg-room');
  if (container) mountRoom({ container, role: 'guest', session });
}

async function join(): Promise<void> {
  if (joining) return;
  clearStatus();
  if (!browserSupported()) {
    showStatus(failureKey('unsupported'), 'none');
    return;
  }
  const consent = byId<HTMLInputElement>('lg-consent');
  if (!consent?.checked) {
    // The refusal is stated in words rather than by disabling the button: a disabled
    // control gives a stranger on a phone no way to learn what is missing.
    showStatus(errorInfo('consent_required', 'guest').key, 'none');
    consent?.focus();
    return;
  }
  const lang = picker?.get();
  if (!lang) return;

  joining = true;
  // Unlock audio inside the tap: iOS will not start an AudioContext once the gesture is spent.
  pcmPlayback.unlock();
  const btn = byId<HTMLButtonElement>('lg-join-btn');
  if (btn) {
    btn.disabled = true;
    btn.textContent = t('liveJoining');
  }
  const name = byId<HTMLInputElement>('lg-name')?.value.trim() || t('liveGuestName');

  const res = await joinSession({ code, name, lang, consent: true });
  joining = false;
  if (btn) {
    btn.disabled = false;
    btn.textContent = t('liveJoin');
  }
  if (!res.ok) {
    const info = errorInfo(res.code, 'guest');
    showStatus(info.key, info.action === 'retry' || info.action === 'home' ? info.action : 'none');
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
    // With several guests on the link, hanging up only LEAVES: the engine needs to know.
    ...(d.max_guests ? { maxGuests: d.max_guests } : {}),
    // The tier the host chose (`standard` after a fallback): shown as a badge in the room.
    ...(d.engine ? { engine: d.engine } : {}),
    // Enhanced + the host's cloned voice: the room says so (the join response is the first the guest can know).
    ...(d.voice_clone ? { voiceClone: true } : {}),
  };
  saveStoredSession(session);
  enterSession(session);
}

/** A stored guest session that is still open on the server, so a reload rejoins it. */
async function resumable(): Promise<StoredSession | null> {
  const stored = loadStoredSession();
  if (!stored || stored.role !== 'guest') return null;
  const res = await getSession(stored.sessionId, stored.token);
  if (res.ok) {
    const info = res.data.session;
    if (info.status !== 'ended' && info.status !== 'expired') {
      // A session stored before multi-guest does not know its size; the server does.
      return {
        ...stored,
        ...(info.max_guests && !stored.maxGuests ? { maxGuests: info.max_guests } : {}),
        // The server knows the tier NOW: a Premium conversation may have fallen back to Standard.
        ...(info.engine ? { engine: info.engine } : {}),
        ...(info.voice_clone ? { voiceClone: true } : {}),
      };
    }
  } else if (res.status === 0 || res.status >= 500) {
    // The server could not be asked: let the engine's reconnect logic find out.
    return stored;
  }
  clearStoredSession();
  return null;
}

export async function mountLiveGuest(): Promise<void> {
  const uiLang = initPage();
  code = byId('lg-root')?.dataset.code ?? '';

  const codes = pickableLanguages().map((l) => l.code);
  const lang$ = byId('lg-lang');
  if (lang$) {
    const browser = typeof navigator !== 'undefined' ? [...(navigator.languages ?? []), navigator.language] : [];
    picker = mountLangPicker(lang$, {
      idPrefix: 'lg',
      codes,
      initial: defaultLanguage(codes, ...browser, uiLang),
      labelKey: 'liveYourLang',
    });
  }

  byId('lg-join-btn')?.addEventListener('click', () => void join());
  byId('lg-retry')?.addEventListener('click', () => void join());

  const again = await resumable();
  if (again) {
    enterSession(again);
    return;
  }
  if (!browserSupported()) {
    showStatus(failureKey('unsupported'), 'none');
    const btn = byId<HTMLButtonElement>('lg-join-btn');
    if (btn) btn.disabled = true;
  }
}
