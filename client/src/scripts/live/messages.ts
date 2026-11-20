// Live Connect — which words to show (spec 0121).
//
// Pure mappings from machine state and server codes to i18n KEYS. Nothing here renders: a
// code is a contract and the sentence is copy, and keeping them apart is what lets the
// copy be translated, reworded and tested without touching protocol code. A server
// `message` is never shown to a user — it is English and may change.

import { DEFAULT_ENGINE_ID } from '../engines';
import type { EndReason, FailureKind, LiveContext } from './session-machine';

/** `staff` is the seat of a member of an organisation answering a desk (spec 0121 B2B). */
export type Role = 'host' | 'guest' | 'staff';

/** What the user can usefully do about an error. The view turns it into a control. */
export type ErrorAction = 'retry' | 'buy' | 'home' | 'signin' | 'refresh' | 'none';

export function statusKey(ctx: LiveContext): string {
  switch (ctx.phase) {
    case 'idle':
    case 'requesting_mic':
      return 'talkStatusMic';
    case 'connecting':
      return 'talkStatusConnecting';
    case 'waiting_peer':
      return 'liveWaiting';
    case 'live':
      return 'liveStatusLive';
    case 'reconnecting':
      return 'talkStatusReconnecting';
    case 'ended':
      return 'liveStatusEnded';
    case 'failed':
      return failureKey(ctx.failure);
  }
}

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
    case 'network':
      return 'liveErrNetwork';
    case 'credits':
      return 'liveErrCredits';
    case 'busy':
      return 'liveErrBusy';
    default:
      return 'talkErrorGeneric';
  }
}

/** Why the conversation is over, said from the point of view of `role`. */
export function endedKey(reason: EndReason, role: Role): string {
  switch (reason) {
    case 'ended_by_me':
      return 'liveEndedByMe';
    case 'left_by_me':
      // A guest hung up in a conversation that carries on without them.
      return 'liveLeft';
    case 'ended_by_host':
      return role === 'host' ? 'liveEndedByMe' : 'liveEndedByPartner';
    case 'ended_by_guest':
      return role === 'guest' ? 'liveEndedByMe' : 'liveEndedByPartner';
    case 'ended_by_staff':
      // The server records this when the staff seat of a desk ends the conversation.
      return role === 'staff' ? 'liveEndedByMe' : 'liveEndedByPartner';
    case 'balance_exhausted':
      return 'liveEndedBalance';
    case 'max_duration':
      return 'liveEndedMaxDuration';
    case 'abandoned':
      return 'liveEndedAbandoned';
    case 'billing_error':
      return 'liveEndedBilling';
    case 'expired':
      return 'liveEndedExpired';
    case 'desk_revoked':
    case 'desk_regenerated':
      return 'liveDeskErrInvalid';
  }
}

