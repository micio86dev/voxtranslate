// @vitest-environment jsdom
// CallTimer (spec 0052): the DOM/countdown side of the voice-command timer. The
// intent parser is pure and covered in timer-intent tests; here we drive the badge,
// the 250 ms tick, the done-flash + auto-hide hold, and cancel/reset teardown with
// fake timers (vitest fake timers also mock Date, so remainingSeconds stays in step).
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { CallTimer, formatClock, parseTimerCommand, type TimerCommand } from './timer';

interface Harness {
  timer: CallTimer;
  badge: HTMLElement;
  remaining: HTMLElement;
  cancelBtn: HTMLElement;
  onSet: ReturnType<typeof vi.fn>;
  onDone: ReturnType<typeof vi.fn>;
  onCancel: ReturnType<typeof vi.fn>;
}

function makeTimer(): Harness {
  const badge = document.createElement('div');
  badge.className = 'hidden';
  const remaining = document.createElement('span');
  const cancelBtn = document.createElement('button');
  document.body.append(badge, remaining, cancelBtn);
  const onSet = vi.fn();
  const onDone = vi.fn();
  const onCancel = vi.fn();
  const timer = new CallTimer({
    badge,
    remaining,
    cancelBtn,
    t: (k) => k,
    onSet,
    onDone,
    onCancel,
  });
  return { timer, badge, remaining, cancelBtn, onSet, onDone, onCancel };
}

beforeEach(() => {
  document.body.innerHTML = '';
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe('re-exports from timer-intent', () => {
  it('exposes the pure helpers for callers that import from timer', () => {
    expect(formatClock(300)).toBe('05:00');
    expect(formatClock(3661)).toBe('1:01:01');
    expect(parseTimerCommand('set a timer for 5 minutes')).toEqual({
      seconds: 300,
      isBreak: false,
    });
  });
});

describe('CallTimer', () => {
  it('ignores ordinary speech (no timer starts)', () => {
    const h = makeTimer();
    expect(h.timer.handleTranscript('hello everyone, welcome to the call')).toBe(false);
    expect(h.timer.active).toBe(false);
    expect(h.badge.classList.contains('hidden')).toBe(true);
    expect(h.onSet).not.toHaveBeenCalled();
  });

  it('starts a countdown from a spoken command and shows the badge', () => {
    const h = makeTimer();
    expect(h.timer.handleTranscript('set a timer for 5 minutes')).toBe(true);
    expect(h.timer.active).toBe(true);
    expect(h.badge.classList.contains('hidden')).toBe(false);
    expect(h.remaining.textContent).toBe('05:00');
    expect(h.onSet).toHaveBeenCalledWith({ seconds: 300, isBreak: false });
  });

  it('keeps the label in step with the wall clock as it ticks', () => {
    const h = makeTimer();
    h.timer.start({ seconds: 300, isBreak: false });
    vi.advanceTimersByTime(61_000);
    expect(h.remaining.textContent).toBe(formatClock(239));
    vi.advanceTimersByTime(120_000);
    expect(h.remaining.textContent).toBe(formatClock(119));
  });

  it('fires onDone at zero, flashes the badge, then auto-hides after the hold', () => {
    const h = makeTimer();
    const cmd: TimerCommand = { seconds: 2, isBreak: true };
    h.timer.start(cmd);
    vi.advanceTimersByTime(2_250);
    expect(h.onDone).toHaveBeenCalledWith(cmd);
    expect(h.remaining.textContent).toBe('00:00');
    expect(h.badge.classList.contains('timer-badge-done')).toBe(true);
    expect(h.badge.classList.contains('hidden')).toBe(false);
    expect(h.timer.active).toBe(false); // done hold, not a running countdown
    vi.advanceTimersByTime(6_000);
    expect(h.badge.classList.contains('hidden')).toBe(true);
    expect(h.badge.classList.contains('timer-badge-done')).toBe(false);
  });

  it('cancel hides the badge and reports once; a second cancel is a no-op', () => {
    const h = makeTimer();
    h.timer.start({ seconds: 60, isBreak: false });
    h.timer.cancel();
    expect(h.badge.classList.contains('hidden')).toBe(true);
    expect(h.timer.active).toBe(false);
    expect(h.onCancel).toHaveBeenCalledTimes(1);
    h.timer.cancel(); // past the running state → no-op
    expect(h.onCancel).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(120_000);
    expect(h.onDone).not.toHaveBeenCalled(); // interval really cleared
  });

  it('the × button cancels a running timer', () => {
    const h = makeTimer();
    h.timer.start({ seconds: 60, isBreak: false });
    h.cancelBtn.dispatchEvent(new MouseEvent('click'));
    expect(h.timer.active).toBe(false);
    expect(h.onCancel).toHaveBeenCalledTimes(1);
  });

  it('restarting replaces the previous countdown (no double ticks)', () => {
    const h = makeTimer();
    h.timer.start({ seconds: 60, isBreak: false });
    vi.advanceTimersByTime(500);
    h.timer.start({ seconds: 120, isBreak: false });
    expect(h.remaining.textContent).toBe('02:00');
    vi.advanceTimersByTime(120_250);
    expect(h.onDone).toHaveBeenCalledTimes(1); // only the second timer fired
  });

  it('starting during the done hold clears the pending auto-hide', () => {
    const h = makeTimer();
    h.timer.start({ seconds: 1, isBreak: false });
    vi.advanceTimersByTime(1_250); // fire → hold begins
    expect(h.badge.classList.contains('timer-badge-done')).toBe(true);
    h.timer.start({ seconds: 60, isBreak: false });
    expect(h.badge.classList.contains('timer-badge-done')).toBe(false);
    vi.advanceTimersByTime(6_000); // the old hide timeout must NOT fire
    expect(h.badge.classList.contains('hidden')).toBe(false);
    expect(h.timer.active).toBe(true);
  });

  it('reset tears everything down without firing callbacks', () => {
    const h = makeTimer();
    h.timer.start({ seconds: 60, isBreak: false });
    h.timer.reset();
    expect(h.badge.classList.contains('hidden')).toBe(true);
    expect(h.badge.classList.contains('timer-badge-done')).toBe(false);
    expect(h.timer.active).toBe(false);
    expect(h.onCancel).not.toHaveBeenCalled();
    vi.advanceTimersByTime(120_000);
    expect(h.onDone).not.toHaveBeenCalled();
  });
});
