import { describe, expect, it } from 'vitest';
import { endedKey, errorInfo, failureKey, noticeKey, staffErrorInfo, statusKey } from './messages';
import { initialContext, type EndReason, type LiveContext } from './session-machine';
import en from '../i18n/en.json';

const EN = en as Record<string, string>;
const ctx = (phase: LiveContext['phase'], over: Partial<LiveContext> = {}): LiveContext => ({ ...initialContext(), phase, ...over });

describe('statusKey', () => {
  it('has a distinct key per phase, and "waiting" for an empty room', () => {
    const keys = (['requesting_mic', 'connecting', 'waiting_peer', 'live', 'reconnecting', 'ended'] as const).map((p) => statusKey(ctx(p)));
    expect(new Set(keys).size).toBe(keys.length);
    expect(statusKey(ctx('waiting_peer'))).toBe('liveWaiting');
  });
});

describe('failureKey', () => {
  it.each(['mic_denied', 'mic_missing', 'mic_blocked', 'unsupported', 'network', 'unknown', 'credits', 'busy'] as const)('%s maps to an existing en.json key', (kind) => {
    expect(EN[failureKey(kind)]).toBeTruthy();
  });
  it('null falls back to the generic message', () => {
    expect(failureKey(null)).toBe('talkErrorGeneric');
  });
});

describe('endedKey', () => {
  const reasons: EndReason[] = ['ended_by_me', 'left_by_me', 'ended_by_host', 'ended_by_guest', 'ended_by_staff', 'balance_exhausted', 'max_duration', 'abandoned', 'billing_error', 'expired', 'desk_revoked', 'desk_regenerated'];
  it.each(reasons)('%s has copy for every role', (reason) => {
    for (const role of ['host', 'guest', 'staff'] as const) expect(EN[endedKey(reason, role)]).toBeTruthy();
  });
  it('a desk seat ending the conversation is "you" for the staff member and "the other person" for the customer', () => {
    // `ended_by_staff` is what the server records when the staff seat of a desk ends it.
    expect(endedKey('ended_by_staff', 'staff')).toBe('liveEndedByMe');
    expect(endedKey('ended_by_staff', 'guest')).toBe('liveEndedByPartner');
    expect(endedKey('ended_by_guest', 'staff')).toBe('liveEndedByPartner');
  });
  it('a revoked or replaced desk code reads as "this QR code is no longer valid"', () => {
    expect(endedKey('desk_revoked', 'guest')).toBe('liveDeskErrInvalid');
    expect(endedKey('desk_regenerated', 'guest')).toBe('liveDeskErrInvalid');
  });
  it('says "you left" for a guest who hung up in a conversation that carries on', () => {
    expect(endedKey('left_by_me', 'guest')).toBe('liveLeft');
    expect(EN.liveLeft).toBeTruthy();
  });
  it('says "the other person ended it" from each side, and "you ended it" for a local end', () => {
    expect(endedKey('ended_by_host', 'guest')).toBe('liveEndedByPartner');
    expect(endedKey('ended_by_guest', 'host')).toBe('liveEndedByPartner');
    expect(endedKey('ended_by_host', 'host')).toBe('liveEndedByMe');
    expect(endedKey('ended_by_me', 'guest')).toBe('liveEndedByMe');
  });
});

