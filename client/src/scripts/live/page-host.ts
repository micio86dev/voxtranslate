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
import { isTerminal, type LiveContext } from './session-machine';
import { clearStoredSession, loadStoredSession, saveStoredSession, type StoredSession } from './session-store';

let picker: LangPicker | null = null;
let creating = false;

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

function enterSession(session: StoredSession, qrText?: string): void {
  // The page title and blurb are for the setup; the conversation needs the room.
  setHidden('lh-hero', true);
  setHidden('lh-setup', true);
  setHidden('lh-session', false);
  const invite = session.inviteUrl;
  if (invite) {
    setHidden('lh-invite', false);
    wireInvite(invite);
    void paintQr(qrText ?? invite);
  }
  const container = byId('lh-room');
  if (!container) return;
  mountRoom({
    container,
    role: 'host',
    session,
    onState: (ctx: LiveContext) => {
      // The invite is for the person who has not arrived. Once they are here it is noise,
      // and when they leave it is useful again.
      setHidden('lh-invite', !invite || !!ctx.partner || isTerminal(ctx));
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
  const res = await createSession({
    lang,
    saveTranscript: !!byId<HTMLInputElement>('lh-transcript')?.checked,
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
  };
  saveStoredSession(session);
  enterSession(session, d.qr_payload);
}

/** A stored host session that is still open on the server, so a reload rejoins it. */
async function resumable(): Promise<StoredSession | null> {
  const stored = loadStoredSession();
  if (!stored || stored.role !== 'host') return null;
  const res = await getSession(stored.sessionId, stored.token);
  if (res.ok) {
    const status = res.data.session.status;
    if (status !== 'ended' && status !== 'expired') return stored;
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

  const again = await resumable();
  if (again) {
    enterSession(again);
    return;
  }
  setHidden('lh-setup', false);
}
