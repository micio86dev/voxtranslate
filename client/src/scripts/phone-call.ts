/**
 * Orchestration logic for placing and ending a translated phone call from inside the
 * call app itself (spec: web-app-voip-dialer, R3-R7). Framework-free and dependency-
 * injected so the two money-safety invariants below are unit-tested here rather than
 * eyeballed in a browser — `app.ts` (the ~9.3k-line call/session controller, excluded
 * from unit coverage by `vitest.config.ts` and instead exercised by the Playwright e2e
 * suite) supplies the real `getUserMedia`/`quoteVoipCall`/`dialVoipCall`/`hangUpVoipCall`
 * and the DOM wiring around these pure orchestrators.
 *
 * Money-safety invariant (a) — R3: a denied or failed microphone must NEVER reach
 * `POST /voip/calls`. `runPhoneDialSequence` enforces the call order and short-circuits
 * before `dial` runs.
 *
 * Money-safety invariant (b) — R5/R6: every exit from a live phone call (the leave
 * button via `leaveCall()`, a `pagehide`, or the audio track ending) must issue hangup
 * EXACTLY once, idempotently. `createPhoneLegController` nulls its held call handle
 * FIRST, so a concurrent `leaveCall()` + `pagehide` race still fires the POST once.
 */

/** R4/R7: only the phone entry path skips the device-check prejoin screen. */
export type EntryMode = 'room' | 'phone';

/**
 * R4 — the phone entry path enters the existing call screen directly, skipping the
 * camera/device-check prejoin screen used for ordinary video-room joins.
 * R7 — an ordinary video-room join is unaffected: the prejoin screen still shows.
 */
export function skipsPrejoin(mode: EntryMode): boolean {
  return mode === 'phone';
}

/** The `{ok, data}` shape `voip.ts`'s `ApiResult<T>` already has — duck-typed here so
 *  this module stays framework-free and does not need to import `voip.ts` at all. */
interface OkData<T> {
  ok: boolean;
  data: T | null;
}

export interface PhoneDialDeps<TMic, TQuote, TCall> {
  /** Step 1 — MUST run and succeed before any network call is made (R3). */
  acquireMic: () => Promise<TMic>;
  /** Step 2 — priced before the call is placed. */
  quote: () => Promise<OkData<TQuote>>;
  /** Step 3 — only reached if the quote succeeded. */
  dial: () => Promise<OkData<TCall>>;
  /** Called with the mic handle whenever the sequence stops before entering the call. */
  releaseMic: (mic: TMic) => void;
}

export type PhoneDialOutcome<TMic, TQuote, TCall> =
  | { stage: 'dialed'; mic: TMic; call: TCall }
  | { stage: 'mic'; error: unknown }
  | { stage: 'quote'; mic: TMic; quote: OkData<TQuote> }
  | { stage: 'dial'; mic: TMic; dial: OkData<TCall> };

/**
 * R3's non-negotiable order: mic → quote → dial. A refusal at any step after the mic
 * releases it and stops — no charge, no half-placed call.
 */
export async function runPhoneDialSequence<TMic, TQuote, TCall>(
  deps: PhoneDialDeps<TMic, TQuote, TCall>,
): Promise<PhoneDialOutcome<TMic, TQuote, TCall>> {
  let mic: TMic;
  try {
    mic = await deps.acquireMic();
  } catch (error) {
    // Money-safety (a): no mic acquired — quote/dial must never run, and there is
    // nothing to release.
    return { stage: 'mic', error };
  }
  const quoted = await deps.quote();
  if (!quoted.ok || !quoted.data) {
    deps.releaseMic(mic);
    return { stage: 'quote', mic, quote: quoted };
  }
  const dialed = await deps.dial();
  if (!dialed.ok || !dialed.data) {
    deps.releaseMic(mic);
    return { stage: 'dial', mic, dial: dialed };
  }
  return { stage: 'dialed', mic, call: dialed.data };
}

export interface PhoneLegHandle {
  orgId: string;
  callId: string;
}

export interface PhoneLegDeps {
  /** POST .../voip/calls/{id}/hangup — best-effort, fire-and-forget from the caller's POV. */
  hangup: (orgId: string, callId: string) => void;
  /** Stop the status-poll timer, if one is running. Safe to call when there isn't one. */
  clearPoll: () => void;
}

export interface PhoneLegController {
  /** Record the call this leg now tracks. */
  start: (handle: PhoneLegHandle) => void;
  /**
   * R5/R6's idempotent exit: `leaveCall()`, the `pagehide` listener, and an audio-track
   * `ended` event all call this. Whichever fires first hangs up; every later call is a
   * no-op — the held handle is cleared FIRST, before `hangup` runs, so a `hangup` that
   * itself throws, or a second call racing the first, can never double-post.
   */
  end: () => void;
  isActive: () => boolean;
  /**
   * Work Unit B (design Decision B3/B4): the call id this leg currently tracks, or
   * `null` once `end()` has run. `app.ts`'s `shouldLeaveOnPhoneCallEnded` guard reads
   * this to confirm a `phone_call_ended` push is about THIS client's own call, and its
   * `null` state after `end()` is also the idempotency signal that makes a second event
   * (or a racing poll) a no-op — reusing existing state rather than a new flag.
   */
  currentCallId: () => string | null;
}

/**
 * The engine the room a telephone lives in must join on.
 *
 * The room runs on whatever engine the SERVER resolved for the phone leg
 * (`resolve_for_phone` may substitute a client-direct tier the telephone cannot use,
 * e.g. Cartesia), never on the caller's own UI-selected engine — joining on any other
 * engine leaves the caller outside the phone leg's audience and the phone party's
 * speech is never translated for them (2026-09-21 silent-phone-party incident).
 */
export function phoneRoomEngine(
  quote: { engine_id: string } | null | undefined,
): string | undefined {
  return quote?.engine_id || undefined;
}

export function createPhoneLegController(deps: PhoneLegDeps): PhoneLegController {
  let active: PhoneLegHandle | null = null;
  return {
    start(handle) {
      active = handle;
    },
    end() {
      const handle = active;
      active = null; // cleared first — see the idempotency note above
      deps.clearPoll();
      if (handle) deps.hangup(handle.orgId, handle.callId);
    },
    isActive() {
      return active !== null;
    },
    currentCallId() {
      return active?.callId ?? null;
    },
  };
}
