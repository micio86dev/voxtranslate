// Live Connect session state (spec 0121): a pure reducer, in the shape of
// `talk/session-machine.ts`.
//
//   idle → requesting_mic → connecting → waiting_peer ⇄ live
//                                          ↘ reconnecting ↗
//   terminal: ended(reason) | failed(kind)
//
// `waiting_peer` is the one phase Talk does not have: the socket is up and the microphone
// is open, but the other phone is not in the room yet (or has left). The UI shows the invite
// there, and the conversation is NOT yet live.
//
// Illegal transitions are ignored and reported (`accepted: false`), never thrown: a race
// between a tap, a socket event and a server frame is normal, not exceptional.

export type Phase =
  | 'idle'
  | 'requesting_mic'
  | 'connecting'
  | 'waiting_peer'
  | 'live'
  | 'reconnecting'
  | 'ended'
  | 'failed';

/** Why a session failed, so the UI can pick copy without parsing a message. */
export type FailureKind =
  | 'mic_denied'
  | 'mic_missing'
  | 'mic_blocked'
  | 'unsupported'
  | 'network'
  | 'unknown'
  | 'credits'
  | 'busy';

/** Why a session ended. `ended_by_me` is local; the rest are the server's `session_state` reasons. */
export type EndReason =
  | 'ended_by_me'
  | 'ended_by_host'
  | 'ended_by_guest'
  | 'ended_by_staff'
  | 'balance_exhausted'
  | 'max_duration'
  | 'abandoned'
  | 'billing_error'
  | 'expired'
  // A desk session nobody answered, closed because its QR was revoked / replaced meanwhile.
  | 'desk_revoked'
  | 'desk_regenerated';

export interface Partner {
  peerId: string;
  name: string;
  lang: string;
  /** Their microphone is muted (from `peer_muted`). */
  muted: boolean;
}

export interface LiveContext {
  phase: Phase;
  partner: Partner | null;
  endReason: EndReason | null;
  failure: FailureKind | null;
}

export type LiveEvent =
  | { type: 'START_REQUESTED' }
  | { type: 'MIC_GRANTED' }
  | { type: 'MIC_DENIED'; kind: FailureKind }
  | { type: 'SOCKET_OPEN' }
  | { type: 'SOCKET_CLOSED' }
  | { type: 'RECONNECT_SUCCEEDED' }
  | { type: 'RECONNECT_EXHAUSTED' }
  | { type: 'ROOM_JOINED'; partner: Partner | null }
  | { type: 'PEER_JOINED'; partner: Partner }
  | { type: 'PEER_LEFT'; peerId: string }
  | { type: 'PEER_MUTED'; peerId: string; muted: boolean }
  | { type: 'SESSION_ENDED'; reason: EndReason }
  | { type: 'STOP_REQUESTED' }
  | { type: 'FATAL'; kind: FailureKind };

export interface TransitionResult {
  context: LiveContext;
  /** False when the event was not legal here and was ignored. */
  accepted: boolean;
}

const RESOURCE_PHASES: ReadonlySet<Phase> = new Set<Phase>([
  'requesting_mic',
  'connecting',
  'waiting_peer',
  'live',
  'reconnecting',
]);

export function initialContext(): LiveContext {
  return { phase: 'idle', partner: null, endReason: null, failure: null };
}

export function isTerminal(ctx: LiveContext): boolean {
  return ctx.phase === 'ended' || ctx.phase === 'failed';
}

/** True when a microphone, a socket or a wake lock are held and must be released. */
export function holdsResources(ctx: LiveContext): boolean {
  return RESOURCE_PHASES.has(ctx.phase);
}

/** The screen stays awake from the moment the socket is up: a host waiting for a guest
 *  on a locked phone would drop out of the room. */
export function wantsWakeLock(ctx: LiveContext): boolean {
  return ctx.phase === 'waiting_peer' || ctx.phase === 'live' || ctx.phase === 'reconnecting';
}

export function transition(ctx: LiveContext, event: LiveEvent): TransitionResult {
  const ignore = (): TransitionResult => ({ context: ctx, accepted: false });
  const to = (phase: Phase, patch: Partial<LiveContext> = {}): TransitionResult => ({
    context: { ...ctx, phase, ...patch },
    accepted: true,
  });
  const patch = (p: Partial<LiveContext>): TransitionResult => ({
    context: { ...ctx, ...p },
    accepted: true,
  });

  if (isTerminal(ctx) || ctx.phase === 'idle') {
    if (event.type === 'START_REQUESTED') {
      return to('requesting_mic', { partner: null, endReason: null, failure: null });
    }
    // Stopping something that never started is still an ending, with a reason.
    if (ctx.phase === 'idle' && event.type === 'STOP_REQUESTED') {
      return to('ended', { endReason: 'ended_by_me' });
    }
    return ignore();
  }

  // Escape hatches, accepted from every live phase.
  switch (event.type) {
    case 'FATAL':
      return to('failed', { partner: null, failure: event.kind });
    case 'STOP_REQUESTED':
      return to('ended', { partner: null, endReason: 'ended_by_me' });
    case 'SESSION_ENDED':
      return to('ended', { partner: null, endReason: event.reason });
    default:
      break;
  }

  switch (ctx.phase) {
    case 'requesting_mic':
      if (event.type === 'MIC_GRANTED') return to('connecting');
      if (event.type === 'MIC_DENIED') return to('failed', { failure: event.kind });
      return ignore();

    case 'connecting':
      if (event.type === 'SOCKET_OPEN') return to('waiting_peer');
      if (event.type === 'SOCKET_CLOSED') return to('reconnecting');
      return ignore();

    case 'waiting_peer':
    case 'live': {
      switch (event.type) {
        case 'ROOM_JOINED':
          return event.partner ? to('live', { partner: event.partner }) : to('waiting_peer', { partner: null });
        case 'PEER_JOINED':
          return to('live', { partner: event.partner });
        case 'PEER_LEFT':
          if (ctx.partner?.peerId !== event.peerId) return ignore();
          return to('waiting_peer', { partner: null });
        case 'PEER_MUTED':
          if (!ctx.partner || ctx.partner.peerId !== event.peerId) return ignore();
          return patch({ partner: { ...ctx.partner, muted: event.muted } });
        case 'SOCKET_CLOSED':
          return to('reconnecting');
        default:
          return ignore();
      }
    }

    case 'reconnecting':
      if (event.type === 'RECONNECT_SUCCEEDED') return to(ctx.partner ? 'live' : 'waiting_peer');
      if (event.type === 'RECONNECT_EXHAUSTED') return to('failed', { partner: null, failure: 'network' });
      return ignore();

    // Handled above; listed so the switch stays exhaustive for the compiler.
    case 'ended':
    case 'failed':
      return ignore();

    default: {
      const exhaustive: never = ctx.phase;
      return exhaustive;
    }
  }
}
