// Live Connect — the conversation screen's controller (spec 0121).
//
// Glue between the DOM built by `view.ts` and a `LiveConversation`. Shared by the host and
// guest pages: it takes credentials (a stored session), starts an engine, and keeps the
// view in step with it. It owns no protocol logic and no audio logic.

import { getUser, setBalance } from '../auth';
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
  formatLiveMoney,
  langLabel,
  renderCaptions,
  renderDirection,
  renderOutputOptions,
  renderPartners,
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
  direction(): { myLang: string; partnerLangs: string[] };
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
  const els = buildRoom(doc, { role: opts.role, maxGuests: opts.session.maxGuests });
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

  // What the host last learned about their wallet, so each new figure repaints the whole line.
  let walletBalance: number | null = null;
  let walletLow = false;
  let spent = 0;

  const paintCredits = (balance: number, low: boolean): void => {
    walletBalance = balance;
    walletLow = low;
    if (!els.credits) return;
    els.credits.hidden = false;
    els.credits.dataset.low = String(low);
    const left = fill(t(low ? 'liveCreditsLow' : 'liveCredits'), { amount: formatLiveMoney(balance) });
    // Language-neutral on purpose: the running cost of THIS conversation, next to what is left.
    els.credits.textContent = `${left} · −${formatLiveMoney(spent)}`;
  };

  // Partners who share my language (peer id -> name): one banner naming them all, kept in step
  // with who is still in the room.
  const sameLang = new Map<string, string>();
  const paintSameLang = (): void => {
    if (!sameLang.size) {
      els.sameLang.hidden = true;
      return;
    }
    const names = [...sameLang.values()].filter(Boolean);
    els.sameLang.textContent = names.length
      ? fill(t('liveSameLanguageWith'), { name: names.join(', ') })
      : fill(t('liveSameLanguage'), { lang: langLabel(session.myLang).native });
    els.sameLang.hidden = false;
  };
  // Names are only worth showing once more than two people can talk.
  let showNames = (session.maxGuests ?? 1) > 1;

  const onState = (ctx: LiveContext): void => {
    renderPhase(els, ctx, role, { waitingKey: opts.waitingKey });
    renderPartners(els, ctx.partners, ctx.phase);
    const dir = convo?.direction() ?? { myLang: session.myLang, partnerLangs: ctx.partners.map((p) => p.lang) };
    renderDirection(els, dir.myLang, dir.partnerLangs);
    if (ctx.partners.length > 1) showNames = true;
    let pruned = false;
    for (const id of [...sameLang.keys()]) {
      if (!ctx.partners.some((p) => p.peerId === id)) {
        sameLang.delete(id);
        pruned = true;
      }
    }
    if (pruned) paintSameLang();
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
    renderCaptions(els, c, session.myLang, { showNames });
    // A caption means translation is working again, so an old "translation is down" is stale.
    if (c.others.length || c.mine) els.notice.hidden = true;
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
      maxGuests: session.maxGuests,
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
      onSpent: (n) => {
        if (role !== 'host') return;
        spent = n;
        if (walletBalance !== null) paintCredits(walletBalance, walletLow);
      },
      onNotice: (code) => {
        els.notice.textContent = t(noticeKey(code));
        els.notice.hidden = false;
      },
      onSameLanguage: (_mine, _theirs, partner) => {
        sameLang.set(partner.peerId, partner.name);
        paintSameLang();
      },
      onPeerChange: (change, partner) => {
        // A screen reader hears the room change; the list itself is not a live region.
        els.announce.textContent = fill(t(change === 'joined' ? 'liveJoinedAnnounce' : 'liveLeftAnnounce'), {
          name: partner.name || t('liveGuestName'),
        });
      },
      onMuteChange: paintMute,
    });

  const begin = (): void => {
    // Show the wallet from the first second instead of after the first charge.
    const cached = role === 'host' ? getUser()?.balance : undefined;
    if (typeof cached === 'number') paintCredits(cached, false);
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
