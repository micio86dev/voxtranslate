// @vitest-environment jsdom
// PR3 needs `location`/`localStorage` for the `errorCode` import from `./voip` below
// (via `./auth`), unlike the rest of this file's pure logic — jsdom, not this suite's
// former default `node` environment, is what the rest of the codebase opts into for
// exactly this reason (see e.g. `avatar.test.ts`).
import { describe, expect, it } from 'vitest';
import {
  announcement,
  canHangUp,
  canShowPhoneCta,
  disclosureSummaryKey,
  estimateCost,
  formatCredits,
  formatDuration,
  hasActiveSubscription,
  hasReasonCopy,
  isKnownTerminalStatus,
  isMicLive,
  isPhonePeer,
  isTerminal,
  looksDialable,
  looksLikeContactSearch,
  normaliseDestination,
  numberProblem,
  phaseFromStatus,
  phaseProgress,
  phoneEndCopyKey,
  refusalKey,
  shouldLeaveOnPhoneCallEnded,
  usableCallerIds,
  willAskConsent,
  type CallPhase,
} from './phone-dialer';
import { errorCode } from './voip';

// Ported from dashboard/src/scripts/phone-dialer.test.ts (spec 0111, R30), read-only
// reference. The refusal-key cases are reshaped for this app's flat i18n convention
// (phoneReason<PascalCase>, not the dashboard's dotted phone.reason.*); isPhonePeer is
// new — the app, unlike the dashboard, receives a `room_joined`/`peer_joined` peer list
// and must recognise the telephone leg without ever treating it as a WebRTC peer.

describe('phaseFromStatus', () => {
  it('maps every status the server can persist', () => {
    expect(phaseFromStatus('created')).toBe('preparing');
    expect(phaseFromStatus('dialing')).toBe('dialing');
    expect(phaseFromStatus('ringing')).toBe('ringing');
    expect(phaseFromStatus('answered')).toBe('connected');
    expect(phaseFromStatus('bridged')).toBe('connected');
    expect(phaseFromStatus('ending')).toBe('ending');
    expect(phaseFromStatus('completed')).toBe('completed');
    expect(phaseFromStatus('failed')).toBe('failed');
  });

  it('never draws a live call for a status it cannot read', () => {
    // A live call is one the user believes they can speak into and are being charged
    // for. Guessing "connected" from an unknown status is the worst possible guess.
    for (const junk of ['in_progress', '', null, undefined, 'CONNECTED']) {
      expect(phaseFromStatus(junk)).toBe('failed');
    }
  });
});

describe('call controls follow the phase', () => {
  it('offers hang-up exactly while there is something to hang up', () => {
    const live: CallPhase[] = ['dialing', 'ringing', 'connected', 'reconnecting'];
    const dead: CallPhase[] = ['idle', 'preparing', 'ending', 'completed', 'failed'];
    for (const p of live) expect(canHangUp(p)).toBe(true);
    for (const p of dead) expect(canHangUp(p)).toBe(false);
  });

  it('says the microphone is live only when the far end can actually hear it', () => {
    expect(isMicLive('connected')).toBe(true);
    expect(isMicLive('reconnecting')).toBe(true);
    for (const p of [
      'idle',
      'preparing',
      'dialing',
      'ringing',
      'ending',
      'completed',
      'failed',
    ] as CallPhase[]) {
      expect(isMicLive(p)).toBe(false);
    }
  });

  it('knows which phases are over', () => {
    expect(isTerminal('completed')).toBe(true);
    expect(isTerminal('failed')).toBe(true);
    expect(isTerminal('connected')).toBe(false);
  });
});

