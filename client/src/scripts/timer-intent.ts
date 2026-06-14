// Voice-command timer — pure intent parsing + formatters (spec 0052).
//
// Kept browser-free and side-effect-free so it unit-tests without a DOM (the
// `CallTimer` UI lives in ./timer). The parser reads a final speech transcript
// and decides whether it's a "set a timer" command and for how long. Italian-first
// (the issue), with English commands working too.

/** Ceiling for a parsed duration — a call timer past this is almost certainly a
 *  mis-parse (e.g. "the meeting runs 8 hours"), so we ignore it. */
const MAX_SECONDS = 6 * 60 * 60; // 6 h
const MIN_SECONDS = 1;

export interface TimerCommand {
  /** Total countdown length in whole seconds (MIN_SECONDS…MAX_SECONDS). */
  seconds: number;
  /** The phrasing was a "break"/"pausa" rather than a plain "timer" — lets the UI
   *  word the confirmation slightly differently if it wants to. */
  isBreak: boolean;
}

// Spoken number words → value. Deepgram's smart_format usually digitises numbers
// ("ten" → "10"), but not always for small ones, so this is a robustness fallback.
// Italian + English, the two MVP languages. 0.5 covers "half"/"mezzo".
const WORD_NUM: Record<string, number> = {
  // English units + teens + tens
  zero: 0, one: 1, two: 2, three: 3, four: 4, five: 5, six: 6, seven: 7, eight: 8,
  nine: 9, ten: 10, eleven: 11, twelve: 12, thirteen: 13, fourteen: 14, fifteen: 15,
  sixteen: 16, seventeen: 17, eighteen: 18, nineteen: 19, twenty: 20, thirty: 30,
  forty: 40, fifty: 50, sixty: 60, half: 0.5,
  // Italian units + teens + tens
  uno: 1, una: 1, due: 2, tre: 3, quattro: 4, cinque: 5, sei: 6, sette: 7, otto: 8,
  nove: 9, dieci: 10, undici: 11, dodici: 12, tredici: 13, quattordici: 14,
  quindici: 15, sedici: 16, diciassette: 17, diciotto: 18, diciannove: 19,
  venti: 20, trenta: 30, quaranta: 40, cinquanta: 50, sessanta: 60, mezzo: 0.5,
  mezza: 0.5,
};

