import { describe, it, expect } from 'vitest';
import {
  initialContext,
  transition,
  isCapturing,
  holdsResources,
  wantsWakeLock,
  acceptsFrameFrom,
  type SessionContext,
  type SessionEvent,
} from './session-machine';

/** Drive a context through a list of events, ignoring acceptance. */
function run(ctx: SessionContext, ...events: SessionEvent[]): SessionContext {
  return events.reduce((c, e) => transition(c, e, 's1').context, ctx);
}

/** A context parked in a live session with a known id. */
function live(): SessionContext {
  return run(
    initialContext(),
    { type: 'START_REQUESTED' },
    { type: 'MIC_GRANTED' },
    { type: 'SOCKET_OPEN' },
  );
}

describe('phase lifecycle', () => {
  it('walks idle → live and back to ended', () => {
    let ctx = initialContext();
    expect(ctx.phase).toBe('idle');

    ctx = transition(ctx, { type: 'START_REQUESTED' }, 'abc').context;
    expect(ctx.phase).toBe('requesting_mic');
    expect(ctx.sessionId).toBe('abc');

    ctx = run(ctx, { type: 'MIC_GRANTED' }, { type: 'SOCKET_OPEN' });
    expect(ctx.phase).toBe('live');

    ctx = run(ctx, { type: 'STOP_REQUESTED' }, { type: 'TEARDOWN_COMPLETE' });
    expect(ctx.phase).toBe('ended');
    expect(ctx.sessionId).toBeNull();
  });

  it('refuses to start a second session over a live one', () => {
    const ctx = live();
    // The guard against two microphones, two sockets and two meters for one tap.
    const r = transition(ctx, { type: 'START_REQUESTED' }, 'other');
    expect(r.accepted).toBe(false);
    expect(r.context.sessionId).toBe('s1');
  });

  it('can start again after ending or failing', () => {
    for (const start of [
      run(live(), { type: 'STOP_REQUESTED' }, { type: 'TEARDOWN_COMPLETE' }),
      run(initialContext(), { type: 'START_REQUESTED' }, { type: 'MIC_DENIED', kind: 'mic_denied' }),
    ]) {
      const r = transition(start, { type: 'START_REQUESTED' }, 'again');
      expect(r.accepted).toBe(true);
      expect(r.context.phase).toBe('requesting_mic');
      // A retry must not inherit the previous failure, or the UI keeps the error copy.
      expect(r.context.failure).toBeNull();
    }
  });

  it('surfaces each microphone failure kind distinctly', () => {
    // The permission copy differs per kind (denied vs permanently blocked vs no device),
    // so the machine must carry the distinction rather than a single boolean.
    for (const kind of ['mic_denied', 'mic_blocked', 'mic_missing', 'unsupported'] as const) {
      const ctx = run(initialContext(), { type: 'START_REQUESTED' }, { type: 'MIC_DENIED', kind });
      expect(ctx.phase).toBe('error');
      expect(ctx.failure).toBe(kind);
      expect(ctx.sessionId).toBeNull();
    }
  });

  it('reconnects on a recoverable close and gives up on an unrecoverable one', () => {
    let ctx = run(live(), { type: 'SOCKET_CLOSED', recoverable: true });
    expect(ctx.phase).toBe('reconnecting');
    ctx = run(ctx, { type: 'RECONNECT_SUCCEEDED' });
    expect(ctx.phase).toBe('live');

    ctx = run(live(), { type: 'SOCKET_CLOSED', recoverable: false });
    expect(ctx.phase).toBe('error');
    expect(ctx.failure).toBe('connection');

    ctx = run(live(), { type: 'SOCKET_CLOSED', recoverable: true }, { type: 'RECONNECT_EXHAUSTED' });
    expect(ctx.phase).toBe('error');
  });

  it('ignores a late socket close during teardown', () => {
    // The close that arrives BECAUSE we are tearing down must not bounce the session
    // into reconnecting and reopen everything we just released.
    const ctx = run(live(), { type: 'STOP_REQUESTED' });
    expect(ctx.phase).toBe('stopping');
    const r = transition(ctx, { type: 'SOCKET_CLOSED', recoverable: true });
    expect(r.accepted).toBe(false);
    expect(r.context.phase).toBe('stopping');
  });

  it('stops from a phase that holds nothing without waiting for a teardown', () => {
    // Otherwise an End tap before anything started strands the machine in `stopping`
    // forever, waiting on a TEARDOWN_COMPLETE nobody will send.
    const r = transition(initialContext(), { type: 'STOP_REQUESTED' });
    expect(r.context.phase).toBe('ended');
  });

  it('pauses and resumes without losing the session', () => {
    let ctx = run(live(), { type: 'PAUSE_REQUESTED' });
    expect(ctx.phase).toBe('paused');
    expect(isCapturing(ctx)).toBe(false);
    // Still holding the mic and the socket — pause is not a teardown.
    expect(holdsResources(ctx)).toBe(true);
    expect(ctx.sessionId).toBe('s1');

    ctx = run(ctx, { type: 'RESUME_REQUESTED' });
    expect(ctx.phase).toBe('live');
    expect(isCapturing(ctx)).toBe(true);
  });

  it('does not raise a recoverable socket close while paused', () => {
    // Nothing is flowing; resume reopens the socket. Showing an error here would be
    // alarming and wrong.
    const paused = run(live(), { type: 'PAUSE_REQUESTED' });
    expect(transition(paused, { type: 'SOCKET_CLOSED', recoverable: true }).accepted).toBe(false);
    expect(
      transition(paused, { type: 'SOCKET_CLOSED', recoverable: false }).context.phase,
    ).toBe('error');
  });

  it('ends a credits-exhausted session in error, not in ended', () => {
    // The user needs the buy-credits path, not a neutral "conversation over".
    const ctx = run(live(), { type: 'CREDITS_EXHAUSTED' }, { type: 'TEARDOWN_COMPLETE' });
    expect(ctx.phase).toBe('error');
    expect(ctx.failure).toBe('credits');
  });

  it('accepts FATAL from anywhere', () => {
    for (const ctx of [initialContext(), live(), run(live(), { type: 'PAUSE_REQUESTED' })]) {
      const r = transition(ctx, { type: 'FATAL', kind: 'provider' });
      expect(r.accepted).toBe(true);
      expect(r.context.phase).toBe('error');
      expect(r.context.sessionId).toBeNull();
    }
  });
});