// R3-unknown-status-tears-down-live-call fix: `phaseFromStatus` deliberately defaults an
// UNRECOGNISED status to `'failed'` (tested above), so `isTerminal(phaseFromStatus(x))`
// can never tell "genuinely over" apart from "we don't understand this status yet". The
// poll's actual hangup trigger must use this stricter check instead — membership in the
// server's own two terminal `voip_calls.status` values (`voip::state::CallStatus`,
// checked everywhere server-side as `NOT IN ('completed', 'failed')`) — so a future or
// unrecognised status never auto-hangs-up a call that might still be live.
describe('isKnownTerminalStatus', () => {
  it('recognises the server\'s two terminal statuses', () => {
    expect(isKnownTerminalStatus('completed')).toBe(true);
    expect(isKnownTerminalStatus('failed')).toBe(true);
  });

  it('stays conservative about a status it does not recognise', () => {
    for (const junk of ['created', 'dialing', 'ringing', 'answered', 'bridged', 'ending', 'in_progress', '']) {
      expect(isKnownTerminalStatus(junk)).toBe(false);
    }
  });
});

describe('normaliseDestination', () => {
  it('accepts the punctuation people type', () => {
    for (const raw of ['+39 320 1234567', '+39-320-1234567', '+39 (320) 123.4567']) {
      expect(normaliseDestination(raw)).toBe('+393201234567');
    }
  });

  it('keeps at most the leading plus', () => {
    expect(normaliseDestination('00393201234567')).toBe('00393201234567');
    expect(normaliseDestination('393201234567')).toBe('393201234567');
    expect(normaliseDestination('  ')).toBe('');
    expect(normaliseDestination('')).toBe('');
  });

  it('drops letters instead of encoding them', () => {
    expect(normaliseDestination('+39320CALL')).toBe('+39320');
  });
});

describe('looksDialable', () => {
  it('is a keystroke filter, not a validator', () => {
    expect(looksDialable('+393201234567')).toBe(true);
    expect(looksDialable('+8613800138000')).toBe(true);
    expect(looksDialable('+3932')).toBe(false);
    expect(looksDialable('')).toBe(false);
    expect(looksDialable('+3932012345678901')).toBe(false);
    expect(looksDialable('0393201234567')).toBe(false);
  });
});

describe('money formatting', () => {
  it('renders credits as the currency amount they are', () => {
    expect(formatCredits(250)).toBe('2.50');
    expect(formatCredits(1)).toBe('0.01');
    expect(formatCredits(0)).toBe('0.00');
    expect(formatCredits(Number.NaN)).toBe('0.00');
  });

  it('rounds an estimate UP, never down', () => {
    expect(estimateCost('0.0468', 10)).toBe('0.47');
    expect(estimateCost('0.05', 10)).toBe('0.50');
    expect(estimateCost(0.0333, 3)).toBe('0.10');
    expect(estimateCost('0', 10)).toBe('0.00');
  });

  it('refuses to invent a number from nonsense', () => {
    expect(estimateCost('abc', 10)).toBe('0.00');
    expect(estimateCost('0.05', 0)).toBe('0.00');
    expect(estimateCost('0.05', -3)).toBe('0.00');
    expect(estimateCost('-1', 10)).toBe('0.00');
  });
});

describe('formatDuration', () => {
  it('reads as a call timer', () => {
    expect(formatDuration(0)).toBe('0:00');
    expect(formatDuration(95)).toBe('1:35');
    expect(formatDuration(3600)).toBe('1:00:00');
    expect(formatDuration(3661)).toBe('1:01:01');
  });

  it('shows a dash rather than a zero for a call that never connected', () => {
    expect(formatDuration(null)).toBe('—');
    expect(formatDuration(undefined)).toBe('—');
    expect(formatDuration(-1)).toBe('—');
  });
});

