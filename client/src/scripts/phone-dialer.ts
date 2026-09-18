/**
 * Dialer logic for translated phone calls, placed from inside the call app itself
 * (spec: web-app-voip-dialer). Ported from `dashboard/src/scripts/phone-dialer.ts`
 * (spec 0111, R30, read-only reference) with two deliberate differences:
 *
 * - The refusal-key shape is flat camelCase (`phoneReasonNoAnswer`), matching this
 *   app's i18n convention (`bizRecord`, `webinarTier`), not the dashboard's dotted
 *   `phone.reason.*` keys.
 * - `joinUrl`/`canJoin` are dropped: the dashboard hands a phone call off to THIS app
 *   to carry the audio, so a deep link back into itself makes no sense here — the app
 *   dials directly into the room via the `entryMode='phone'` branch instead.
 * - `isPhonePeer` is new: the app receives the full `room_joined`/`peer_joined` peer
 *   list (the dashboard never does) and must recognise the telephone leg without ever
 *   treating it as a WebRTC peer to open a connection to.
 *
 * Everything here is pure and framework-free, so the parts that decide what a user is
 * told — the call phase, the price, the refusal reason, the screen-reader announcement —
 * are unit-tested rather than eyeballed in a browser.
 *
 * The accessibility rule this module exists to enforce: **a call's state must be
 * announced, not merely coloured**. A dialer where "connected" is a green dot is unusable
 * without sight, and a phone call is exactly the feature where that matters most.
 */

/** What the UI shows. Derived from the server's status, never invented client-side. */
export type CallPhase =
  | 'idle'
  | 'preparing'
  | 'dialing'
  | 'ringing'
  | 'connected'
  | 'reconnecting'
  | 'ending'
  | 'completed'
  | 'failed';

/**
 * Map a persisted server status onto a display phase.
 *
 * An unrecognised status becomes `failed`, not `connected`: a UI that cannot read the
 * state must not draw a live call, because a live call is one the user believes they are
 * being charged for and can speak into.
 */
export function phaseFromStatus(status: string | null | undefined): CallPhase {
  switch (status) {
    case 'created':
      return 'preparing';
    case 'dialing':
      return 'dialing';
    case 'ringing':
      return 'ringing';
    case 'answered':
    case 'bridged':
      return 'connected';
    case 'ending':
      return 'ending';
    case 'completed':
      return 'completed';
    case 'failed':
      return 'failed';
    default:
      return 'failed';
  }
}

export function isTerminal(phase: CallPhase): boolean {
  return phase === 'completed' || phase === 'failed';
}

/**
 * The poll's ACTUAL hangup trigger (R3-unknown-status-tears-down-live-call fix) — not
 * `isTerminal(phaseFromStatus(status))`. `phaseFromStatus` deliberately maps ANY status
 * it doesn't recognise onto `'failed'` for display purposes (a cosmetic "failed"-looking
 * label is fine for the UI), but that same fallback must never be trusted to hang up a
 * call that might still be perfectly live — a status the client has never seen before
 * (a future server release, a renamed status, a malformed body) is not evidence the call
 * is over.
 *
 * These two strings are the server's own terminal `voip_calls.status` values
 * (`voip::state::CallStatus::Completed`/`Failed` — see every `status NOT IN ('completed',
 * 'failed')` guard in `server/src/voip/`), so this is closed and known-safe, not a display
 * concern like `phaseFromStatus`.
 */
export function isKnownTerminalStatus(status: string | null | undefined): boolean {
  return status === 'completed' || status === 'failed';
}

/** Whether the hang-up control should be offered. */
export function canHangUp(phase: CallPhase): boolean {
  return (
    phase === 'dialing' || phase === 'ringing' || phase === 'connected' || phase === 'reconnecting'
  );
}

/** Whether the microphone is live, i.e. whether the far end can hear the user. */
export function isMicLive(phase: CallPhase): boolean {
  return phase === 'connected' || phase === 'reconnecting';
}

/**
 * Strip the punctuation people type, keeping a single leading `+`.
 *
 * A convenience only — the SERVER validates. Duplicating the full E.164 rules here would
 * guarantee the two drift apart, and the one that matters is the one that spends money.
 */
export function normaliseDestination(raw: string): string {
  const trimmed = (raw ?? '').trim();
  const plus = trimmed.startsWith('+');
  const digits = trimmed.replace(/[^0-9]/g, '');
  return digits ? `${plus ? '+' : ''}${digits}` : '';
}

/**
 * Why a typed number cannot be dialled, named with the **server's own reason code**.
 *
 * This stays a convenience: `E164::parse` on the server is authoritative, and it is the
 * server that spends the money. Duplicating the full E.164 rules here would guarantee the
 * two drift.
 */
