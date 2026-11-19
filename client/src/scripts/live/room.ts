// Live Connect — the conversation screen's controller (spec 0121).
//
// Glue between the DOM built by `view.ts` and a `LiveConversation`. Shared by the host and
// guest pages: it takes credentials (a stored session), starts an engine, and keeps the
// view in step with it. It owns no protocol logic and no audio logic.

import { setBalance } from '../auth';
import { toast } from '../toast';
import { icon } from '../icons';
import { t } from '../i18n';
import {
  LiveConversation,
  type Captions,
  type LiveConversationOptions,
} from './conversation';
import { noticeKey, type Role } from './messages';
import type { StoredSession } from './session-store';
import {
  buildRoom,
  fill,
  formatCredits,
  langLabel,
  renderCaptions,
  renderDirection,
  renderOutputOptions,
  renderPartner,
  renderPhase,
  type RoomEls,
} from './view';
import type { LiveContext } from './session-machine';

/** The slice of `LiveConversation` the controller drives; tests supply a fake. */
export interface ConversationLike {
  start(): Promise<void>;
  end(): void;
  release(): void;
  setMuted(muted: boolean): void;
  isMuted(): boolean;
  setHeadphones(on: boolean): void;
  headphones(): boolean;
  canSelectOutput(): boolean;
  listOutputDevices(): Promise<MediaDeviceInfo[]>;
  setOutputDevice(deviceId: string): Promise<boolean>;
  direction(): { myLang: string; partnerLang: string | null };
  state(): LiveContext;
}

export interface RoomOptions {
  container: HTMLElement;
  role: Role;
  session: StoredSession;
  /** i18n key for the "waiting" chip, when "the other person" is the wrong words (a desk). */
  waitingKey?: string;
  /** Every engine state change, after the room has repainted. The page owns the invite panel. */
  onState?: (ctx: LiveContext) => void;
  createConversation?: (opts: LiveConversationOptions) => ConversationLike;
}

export interface RoomHandle {
  els: RoomEls;
  destroy(): void;
}

export function mountRoom(opts: RoomOptions): RoomHandle {
  const doc = opts.container.ownerDocument;
  const els = buildRoom(doc, { role: opts.role });
  opts.container.append(els.root);
  const make = opts.createConversation ?? ((o) => new LiveConversation(o));
  const { session, role } = opts;
  let convo: ConversationLike;
  let outputsListed = false;
  let destroyed = false;

  const paintMute = (muted: boolean): void => {
    els.mute.setAttribute('aria-pressed', String(muted));
    els.muteLabel.textContent = t(muted ? 'talkUnmute' : 'talkMute');
    els.muteLabel.dataset.i18n = muted ? 'talkUnmute' : 'talkMute';
    els.mute.querySelector('svg')?.remove();
    els.mute.insertAdjacentHTML('afterbegin', icon(muted ? 'mic-off' : 'mic', 20));
  };

  const paintCredits = (balance: number, low: boolean): void => {
    if (!els.credits) return;
    els.credits.hidden = false;
    els.credits.dataset.low = String(low);
    els.credits.textContent = fill(t(low ? 'liveCreditsLow' : 'liveCredits'), { amount: formatCredits(balance) });
  };

  const onState = (ctx: LiveContext): void => {
    renderPhase(els, ctx, role, { waitingKey: opts.waitingKey });
    renderPartner(els, ctx.partner, ctx.phase);
    const dir = convo?.direction() ?? { myLang: session.myLang, partnerLang: ctx.partner?.lang ?? null };
    renderDirection(els, dir.myLang, ctx.partner?.lang ?? null);
    if (!ctx.partner) els.sameLang.hidden = true;
    if (ctx.phase === 'waiting_peer' && !outputsListed && convo?.canSelectOutput()) {
      // Labels are only available once the microphone has been granted.
      outputsListed = true;
      void convo.listOutputDevices().then((d) => {
        if (!destroyed) renderOutputOptions(els, d);
      });
    }
    opts.onState?.(ctx);
  };

  const onCaptions = (c: Captions): void => {
    renderCaptions(els, c, session.myLang);
    // A caption means translation is working again, so an old "translation is down" is stale.
    if (c.partner || c.mine) els.notice.hidden = true;
  };

  const build = (): ConversationLike =>
    make({
      sessionId: session.sessionId,
      token: session.token,
      wsPath: session.wsPath,
      role,
      myLang: session.myLang,
      myPeerId: session.myPeerId,
      inviteUrl: session.inviteUrl,
      onState,
      onCaptions,
      onBalance: (b) => {
        if (role !== 'host') return;
        // Keep the cached profile in step too, so leaving this page shows the real balance.
        setBalance(b);
        paintCredits(b, false);
      },
      onLowBalance: (b) => {
        if (role !== 'host') return;
        setBalance(b);
        paintCredits(b, true);
      },
      onNotice: (code) => {
        els.notice.textContent = t(noticeKey(code));
        els.notice.hidden = false;
      },
      onSameLanguage: (mine) => {
        els.sameLang.textContent = fill(t('liveSameLanguage'), { lang: langLabel(mine).native });
        els.sameLang.hidden = false;
      },
      onMuteChange: paintMute,
    });

  const begin = (): void => {
    convo = build();
    els.headphones.checked = convo.headphones();
    paintMute(false);
    void convo.start();
  };

  els.mute.addEventListener('click', () => convo.setMuted(!convo.isMuted()));
  els.end.addEventListener('click', () => {
    els.confirm.hidden = false;
    els.endYes.focus();
  });
  els.endNo.addEventListener('click', () => {
    els.confirm.hidden = true;
    els.end.focus();
  });
  els.endYes.addEventListener('click', () => {
    els.confirm.hidden = true;
    convo.end();
  });
  els.headphones.addEventListener('change', () => convo.setHeadphones(els.headphones.checked));
  els.output.addEventListener('change', () => {
    void convo.setOutputDevice(els.output.value).then((ok) => {
      if (!ok) toast(t('liveOutputFailed'), 'err');
    });
  });
  els.retry.addEventListener('click', () => {
    // A fresh engine on the SAME credentials: the session lives on the server, and a failed
    // microphone or a dropped network says nothing about it.
    outputsListed = false;
    begin();
  });

  // Leaving the page must not leave a microphone track live. It must NOT end the
  // conversation either: a reload rejoins from the stored session.
  const onPageHide = (): void => convo.release();
  window.addEventListener('pagehide', onPageHide);

  begin();

  return {
    els,
    destroy(): void {
      destroyed = true;
      window.removeEventListener('pagehide', onPageHide);
      convo.release();
      els.root.remove();
    },
  };
}
