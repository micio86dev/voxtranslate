import { describe, expect, it } from 'vitest';
import {
  holdsResources,
  initialContext,
  isTerminal,
  transition,
  wantsWakeLock,
  type LiveContext,
  type LiveEvent,
  type Partner,
} from './session-machine';

const PARTNER: Partner = { peerId: 'b', name: 'Bea', lang: 'es', muted: false };

function run(events: LiveEvent[], from: LiveContext = initialContext()): LiveContext {
  return events.reduce((ctx, e) => transition(ctx, e).context, from);
}

const connected = (): LiveContext =>
  run([{ type: 'START_REQUESTED' }, { type: 'MIC_GRANTED' }, { type: 'SOCKET_OPEN' }]);

describe('happy path', () => {
  it('idle -> requesting_mic -> connecting -> waiting_peer', () => {
    let ctx = initialContext();
    expect(ctx.phase).toBe('idle');
    ctx = run([{ type: 'START_REQUESTED' }], ctx);
    expect(ctx.phase).toBe('requesting_mic');
    ctx = run([{ type: 'MIC_GRANTED' }], ctx);
    expect(ctx.phase).toBe('connecting');
    ctx = run([{ type: 'SOCKET_OPEN' }], ctx);
    expect(ctx.phase).toBe('waiting_peer');
    expect(ctx.partner).toBeNull();
  });

  it('room_joined seeds a partner who was already there: straight to live', () => {
    const ctx = run([{ type: 'ROOM_JOINED', partner: PARTNER }], connected());
    expect(ctx.phase).toBe('live');
    expect(ctx.partner).toEqual(PARTNER);
  });

  it('room_joined with nobody else keeps waiting', () => {
    const ctx = run([{ type: 'ROOM_JOINED', partner: null }], connected());
    expect(ctx.phase).toBe('waiting_peer');
  });

  it('peer_joined -> live, peer_left -> back to waiting_peer', () => {
    let ctx = run([{ type: 'PEER_JOINED', partner: PARTNER }], connected());
    expect(ctx.phase).toBe('live');
    expect(ctx.partner?.name).toBe('Bea');
    ctx = run([{ type: 'PEER_LEFT', peerId: 'b' }], ctx);
    expect(ctx.phase).toBe('waiting_peer');
    expect(ctx.partner).toBeNull();
  });

  it('ignores a peer_left for somebody who is not the partner', () => {
    const live = run([{ type: 'PEER_JOINED', partner: PARTNER }], connected());
    const r = transition(live, { type: 'PEER_LEFT', peerId: 'zzz' });
    expect(r.accepted).toBe(false);
    expect(r.context).toBe(live);
  });

  it('tracks the partner mute flag', () => {
    const live = run([{ type: 'PEER_JOINED', partner: PARTNER }], connected());
    const muted = run([{ type: 'PEER_MUTED', peerId: 'b', muted: true }], live);
    expect(muted.partner?.muted).toBe(true);
    expect(muted.phase).toBe('live');
    // A mute for someone else changes nothing.
    expect(transition(live, { type: 'PEER_MUTED', peerId: 'q', muted: true }).accepted).toBe(false);
  });
});