// Idiomatic durations that carry their own unit — normalised to a plain
// "<n> minute(s)/hour" token the generic number+unit scanner below understands.
// Apostrophes are already unified to ' before this runs.
const IDIOMS: [RegExp, string][] = [
  [/\bmezz['\s]*ora\b/g, ' 30 minute '], // mezz'ora / mezzora / mezz ora
  [/\bmezza\s+ora\b/g, ' 30 minute '],
  [/\bun\s+quarto\s+d['\s]*ora\b/g, ' 15 minute '], // un quarto d'ora
  [/\bquarto\s+d['\s]*ora\b/g, ' 15 minute '],
  [/\bhalf\s+an?\s+hour\b/g, ' 30 minute '],
  [/\bhalf\s+hour\b/g, ' 30 minute '],
  [/\ba\s+quarter\s+of\s+an\s+hour\b/g, ' 15 minute '],
  [/\bquarter\s+of\s+an\s+hour\b/g, ' 15 minute '],
  [/\bquarter\s+hour\b/g, ' 15 minute '],
  [/\bun['\s]*ora\b/g, ' 1 hour '], // un'ora / un ora
  [/\buna\s+ora\b/g, ' 1 hour '],
  [/\b(?:an|a|one)\s+hour\b/g, ' 1 hour '],
  // "un minuto" / "a minute" / "uno secondo" — an unspoken count of one.
  [/\b(?:un|uno|una)\s+(minut\w*|second\w*)\b/g, ' 1 $1 '],
  [/\ba\s+(minute|second)\b/g, ' 1 $1 '],
];

// Unit vocabulary, by kind. Multi-language so a transcript in any of the eight UI
// languages still parses (the speaker's STT language need not be IT/EN).
const HOURS = ['hours', 'hour', 'hrs', 'hr', 'h', 'ore', 'ora', 'horas', 'hora', 'heures', 'heure', 'stunden', 'stunde'];
const MINUTES = ['minutes', 'minute', 'minuti', 'minuto', 'minutos', 'minuten', 'mins', 'min', 'm'];
const SECONDS = ['seconds', 'second', 'secondi', 'secondo', 'segundos', 'segundo', 'secondes', 'seconde', 'sekunden', 'sekunde', 'secs', 'sec', 's'];

const HOUR_SET = new Set(HOURS);
const MIN_SET = new Set(MINUTES);

// One regex matching "<number><opt-space><unit>". Alternatives are sorted longest
// first so "minutes" wins over "min" over "m" (regex alternation is first-match).
const UNIT_RE = (() => {
  const all = [...HOURS, ...MINUTES, ...SECONDS]
    .sort((a, b) => b.length - a.length)
    .map((u) => u.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'));
  return new RegExp(`(\\d+(?:\\.\\d+)?)\\s*(${all.join('|')})\\b`, 'g');
})();

// Any trigger keyword that turns "10 minutes" from idle chatter into a command.
const TIMER_RE = /\btimer\b/;
const BREAK_RE = /\b(break|paus[ae])\b/; // break (en) · pausa/pause (it)

/** Lower-case, unify apostrophes, drop punctuation (keep letters/digits/'), and
 *  collapse whitespace. Unicode-aware so accented words survive. */
function normalize(raw: string): string {
  return raw
    .toLowerCase()
    .replace(/[‘’ʼ`]/g, "'") // smart quotes → '
    .replace(/[^\p{L}\p{N}'\s]/gu, ' ')
    .replace(/\s+/g, ' ')
    .trim();
}

/** Replace standalone spoken number-words with digits so the unit scanner sees
 *  them. Handles English "twenty five" style compounds (tens + unit). */
function wordsToDigits(text: string): string {
  const toks = text.split(' ');
  const out: string[] = [];
  for (let i = 0; i < toks.length; i++) {
    const w = toks[i];
    const v = WORD_NUM[w];
    if (v === undefined) {
      out.push(w);
      continue;
    }
    let value = v;
    const next = WORD_NUM[toks[i + 1]];
    // "twenty five" → 25 (tens 20–90 followed by a 1–9 unit word).
    if (value >= 20 && value % 10 === 0 && next !== undefined && next >= 1 && next <= 9) {
      value += next;
      i++;
    }
    out.push(String(value));
  }
  return out.join(' ');
}

/** Parse a (final) transcript into a timer command, or null if it isn't one.
 *  Requires a trigger keyword ("timer"/"break"/"pausa") AND a recognisable
 *  duration, so ordinary talk mentioning "30 minutes" never starts a timer. */
export function parseTimerCommand(raw: string): TimerCommand | null {
  if (!raw) return null;
  let text = normalize(raw);
  if (!text) return null;

  const isBreak = BREAK_RE.test(text);
  if (!TIMER_RE.test(text) && !isBreak) return null;

  for (const [re, repl] of IDIOMS) text = text.replace(re, repl);
  text = wordsToDigits(text);

  let total = 0;
  let found = false;
  for (const m of text.matchAll(UNIT_RE)) {
    const n = parseFloat(m[1]);
    if (!isFinite(n)) continue;
    const unit = m[2];
    if (HOUR_SET.has(unit)) total += n * 3600;
    else if (MIN_SET.has(unit)) total += n * 60;
    else total += n;
    found = true;
  }
  if (!found) return null;

  total = Math.round(total);
  if (total < MIN_SECONDS || total > MAX_SECONDS) return null;
  return { seconds: total, isBreak };
}

/** Countdown clock label: "MM:SS", or "H:MM:SS" once it's an hour or more. */
export function formatClock(seconds: number): string {
  const s = Math.max(0, Math.round(seconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const mm = String(m).padStart(2, '0');
  const ss = String(sec).padStart(2, '0');
  return h > 0 ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
}

/** Human, localised duration for confirmations ("15 minuti", "1 ora 30 minuti").
 *  Uses singular/plural unit words from i18n via the passed `t`. */
export function spokenDuration(seconds: number, t: (k: string) => string): string {
  const s = Math.max(0, Math.round(seconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const parts: string[] = [];
  if (h) parts.push(`${h} ${t(h === 1 ? 'unitH1' : 'unitHN')}`);
  if (m) parts.push(`${m} ${t(m === 1 ? 'unitM1' : 'unitMN')}`);
  if (sec) parts.push(`${sec} ${t(sec === 1 ? 'unitS1' : 'unitSN')}`);
  if (!parts.length) parts.push(`0 ${t('unitSN')}`);
  return parts.join(' ');
}
