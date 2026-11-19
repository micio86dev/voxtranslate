import { describe, expect, it } from 'vitest';
import { endedKey, errorInfo, failureKey, noticeKey, statusKey } from './messages';
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
  const reasons: EndReason[] = ['ended_by_me', 'ended_by_host', 'ended_by_guest', 'ended_by_staff', 'balance_exhausted', 'max_duration', 'abandoned', 'billing_error', 'expired'];
  it.each(reasons)('%s has copy for both roles', (reason) => {
    for (const role of ['host', 'guest'] as const) expect(EN[endedKey(reason, role)]).toBeTruthy();
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

describe('noticeKey', () => {
  it('maps the recoverable server codes and falls back to generic', () => {
    expect(EN[noticeKey('audio_session_closed')]).toBeTruthy();
    expect(noticeKey('engine_unavailable')).toBe(noticeKey('audio_session_closed'));
    expect(noticeKey('billing_unavailable')).not.toBe(noticeKey('audio_session_closed'));
    expect(noticeKey('server_busy')).not.toBe(noticeKey('billing_unavailable'));
    expect(noticeKey('xyz')).toBe('talkErrorGeneric');
  });
});