/** Copy and next step for a REST error code. Every documented code has its own message. */
export function errorInfo(
  code: string,
  role: Role,
  /** Set when the host is CREATING a conversation: `engine` is the tier they asked for. */
  create?: { engine?: string },
): { key: string; action: ErrorAction } {
  switch (code) {
    case 'invite_not_found':
      return { key: 'liveErrInviteNotFound', action: 'home' };
    case 'invite_expired':
      return { key: 'liveErrInviteExpired', action: 'home' };
    case 'session_full':
      // Several guests can share a link now, so "this link was already used" is no longer true.
      return { key: 'liveErrConversationFull', action: 'home' };
    case 'session_ended':
      return { key: 'liveErrSessionEnded', action: 'home' };
    case 'consent_required':
      return { key: 'liveConsentRequired', action: 'none' };
    case 'bad_lang':
      return { key: 'liveErrBadLang', action: 'none' };
    case 'bad_max_guests':
      return { key: 'liveErrBadGuests', action: 'none' };
    case 'too_many_sessions':
      return { key: 'liveErrTooMany', action: 'none' };
    case 'insufficient_balance':
    case 'insufficient_credits':
      // The host pays, so only the host can fix it; a guest is told whose problem it is.
      return role === 'host'
        ? { key: 'liveErrInsufficient', action: 'buy' }
        : { key: 'liveErrOwnerCredits', action: 'home' };
    case 'subscription_required':
      return { key: 'liveErrSubscription', action: 'home' };
    case 'project_required':
    case 'project_not_found':
      return { key: 'liveErrProject', action: 'home' };
    case 'live_busy':
      // The tier asked for has no room: Standard is the way out, not "try again in a minute".
      if (role === 'host' && create?.engine && create.engine !== DEFAULT_ENGINE_ID) {
        return { key: 'liveErrBusyTier', action: 'none' };
      }
      return { key: 'liveErrBusy', action: 'retry' };
    case 'engine_not_available':
      return { key: 'liveErrEngineNotAvailable', action: 'none' };
    case 'engine_not_supported_yet':
      return { key: 'liveErrEngineSoon', action: 'none' };
    case 'engine_not_allowed':
      return { key: 'liveErrEngineNotAllowed', action: 'none' };
    case 'voice_clone_unavailable':
      return { key: 'liveErrVoiceCloneUnavailable', action: 'none' };
    case 'bad_voice_clone':
      return { key: 'liveErrBadVoiceClone', action: 'none' };
    case 'rate_limited':
      return { key: 'liveErrRateLimited', action: 'retry' };
    case 'enhanced_unavailable':
    case 'not_enhanced':
      return { key: 'liveErrEnhancedUnavailable', action: 'none' };
    case 'unauthorized':
    case 'bad_token':
    case 'token_expired':
      return { key: 'liveErrAuth', action: role === 'host' ? 'signin' : 'home' };
    case 'wrong_session':
      return { key: 'liveErrWrongSession', action: 'home' };
    case 'live_disabled':
      return { key: 'liveUnavailable', action: 'home' };
    // Desk scan (B2B). A forged, unknown, revoked and replaced QR all answer the same way on
    // purpose, so the client cannot say which: it can only say the code is not valid.
    case 'invalid_desk_qr':
      return { key: 'liveDeskErrInvalid', action: 'none' };
    case 'desk_unavailable':
      return { key: 'liveDeskErrUnavailable', action: 'retry' };
    case 'desk_busy':
      return { key: 'liveDeskErrBusy', action: 'retry' };
    case 'network':
      return { key: 'liveErrNetworkRequest', action: 'retry' };
    default:
      // server_error, http_error and anything the server invents later.
      return { key: 'talkErrorGeneric', action: 'retry' };
  }
}

/**
 * Copy and next step for a failed staff call (list, answer). Keyed on the STATUS as well as
 * the code: the org routes answer in plain text, so a 403/404 arrives with no code at all.
 */
export function staffErrorInfo(res: { status: number; code: string }): { key: string; action: ErrorAction } {
  const { status, code } = res;
  if (status === 401) return { key: 'liveStaffErrAuth', action: 'signin' };
  if (code === 'already_answered') return { key: 'liveStaffErrTaken', action: 'refresh' };
  if (code === 'session_expired' || code === 'session_ended' || status === 410) {
    return { key: 'liveStaffErrGone', action: 'refresh' };
  }
  if (code === 'insufficient_credits' || code === 'insufficient_balance' || code === 'subscription_required') {
    // Only an administrator can top up or change the plan.
    return { key: 'liveStaffErrOrg', action: 'none' };
  }
  if (status === 403 || status === 404) return { key: 'liveStaffErrNotFound', action: 'none' };
  if (code === 'live_busy') return { key: 'liveErrBusy', action: 'retry' };
  if (code === 'rate_limited') return { key: 'liveErrRateLimited', action: 'retry' };
  if (code === 'bad_lang') return { key: 'liveErrBadLang', action: 'none' };
  if (code === 'network') return { key: 'liveErrNetworkRequest', action: 'retry' };
  return { key: 'talkErrorGeneric', action: 'retry' };
}

/** Recoverable, in-conversation problems (`error` frames that do not end it). */
export function noticeKey(code: string): string {
  switch (code) {
    case 'audio_session_closed':
    case 'engine_unavailable':
      return 'liveNoticeEngine';
    case 'billing_unavailable':
      return 'liveNoticeBilling';
    case 'server_busy':
      return 'liveNoticeBusy';
    case 'translation_unavailable':
      return 'liveNoticeEngine';
    case 'text_too_long':
      return 'liveNoticeTextTooLong';
    default:
      return 'talkErrorGeneric';
  }
}
