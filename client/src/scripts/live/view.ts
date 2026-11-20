// Live Connect — presentation (spec 0121).
//
// All DOM lives here; all logic lives in `conversation.ts`. `buildRoom` creates the
// conversation view once, the `render*` functions repaint pieces of it from engine state,
// and both pages share it (the host and the guest see the same room, only the credit line
// and the way out differ). Text is only ever set through `textContent`: transcripts are
// strangers' words.
//
// Design target: phones on a table between two to four people. Each OTHER speaker's translation
// is the largest thing on screen (one block per speaker, named when there are several), their
// original sits beneath it, and what I said is small. Controls are two buttons; everything else
// is quiet.

import './live.css';
import { icon } from '../icons';
import { t } from '../i18n';
import { isRtlLang, langMeta } from '../langmap';
import type { Captions, Exchange, SpeakerCaption } from './conversation';
import { endedKey, failureKey, statusKey, type Role } from './messages';
import type { LiveContext, Partner, Phase } from './session-machine';
import { tierLabel } from './tiers';

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

/**
 * A wallet amount to four decimals. A live conversation costs a fraction of a cent per tick, so
 * the usual two decimals would show a balance that never seems to move.
 */
export function formatLiveMoney(amount: number): string {
  return `$${Math.max(0, amount).toFixed(4)}`;
}

