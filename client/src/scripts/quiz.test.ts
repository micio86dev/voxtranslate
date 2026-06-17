import { describe, expect, it } from 'vitest';

import { isQuizActive, quizCreationFormState, type QuizState } from './quiz';

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

describe('quizCreationFormState (#220 — gate creation while a quiz is in progress)', () => {
  it('hides + disables the creation form and shows the in-progress notice while live', () => {
    for (const phase of ['question', 'reveal'] as const) {
      expect(quizCreationFormState(state(phase))).toEqual({
        formHidden: true,
        controlsDisabled: true,
        busyShown: true,
      });
    }
  });

  it('restores the creation form (no notice) when there is no quiz', () => {
    expect(quizCreationFormState(null)).toEqual({
      formHidden: false,
      controlsDisabled: false,
      busyShown: false,
    });
  });

  it('restores the creation form once the quiz is completed or cancelled', () => {
    for (const phase of ['done', 'cancelled'] as const) {
      expect(quizCreationFormState(state(phase))).toEqual({
        formHidden: false,
        controlsDisabled: false,
        busyShown: false,
      });
    }
  });
});