describe('refusal copy (flat phoneReason<PascalCase> keys)', () => {
  it('localises every reason the server can send on quote/dial/hangup', () => {
    expect(refusalKey('insufficient_credits')).toBe('phoneReasonInsufficientCredits');
    expect(refusalKey('eu_processing_unavailable')).toBe('phoneReasonEuProcessingUnavailable');
    expect(refusalKey('number_too_short')).toBe('phoneReasonNumberTooShort');
    expect(refusalKey('no_answer')).toBe('phoneReasonNoAnswer');
    expect(refusalKey('caller_id_unverified')).toBe('phoneReasonCallerIdUnverified');
  });

  it('never prints a raw server code at a customer', () => {
    expect(refusalKey('something_new_from_the_server')).toBe('phoneReasonGeneric');
    expect(refusalKey(null)).toBe('phoneReasonGeneric');
    expect(refusalKey('')).toBe('phoneReasonGeneric');
    expect(refusalKey(undefined)).toBe('phoneReasonGeneric');
  });

  it('lets a caller choose its own fallback; a named code still wins', () => {
    expect(refusalKey(null, 'phoneQuoteFailed')).toBe('phoneQuoteFailed');
    expect(refusalKey('whatever_is_new', 'phoneQuoteFailed')).toBe('phoneQuoteFailed');
    expect(refusalKey('invalid_country_code', 'phoneQuoteFailed')).toBe(
      'phoneReasonInvalidCountryCode',
    );
  });

  it('still distinguishes an unknown reason, so missing copy is findable', () => {
    expect(hasReasonCopy('busy')).toBe(true);
    expect(hasReasonCopy('something_new_from_the_server')).toBe(false);
    expect(hasReasonCopy(null)).toBe(false);
    expect(hasReasonCopy('')).toBe(false);
  });

  it('carries copy for every quote/dial/hangup-reachable code (design contract, 30 codes)', () => {
    const codes = [
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
    ];
    expect(codes).toHaveLength(30);
    for (const code of codes) {
      expect(hasReasonCopy(code)).toBe(true);
    }
  });
});

describe('announcement', () => {
  const t = (k: string) => k;

  it('announces a change once and only once', () => {
    expect(announcement(null, 'dialing', t)).toBe('phase.dialing');
    expect(announcement('dialing', 'dialing', t)).toBeNull();
    expect(announcement('dialing', 'ringing', t)).toBe('phase.ringing');
  });

  it('announces every phase a user can land in', () => {
    const phases: CallPhase[] = [
      'preparing',
      'dialing',
      'ringing',
      'connected',
      'reconnecting',
      'ending',
      'completed',
      'failed',
    ];
    for (const p of phases) {
      expect(announcement('idle', p, t)).toBe(`phase.${p}`);
    }
  });
});

describe('phaseProgress', () => {
  it('advances monotonically through a normal call', () => {
    const order: CallPhase[] = ['preparing', 'dialing', 'ringing', 'connected', 'ending', 'completed'];
    let last = -1;
    for (const p of order) {
      const now = phaseProgress(p);
      expect(now).toBeGreaterThan(last);
      last = now;
    }
    expect(last).toBe(1);
  });

  it('does not rewind while reconnecting', () => {
    expect(phaseProgress('reconnecting')).toBe(phaseProgress('connected'));
  });

  it('is complete for a failed call too', () => {
    expect(phaseProgress('failed')).toBe(1);
    expect(phaseProgress('idle')).toBe(0);
  });
});

describe('disclosure preview', () => {
  it('tells the user what the recipient will hear, before the call', () => {
    expect(
      disclosureSummaryKey({ recording: true, transcription: true, consent_policy: 'press_key' }),
    ).toBe('phone.disclosure.both');
    expect(
      disclosureSummaryKey({ recording: true, transcription: false, consent_policy: 'press_key' }),
    ).toBe('phone.disclosure.recording');
    expect(
      disclosureSummaryKey({ recording: false, transcription: true, consent_policy: 'press_key' }),
    ).toBe('phone.disclosure.transcription');
    expect(
      disclosureSummaryKey({ recording: false, transcription: false, consent_policy: 'press_key' }),
    ).toBe('phone.disclosure.translationOnly');
  });

  it('warns that the recipient will be asked, but only when they will be', () => {
    expect(
      willAskConsent({ recording: true, transcription: false, consent_policy: 'press_key' }),
    ).toBe(true);
    expect(
      willAskConsent({ recording: true, transcription: false, consent_policy: 'verbal' }),
    ).toBe(true);
    expect(
      willAskConsent({ recording: true, transcription: false, consent_policy: 'notice_only' }),
    ).toBe(false);
    expect(
      willAskConsent({ recording: false, transcription: false, consent_policy: 'press_key' }),
    ).toBe(false);
  });
});

