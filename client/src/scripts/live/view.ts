// Live Connect — presentation (spec 0121).
//
// All DOM lives here; all logic lives in `conversation.ts`. `buildRoom` creates the
// conversation view once, the `render*` functions repaint pieces of it from engine state,
// and both pages share it (the host and the guest see the same room, only the credit line
// and the way out differ). Text is only ever set through `textContent`: transcripts are
// strangers' words.
//
// Design target: two phones on a table between two people. The PARTNER's translation is the
// largest thing on screen, their original sits beneath it, and what I said is small.
// Controls are two buttons; everything else is quiet.

import './live.css';
import { icon } from '../icons';
import { t } from '../i18n';
import { isRtlLang, langMeta } from '../langmap';
import type { Captions, Exchange } from './conversation';
import { endedKey, failureKey, statusKey, type Role } from './messages';
import type { LiveContext, Partner, Phase } from './session-machine';

/** How many past exchanges stay in the DOM. The engine already bounds its own list. */
export const HISTORY_RENDER_LIMIT = 30;

export interface LangLabel {
  flag: string;
  native: string;
  english: string;
  rtl: boolean;
}

export function langLabel(code: string): LangLabel {
  const meta = langMeta(code);
  return {
    flag: meta?.flag ?? '🌐',
    native: meta?.native ?? code,
    english: meta?.english ?? code,
    rtl: isRtlLang(code),
  };
}

/** Substitute `{name}`-style placeholders — `t()` takes no parameters of its own. */
export function fill(template: string, vars: Record<string, string>): string {
  return Object.entries(vars).reduce((out, [k, v]) => out.split(`{${k}}`).join(v), template);
}

export function formatCredits(balance: number): string {
  return `$${Math.max(0, balance).toFixed(2)}`;
}

export interface RoomEls {
  root: HTMLElement;
  live: HTMLElement;
  status: HTMLElement;
  statusText: HTMLElement;
  partnerCard: HTMLElement;
  partnerName: HTMLElement;
  partnerMeta: HTMLElement;
  partnerMuted: HTMLElement;
  avatar: HTMLElement;
  dirOut: HTMLElement;
  dirIn: HTMLElement;
  sameLang: HTMLElement;
  caption: HTMLElement;
  captionOriginal: HTMLElement;
  mine: HTMLElement;
  mineText: HTMLElement;
  notice: HTMLElement;
  historyTitle: HTMLElement;
  history: HTMLElement;
  /** Host only. */
  credits: HTMLElement | null;
  headphones: HTMLInputElement;
  outputField: HTMLElement;
  output: HTMLSelectElement;
  mute: HTMLButtonElement;
  muteLabel: HTMLElement;
  end: HTMLButtonElement;
  confirm: HTMLElement;
  endYes: HTMLButtonElement;
  endNo: HTMLButtonElement;
  ended: HTMLElement;
  endedText: HTMLElement;
  endedAction: HTMLAnchorElement;
  problem: HTMLElement;
  problemText: HTMLElement;
  retry: HTMLButtonElement;
  buy: HTMLAnchorElement;
  home: HTMLAnchorElement;
}

type Doc = Document;