describe('errorInfo', () => {
  const codes = [
    'invite_not_found', 'invite_expired', 'session_full', 'session_ended', 'consent_required', 'bad_lang',
    'too_many_sessions', 'insufficient_balance', 'insufficient_credits', 'subscription_required', 'project_required',
    'project_not_found', 'live_busy', 'rate_limited', 'unauthorized', 'bad_token', 'token_expired', 'wrong_session',
    'live_disabled', 'network', 'server_error', 'http_error', 'totally_new',
    'invalid_desk_qr', 'desk_unavailable', 'desk_busy', 'bad_max_guests',
  ];
  it.each(codes)('%s maps to existing copy for both roles', (code) => {
    for (const role of ['host', 'guest'] as const) {
      const info = errorInfo(code, role);
      expect(EN[info.key], `${code}/${role}`).toBeTruthy();
    }
  });

  it('gives the distinct guest-facing codes distinct messages and sensible actions', () => {
    const guest = (c: string) => errorInfo(c, 'guest');
    const distinct = ['invite_not_found', 'invite_expired', 'session_full', 'session_ended'].map((c) => guest(c).key);
    expect(new Set(distinct).size).toBe(4);
    expect(guest('session_full').action).toBe('home');
    expect(guest('invite_expired').action).toBe('home');
    expect(guest('live_busy').action).toBe('retry');
    expect(guest('rate_limited').action).toBe('retry');
    expect(guest('network').action).toBe('retry');
    expect(guest('consent_required').action).toBe('none');
  });

  it('a full conversation no longer claims the link was "used", and a bad guest count has its own message', () => {
    expect(errorInfo('session_full', 'guest')).toEqual({ key: 'liveErrConversationFull', action: 'home' });
    expect(errorInfo('bad_max_guests', 'host')).toEqual({ key: 'liveErrBadGuests', action: 'none' });
  });

  it('sends a host with no credit to buy some, and a guest to a plain explanation', () => {
    expect(errorInfo('insufficient_balance', 'host')).toEqual({ key: 'liveErrInsufficient', action: 'buy' });
    expect(errorInfo('insufficient_credits', 'host').action).toBe('buy');
    expect(errorInfo('insufficient_balance', 'guest').key).toBe('liveErrOwnerCredits');
    expect(errorInfo('insufficient_balance', 'guest').action).toBe('home');
  });

  it('asks a signed-out host to sign in again', () => {
    expect(errorInfo('unauthorized', 'host').action).toBe('signin');
  });
});

describe('errorInfo for a desk scan', () => {
  const guest = (c: string) => errorInfo(c, 'guest');
  it('gives every desk code its own message', () => {
    const keys = ['invalid_desk_qr', 'desk_unavailable', 'desk_busy', 'rate_limited', 'live_busy', 'consent_required', 'bad_lang'].map((c) => guest(c).key);
    expect(new Set(keys).size).toBe(keys.length);
  });
  it('a QR that is not valid cannot be retried; the others can, except the ones the form fixes', () => {
    expect(guest('invalid_desk_qr').action).toBe('none');
    expect(guest('desk_unavailable').action).toBe('retry');
    expect(guest('desk_busy').action).toBe('retry');
    expect(guest('rate_limited').action).toBe('retry');
    expect(guest('live_busy').action).toBe('retry');
  });
});

describe('errorInfo: translation tier codes (create)', () => {
  it('has its own copy for each, all present in en.json', () => {
    expect(errorInfo('engine_not_available', 'host')).toEqual({ key: 'liveErrEngineNotAvailable', action: 'none' });
    expect(errorInfo('engine_not_supported_yet', 'host')).toEqual({ key: 'liveErrEngineSoon', action: 'none' });
    expect(errorInfo('engine_not_allowed', 'host')).toEqual({ key: 'liveErrEngineNotAllowed', action: 'none' });
    for (const k of ['liveErrEngineNotAvailable', 'liveErrEngineSoon', 'liveErrEngineNotAllowed', 'liveErrBusyTier']) {
      expect(EN[k]).toBeTruthy();
    }
  });

  it('live_busy on a non-Standard create suggests Standard; Standard and guests keep the plain message', () => {
    expect(errorInfo('live_busy', 'host', { engine: 'gemini_live_translate' })).toEqual({ key: 'liveErrBusyTier', action: 'none' });
    expect(errorInfo('live_busy', 'host', { engine: 'standard' })).toEqual({ key: 'liveErrBusy', action: 'retry' });
    expect(errorInfo('live_busy', 'host')).toEqual({ key: 'liveErrBusy', action: 'retry' });
    expect(errorInfo('live_busy', 'guest', { engine: 'gemini_live_translate' })).toEqual({ key: 'liveErrBusy', action: 'retry' });
  });

  it('a 402 on a paid tier still leads to the buy-credits path', () => {
    expect(errorInfo('insufficient_balance', 'host', { engine: 'gemini_live_translate' })).toEqual({ key: 'liveErrInsufficient', action: 'buy' });
  });
});