describe('numberProblem', () => {
  it('names the same problem the server would name', () => {
    expect(numberProblem('')).toBe('number_empty');
    expect(numberProblem('   ')).toBe('number_empty');
    expect(numberProblem('nope')).toBe('number_non_numeric');
    expect(numberProblem('0320 123 4567')).toBe('number_leading_zero');
    expect(numberProblem('+39 320')).toBe('number_too_short');
    expect(numberProblem('+3912345678901234567')).toBe('number_too_long');
  });

  it('passes a dialable number', () => {
    expect(numberProblem('+39 320 123 4567')).toBeNull();
    expect(numberProblem('+8613800138000')).toBeNull();
  });

  it('agrees with looksDialable, which is the same rule asked as a question', () => {
    for (const raw of ['', 'nope', '0320123456', '+39320', '+39 320 123 4567', '+8613800138000']) {
      expect(looksDialable(raw)).toBe(numberProblem(raw) === null);
    }
  });
});

describe('estimateCost and binary floating point', () => {
  it('does not invent a cent that is not owed', () => {
    expect(estimateCost(0.005, 14)).toBe('0.07');
    expect(estimateCost(0.002, 35)).toBe('0.07');
    expect(estimateCost(0.0025, 28)).toBe('0.07');
    expect(estimateCost(0.0025, 56)).toBe('0.14');
  });

  it('still rounds a real fraction of a cent up', () => {
    expect(estimateCost(0.0468, 10)).toBe('0.47');
    expect(estimateCost(0.0001, 1)).toBe('0.01');
  });

  it('never reads lower than the exact price, across the deck', () => {
    let over = 0;
    let under = 0;
    for (let c = 1; c <= 500; c++) {
      for (let m = 1; m <= 60; m++) {
        const exact = Math.ceil((c * m) / 100);
        const got = Math.round(parseFloat(estimateCost(c / 10000, m)) * 100);
        if (got > exact) over++;
        if (got < exact) under++;
      }
    }
    expect(under).toBe(0);
    expect(over).toBe(0);
  });
});

describe('isPhonePeer', () => {
  it('recognises the id shape the server mints for a telephone leg', () => {
    // `format!("phone-{}", Uuid::new_v4().simple())` — server/src/voip/session.rs.
    expect(isPhonePeer('phone-0123456789abcdef0123456789abcdef')).toBe(true);
    expect(isPhonePeer('phone-0123456789ABCDEF0123456789ABCDEF')).toBe(true);
  });

  it('never mistakes an ordinary browser peer for the telephone', () => {
    expect(isPhonePeer('id-abc123-1700000000000')).toBe(false);
    expect(isPhonePeer('a1b2c3d4-e5f6-47a8-89ab-cdef01234567')).toBe(false);
    expect(isPhonePeer('phone-tooshort')).toBe(false);
    expect(isPhonePeer('phone-0123456789abcdef0123456789abcdefextra')).toBe(false);
    expect(isPhonePeer('')).toBe(false);
    expect(isPhonePeer(null)).toBe(false);
    expect(isPhonePeer(undefined)).toBe(false);
  });
});

// PR3 (spec: web-app-voip-dialer, R1/R10): the home-screen CTA decision, extracted here
// rather than left as untested app.ts glue — the two inputs (an active-subscription org,
// a WebRTC-capable browser) are duck-typed so this stays framework-free, exactly like
// `phone-call.ts`'s `OkData<T>`.
describe('canShowPhoneCta', () => {
  it('needs at least one active-subscription org AND WebRTC support', () => {
    const active = { subscription_status: 'active' };
    const pastDue = { subscription_status: 'past_due' };
    expect(canShowPhoneCta([active], true)).toBe(true);
    expect(canShowPhoneCta([pastDue], true)).toBe(false);
    expect(canShowPhoneCta([], true)).toBe(false);
    expect(canShowPhoneCta([active], false)).toBe(false);
    expect(canShowPhoneCta([pastDue, active], true)).toBe(true);
  });
});

