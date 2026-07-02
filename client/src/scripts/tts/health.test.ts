import { describe, expect, it } from 'vitest';

import { HealthMonitor } from './health';

describe('HealthMonitor', () => {
  it('stays healthy while startup is under the threshold', () => {
    const h = new HealthMonitor(100, 3);
    for (let i = 0; i < 10; i++) expect(h.record(50)).toBe(false);
    expect(h.isDegraded()).toBe(false);
  });

  it('degrades after MAX_STRIKES consecutive slow utterances, signalling the transition once', () => {
    const h = new HealthMonitor(100, 3);
    expect(h.record(200)).toBe(false); // strike 1
    expect(h.record(200)).toBe(false); // strike 2
    expect(h.record(200)).toBe(true); //  strike 3 → degrade (transition)
    expect(h.isDegraded()).toBe(true);
    expect(h.record(200)).toBe(false); // already degraded → no repeat signal
  });

  it('a healthy utterance clears the streak (only a SUSTAINED slowdown degrades)', () => {
    const h = new HealthMonitor(100, 3);
    h.record(200);
    h.record(200);
    h.record(50); // resets the streak
    h.record(200);
    h.record(200);
    expect(h.isDegraded()).toBe(false); // never hit 3 in a row
  });

  it('forceDegrade transitions once, then reset retries Vox', () => {
    const h = new HealthMonitor(100, 3);
    expect(h.forceDegrade()).toBe(true);
    expect(h.forceDegrade()).toBe(false);
    expect(h.isDegraded()).toBe(true);
    h.reset();
    expect(h.isDegraded()).toBe(false);
    expect(h.record(50)).toBe(false);
  });
});
