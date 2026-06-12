import { describe, expect, it } from 'vitest';

import { RateLimiter } from './reaction-rate-limit';

/** A controllable clock so the sliding window is deterministic. */
function fakeClock(start = 0) {
  let t = start;
  return {
    now: () => t,
    advance: (ms: number) => {
      t += ms;
    },
  };
}

describe('RateLimiter', () => {
  it('allows up to `max` attempts within the window, then drops', () => {
    const clock = fakeClock();
    const rl = new RateLimiter(3, 1000, clock.now);
    expect(rl.tryAcquire()).toBe(true);
    expect(rl.tryAcquire()).toBe(true);
    expect(rl.tryAcquire()).toBe(true);
    expect(rl.tryAcquire()).toBe(false); // 4th in the same window → dropped
    expect(rl.tryAcquire()).toBe(false);
  });

  it('refills as old hits age out of the sliding window', () => {
    const clock = fakeClock();
    const rl = new RateLimiter(2, 1000, clock.now);
    expect(rl.tryAcquire()).toBe(true); // t=0
    expect(rl.tryAcquire()).toBe(true); // t=0
    expect(rl.tryAcquire()).toBe(false); // window full
    clock.advance(1001); // both hits now older than the window
    expect(rl.tryAcquire()).toBe(true);
    expect(rl.tryAcquire()).toBe(true);
    expect(rl.tryAcquire()).toBe(false);
  });

  it('frees exactly one slot when only the oldest hit ages out', () => {
    const clock = fakeClock();
    const rl = new RateLimiter(2, 1000, clock.now);
    rl.tryAcquire(); // t=0
    clock.advance(600);
    rl.tryAcquire(); // t=600 → window full (two hits)
    expect(rl.tryAcquire()).toBe(false);
    clock.advance(500); // t=1100: t=0 hit aged out, t=600 still in window
    expect(rl.tryAcquire()).toBe(true); // one slot freed
    expect(rl.tryAcquire()).toBe(false); // full again
  });

  it('boundary hit on the window edge is still counted (strictly-greater cutoff)', () => {
    const clock = fakeClock();
    const rl = new RateLimiter(1, 1000, clock.now);
    rl.tryAcquire(); // t=0
    clock.advance(1000); // exactly windowMs later — cutoff = 0, hit at 0 is NOT > 0
    expect(rl.tryAcquire()).toBe(true); // the t=0 hit has aged out
  });

  it('defaults to Date.now() when no clock is injected', () => {
    const rl = new RateLimiter(1, 100_000);
    expect(rl.tryAcquire()).toBe(true);
    expect(rl.tryAcquire()).toBe(false); // window far from elapsing
  });
});