export type NumberProblem =
  | 'number_empty'
  | 'number_non_numeric'
  | 'number_leading_zero'
  | 'number_too_short'
  | 'number_too_long';

export function numberProblem(raw: string | null | undefined): NumberProblem | null {
  const trimmed = (raw ?? '').trim();
  if (!trimmed) return 'number_empty';
  const digits = normaliseDestination(trimmed).replace(/^\+/, '');
  // Something was typed, and none of it was a digit.
  if (!digits) return 'number_non_numeric';
  // A national trunk prefix, not an international number: `+0…` is never dialable.
  if (digits.startsWith('0')) return 'number_leading_zero';
  if (digits.length < 8) return 'number_too_short';
  if (digits.length > 15) return 'number_too_long';
  return null;
}

/**
 * A cheap "is this worth asking the server about" check, so the dialer does not fire a
 * quote on every keystroke of a half-typed number.
 *
 * The same rule as [`numberProblem`], asked as a yes/no question — one set of thresholds,
 * so the number the dialer quotes and the number it will let you send cannot disagree.
 */
export function looksDialable(raw: string): boolean {
  return numberProblem(raw) === null;
}

/** Credits (integers, 1 = $0.01) as a currency amount. */
export function formatCredits(credits: number): string {
  if (!Number.isFinite(credits)) return '0.00';
  return (Math.trunc(credits) / 100).toFixed(2);
}

/**
 * Cost of `minutes` at `pricePerMinute`, for display.
 *
 * Rounded UP to the cent, matching how the server settles: showing a number lower than
 * what will be charged is the one direction a price estimate must never be wrong in.
 */
export function estimateCost(pricePerMinute: string | number, minutes: number): string {
  const rate = typeof pricePerMinute === 'number' ? pricePerMinute : Number(pricePerMinute);
  if (!Number.isFinite(rate) || rate < 0 || !Number.isFinite(minutes) || minutes <= 0) {
    return '0.00';
  }
  // Work in cents and ceil, so the displayed figure is never under the settled one.
  //
  // `toFixed(6)` first, because ceiling a binary float is not the same as ceiling the
  // price it stands for: `0.005 * 14 * 100` evaluates to 7.000000000000001, and the ceil
  // turns that hair into a whole extra cent. Snapping to a hundredth of a cent removes the
  // tail without touching any real fraction of a cent, which must still round up.
  const cents = Math.ceil(Number((rate * minutes * 100).toFixed(6)));
  return (cents / 100).toFixed(2);
}