// R2-dial-org-gate-divergence fix: the CTA's visibility (`canShowPhoneCta`, above) and
// the org actually used to PLACE the call (`app.ts`'s `openPhoneDialPanel`) answered the
// same "may this org dial" question with two separately-written predicates that could
// silently drift apart. `hasActiveSubscription` is now the single source of truth both
// call sites share, so they cannot disagree again.
describe('hasActiveSubscription', () => {
  it('is true only for an org with an active subscription', () => {
    expect(hasActiveSubscription({ subscription_status: 'active' })).toBe(true);
    expect(hasActiveSubscription({ subscription_status: 'past_due' })).toBe(false);
    expect(hasActiveSubscription({ subscription_status: 'none' })).toBe(false);
    expect(hasActiveSubscription({ subscription_status: 'canceled' })).toBe(false);
  });
});

// PR3 (spec: web-app-voip-dialer, R8): the destination field doubles as a contact
// search — letters mean "search the address book", digits (with the punctuation people
// type) mean "this is already a number". One rule, so the field and the quote it fires
// can never disagree about what the user typed.
describe('looksLikeContactSearch', () => {
  it('treats typed letters as a contact query', () => {
    expect(looksLikeContactSearch('Maria')).toBe(true);
    expect(looksLikeContactSearch('acme corp')).toBe(true);
  });

  it('treats digits and dial punctuation as a raw number, not a search', () => {
    expect(looksLikeContactSearch('+39 320 123 4567')).toBe(false);
    expect(looksLikeContactSearch('0320-123-4567')).toBe(false);
    expect(looksLikeContactSearch('')).toBe(false);
  });
});

// PR3 (spec: web-app-voip-dialer, R10): a quote/dial refusal must map to real copy, not
// a placeholder — `errorCode` (voip.ts) reads the server's `{error}` body, `refusalKey`
// (this module, already RED→GREEN→TRIANGULATE tested above) turns it into the flat i18n
// key `app.ts` writes into the dial panel's status line.
describe('refusalKey(errorCode(...)) — the quote/dial refusal pipeline', () => {
  it('maps a real server refusal body to its reason key', () => {
    expect(refusalKey(errorCode({ error: 'insufficient_credits' }))).toBe(
      'phoneReasonInsufficientCredits',
    );
  });

  it('falls back to the generic key for an unmapped code or a transport failure', () => {
    expect(refusalKey(errorCode({ error: 'brand_new_code' }))).toBe('phoneReasonGeneric');
    expect(refusalKey(errorCode(null))).toBe('phoneReasonGeneric');
  });
});

// Work Unit B (spec: web-app-voip-dialer, R11/R12) — the visible remote-hangup notice's
// copy-selection. `reason` comes straight off the server's `phone_call_ended` message
// (design Decision A3): "remote_hangup" for an ordinary hangup, or an existing
// `FailureReason` string (e.g. "media_lost") for a pump-error teardown. An unmapped or
// absent reason falls back to the existing neutral `phoneReasonUnmapped` sentence rather
// than asserting who hung up — the same "never print/assert a raw or guessed thing"
// convention `refusalKey` already enforces above.
describe('phoneEndCopyKey (Decision B2)', () => {
  it('names the other party for an ordinary remote hangup', () => {
    expect(phoneEndCopyKey('remote_hangup')).toBe('phoneRemoteEnded');
  });

  it('reuses the existing media-lost copy for a pump-error teardown', () => {
    expect(phoneEndCopyKey('media_lost')).toBe('phoneReasonMediaLost');
  });

  it('never asserts who hung up on an unknown or missing reason', () => {
    expect(phoneEndCopyKey(null)).toBe('phoneReasonUnmapped');
    expect(phoneEndCopyKey(undefined)).toBe('phoneReasonUnmapped');
    expect(phoneEndCopyKey('')).toBe('phoneReasonUnmapped');
    expect(phoneEndCopyKey('something_new')).toBe('phoneReasonUnmapped');
  });
});

