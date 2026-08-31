// Talk to Anyone — presentation (spec 0110).
//
// All DOM lives here; all logic lives in `conversation.ts`. The split is what lets the
// state machine and the frame handling be unit-tested without a browser, and it is the
// brief's §42 rule ("keep business logic outside presentation components") applied
// honestly rather than nominally.
//
// The design brief is a person standing in a taxi: the TRANSLATION is the largest thing
// on screen, the original sits smaller beneath it, and there are exactly three controls.
// Nothing here explains itself with jargon, and no flag is ever the only way a language
// is named — screen readers and anyone who does not recognise a flag get the name too
// (brief §30).

import { t } from '../i18n';
import { langMeta, isRtlLang } from '../langmap';
import type { Exchange, LiveExchange } from './conversation';
import type { FailureKind, SessionContext } from './session-machine';

/** How many past exchanges stay in the DOM. Beyond this it is scrollback, not context. */
export const HISTORY_LIMIT = 20;

/** Flag + endonym + English name for a language code. */
export interface LangLabel {
  flag: string;
  native: string;
  english: string;
  rtl: boolean;
}

export function langLabel(code: string): LangLabel {
  const meta = langMeta(code);
  return {
    // `languages.json` already uses 🌐 for stateless languages rather than a misleading
    // flag, so this needs no special-casing.
    flag: meta?.flag ?? '🌐',
    native: meta?.native ?? code,
    english: meta?.english ?? code,
    rtl: isRtlLang(code),
  };
}

/**
 * The i18n key for the status line.
 *
 * `detecting` deliberately reads the same as `listening`. While the direction is
 * unresolved the honest thing to show is that we are still listening — a "detecting
 * language" state would be a technical detail leaking into a tourist's screen, and an
 * error would be a lie (brief §16).
 */
export function statusKey(ctx: SessionContext): string {
  switch (ctx.phase) {
    case 'idle':
    case 'ended':
      return 'talkStatusIdle';
    case 'requesting_mic':
      return 'talkStatusMic';
    case 'connecting':
      return 'talkStatusConnecting';
    case 'reconnecting':
      return 'talkStatusReconnecting';
    case 'paused':
      return 'talkStatusPaused';
    case 'stopping':
      return 'talkStatusEnding';
    case 'error':
      return failureKey(ctx.failure);
    case 'live':
      switch (ctx.activity) {
        case 'translating':
          return 'talkStatusTranslating';
        case 'speaking':
          return 'talkStatusSpeaking';
        case 'detecting':
        case 'listening':
        default:
          return 'talkStatusListening';
      }
  }
}

/** The i18n key explaining a failure, in plain language with a way forward. */
export function failureKey(kind: FailureKind | null): string {
  switch (kind) {
    case 'mic_denied':
      return 'talkErrorMicDenied';
    case 'mic_blocked':
      return 'talkErrorMicBusy';
    case 'mic_missing':
      return 'talkErrorMicMissing';
    case 'unsupported':
      return 'talkErrorUnsupported';
    case 'connection':
      return 'talkErrorConnection';
    case 'credits':
      return 'talkErrorCredits';
    case 'provider':
      return 'talkErrorProvider';
    default:
      return 'talkErrorGeneric';
  }
}

/** True while the conversation is doing something the user should see as "active". */
export function isActivePhase(ctx: SessionContext): boolean {
  return (
    ctx.phase === 'live' || ctx.phase === 'connecting' || ctx.phase === 'reconnecting'
  );
}

/** Substitute `{lang}`-style placeholders — `t()` takes no parameters of its own. */
export function fill(template: string, vars: Record<string, string>): string {
  return Object.entries(vars).reduce(
    (out, [k, v]) => out.split(`{${k}}`).join(v),
    template,
  );
}

/**
 * Format the live credit meter: what this conversation has cost, and what is left.
 *
 * The two numbers deliberately carry different precision. A Standard conversation burns
 * about $0.0009 every five seconds, so the app's usual two decimals would sit at "$0.00"
 * for the first half-minute and the meter would look broken — the spend needs four to
 * visibly move. The remaining balance keeps two, matching every other place the balance
 * is shown, because a fourth decimal on twelve dollars is noise.
 *
 * `startBalance` is what the account held when Start was pressed. A null balance (the
 * backend running without billing) yields null: no meter rather than "$0.0000".
 */
export function creditMeter(
  startBalance: number | null,
  currentBalance: number | null,
): { used: string; left: string } | null {
  if (currentBalance === null) return null;
  // Clamp: a top-up mid-conversation would otherwise render a negative spend.
  const used = Math.max(0, (startBalance ?? currentBalance) - currentBalance);
  return { used: `$${used.toFixed(4)}`, left: `$${currentBalance.toFixed(2)}` };
}

/** The elements the view drives. Missing ones are tolerated so partial mounts work. */
export interface TalkElements {
  status?: HTMLElement | null;
  detected?: HTMLElement | null;
  liveTranslation?: HTMLElement | null;
  liveOriginal?: HTMLElement | null;
  history?: HTMLElement | null;
  pair?: HTMLElement | null;
  root?: HTMLElement | null;
}

