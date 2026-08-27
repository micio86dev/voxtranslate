// Talk to Anyone session state (spec 0110).
//
// A pure reducer, ported from the Chrome extension's `state/session-machine.ts` — the
// pattern exists there precisely to stop the failures that bite a capture surface:
// starting twice, opening two sockets for one session, sending audio after stop, and
// applying frames from a session that already ended. Cross-repo imports are impossible
// (separate git repo), so this mirrors it the same way `pcm-playback-worklet.js` is
// mirrored.
//
// Two ORTHOGONAL axes, deliberately. The brief lists eleven states, but they are not one
// dimension: `PAUSED` and `REQUESTING_MIC` describe the session, while `TRANSLATING` and
// `SPEAKING` describe what the current sentence is doing. Folding them into one enum is
// how you end up with a `PAUSED_WHILE_SPEAKING` and, shortly after, state spaghetti.
//
//   phase     idle → requesting_mic → connecting → live ⇄ paused → ended
//                                                   ↘ error
//   activity  listening → detecting → translating → speaking     (only while live)
//
// Illegal transitions are ignored and reported, never thrown: a race between a tap and a
// socket event is normal, not exceptional.

/** Session lifecycle. Owns microphone, socket and billing. */
export type Phase =
  | 'idle'
  | 'requesting_mic'
  | 'connecting'
  | 'live'
  | 'reconnecting'
  | 'paused'
  | 'stopping'
  | 'ended'
  | 'error';

/** What the CURRENT utterance is doing. Only meaningful while `phase === 'live'`. */
export type Activity =
  | 'listening'
  | 'detecting'
  | 'translating'
  | 'speaking';

/** Why a session failed, so the UI can pick copy without parsing a message. */
export type FailureKind =
  | 'mic_denied'
  | 'mic_blocked'
  | 'mic_missing'
  | 'unsupported'
  | 'connection'
  | 'credits'
  | 'provider'
  | 'unknown';

export type SessionEvent =
  | { type: 'START_REQUESTED' }
  | { type: 'MIC_GRANTED' }
  | { type: 'MIC_DENIED'; kind: FailureKind }
  | { type: 'SOCKET_OPEN' }
  | { type: 'SOCKET_CLOSED'; recoverable: boolean }
  | { type: 'RECONNECT_SUCCEEDED' }
  | { type: 'RECONNECT_EXHAUSTED' }
  // Activity, driven by what actually arrives from the server — never by a timer.
  | { type: 'SPEECH_DETECTED' }
  | { type: 'DIRECTION_RESOLVED' }
  | { type: 'PLAYBACK_STARTED' }
  | { type: 'PLAYBACK_STOPPED' }
  | { type: 'UTTERANCE_ENDED' }
  | { type: 'PAUSE_REQUESTED' }
  | { type: 'RESUME_REQUESTED' }
  | { type: 'CREDITS_EXHAUSTED' }
  | { type: 'STOP_REQUESTED' }
  | { type: 'TEARDOWN_COMPLETE' }
  | { type: 'FATAL'; kind: FailureKind };

export interface SessionContext {
  phase: Phase;
  activity: Activity;
  /** Unique per start attempt. Every inbound frame is checked against it. */
  sessionId: string | null;
  failure: FailureKind | null;
}

/** Phases in which capture, a socket, or a wake lock are held and must be released. */
const RESOURCE_PHASES: ReadonlySet<Phase> = new Set<Phase>([
  'requesting_mic',
  'connecting',
  'live',
  'reconnecting',
  'paused',
  'stopping',
]);

export function initialContext(): SessionContext {
  return { phase: 'idle', activity: 'listening', sessionId: null, failure: null };
}

/** True when microphone frames may be sent. Paused and reconnecting deliberately are not. */
export function isCapturing(ctx: SessionContext): boolean {
  return ctx.phase === 'live';
}

/** True when capture, socket or wake lock are held and need releasing. */
export function holdsResources(ctx: SessionContext): boolean {
  return RESOURCE_PHASES.has(ctx.phase);
}

/** True when the screen should be kept awake — someone is mid-conversation. */
export function wantsWakeLock(ctx: SessionContext): boolean {
  return ctx.phase === 'live' || ctx.phase === 'reconnecting';
}

/**
 * Whether a frame carrying `frameSessionId` belongs to the live session. Frames from a
 * previous session must never move state, play audio, or update the transcript.
 */
export function acceptsFrameFrom(
  ctx: SessionContext,
  frameSessionId: string | null,
): boolean {
  if (ctx.sessionId === null) return false;
  return ctx.sessionId === frameSessionId;
}

export interface TransitionResult {
  context: SessionContext;
  /** False when the event was not legal here and was ignored. */
  accepted: boolean;
}

/**
 * Apply one event. `newSessionId` is only consulted for START_REQUESTED; the caller
 * supplies it so this stays free of id generation, and therefore deterministic.
 */
