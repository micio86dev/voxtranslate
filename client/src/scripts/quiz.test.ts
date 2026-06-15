import { describe, expect, it } from 'vitest';

import { isQuizActive, type QuizState } from './quiz';

const state = (phase: QuizState['phase']): QuizState => ({
  game: 'quiz',
  t: 'state',
  phase,
  hostId: 'h',
  players: {},
  qIndex: 0,
  total: 8,
  packIndex: 0,
  answeredIds: [],
});

describe('isQuizActive', () => {
  it('is false when there is no quiz', () => {
    expect(isQuizActive(null)).toBe(false);
  });

  it('is true while a quiz is live (question / reveal)', () => {
    expect(isQuizActive(state('question'))).toBe(true);
    expect(isQuizActive(state('reveal'))).toBe(true);
  });

  it('is false once finished or cancelled (so a new quiz can start)', () => {
    expect(isQuizActive(state('done'))).toBe(false);
    expect(isQuizActive(state('cancelled'))).toBe(false);
  });
});
