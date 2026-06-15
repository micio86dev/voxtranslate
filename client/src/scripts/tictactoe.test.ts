import { describe, expect, it } from 'vitest';

import { isFull, isStaleSeq, nextSeats, winningLine, type GameState } from './tictactoe';

describe('winningLine', () => {
  it('detects rows, columns and diagonals', () => {
    expect(winningLine([1, 1, 1, 0, 0, 0, 0, 0, 0])).toEqual([0, 1, 2]); // top row
    expect(winningLine([2, 0, 0, 2, 0, 0, 2, 0, 0])).toEqual([0, 3, 6]); // left col
    expect(winningLine([1, 0, 0, 0, 1, 0, 0, 0, 1])).toEqual([0, 4, 8]); // diagonal
  });
  it('returns undefined with no line', () => {
    expect(winningLine([1, 2, 1, 0, 0, 0, 0, 0, 0])).toBeUndefined();
    expect(winningLine([0, 0, 0, 0, 0, 0, 0, 0, 0])).toBeUndefined();
  });
});

describe('isFull', () => {
  it('is true only when every cell is taken', () => {
    expect(isFull([1, 2, 1, 2, 1, 2, 1, 2, 1])).toBe(true);
    expect(isFull([1, 2, 1, 2, 0, 2, 1, 2, 1])).toBe(false);
  });
});

describe('isStaleSeq', () => {
  it('drops a seq not newer than the highest seen', () => {
    expect(isStaleSeq(5, 5)).toBe(true);
    expect(isStaleSeq(3, 5)).toBe(true);
  });
  it('accepts a strictly newer seq', () => {
    expect(isStaleSeq(6, 5)).toBe(false);
  });
  it('accepts a state with no seq (older client)', () => {
    expect(isStaleSeq(undefined, 5)).toBe(false);
  });
});

const won = (xId: string, oId: string, winner: 1 | 2): GameState => ({
  board: [], turn: 1, status: 'won', winner, xId, xName: xId, oId, oName: oId, seq: 1,
});
const draw = (xId: string, oId: string): GameState => ({
  board: [], turn: 1, status: 'draw', xId, xName: xId, oId, oName: oId, seq: 1,
});

describe('nextSeats', () => {
  it('seats the starter alone when no one else is present (→ waiting)', () => {
    expect(nextSeats(['a'], null, 'a')).toEqual({ xId: 'a', oId: undefined });
  });

  it('fresh start: starter is X, the next participant is O (immediately playable)', () => {
    expect(nextSeats(['a', 'b'], null, 'a')).toEqual({ xId: 'a', oId: 'b' });
    expect(nextSeats(['a', 'b', 'c'], null, 'a')).toEqual({ xId: 'a', oId: 'b' });
    expect(nextSeats(['a', 'b', 'c'], null, 'b')).toEqual({ xId: 'b', oId: 'c' });
    // a spectator starting wraps around to the next participant after itself
    expect(nextSeats(['a', 'b', 'c'], null, 'c')).toEqual({ xId: 'c', oId: 'a' });
  });

  it('two players: a finished game rematches (no spectators to rotate in)', () => {
    expect(nextSeats(['a', 'b'], won('a', 'b', 1), 'a')).toEqual({ xId: 'a', oId: 'b' });
    expect(nextSeats(['a', 'b'], won('a', 'b', 2), 'b')).toEqual({ xId: 'b', oId: 'a' });
  });

  it('three players: winner stays, the loser sits out and the spectator rotates in', () => {
    // a beat b → a stays, c (the spectator) comes in, b waits.
    expect(nextSeats(['a', 'b', 'c'], won('a', 'b', 1), 'a')).toEqual({ xId: 'a', oId: 'c' });
    // o (b) won → b stays, the next after loser a is b(skip)→c.
    expect(nextSeats(['a', 'b', 'c'], won('a', 'b', 2), 'b')).toEqual({ xId: 'b', oId: 'c' });
  });

  it('draw keeps X and rotates the next participant after O in', () => {
    expect(nextSeats(['a', 'b', 'c'], draw('a', 'b'), 'a')).toEqual({ xId: 'a', oId: 'c' });
  });

  it('four players: winner stays, the participant after the loser rotates in', () => {
    // X=a beat O=b → a stays, the participant after loser b (index 1) is c → c in, b & d wait.
    expect(nextSeats(['a', 'b', 'c', 'd'], won('a', 'b', 1), 'a')).toEqual({ xId: 'a', oId: 'c' });
    // O=c beat X=a → c stays, the participant after loser a (index 0) is b → b in, a & d wait.
    expect(nextSeats(['a', 'b', 'c', 'd'], won('a', 'c', 2), 'c')).toEqual({ xId: 'c', oId: 'b' });
  });
});