export function transition(
  ctx: SessionContext,
  event: SessionEvent,
  newSessionId?: string,
): TransitionResult {
  const ignore = (): TransitionResult => ({ context: ctx, accepted: false });
  const to = (phase: Phase, patch: Partial<SessionContext> = {}): TransitionResult => ({
    context: { ...ctx, phase, ...patch },
    accepted: true,
  });
  const act = (activity: Activity): TransitionResult => ({
    context: { ...ctx, activity },
    accepted: true,
  });

  // The escape hatches, accepted from anywhere.
  if (event.type === 'FATAL') {
    return to('error', { sessionId: null, failure: event.kind, activity: 'listening' });
  }
  if (event.type === 'STOP_REQUESTED') {
    // Stopping from a phase that holds nothing would strand the machine in `stopping`
    // waiting for a teardown that has nothing to tear down.
    return holdsResources(ctx)
      ? to('stopping', { activity: 'listening' })
      : to('ended', { sessionId: null, activity: 'listening' });
  }

  switch (ctx.phase) {
    case 'idle':
    case 'ended':
    case 'error':
      if (event.type === 'START_REQUESTED') {
        return to('requesting_mic', {
          sessionId: newSessionId ?? null,
          failure: null,
          activity: 'listening',
        });
      }
      return ignore();

    case 'requesting_mic':
      if (event.type === 'MIC_GRANTED') return to('connecting');
      if (event.type === 'MIC_DENIED') {
        return to('error', { sessionId: null, failure: event.kind });
      }
      return ignore();

    case 'connecting':
      if (event.type === 'SOCKET_OPEN') return to('live', { activity: 'listening' });
      if (event.type === 'SOCKET_CLOSED') {
        return event.recoverable
          ? to('reconnecting')
          : to('error', { sessionId: null, failure: 'connection' });
      }
      if (event.type === 'CREDITS_EXHAUSTED') {
        return to('stopping', { failure: 'credits' });
      }
      return ignore();

    case 'live':
      if (event.type === 'SOCKET_CLOSED') {
        return event.recoverable
          ? to('reconnecting', { activity: 'listening' })
          : to('error', { sessionId: null, failure: 'connection' });
      }
      if (event.type === 'PAUSE_REQUESTED') return to('paused', { activity: 'listening' });
      // Credits exhausted tears the pipeline down rather than just showing a banner: the
      // server has already stopped billing and translating.
      if (event.type === 'CREDITS_EXHAUSTED') {
        return to('stopping', { failure: 'credits', activity: 'listening' });
      }
      // --- activity, within a live session -------------------------------
      // Ordered so a later stage cannot be dragged backwards by a stale frame: audio
      // for an utterance keeps arriving after its translation is on screen.
      if (event.type === 'SPEECH_DETECTED') {
        return ctx.activity === 'listening' ? act('detecting') : ignore();
      }
      if (event.type === 'DIRECTION_RESOLVED') {
        return ctx.activity === 'speaking' ? ignore() : act('translating');
      }
      if (event.type === 'PLAYBACK_STARTED') return act('speaking');
      if (event.type === 'PLAYBACK_STOPPED') {
        return ctx.activity === 'speaking' ? act('listening') : ignore();
      }
      if (event.type === 'UTTERANCE_ENDED') {
        // Do not cut a translation off mid-word: the sentence is over on screen, but the
        // voice is still finishing it. PLAYBACK_STOPPED returns us to listening.
        return ctx.activity === 'speaking' ? ignore() : act('listening');
      }
      return ignore();

    case 'reconnecting':
      if (event.type === 'RECONNECT_SUCCEEDED') return to('live', { activity: 'listening' });
      if (event.type === 'RECONNECT_EXHAUSTED') {
        return to('error', { sessionId: null, failure: 'connection' });
      }
      if (event.type === 'CREDITS_EXHAUSTED') {
        return to('stopping', { failure: 'credits' });
      }
      if (event.type === 'PAUSE_REQUESTED') return to('paused');
      return ignore();

    case 'paused':
      if (event.type === 'RESUME_REQUESTED') return to('live', { activity: 'listening' });
      if (event.type === 'SOCKET_CLOSED') {
        // A socket that dies while paused is not an error the user needs to see; resume
        // reopens it. Only an unrecoverable close is worth surfacing.
        return event.recoverable
          ? ignore()
          : to('error', { sessionId: null, failure: 'connection' });
      }
      if (event.type === 'CREDITS_EXHAUSTED') {
        return to('stopping', { failure: 'credits' });
      }
      return ignore();

    case 'stopping':
      if (event.type === 'TEARDOWN_COMPLETE') {
        // A teardown that began because credits ran out ends in `error`, not `ended`:
        // the user needs the buy-credits path, not a neutral "conversation over".
        return ctx.failure === 'credits'
          ? to('error', { sessionId: null })
          : to('ended', { sessionId: null });
      }
      // Everything else during teardown is ignored — notably a late SOCKET_CLOSED, which
      // must not bounce the session back into `reconnecting`.
      return ignore();

    default: {
      const exhaustive: never = ctx.phase;
      return exhaustive;
    }
  }
}
