import { describe, expect, it } from 'vitest';

import {
  CHAT_MAX_HEIGHT,
  CHAT_MAX_LEN,
  counterLabel,
  counterState,
  insertAt,
  resizeBox,
} from './chat-input';

describe('counterState', () => {
  it('is hidden well below the cap', () => {
    expect(counterState(0)).toBe('hidden');
    expect(counterState(100)).toBe('hidden'); // 20% of 500
    expect(counterState(399)).toBe('hidden'); // just under 80%
  });

  it('is shown (normal) once near the cap', () => {
    expect(counterState(400)).toBe('normal'); // exactly 80%
    expect(counterState(450)).toBe('normal'); // 90%
    expect(counterState(474)).toBe('normal'); // just under 95%
  });

  it('warns at/above 95% and at the cap', () => {
    expect(counterState(475)).toBe('warn'); // 95%
    expect(counterState(500)).toBe('warn'); // at the cap
  });

  it('never divides by zero', () => {
    expect(counterState(5, 0)).toBe('hidden');
  });
});

describe('counterLabel', () => {
  it('renders used/max', () => {
    expect(counterLabel(0)).toBe(`0/${CHAT_MAX_LEN}`);
    expect(counterLabel(480)).toBe('480/500');
    expect(counterLabel(3, 10)).toBe('3/10');
  });
});

describe('resizeBox', () => {
  it('grows to content height while under the cap, no scroll', () => {
    expect(resizeBox(44)).toEqual({ height: 44, overflowY: 'hidden' });
    expect(resizeBox(90)).toEqual({ height: 90, overflowY: 'hidden' });
    expect(resizeBox(CHAT_MAX_HEIGHT)).toEqual({ height: CHAT_MAX_HEIGHT, overflowY: 'hidden' });
  });

  it('caps at the max height and scrolls past it', () => {
    expect(resizeBox(CHAT_MAX_HEIGHT + 1)).toEqual({ height: CHAT_MAX_HEIGHT, overflowY: 'auto' });
    expect(resizeBox(400)).toEqual({ height: CHAT_MAX_HEIGHT, overflowY: 'auto' });
  });

  it('honours an explicit max', () => {
    expect(resizeBox(60, 50)).toEqual({ height: 50, overflowY: 'auto' });
    expect(resizeBox(40, 50)).toEqual({ height: 40, overflowY: 'hidden' });
  });
});

describe('insertAt', () => {
  it('inserts at the caret and reports the new caret position', () => {
    expect(insertAt('ab', '😀', 1, 1)).toEqual({ value: 'a😀b', caret: 1 + '😀'.length });
  });

  it('replaces the current selection', () => {
    expect(insertAt('abcd', 'X', 1, 3)).toEqual({ value: 'aXd', caret: 2 });
  });

  it('appends at the end', () => {
    const r = insertAt('hi', '🎉', 2, 2);
    expect(r?.value).toBe('hi🎉');
  });

  it('refuses an insert that would exceed the cap', () => {
    const full = 'x'.repeat(CHAT_MAX_LEN);
    expect(insertAt(full, '😀', full.length, full.length)).toBeNull();
  });

  it('allows an insert that lands exactly on the cap', () => {
    const almost = 'x'.repeat(CHAT_MAX_LEN - 1);
    expect(insertAt(almost, 'y', almost.length, almost.length)).toEqual({
      value: almost + 'y',
      caret: CHAT_MAX_LEN,
    });
  });

  it('counts replaced selection toward the budget', () => {
    // 10-char value, cap 10, replace 2 chars with 2 chars → still fits.
    const v = '0123456789';
    expect(insertAt(v, 'ab', 0, 2, 10)).toEqual({ value: 'ab23456789', caret: 2 });
    // replace 1 char with 3 → would be 12 > 10 → null.
    expect(insertAt(v, 'abc', 0, 1, 10)).toBeNull();
  });
});