describe('activity within a live session', () => {
  it('walks listening → detecting → translating → speaking → listening', () => {
    let ctx = live();
    expect(ctx.activity).toBe('listening');
    ctx = run(ctx, { type: 'SPEECH_DETECTED' });
    expect(ctx.activity).toBe('detecting');
    ctx = run(ctx, { type: 'DIRECTION_RESOLVED' });
    expect(ctx.activity).toBe('translating');
    ctx = run(ctx, { type: 'PLAYBACK_STARTED' });
    expect(ctx.activity).toBe('speaking');
    ctx = run(ctx, { type: 'PLAYBACK_STOPPED' });
    expect(ctx.activity).toBe('listening');
  });

  it('is not dragged backwards by a late frame', () => {
    // Audio keeps arriving after the translation is on screen; a stale DIRECTION_RESOLVED
    // must not pull a speaking utterance back to "Translating…".
    const speaking = run(
      live(),
      { type: 'SPEECH_DETECTED' },
      { type: 'DIRECTION_RESOLVED' },
      { type: 'PLAYBACK_STARTED' },
    );
    expect(transition(speaking, { type: 'DIRECTION_RESOLVED' }).accepted).toBe(false);
    expect(transition(speaking, { type: 'SPEECH_DETECTED' }).accepted).toBe(false);
  });

  it('lets the voice finish the sentence after the subtitle is final', () => {
    // UTTERANCE_ENDED means the text is done, not the speech. Cutting to "Listening…"
    // here would contradict a voice the user can still hear.
    const speaking = run(
      live(),
      { type: 'SPEECH_DETECTED' },
      { type: 'DIRECTION_RESOLVED' },
      { type: 'PLAYBACK_STARTED' },
    );
    expect(transition(speaking, { type: 'UTTERANCE_ENDED' }).accepted).toBe(false);
    expect(run(speaking, { type: 'PLAYBACK_STOPPED' }).activity).toBe('listening');

    // With no audio playing, the final does return to listening.
    const detecting = run(live(), { type: 'SPEECH_DETECTED' });
    expect(run(detecting, { type: 'UTTERANCE_ENDED' }).activity).toBe('listening');
  });

  it('resets activity when the session leaves live', () => {
    // A paused or reconnecting session showing "Speaking…" is a lie.
    const speaking = run(
      live(),
      { type: 'SPEECH_DETECTED' },
      { type: 'DIRECTION_RESOLVED' },
      { type: 'PLAYBACK_STARTED' },
    );
    expect(run(speaking, { type: 'PAUSE_REQUESTED' }).activity).toBe('listening');
    expect(run(speaking, { type: 'SOCKET_CLOSED', recoverable: true }).activity).toBe('listening');
    expect(run(speaking, { type: 'STOP_REQUESTED' }).activity).toBe('listening');
  });

  it('ignores activity events outside a live session', () => {
    for (const ctx of [initialContext(), run(live(), { type: 'PAUSE_REQUESTED' })]) {
      expect(transition(ctx, { type: 'SPEECH_DETECTED' }).accepted).toBe(false);
      expect(transition(ctx, { type: 'PLAYBACK_STARTED' }).accepted).toBe(false);
    }
  });
});

describe('resource and frame guards', () => {
  it('holds resources exactly while something is acquired', () => {
    expect(holdsResources(initialContext())).toBe(false);
    expect(holdsResources(live())).toBe(true);
    expect(holdsResources(run(live(), { type: 'STOP_REQUESTED' }))).toBe(true);
    expect(
      holdsResources(run(live(), { type: 'STOP_REQUESTED' }, { type: 'TEARDOWN_COMPLETE' })),
    ).toBe(false);
  });

  it('keeps the screen awake only mid-conversation', () => {
    // Holding a wake lock while paused would burn a traveller's battery for nothing.
    expect(wantsWakeLock(live())).toBe(true);
    expect(wantsWakeLock(run(live(), { type: 'SOCKET_CLOSED', recoverable: true }))).toBe(true);
    expect(wantsWakeLock(run(live(), { type: 'PAUSE_REQUESTED' }))).toBe(false);
    expect(wantsWakeLock(initialContext())).toBe(false);
  });

  it('rejects frames from a dead or different session', () => {
    const ctx = live();
    expect(acceptsFrameFrom(ctx, 's1')).toBe(true);
    expect(acceptsFrameFrom(ctx, 's2')).toBe(false);
    expect(acceptsFrameFrom(ctx, null)).toBe(false);
    // With no session at all nothing is accepted — not even a null-for-null match, which
    // would let frames from before the first start through.
    expect(acceptsFrameFrom(initialContext(), null)).toBe(false);
  });
});