export interface RoomEls {
  root: HTMLElement;
  live: HTMLElement;
  status: HTMLElement;
  statusText: HTMLElement;
  /** The tier the conversation runs on, in the status chip. Hidden while it is not known. */
  tier: HTMLElement;
  /** Polite notice: the tier asked for was busy and the conversation went on Standard. */
  tierNotice: HTMLElement;
  tierNoticeText: HTMLElement;
  tierNoticeClose: HTMLButtonElement;
  /** The list of everybody else in the room (`ul`, one row each). */
  partners: HTMLElement;
  dirOut: HTMLElement;
  dirIn: HTMLElement;
  sameLang: HTMLElement;
  /** Polite live region: who joined, who left. Visually hidden. */
  announce: HTMLElement;
  /** The captions of the other speakers, one block each. A polite live region. */
  stage: HTMLElement;
  mine: HTMLElement;
  mineText: HTMLElement;
  /** Enhanced: this phone is speaking a translation right now. Polite, hidden otherwise. */
  speaking: HTMLElement;
  /** Guest, Enhanced: what they hear is spoken in an imitation of the host's voice. */
  voiceNote: HTMLElement;
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
  confirmText: HTMLElement;
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

/**
 * Build the whole conversation view (room, ended and problem screens) once. `maxGuests` is how
 * many guests the invitation admits: with more than one, a guest's hang-up only LEAVES (the others
 * carry on) and the host's ends it for everybody, and the words say so.
 */
export function buildRoom(doc: Doc, opts: { role: Role; maxGuests?: number }): RoomEls {
  const several = (opts.maxGuests ?? 1) > 1;
  const leaves = opts.role === 'guest' && several;
  const endKeys = leaves
    ? { button: 'liveLeave', confirm: 'liveLeaveConfirm', yes: 'liveLeave' }
    : opts.role === 'host' && several
      ? { button: 'talkEnd', confirm: 'liveEndConfirmAll', yes: 'liveEnd' }
      : { button: 'talkEnd', confirm: 'liveEndConfirm', yes: 'liveEnd' };
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
  const tier = el(doc, 'span', 'lc-tier-badge');
  tier.id = 'lc-tier';
  tier.hidden = true;
  status.append(dot, statusText, tier);

  const partnersTitle = i18n(doc, 'h2', 'lc-sr-only', 'livePartnersTitle');
  partnersTitle.id = 'lc-partners-title';
  const partners = el(doc, 'ul', 'lc-partners');
  partners.id = 'lc-partners';
  partners.setAttribute('aria-labelledby', 'lc-partners-title');

  const direction = el(doc, 'ul', 'lc-direction');
  direction.id = 'lc-direction';
  const dirOut = el(doc, 'li');
  const dirIn = el(doc, 'li');
  direction.append(dirOut, dirIn);

  const sameLang = el(doc, 'p', 'lc-banner');
  sameLang.id = 'lc-same-lang';
  sameLang.setAttribute('role', 'status');
  sameLang.hidden = true;

  // Join/leave announcements for assistive tech; the partners list itself is not a live region.
  const announce = el(doc, 'p', 'lc-sr-only');
  announce.id = 'lc-announce';
  announce.setAttribute('role', 'status');
  announce.setAttribute('aria-live', 'polite');

  const stage = el(doc, 'div', 'lc-stage');
  stage.id = 'lc-stage';
  stage.setAttribute('aria-live', 'polite');
  stage.dataset.empty = 'true';

  const mine = el(doc, 'p', 'lc-mine');
  mine.id = 'lc-mine';
  mine.hidden = true;
  const mineLabel = i18n(doc, 'span', 'lc-mine-label', 'liveYouSaid');
  const mineText = el(doc, 'span', 'lc-mine-text');
  mine.append(mineLabel, ' ', mineText);

  const speaking = i18n(doc, 'p', 'lc-speaking', 'talkStatusSpeaking');
  speaking.id = 'lc-speaking';
  speaking.setAttribute('role', 'status');
  speaking.setAttribute('aria-live', 'polite');
  speaking.hidden = true;

  const voiceNote = i18n(doc, 'p', 'lc-banner', 'liveJoinNoticeVoiceClone');
  voiceNote.id = 'lc-voice-note';
  voiceNote.setAttribute('role', 'status');
  voiceNote.hidden = true;

  const notice = el(doc, 'p', 'lc-banner');
  notice.id = 'lc-notice';
  notice.setAttribute('role', 'status');
  notice.hidden = true;

  // Not the `notice` above: that one clears itself when a caption arrives, and this one is news
  // the person should get to read and dismiss. Polite: the conversation is not interrupted.
  const tierNotice = el(doc, 'div', 'lc-banner lc-tier-notice');
  tierNotice.id = 'lc-tier-notice';
  tierNotice.setAttribute('role', 'status');
  tierNotice.setAttribute('aria-live', 'polite');
  tierNotice.hidden = true;
  const tierNoticeText = el(doc, 'span');
  const tierNoticeClose = button(doc, 'lc-tier-notice-close', 'btn-ghost lc-tier-notice-close', 'dismiss');
  tierNotice.append(tierNoticeText, tierNoticeClose);

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
  const end = button(doc, 'lc-end', 'btn-primary lc-ctl lc-end', endKeys.button);
  controls.append(mute, end);

  const confirm = el(doc, 'div', 'card lc-confirm');
  confirm.id = 'lc-confirm';
  confirm.setAttribute('role', 'alertdialog');
  confirm.setAttribute('aria-labelledby', 'lc-confirm-text');
  confirm.hidden = true;
  const confirmText = i18n(doc, 'p', 'lc-confirm-text', endKeys.confirm);
  confirmText.id = 'lc-confirm-text';
  const confirmBtns = el(doc, 'div', 'lc-confirm-btns');
  const endNo = button(doc, 'lc-end-no', 'btn-ghost lc-ctl', 'cancel');
  const endYes = button(doc, 'lc-end-yes', 'btn-primary lc-ctl lc-end', endKeys.yes);
  confirmBtns.append(endNo, endYes);
  confirm.append(confirmText, confirmBtns);

  live.append(
    status,
    partnersTitle,
    partners,
    announce,
    direction,
    sameLang,
    stage,
    mine,
    speaking,
    voiceNote,
    notice,
    tierNotice,
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
    root, live, status, statusText, tier, tierNotice, tierNoticeText, tierNoticeClose, partners, dirOut, dirIn, sameLang, announce, stage, mine, mineText, speaking, voiceNote,
    notice, historyTitle, history, credits, headphones, outputField, output, mute, muteLabel, end,
    confirm, confirmText, endYes, endNo, ended,
    endedText, endedAction, problem, problemText, retry, buy, home,
  };
}

/** The tier badge: its name, or nothing when it is not known (a desk conversation has none). */
export function renderTier(els: RoomEls, engine: string | undefined): void {
  els.tier.hidden = !engine;
  els.tier.textContent = engine ? fill(t('liveTierBadge'), { tier: tierLabel(engine) }) : '';
}

/** Guest: tell them the voice they hear imitates the host's, for as long as that is true. */
export function renderVoiceNote(els: RoomEls, on: boolean): void {
  els.voiceNote.hidden = !on;
}

/** Enhanced: show or hide the quiet "Speaking…" line while a translation is being spoken. */
export function renderSpeaking(els: RoomEls, on: boolean): void {
  els.speaking.hidden = !on;
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

/** One row per person in the room: who, which language, connected, muted. */
export function renderPartners(els: RoomEls, partners: Partner[], phase: Phase): void {
  const doc = els.partners.ownerDocument;
  els.partners.textContent = '';
  const online = phase === 'live';
  if (!partners.length) {
    els.partners.append(partnerRow(doc, null, false));
    return;
  }
  for (const p of partners) els.partners.append(partnerRow(doc, p, online));
}

function partnerRow(doc: Doc, partner: Partner | null, online: boolean): HTMLElement {
  const row = el(doc, 'li', 'card lc-partner');
  row.dataset.online = String(online);
  if (partner) row.dataset.peer = partner.peerId;
  const name = partner ? partner.name || t('liveGuestName') : '';
  const avatar = el(doc, 'span', 'lc-avatar', name ? Array.from(name)[0].toUpperCase() : '·');
  avatar.setAttribute('aria-hidden', 'true');
  const who = el(doc, 'div', 'lc-partner-who');
  const nameEl = el(doc, 'p', 'lc-partner-name', name);
  const meta = el(doc, 'p', 'lc-partner-meta');
  if (!partner) {
    meta.textContent = t('livePartnerOffline');
  } else {
    const label = langLabel(partner.lang);
    meta.textContent = online
      ? `${label.flag} ${label.native} · ${t('livePartnerOnline')}`
      : `${label.flag} ${label.native}`;
  }
  who.append(nameEl, meta);
  row.append(avatar, who);
  if (partner?.muted) {
    const badge = el(doc, 'span', 'lc-muted-badge');
    badge.insertAdjacentHTML('beforeend', icon('mic-off', 16));
    badge.append(el(doc, 'span', '', t('livePartnerMuted')));
    row.append(badge);
  }
  return row;
}

/** Distinct languages (by primary subtag), first seen first. */
function distinctLangs(codes: string[]): string[] {
  const out: string[] = [];
  for (const c of codes) {
    const key = c.toLowerCase().split(/[-_]/)[0];
    if (c && !out.some((o) => o.toLowerCase().split(/[-_]/)[0] === key)) out.push(c);
  }
  return out;
}

/** Both directions, spelled out: who speaks what, who hears what. With several partners, every distinct language. */
export function renderDirection(els: RoomEls, myLang: string, partnerLangs: string[]): void {
  const mine = langLabel(myLang).native;
  const theirs = distinctLangs(partnerLangs).map((l) => langLabel(l).native);
  if (!theirs.length) {
    els.dirOut.textContent = fill(t('liveYouSpeakOnly'), { from: mine });
    els.dirIn.textContent = t('liveDirectionWaiting');
    return;
  }
  const list = theirs.join(', ');
  els.dirOut.textContent = fill(t('liveYouSpeak'), { from: mine, to: list });
  els.dirIn.textContent = fill(t('liveTheySpeak'), { from: list, to: mine });
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

function buildSpeakerBlock(doc: Doc): HTMLElement {
  const block = el(doc, 'div', 'lc-speaker');
  const name = el(doc, 'p', 'lc-speaker-name');
  const caption = el(doc, 'p', 'lc-caption');
  const original = el(doc, 'p', 'lc-caption-original');
  block.append(name, caption, original);
  return block;
}

function paintSpeaker(block: HTMLElement, sp: SpeakerCaption, first: boolean, myLang: string | undefined, showName: boolean): void {
  const [name, caption, original] = Array.from(block.children) as HTMLElement[];
  block.dataset.speaker = sp.speakerId;
  name.textContent = sp.name;
  name.hidden = !(showName && sp.name);
  caption.textContent = sp.text;
  applyDir(caption, myLang);
  caption.dataset.settled = String(sp.settled);
  original.textContent = sp.original;
  applyDir(original, sp.original ? sp.lang : null);
  // The first block keeps the ids a single-speaker room always had.
  for (const [node, id] of [[caption, 'lc-caption'], [original, 'lc-caption-original']] as const) {
    if (first) node.id = id;
    else node.removeAttribute('id');
  }
}

/**
 * Paint every speaker's line, mine and the history. Blocks are reused per speaker (so a
 * screen reader hears an update, not a rebuild) and reordered only when the order changed.
 * `showNames`: label each block and history card with who spoke (a room with several people).
 */
export function renderCaptions(els: RoomEls, c: Captions, myLang?: string, opts: { showNames?: boolean } = {}): void {
  const showNames = opts.showNames ?? false;
  const doc = els.stage.ownerDocument;
  const blocks = new Map<string, HTMLElement>();
  for (const child of Array.from(els.stage.children) as HTMLElement[]) {
    if (child.dataset.speaker) blocks.set(child.dataset.speaker, child);
  }
  const keep = new Set<HTMLElement>();
  c.others.forEach((sp, i) => {
    const block = blocks.get(sp.speakerId) ?? buildSpeakerBlock(doc);
    paintSpeaker(block, sp, i === 0, myLang, showNames);
    if (els.stage.children[i] !== block) els.stage.insertBefore(block, els.stage.children[i] ?? null);
    keep.add(block);
  });
  for (const child of Array.from(els.stage.children) as HTMLElement[]) if (!keep.has(child)) child.remove();
  // An empty stage collapses (the live region stays in the DOM, so the first caption is
  // still announced) instead of showing a big blank card while nobody has spoken.
  els.stage.dataset.empty = String(c.others.length === 0);

  els.mine.hidden = !c.mine;
  els.mineText.textContent = c.mine?.text ?? '';
  applyDir(els.mineText, c.mine?.lang);

  els.history.textContent = '';
  // Newest first, so older exchanges scroll away.
  const shown = c.history.slice(-HISTORY_RENDER_LIMIT).reverse();
  for (const ex of shown) els.history.append(buildExchangeCard(ex, els.history.ownerDocument, showNames));
  els.historyTitle.hidden = shown.length === 0;
}

/** One history card. The partner's translation leads; my own words are quieter. */
export function buildExchangeCard(ex: Exchange, doc: Doc, showName = false): HTMLElement {
  const card = el(doc, 'article', 'lc-card');
  card.dataset.exchange = String(ex.id);
  card.dataset.who = ex.who;

  const useTranslated = ex.who === 'partner' && ex.translated !== '';
  const main = el(doc, 'p', 'lc-card-main');
  if (ex.who === 'me') main.append(el(doc, 'span', 'lc-card-label', t('liveYouSaid')), ' ');
  else if (showName && ex.speakerName) main.append(el(doc, 'span', 'lc-card-label', ex.speakerName), ' ');
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