// Work Unit B (Decision B3) — the auto-leave guard: the exact state that distinguishes
// "my B2B-dialer phone call" from "a meeting that happens to have a phone participant in
// it". All four conditions are required; see the design's rationale for why none can be
// dropped without either re-ejecting a multi-human meeting or leaking a stray auto-leave
// across calls.
describe('shouldLeaveOnPhoneCallEnded (Decision B3)', () => {
  const PHONE_ID = 'phone-0123456789abcdef0123456789abcdef';

  it('fires for the dialer\'s own call with no human counterpart remaining', () => {
    expect(
      shouldLeaveOnPhoneCallEnded({
        entry: 'phone',
        endedPeerId: PHONE_ID,
        remotePeerIds: [],
        myCallId: 'call-1',
        eventCallId: 'call-1',
      }),
    ).toBe(true);
  });

  it('does NOT fire in a multi-human room with a dialed-in phone participant', () => {
    expect(
      shouldLeaveOnPhoneCallEnded({
        entry: 'phone',
        endedPeerId: PHONE_ID,
        remotePeerIds: ['human-1', PHONE_ID],
        myCallId: 'call-1',
        eventCallId: 'call-1',
      }),
    ).toBe(false);
  });

  it('does NOT fire for a human who joined a phone-hosting room by any other route', () => {
    expect(
      shouldLeaveOnPhoneCallEnded({
        entry: 'room',
        endedPeerId: PHONE_ID,
        remotePeerIds: [],
        myCallId: null,
        eventCallId: 'call-1',
      }),
    ).toBe(false);
  });

  it('does NOT fire for someone else\'s call, and never falls back to a looser rule', () => {
    expect(
      shouldLeaveOnPhoneCallEnded({
        entry: 'phone',
        endedPeerId: PHONE_ID,
        remotePeerIds: [],
        myCallId: 'call-1',
        eventCallId: 'call-2',
      }),
    ).toBe(false);
    expect(
      shouldLeaveOnPhoneCallEnded({
        entry: 'phone',
        endedPeerId: PHONE_ID,
        remotePeerIds: [],
        myCallId: null,
        eventCallId: 'call-1',
      }),
    ).toBe(false);
  });

  it('does NOT fire a second time after the exit already ran (idempotency reuses existing state)', () => {
    // Mirrors what app.ts observes once leaveCall() has already run once: entryMode is
    // back to 'room' and currentCallId() is null (Decision B4 — no new flag needed).
    expect(
      shouldLeaveOnPhoneCallEnded({
        entry: 'room',
        endedPeerId: PHONE_ID,
        remotePeerIds: [],
        myCallId: null,
        eventCallId: 'call-1',
      }),
    ).toBe(false);
  });
});

// Ported from dashboard/src/scripts/phone-catalogue.test.ts (same cases, same function —
// see phone-dialer.ts's usableCallerIds doc comment for why a released/pending number
// must never reach this app's caller-id select).
describe('usableCallerIds', () => {
  const numbers = [
    {
      id: '1',
      e164: '+390212345678',
      country: 'IT',
      label: 'Milan Office',
      is_default: true,
      inbound_enabled: false,
      outbound_enabled: true,
      verification_status: 'verified',
    },
    {
      id: '2',
      e164: '+34911234567',
      country: 'ES',
      label: 'Sales Spain',
      is_default: false,
      inbound_enabled: false,
      outbound_enabled: true,
      verification_status: 'pending',
    },
    {
      id: '3',
      e164: '+390687654321',
      country: 'IT',
      label: 'Fax',
      is_default: false,
      inbound_enabled: false,
      outbound_enabled: false,
      verification_status: 'verified',
    },
    {
      id: '4',
      e164: '+390612345678',
      country: 'IT',
      label: 'Released',
      is_default: false,
      inbound_enabled: false,
      outbound_enabled: false,
      verification_status: 'verified',
    },
  ];

  it('offers only a number that is both verified and outbound-enabled', () => {
    // `resolve_caller_id` on the server refuses anything else, so offering it would
    // produce a refusal after the user had already chosen. Same rule, stated early.
    expect(usableCallerIds(numbers).map((n) => n.e164)).toEqual(['+390212345678']);
  });

  it('offers nothing rather than something fabricated when the org owns nothing', () => {
    expect(usableCallerIds([])).toEqual([]);
  });
});