describe('staffErrorInfo', () => {
  const info = (status: number, code: string) => staffErrorInfo({ status, code });
  it('409 already_answered: a colleague got there first, refresh the list', () => {
    expect(info(409, 'already_answered')).toEqual({ key: 'liveStaffErrTaken', action: 'refresh' });
  });
  it('410 session_expired / session_ended: the customer left, refresh the list', () => {
    expect(info(410, 'session_expired')).toEqual({ key: 'liveStaffErrGone', action: 'refresh' });
    expect(info(410, 'session_ended')).toEqual({ key: 'liveStaffErrGone', action: 'refresh' });
  });
  it('402 insufficient_credits / subscription_required: the organisation cannot take calls, and the staff member cannot fix it', () => {
    expect(info(402, 'insufficient_credits')).toEqual({ key: 'liveStaffErrOrg', action: 'none' });
    expect(info(402, 'subscription_required')).toEqual({ key: 'liveStaffErrOrg', action: 'none' });
  });
  it('503 live_busy: lines are busy, try again', () => {
    expect(info(503, 'live_busy')).toEqual({ key: 'liveErrBusy', action: 'retry' });
  });
  it('404 and 403 (the org routes answer in plain text, so there is no code): not available to you', () => {
    expect(info(404, 'http_error').key).toBe('liveStaffErrNotFound');
    expect(info(404, 'session_not_found').key).toBe('liveStaffErrNotFound');
    expect(info(403, 'http_error').key).toBe('liveStaffErrNotFound');
  });
  it('401: the sign-in expired', () => {
    expect(info(401, 'http_error')).toEqual({ key: 'liveStaffErrAuth', action: 'signin' });
  });
  it('a dead network and anything unknown can be retried', () => {
    expect(info(0, 'network').action).toBe('retry');
    expect(info(500, 'server_error').action).toBe('retry');
    expect(info(418, 'totally_new').key).toBe('talkErrorGeneric');
  });
  it.each([
    [409, 'already_answered'], [410, 'session_expired'], [402, 'subscription_required'], [503, 'live_busy'],
    [404, 'http_error'], [401, 'http_error'], [0, 'network'], [400, 'bad_lang'], [429, 'rate_limited'], [500, 'server_error'],
  ])('%s %s maps to existing copy', (status, code) => {
    expect(EN[info(status, code).key]).toBeTruthy();
  });
});

describe('noticeKey', () => {
  it('maps the recoverable server codes and falls back to generic', () => {
    expect(EN[noticeKey('audio_session_closed')]).toBeTruthy();
    expect(noticeKey('engine_unavailable')).toBe(noticeKey('audio_session_closed'));
    expect(noticeKey('billing_unavailable')).not.toBe(noticeKey('audio_session_closed'));
    expect(noticeKey('server_busy')).not.toBe(noticeKey('billing_unavailable'));
    expect(noticeKey('xyz')).toBe('talkErrorGeneric');
  });
});

describe('Enhanced messages', () => {
  it.each([
    ['voice_clone_unavailable', 'liveErrVoiceCloneUnavailable', 'none'],
    ['bad_voice_clone', 'liveErrBadVoiceClone', 'none'],
    ['enhanced_unavailable', 'liveErrEnhancedUnavailable', 'none'],
    ['not_enhanced', 'liveErrEnhancedUnavailable', 'none'],
    ['rate_limited', 'liveErrRateLimited', 'retry'],
  ])('%s -> %s', (code, key, action) => {
    expect(errorInfo(code, 'host')).toEqual({ key, action });
    expect(EN[key]).toBeTruthy();
  });

  it('insufficient_balance on the token endpoint keeps its host/guest wording', () => {
    expect(errorInfo('insufficient_balance', 'host').key).toBe('liveErrInsufficient');
    expect(errorInfo('insufficient_balance', 'guest').key).toBe('liveErrOwnerCredits');
  });

  it('in-conversation Enhanced errors have their own notices', () => {
    expect(noticeKey('translation_unavailable')).toBe('liveNoticeEngine');
    // A guest whose phone cannot use Enhanced is told so; it keeps the captions.
    expect(noticeKey('fallback_not_allowed')).toBe('liveNoticeEngine');
    expect(noticeKey('text_too_long')).toBe('liveNoticeTextTooLong');
    expect(EN.liveNoticeTextTooLong).toBeTruthy();
  });
});