function el<K extends keyof HTMLElementTagNameMap>(
  doc: Doc,
  tag: K,
  cls?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const e = doc.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

/** A node whose text follows the UI language when `applyI18n()` runs. */
function i18n<K extends keyof HTMLElementTagNameMap>(
  doc: Doc,
  tag: K,
  cls: string,
  key: string,
): HTMLElementTagNameMap[K] {
  const e = el(doc, tag, cls, t(key));
  e.dataset.i18n = key;
  return e;
}

function button(doc: Doc, id: string, cls: string, key: string): HTMLButtonElement {
  const b = i18n(doc, 'button', cls, key);
  b.type = 'button';
  b.id = id;
  return b;
}

/** Build the whole conversation view (room, ended and problem screens) once. */
export function buildRoom(doc: Doc, opts: { role: Role }): RoomEls {
  const root = el(doc, 'div', 'lc-room');
  root.id = 'lc-room';
  root.dataset.phase = 'idle';
  // The page's visible heading is hidden once a conversation starts; assistive tech still
  // needs one, and the conversation is what this page is now.
  root.append(i18n(doc, 'h1', 'lc-sr-only', 'liveConnect'));

  // --- the conversation ----------------------------------------------------
  const live = el(doc, 'section', 'lc-live');
  live.id = 'lc-live';
  live.hidden = true;

  const status = el(doc, 'p', 'lc-chip');
  status.id = 'lc-status';
  status.setAttribute('role', 'status');
  status.setAttribute('aria-live', 'polite');
  const dot = el(doc, 'span', 'lc-dot');
  dot.setAttribute('aria-hidden', 'true');
  const statusText = el(doc, 'span', 'lc-chip-text');
  statusText.id = 'lc-status-text';
  status.append(dot, statusText);

  const partnerCard = el(doc, 'div', 'card lc-partner');
  partnerCard.id = 'lc-partner';
  partnerCard.dataset.online = 'false';
  const avatar = el(doc, 'span', 'lc-avatar');
  avatar.setAttribute('aria-hidden', 'true');
  const who = el(doc, 'div', 'lc-partner-who');
  const partnerName = el(doc, 'p', 'lc-partner-name');
  const partnerMeta = el(doc, 'p', 'lc-partner-meta');
  who.append(partnerName, partnerMeta);
  const partnerMuted = el(doc, 'span', 'lc-muted-badge');
  partnerMuted.hidden = true;
  partnerMuted.insertAdjacentHTML('beforeend', icon('mic-off', 16));
  const mutedText = i18n(doc, 'span', '', 'livePartnerMuted');
  partnerMuted.append(mutedText);
  partnerCard.append(avatar, who, partnerMuted);

  const direction = el(doc, 'ul', 'lc-direction');
  direction.id = 'lc-direction';
  const dirOut = el(doc, 'li');
  const dirIn = el(doc, 'li');
  direction.append(dirOut, dirIn);

  const sameLang = el(doc, 'p', 'lc-banner');
  sameLang.id = 'lc-same-lang';
  sameLang.setAttribute('role', 'status');
  sameLang.hidden = true;

  const stage = el(doc, 'div', 'lc-stage');
  stage.setAttribute('aria-live', 'polite');
  stage.dataset.empty = 'true';
  const caption = el(doc, 'p', 'lc-caption');
  caption.id = 'lc-caption';
  const captionOriginal = el(doc, 'p', 'lc-caption-original');
  captionOriginal.id = 'lc-caption-original';
  stage.append(caption, captionOriginal);

  const mine = el(doc, 'p', 'lc-mine');
  mine.id = 'lc-mine';
  mine.hidden = true;
  const mineLabel = i18n(doc, 'span', 'lc-mine-label', 'liveYouSaid');
  const mineText = el(doc, 'span', 'lc-mine-text');
  mine.append(mineLabel, ' ', mineText);

  const notice = el(doc, 'p', 'lc-banner');
  notice.id = 'lc-notice';
  notice.setAttribute('role', 'status');
  notice.hidden = true;

  const historyTitle = i18n(doc, 'h2', 'lc-history-title', 'talkHistoryTitle');
  historyTitle.hidden = true;
  const history = el(doc, 'div', 'lc-history');
  history.id = 'lc-history';
  // Off on purpose: every finished sentence landing here would flood a screen reader that
  // already heard it in the live region above.
  history.setAttribute('aria-live', 'off');

  let credits: HTMLElement | null = null;
  if (opts.role === 'host') {
    credits = el(doc, 'p', 'lc-credits');
    credits.id = 'lc-credits';
    credits.setAttribute('role', 'status');
    credits.hidden = true;
  }

  const settings = el(doc, 'div', 'lc-settings');
  const headLabel = el(doc, 'label', 'lc-check');
  const headphones = el(doc, 'input');
  headphones.type = 'checkbox';
  headphones.id = 'lc-headphones';
  const headText = el(doc, 'span');
  headText.append(i18n(doc, 'span', '', 'liveHeadphones'), i18n(doc, 'small', 'lc-hint', 'liveHeadphonesHint'));
  headLabel.append(headphones, headText);

  const outputField = el(doc, 'label', 'field lc-output');
  outputField.id = 'lc-output-field';
  outputField.hidden = true;
  const output = el(doc, 'select');
  output.id = 'lc-output';
  outputField.append(i18n(doc, 'span', 'field-label', 'liveOutput'), output);
  settings.append(headLabel, outputField);

  const controls = el(doc, 'div', 'lc-controls');
  const mute = button(doc, 'lc-mute', 'btn-ghost lc-ctl', 'talkMute');
  mute.textContent = '';
  mute.removeAttribute('data-i18n');
  mute.setAttribute('aria-pressed', 'false');
  mute.insertAdjacentHTML('beforeend', icon('mic', 20));
  const muteLabel = i18n(doc, 'span', '', 'talkMute');
  mute.append(muteLabel);
  const end = button(doc, 'lc-end', 'btn-primary lc-ctl lc-end', 'talkEnd');
  controls.append(mute, end);

  const confirm = el(doc, 'div', 'card lc-confirm');
  confirm.id = 'lc-confirm';
  confirm.setAttribute('role', 'alertdialog');
  confirm.setAttribute('aria-labelledby', 'lc-confirm-text');
  confirm.hidden = true;
  const confirmText = i18n(doc, 'p', 'lc-confirm-text', 'liveEndConfirm');
  confirmText.id = 'lc-confirm-text';
  const confirmBtns = el(doc, 'div', 'lc-confirm-btns');
  const endNo = button(doc, 'lc-end-no', 'btn-ghost lc-ctl', 'cancel');
  const endYes = button(doc, 'lc-end-yes', 'btn-primary lc-ctl lc-end', 'liveEnd');
  confirmBtns.append(endNo, endYes);
  confirm.append(confirmText, confirmBtns);

  live.append(
    status,
    partnerCard,
    direction,
    sameLang,
    stage,
    mine,
    notice,
    ...(credits ? [credits] : []),
    historyTitle,
    history,
    settings,
    controls,
    confirm,
  );

  // --- ended ----------------------------------------------------------------
  const ended = el(doc, 'section', 'lc-end-screen');
  ended.id = 'lc-ended';
  ended.hidden = true;
  const endedText = el(doc, 'p', 'lc-end-copy');
  endedText.setAttribute('role', 'status');
  const endedAction = el(doc, 'a', 'btn-primary lc-action');
  // Where "next" is depends on who you are: the host starts another conversation, a staff
  // member goes back to the queue, a guest has nothing further to do here.
  const way = {
    host: { key: 'liveNewConversation', href: '/live' },
    staff: { key: 'liveStaffBackToDesks', href: '/live/desks' },
    guest: { key: 'liveBackHome', href: '/' },
  }[opts.role];
  endedAction.dataset.i18n = way.key;
  endedAction.textContent = t(way.key);
  endedAction.href = way.href;
  ended.append(endedText, endedAction);

  // --- problem --------------------------------------------------------------
  const problem = el(doc, 'section', 'lc-end-screen');
  problem.id = 'lc-problem';
  problem.hidden = true;
  const problemText = el(doc, 'p', 'lc-end-copy');
  problemText.setAttribute('role', 'alert');
  const retry = button(doc, 'lc-retry', 'btn-primary lc-action', 'talkRetry');
  const buy = i18n(doc, 'a', 'btn-primary lc-action', 'buyCredits');
  buy.href = '/?buy=1';
  buy.hidden = true;
  const home = i18n(doc, 'a', 'btn-ghost lc-action', 'liveBackHome');
  home.href = '/';
  problem.append(problemText, retry, buy, home);

  root.append(live, ended, problem);
  return {
    root, live, status, statusText, partnerCard, partnerName, partnerMeta, partnerMuted, avatar,
    dirOut, dirIn, sameLang, caption, captionOriginal, mine, mineText, notice, historyTitle, history,
    credits, headphones, outputField, output, mute, muteLabel, end, confirm, endYes, endNo, ended,
    endedText, endedAction, problem, problemText, retry, buy, home,
  };
}

const ACTIVE: ReadonlySet<Phase> = new Set<Phase>(['idle', 'requesting_mic', 'connecting', 'waiting_peer', 'live', 'reconnecting']);

/** Which of the three screens shows, and the status chip. */
export function renderPhase(
  els: RoomEls,
  ctx: LiveContext,
  role: Role,
  /** `waitingKey` replaces the "waiting for the other person" chip text. */
  opts: { waitingKey?: string } = {},
): void {
  els.root.dataset.phase = ctx.phase;
  els.live.hidden = !ACTIVE.has(ctx.phase) || ctx.phase === 'idle';
  els.ended.hidden = ctx.phase !== 'ended';
  els.problem.hidden = ctx.phase !== 'failed';
  els.statusText.textContent = t(ctx.phase === 'waiting_peer' && opts.waitingKey ? opts.waitingKey : statusKey(ctx));
  els.confirm.hidden = true;

  if (ctx.phase === 'ended' && ctx.endReason) {
    els.endedText.textContent = t(endedKey(ctx.endReason, role));
  }
  if (ctx.phase === 'failed') {
    const kind = ctx.failure;
    els.problemText.textContent = t(failureKey(kind));
    // Out of credits is the one failure only the host can fix, by topping up; a retry
    // would just run into it again. An unsupported browser cannot be retried either.
    const credits = kind === 'credits';
    els.buy.hidden = !(credits && role === 'host');
    els.retry.hidden = credits || kind === 'unsupported';
  }
}

export function renderPartner(els: RoomEls, partner: Partner | null, phase: Phase): void {
  const online = !!partner && phase === 'live';
  els.partnerCard.dataset.online = String(online);
  els.partnerName.textContent = partner?.name ?? '';
  els.avatar.textContent = partner?.name ? Array.from(partner.name)[0].toUpperCase() : '·';
  if (!partner) {
    els.partnerMeta.textContent = t('livePartnerOffline');
    els.partnerMuted.hidden = true;
    return;
  }
  const label = langLabel(partner.lang);
  els.partnerMeta.textContent = online
    ? `${label.flag} ${label.native} · ${t('livePartnerOnline')}`
    : `${label.flag} ${label.native}`;
  els.partnerMuted.hidden = !partner.muted;
}

/** Both directions, spelled out: who speaks what, who hears what. */
export function renderDirection(els: RoomEls, myLang: string, partnerLang: string | null): void {
  const mine = langLabel(myLang).native;
  if (!partnerLang) {
    els.dirOut.textContent = fill(t('liveYouSpeakOnly'), { from: mine });
    els.dirIn.textContent = t('liveDirectionWaiting');
    return;
  }
  const theirs = langLabel(partnerLang).native;
  els.dirOut.textContent = fill(t('liveYouSpeak'), { from: mine, to: theirs });
  els.dirIn.textContent = fill(t('liveTheySpeak'), { from: theirs, to: mine });
}

function applyDir(node: HTMLElement, code: string | null | undefined): void {
  if (!code) {
    node.removeAttribute('dir');
    node.removeAttribute('lang');
    return;
  }
  node.setAttribute('dir', isRtlLang(code) ? 'rtl' : 'ltr');
  node.setAttribute('lang', code);
}

export function renderCaptions(els: RoomEls, c: Captions, myLang?: string): void {
  els.caption.textContent = c.partner?.text ?? '';
  // An empty stage collapses (the live region stays in the DOM, so the first caption is
  // still announced) instead of showing a big blank card while nobody has spoken.
  els.caption.parentElement?.setAttribute('data-empty', String(!c.partner));
  applyDir(els.caption, c.partner ? myLang : null);
  els.captionOriginal.textContent = c.partner?.original ?? '';
  applyDir(els.captionOriginal, c.partner?.original ? c.partner.lang : null);
  els.caption.dataset.settled = String(c.partner?.settled ?? false);

  els.mine.hidden = !c.mine;
  els.mineText.textContent = c.mine?.text ?? '';
  applyDir(els.mineText, c.mine?.lang);

  els.history.textContent = '';
  // Newest first, so older exchanges scroll away.
  const shown = c.history.slice(-HISTORY_RENDER_LIMIT).reverse();
  for (const ex of shown) els.history.append(buildExchangeCard(ex, els.history.ownerDocument));
  els.historyTitle.hidden = shown.length === 0;
}

/** One history card. The partner's translation leads; my own words are quieter. */
export function buildExchangeCard(ex: Exchange, doc: Doc): HTMLElement {
  const card = el(doc, 'article', 'lc-card');
  card.dataset.exchange = String(ex.id);
  card.dataset.who = ex.who;

  const useTranslated = ex.who === 'partner' && ex.translated !== '';
  const main = el(doc, 'p', 'lc-card-main');
  if (ex.who === 'me') main.append(el(doc, 'span', 'lc-card-label', t('liveYouSaid')), ' ');
  const mainText = el(doc, 'span', 'lc-card-text', useTranslated ? ex.translated : ex.original);
  applyDir(mainText, useTranslated ? ex.translatedLang : ex.originalLang);
  main.append(mainText);
  card.append(main);

  const sub = useTranslated ? ex.original : ex.who === 'me' ? ex.translated : '';
  if (sub) {
    const p = el(doc, 'p', 'lc-card-sub');
    const text = el(doc, 'span', 'lc-card-text', sub);
    applyDir(text, useTranslated ? ex.originalLang : ex.translatedLang);
    p.append(text);
    card.append(p);
  }
  return card;
}

export function renderOutputOptions(els: RoomEls, devices: MediaDeviceInfo[]): void {
  els.output.textContent = '';
  els.outputField.hidden = devices.length === 0;
  devices.forEach((d, i) => {
    const o = el(els.output.ownerDocument, 'option');
    o.value = d.deviceId;
    o.textContent =
      d.label || (d.deviceId === 'default' ? t('liveOutputDefault') : `${t('liveOutput')} ${i + 1}`);
    els.output.append(o);
  });
}