/** `95` → `1:35`. */
export function formatDuration(seconds: number | null | undefined): string {
  if (seconds == null || !Number.isFinite(seconds) || seconds < 0) return '—';
  const s = Math.floor(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const pad = (n: number) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${m}:${pad(sec)}`;
}

/**
 * The server refusal codes reachable from quote/dial/hangup (design contract, 30 codes) —
 * the only three calls this app's dialer makes. An unknown code falls back to a generic
 * message rather than printing the raw code at a customer, but stays distinguishable via
 * `hasReasonCopy`, so a new server reason shows up as missing copy rather than as silence.
 */
const KNOWN_REASONS = new Set([
  'busy',
  'rejected',
  'no_answer',
  'unallocated_number',
  'destination_not_allowed',
  'destination_too_expensive',
  'rate_unavailable',
  'insufficient_credits',
  'credits_exhausted',
  'eu_processing_unavailable',
  'concurrency_limit',
  'max_duration_reached',
  'media_lost',
  'provider_unavailable',
  'consent_declined',
  'unmapped',
  'number_empty',
  'number_non_numeric',
  'number_too_short',
  'number_too_long',
  'number_leading_zero',
  'number_unknown_country',
  'voip_misconfigured',
  'storage_error',
  'project_required',
  'project_not_in_org',
  'caller_id_unverified',
  'caller_id_missing',
  'invalid_country_code',
  'consent_required_for_capture',
]);

export function hasReasonCopy(code: string | null | undefined): boolean {
  return !!code && KNOWN_REASONS.has(code);
}

/** `no_answer` → `NoAnswer`. Internal to `refusalKey`'s flat-key construction. */
function pascalCase(snake: string): string {
  return snake
    .split('_')
    .filter(Boolean)
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join('');
}

/**
 * Translation key for a server refusal code, in this app's flat camelCase convention.
 *
 * The server sends stable machine-readable codes precisely so the UI can localise them.
 * An unknown code falls back to a generic message rather than printing the raw code at a
 * customer — but it is still distinguishable in `hasReasonCopy`, so a new server reason
 * shows up as missing copy rather than as silence.
 *
 * `fallback` exists because a transport failure (a CORS preflight the API refused, the
 * network dropping) gives the client no code at all, and "the call could not be placed"
 * is the correct sentence only on a surface that really is placing a call.
 *
 * A named code always wins over the fallback: only the last resort moves.
 */
export function refusalKey(
  code: string | null | undefined,
  fallback: string = 'phoneReasonGeneric',
): string {
  return hasReasonCopy(code) ? `phoneReason${pascalCase(code as string)}` : fallback;
}

/**
 * What an assistive technology should say when the phase changes.
 *
 * Returns `null` when nothing changed, so an `aria-live` region is not re-announced on
 * every poll — a screen reader repeating "ringing" four times a second is worse than
 * saying nothing.
 */
export function announcement(
  previous: CallPhase | null,
  next: CallPhase,
  t: (key: string) => string,
): string | null {
  if (previous === next) return null;
  return t(`phase.${next}`);
}

/** Phases in the order they normally occur, for a progress indicator. */
export const PHASE_ORDER: CallPhase[] = [
  'preparing',
  'dialing',
  'ringing',
  'connected',
  'ending',
  'completed',
];

/**
 * How far through the call we are, 0..1, for a progress indicator that is **also**
 * labelled — never colour alone.
 */
export function phaseProgress(phase: CallPhase): number {
  if (phase === 'failed') return 1;
  if (phase === 'reconnecting') return phaseProgress('connected');
  const i = PHASE_ORDER.indexOf(phase);
  if (i < 0) return 0;
  return (i + 1) / PHASE_ORDER.length;
}

/**
 * Whether the recipient will hear a disclosure announcement, and what it will say — the
 * dialer shows this **before** the call so the user is not surprised by three seconds of
 * speech at the far end.
 */
export function disclosureSummaryKey(quote: {
  recording: boolean;
  transcription: boolean;
  consent_policy: string;
}): string {
  if (quote.recording && quote.transcription) return 'phone.disclosure.both';
  if (quote.recording) return 'phone.disclosure.recording';
  if (quote.transcription) return 'phone.disclosure.transcription';
  return 'phone.disclosure.translationOnly';
}

/** Whether the recipient will be asked to press a key before capture starts. */
export function willAskConsent(quote: {
  recording: boolean;
  transcription: boolean;
  consent_policy: string;
}): boolean {
  if (!quote.recording && !quote.transcription) return false;
  return quote.consent_policy === 'press_key' || quote.consent_policy === 'verbal';
}

/**
 * The id shape the server mints for the telephone leg (`voip/session.rs`'s
 * `create_phone_peer`): `format!("phone-{}", Uuid::new_v4().simple())` — the literal
 * `phone-` prefix plus a 32-hex-digit UUID with no dashes.
 *
 * This match is COSMETIC ONLY, never a security boundary: peer ids are client-supplied
 * over the WebSocket, and the room the call happens in is private with a random `ph-`
 * name. What this function protects against is a UI bug, not an attacker — treating the
 * telephone leg as an ordinary WebRTC peer would `mesh.addPeer` a connection that will
 * never negotiate, leaving a permanently black tile and a dead PeerConnection.
 */
const PHONE_PEER_ID = /^phone-[0-9a-f]{32}$/i;

export function isPhonePeer(id: string | null | undefined): boolean {
  return !!id && PHONE_PEER_ID.test(id);
}

/**
 * Whether an org's subscription authorizes it to place a phone call (R2-dial-org-gate-
 * divergence fix): the SINGLE predicate `canShowPhoneCta` below and `app.ts`'s
 * `openPhoneDialPanel` (which picks the org actually used to dial) both call, so the CTA
 * that reveals the dial panel and the org selection that places the call can never
 * disagree about what "may this org dial" means. Duck-typed (not `BusinessOrg`) so this
 * stays framework-free, same as `phone-call.ts`'s `OkData<T>`.
 */
export function hasActiveSubscription(org: { subscription_status: string }): boolean {
  return org.subscription_status === 'active';
}

/**
 * The home-screen CTA's visibility rule (R1, R10): at least one org with an active
 * subscription, on a browser that can actually open the call `startCall()` needs.
 */
export function canShowPhoneCta(
  orgs: { subscription_status: string }[],
  webrtcSupported: boolean,
): boolean {
  return webrtcSupported && orgs.some(hasActiveSubscription);
}

/**
 * R8: the destination field doubles as a contact search. Digits (plus the punctuation
 * people type into a phone number — spaces, dashes, parens, a leading `+`) mean "this is
 * already a number"; any letter means "search the address book". One rule, so the field
 * and the quote/search it fires can never disagree about what the user typed.
 */
export function looksLikeContactSearch(raw: string): boolean {
  return /[a-zA-Z]/.test(raw);
}