/**
 * Paint the status line and the detected-language chip.
 *
 * The chip is hidden whenever the direction is unknown rather than showing a
 * placeholder — a chip that flickers between two flags on every partial is exactly the
 * "visually noisy" outcome brief §15 warns against.
 */
export function renderStatus(
  els: TalkElements,
  ctx: SessionContext,
  live: LiveExchange,
): void {
  if (els.status) {
    els.status.textContent = t(statusKey(ctx));
  }
  if (els.root) {
    els.root.dataset.phase = ctx.phase;
    els.root.dataset.activity = ctx.phase === 'live' ? ctx.activity : 'listening';
  }
  if (!els.detected) return;
  const spoken = ctx.phase === 'live' ? live.spokenLang : null;
  if (!spoken) {
    els.detected.hidden = true;
    els.detected.textContent = '';
    return;
  }
  const label = langLabel(spoken);
  els.detected.hidden = false;
  // Flag AND name, always. A flag alone is unreadable to a screen reader and ambiguous
  // to anyone who does not recognise it.
  els.detected.textContent = fill(t('talkDetected'), { lang: label.native });
  els.detected.setAttribute('data-lang', spoken);
}

/** Paint the live card: translation dominant, original smaller beneath it. */
export function renderLive(els: TalkElements, live: LiveExchange): void {
  if (els.liveTranslation) {
    els.liveTranslation.textContent = live.translatedText;
    applyDir(els.liveTranslation, live.targetLang);
  }
  if (els.liveOriginal) {
    els.liveOriginal.textContent = live.originalText;
    applyDir(els.liveOriginal, live.spokenLang);
  }
}

/** Build one history card. Newest is prepended, so older exchanges scroll away. */
export function buildExchangeCard(exchange: Exchange, doc: Document): HTMLElement {
  const spoken = langLabel(exchange.spokenLang);
  const target = langLabel(exchange.targetLang);

  const card = doc.createElement('article');
  card.className = 'tk-card';
  card.dataset.exchange = String(exchange.id);

  // A card carries BOTH languages. An empty side would render as a labelled blank row,
  // which reads as "the translation is missing" rather than "the engine sent nothing" —
  // so a side with no text is left out entirely instead of shown hollow.
  if (exchange.originalText) {
    card.appendChild(
      line(doc, 'tk-card-source', spoken, exchange.originalText, exchange.spokenLang),
    );
  }
  if (exchange.translatedText) {
    card.appendChild(
      line(doc, 'tk-card-target', target, exchange.translatedText, exchange.targetLang),
    );
  }
  return card;
}

function line(
  doc: Document,
  cls: string,
  label: LangLabel,
  text: string,
  code: string,
): HTMLElement {
  const row = doc.createElement('p');
  row.className = cls;

  const flag = doc.createElement('span');
  flag.className = 'tk-flag';
  // Decorative: the language name next to it carries the meaning.
  flag.setAttribute('aria-hidden', 'true');
  flag.textContent = label.flag;

  const name = doc.createElement('span');
  name.className = 'tk-lang';
  name.textContent = label.native;

  const body = doc.createElement('span');
  body.className = 'tk-text';
  body.textContent = text;
  applyDir(body, code);

  row.append(flag, name, body);
  return row;
}

/** Prepend `card`, trimming the tail so the DOM cannot grow without bound. */
export function pushHistory(history: HTMLElement, card: HTMLElement): void {
  history.prepend(card);
  while (history.children.length > HISTORY_LIMIT) {
    history.lastElementChild?.remove();
  }
}

/** Render the `🇮🇹 Italian ⇄ 🇪🇸 Spanish` header. */
export function renderPair(els: TalkElements, userLang: string, otherLang: string): void {
  if (!els.pair) return;
  const a = langLabel(userLang);
  const b = langLabel(otherLang);
  els.pair.textContent = '';
  const doc = els.pair.ownerDocument;
  els.pair.append(
    pairSide(doc, a, userLang),
    Object.assign(doc.createElement('span'), {
      className: 'tk-pair-swap',
      textContent: '⇄',
    }),
    pairSide(doc, b, otherLang),
  );
  els.pair.setAttribute(
    'aria-label',
    fill(t('talkPairLabel'), { from: a.english, to: b.english }),
  );
}

function pairSide(doc: Document, label: LangLabel, code: string): HTMLElement {
  const side = doc.createElement('span');
  side.className = 'tk-pair-side';
  side.dataset.lang = code;
  const flag = doc.createElement('span');
  flag.setAttribute('aria-hidden', 'true');
  flag.className = 'tk-flag';
  flag.textContent = label.flag;
  const name = doc.createElement('span');
  name.className = 'tk-lang';
  name.textContent = label.native;
  side.append(flag, name);
  return side;
}

function applyDir(el: HTMLElement, code: string | null): void {
  if (!code) {
    el.removeAttribute('dir');
    return;
  }
  el.setAttribute('dir', langLabel(code).rtl ? 'rtl' : 'ltr');
  el.lang = code;
}