describe('failures', () => {
  it.each(['mic_denied', 'mic_blocked', 'mic_missing', 'unknown'] as const)(
    'mic %s fails the session with that kind',
    (kind) => {
      const ctx = run([{ type: 'START_REQUESTED' }, { type: 'MIC_DENIED', kind }]);
      expect(ctx.phase).toBe('failed');
      expect(ctx.failure).toBe(kind);
    },
  );

  it('FATAL is accepted from anywhere (credits, busy, unsupported)', () => {
    for (const kind of ['credits', 'busy', 'unsupported', 'network'] as const) {
      const ctx = run([{ type: 'FATAL', kind }], connected());
      expect(ctx.phase).toBe('failed');
      expect(ctx.failure).toBe(kind);
    }
  });

  it('a dropped socket reconnects, and success returns to the right phase', () => {
    const live = run([{ type: 'PEER_JOINED', partner: PARTNER }], connected());
    const re = run([{ type: 'SOCKET_CLOSED' }], live);
    expect(re.phase).toBe('reconnecting');
    expect(re.partner).toEqual(PARTNER);
    expect(run([{ type: 'RECONNECT_SUCCEEDED' }], re).phase).toBe('live');
    const alone = run([{ type: 'SOCKET_CLOSED' }, { type: 'RECONNECT_SUCCEEDED' }], connected());
    expect(alone.phase).toBe('waiting_peer');
  });

  it('a socket that never opened also goes to reconnecting', () => {
    const ctx = run([{ type: 'START_REQUESTED' }, { type: 'MIC_GRANTED' }, { type: 'SOCKET_CLOSED' }]);
    expect(ctx.phase).toBe('reconnecting');
  });

  it('exhausted reconnects fail with kind network', () => {
    const ctx = run([{ type: 'SOCKET_CLOSED' }, { type: 'RECONNECT_EXHAUSTED' }], connected());
    expect(ctx.phase).toBe('failed');
    expect(ctx.failure).toBe('network');
  });
});

describe('ending', () => {
  it('session_state carries the reason through to ended', () => {
    const ctx = run([{ type: 'SESSION_ENDED', reason: 'ended_by_host' }], connected());
    expect(ctx.phase).toBe('ended');
    expect(ctx.endReason).toBe('ended_by_host');
    expect(ctx.partner).toBeNull();
  });

  it('a local stop ends with ended_by_me, from any phase including idle', () => {
    expect(run([{ type: 'STOP_REQUESTED' }], connected()).endReason).toBe('ended_by_me');
    expect(run([{ type: 'STOP_REQUESTED' }]).phase).toBe('ended');
  });

  it('terminal phases ignore everything except START_REQUESTED', () => {
    const ended = run([{ type: 'SESSION_ENDED', reason: 'expired' }], connected());
    for (const e of [
      { type: 'SOCKET_CLOSED' },
      { type: 'PEER_JOINED', partner: PARTNER },
      { type: 'FATAL', kind: 'unknown' },
      { type: 'SESSION_ENDED', reason: 'abandoned' },
    ] as LiveEvent[]) {
      const r = transition(ended, e);
      expect(r.accepted).toBe(false);
      expect(r.context).toBe(ended);
    }
    expect(transition(ended, { type: 'START_REQUESTED' }).context.phase).toBe('requesting_mic');
    const failed = run([{ type: 'FATAL', kind: 'busy' }], connected());
    expect(transition(failed, { type: 'START_REQUESTED' }).context.failure).toBeNull();
  });

  it('events out of order are ignored, not thrown (MIC_GRANTED while live)', () => {
    const live = run([{ type: 'PEER_JOINED', partner: PARTNER }], connected());
    expect(transition(live, { type: 'MIC_GRANTED' }).accepted).toBe(false);
    expect(transition(initialContext(), { type: 'SOCKET_OPEN' }).accepted).toBe(false);
  });
});

describe('predicates', () => {
  it('isTerminal / holdsResources / wantsWakeLock follow the phase', () => {
    const table: Array<[LiveContext['phase'], boolean, boolean, boolean]> = [
      ['idle', false, false, false],
      ['requesting_mic', false, true, false],
      ['connecting', false, true, false],
      ['waiting_peer', false, true, true],
      ['live', false, true, true],
      ['reconnecting', false, true, true],
      ['ended', true, false, false],
      ['failed', true, false, false],
    ];
    for (const [phase, terminal, holds, wake] of table) {
      const ctx = { ...initialContext(), phase };
      expect(isTerminal(ctx), phase).toBe(terminal);
      expect(holdsResources(ctx), phase).toBe(holds);
      expect(wantsWakeLock(ctx), phase).toBe(wake);
    }
  });
});
