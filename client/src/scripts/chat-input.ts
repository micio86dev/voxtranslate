// Pure helpers for the chat composer (spec 0070 S1). Kept DOM-free so the
// counter + auto-resize maths are unit-testable; app.ts wires them to the
// <textarea> and counter elements.

/** Max characters in a single chat message. Single source of truth: drives
 *  both the textarea `maxlength` attribute and the live counter. */
export const CHAT_MAX_LEN = 500;

/** Counter becomes visible once the used/max ratio reaches this. */
export const COUNTER_SHOW_RATIO = 0.8;
/** Counter switches to the warning style at or above this ratio. */
export const COUNTER_WARN_RATIO = 0.95;

/** Max rendered height of the auto-growing textarea, in px (~5 rows). Past
 *  this the box stops growing and scrolls internally. */
export const CHAT_MAX_HEIGHT = 120;

export type CounterState = 'hidden' | 'normal' | 'warn';

/** Whether/how to show the character counter for `used` of `max` chars. */
export function counterState(used: number, max: number = CHAT_MAX_LEN): CounterState {
  if (max <= 0) return 'hidden';
  const ratio = used / max;
  if (ratio >= COUNTER_WARN_RATIO) return 'warn';
  if (ratio >= COUNTER_SHOW_RATIO) return 'normal';
  return 'hidden';
}

/** Counter label, e.g. "480/500". */
export function counterLabel(used: number, max: number = CHAT_MAX_LEN): string {
  return `${used}/${max}`;
}

/** Rendered height + overflow for an auto-growing textarea, given the content's
 *  natural scrollHeight and a max. The caller resets the element height to
 *  'auto' before measuring scrollHeight so this receives the true content size. */
export function resizeBox(
  scrollHeight: number,
  maxPx: number = CHAT_MAX_HEIGHT,
): { height: number; overflowY: 'hidden' | 'auto' } {
  if (scrollHeight <= maxPx) return { height: scrollHeight, overflowY: 'hidden' };
  return { height: maxPx, overflowY: 'auto' };
}

/** Format an elapsed recording duration (milliseconds) as `m:ss` for the voice-
 *  message timer, e.g. 0 → "0:00", 5_000 → "0:05", 65_000 → "1:05". Negative
 *  inputs clamp to zero. */
export function recTimeLabel(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, '0')}`;
}

/** Result of inserting `emoji` into `value` over the [start,end) selection,
 *  hard-capped at `max`. Returns null (no change) if it would exceed the cap. */
export function insertAt(
  value: string,
  emoji: string,
  start: number,
  end: number,
  max: number = CHAT_MAX_LEN,
): { value: string; caret: number } | null {
  const next = value.slice(0, start) + emoji + value.slice(end);
  if (next.length > max) return null;
  return { value: next, caret: start + emoji.length };
}
