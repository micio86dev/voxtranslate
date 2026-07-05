// @vitest-environment jsdom
//
// Pure helpers (isQuizActive / quizCreationFormState) plus the full Quiz class:
// the host-authoritative flow (start → answer → reveal → next → done), the inline
// AI pack (spec 0067), remote application for peers, cancel (spec 0070 R4.3) and
// the DOM rendering — under jsdom with the relay `send` and callbacks mocked.
import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  Quiz,
  isQuizActive,
  quizCreationFormState,
  type AiQuestion,
  type QuizState,
  type QuizSummary,
} from './quiz';

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

// ---- Quiz class (jsdom) ---------------------------------------------------------

const FIXTURE = `
  <div id="quiz-root">
    <p id="quiz-status"></p>
    <p id="quiz-question"></p>
    <div id="quiz-options"></div>
    <button id="quiz-action"></button>
    <button id="quiz-cancel" hidden></button>
    <div id="quiz-ai">
      <input id="quiz-ai-prompt" />
      <button id="quiz-ai-gen"></button>
      <p id="quiz-ai-msg" hidden></p>
      <a id="quiz-ai-buy" class="hidden"></a>
    </div>
  </div>`;

// A quiz panel with none of the optional nodes (no cancel button, no AI form).
const MINIMAL_FIXTURE = `
  <div id="quiz-root">
    <p id="quiz-status"></p>
    <p id="quiz-question"></p>
    <div id="quiz-options"></div>
    <button id="quiz-action"></button>
  </div>`;

// Translate stub: identity keys, except quizAnswered keeps its {n} placeholder so
// the interpolation of the answered-count is observable.
const tt = (k: string): string => (k === 'quizAnswered' ? 'answered:{n}' : k);

function makeQuiz(opts: { lang?: string; minimal?: boolean; defaults?: boolean } = {}) {
  document.body.innerHTML = opts.minimal ? MINIMAL_FIXTURE : FIXTURE;
  const root = document.getElementById('quiz-root') as HTMLElement;
  const send = vi.fn();
  const onModal = vi.fn();
  const onComplete = vi.fn();
  const nameOf = (id: string): string => `N(${id})`;
  const myLang = (): string => opts.lang ?? 'en';
  const quiz = opts.defaults
    ? new Quiz(root, 'me', nameOf, myLang, send, tt) // exercise the default callbacks
    : new Quiz(root, 'me', nameOf, myLang, send, tt, onModal, onComplete);
  const $ = (sel: string): HTMLElement => root.querySelector(sel) as HTMLElement;
  return {
    quiz,
    root,
    send,
    onModal,
    onComplete,
    status: () => $('#quiz-status'),
    question: () => $('#quiz-question'),
    optsWrap: () => $('#quiz-options'),
    action: () => $('#quiz-action') as HTMLButtonElement,
    cancel: () => $('#quiz-cancel') as HTMLButtonElement,
    options: () => [...root.querySelectorAll('.quiz-option')] as HTMLButtonElement[],
    aiForm: () => $('#quiz-ai'),
    aiInput: () => $('#quiz-ai-prompt') as HTMLInputElement,
    aiGen: () => $('#quiz-ai-gen') as HTMLButtonElement,
    aiMsg: () => $('#quiz-ai-msg'),
    aiBuy: () => $('#quiz-ai-buy'),
    busy: () => document.getElementById('quiz-busy'),
    lastSent: () => send.mock.calls.at(-1)?.[0] as QuizState,
  };
}

afterEach(() => {
  vi.restoreAllMocks();
  document.body.innerHTML = '';
});

const AI_Q1: AiQuestion = { q: { en: 'Q1' }, options: { en: ['a', 'b', 'c', 'd'] }, answer: 1 };
const AI_Q2: AiQuestion = { q: { en: 'Q2' }, options: { en: ['x', 'y', 'z'] }, answer: 0 };

describe('Quiz — initial render', () => {
  it('boots to the start screen with 4 option buttons and the AI busy notice injected', () => {
    const h = makeQuiz();
    expect(h.options()).toHaveLength(4);
    expect(h.status().textContent).toBe('');
    expect(h.question().textContent).toBe('');
    expect(h.optsWrap().hidden).toBe(true);
    expect(h.action().textContent).toBe('quizStart');
    expect(h.action().hidden).toBe(false);
    expect(h.cancel().hidden).toBe(true); // no quiz → no cancel
    // creation form untouched, busy notice present but hidden
    expect(h.aiForm().hidden).toBe(false);
    expect(h.aiInput().disabled).toBe(false);
    expect(h.busy()).not.toBeNull();
    expect(h.busy()?.hidden).toBe(true);
    expect(h.quiz.isActive()).toBe(false);
  });

  it('works without the optional cancel button and AI form', () => {
    const h = makeQuiz({ minimal: true });
    expect(h.busy()).toBeNull();
    expect(h.root.querySelector('#quiz-cancel')).toBeNull();
    expect(h.quiz.startAiQuiz([AI_Q1])).toBe(true);
    expect(h.question().textContent).toBe('1/1 · Q1');
    expect(h.quiz.isActive()).toBe(true);
  });
});

describe('Quiz — host flow with an inline AI pack (spec 0067)', () => {
  it('runs start → answer/change → peer answers → reveal → next → done', () => {
    const h = makeQuiz();

    // -- start ---------------------------------------------------------------
    expect(h.quiz.startAiQuiz([AI_Q1, AI_Q2])).toBe(true);
    expect(h.onModal).toHaveBeenCalledWith(true); // R4.1
    expect(h.quiz.isActive()).toBe(true);
    expect(h.quiz.startAiQuiz([AI_Q1])).toBe(false); // one live quiz at a time (R4.2)
    expect(h.question().textContent).toBe('1/2 · Q1');
    expect(h.options().map((b) => b.textContent)).toEqual(['a', 'b', 'c', 'd']);
    expect(h.options().every((b) => !b.hidden && !b.disabled)).toBe(true);
    expect(h.status().textContent).toBe('answered:0');
    expect(h.action().hidden).toBe(false); // host drives the phases
    expect(h.action().textContent).toBe('quizReveal');
    expect(h.cancel().hidden).toBe(false); // host + live → cancellable
    expect(h.lastSent().phase).toBe('question');
    expect(h.lastSent().pack).toHaveLength(2);
    // creation form gated (#220)
    expect(h.aiForm().hidden).toBe(true);
    expect(h.aiInput().disabled).toBe(true);
    expect(h.aiGen().disabled).toBe(true);
    expect(h.aiMsg().hidden).toBe(true);
    expect(h.aiBuy().classList.contains('hidden')).toBe(true);
    expect(h.busy()?.hidden).toBe(false);
    expect(h.busy()?.textContent).toBe('quizBusy');

    // -- host answers (and may change until the reveal) ------------------------
    let sends = h.send.mock.calls.length;
    h.options()[1].click();
    expect(h.options()[1].classList.contains('chosen')).toBe(true);
    expect(h.status().textContent).toBe('quizWaiting');
    expect(h.lastSent().answeredIds).toEqual(['me']);
    expect(h.send.mock.calls.length).toBe(sends + 1);
    sends = h.send.mock.calls.length;
    h.options()[1].click(); // re-picking the same option is a no-op
    expect(h.send.mock.calls.length).toBe(sends);
    h.options()[2].click(); // changing the answer updates in place, no re-broadcast
    expect(h.options()[2].classList.contains('chosen')).toBe(true);
    expect(h.options()[1].classList.contains('chosen')).toBe(false);
    expect(h.send.mock.calls.length).toBe(sends);

    // -- a peer's answer reaches the host over the relay -----------------------
    h.quiz.applyRemote({ game: 'quiz', t: 'answer', q: 0, choice: 1, by: 'p1', name: '<b>P1</b>' });
    expect(h.lastSent().answeredIds).toEqual(['me', 'p1']);
    expect(h.lastSent().players.p1.answered).toBe(1);
    sends = h.send.mock.calls.length;
    // an answer for a different question is ignored
    h.quiz.applyRemote({ game: 'quiz', t: 'answer', q: 5, choice: 0, by: 'p2', name: 'P2' });
    expect(h.send.mock.calls.length).toBe(sends);

    // -- reveal ----------------------------------------------------------------
    h.action().click();
    const revealed = h.lastSent();
    expect(revealed.phase).toBe('reveal');
    expect(revealed.correct).toBe(1);
    expect(revealed.choices).toEqual({ me: 2, p1: 1 });
    expect(revealed.players.p1.score).toBe(1);
    expect(revealed.players.me.score).toBe(0);
    expect(h.options()[1].classList.contains('correct')).toBe(true);
    expect(h.options()[2].classList.contains('chosen')).toBe(true);
    expect(h.options()[2].classList.contains('wrong')).toBe(true);
    expect(h.options().every((b) => b.disabled)).toBe(true); // locked on reveal
    expect(h.status().textContent).toBe('<b>P1</b>: 1 · N(me): 0'); // score line
    expect(h.action().textContent).toBe('quizNext'); // not the last question yet

    // -- next question ----------------------------------------------------------
    h.action().click();
    expect(h.question().textContent).toBe('2/2 · Q2');
    expect(h.status().textContent).toBe('answered:0'); // choice + tally reset
    expect(h.options().some((b) => b.classList.contains('chosen'))).toBe(false);
    expect(h.options()[3].hidden).toBe(true); // only 3 options this time
    expect(h.options()[3].textContent).toBe('');

    // -- answer correctly, reveal, finish ----------------------------------------
    h.options()[0].click();
    h.action().click(); // reveal
    expect(h.options()[0].classList.contains('correct')).toBe(true);
    expect(h.options()[0].classList.contains('wrong')).toBe(false);
    expect(h.action().textContent).toBe('quizFinish'); // last question
    h.action().click(); // finish → done

    const done = h.lastSent();
    expect(done.phase).toBe('done');
    expect(done.history).toHaveLength(2);
    expect(h.quiz.isActive()).toBe(false);
    expect(h.question().textContent).toBe('🏆');
    expect(h.optsWrap().hidden).toBe(true);
    expect(h.action().textContent).toBe('quizNew');
    expect(h.cancel().hidden).toBe(true);
    // creation form restored (#220)
    expect(h.aiForm().hidden).toBe(false);
    expect(h.aiInput().disabled).toBe(false);
    expect(h.busy()?.hidden).toBe(true);

    // leaderboard + recap: escaped names, answered counts, per-question ✓/✗/—
    const html = h.status().innerHTML;
    expect(html).toContain('quiz-rank');
    expect(html).toContain('&lt;b&gt;P1&lt;/b&gt;'); // escapeText
    expect(html).toContain('2/2'); // host answered both
    expect(html).toContain('1/2'); // p1 answered only the first
    expect(html).toContain('quiz-recap-ok'); // a correct row
    expect(html).toContain('quiz-recap-ko'); // a wrong row
    expect(html).toContain('quiz-recap-na'); // an unanswered row (—)

    // host persists the summary (#221)
    expect(h.onComplete).toHaveBeenCalledTimes(1);
    const summary = h.onComplete.mock.calls[0]?.[0] as QuizSummary;
    expect(summary.title).toBeNull();
    expect(summary.questions).toEqual([
      { prompt: 'Q1', options: ['a', 'b', 'c', 'd'], correct_index: 1 },
      { prompt: 'Q2', options: ['x', 'y', 'z'], correct_index: 0 },
    ]);
    expect(summary.results).toContainEqual({
      peer_id: 'me',
      display_name: 'N(me)',
      score: 1,
      total: 2,
    });
    expect(summary.results).toContainEqual({
      peer_id: 'p1',
      display_name: '<b>P1</b>',
      score: 1,
      total: 2,
    });

    // From the done screen the host can start a fresh (built-in) quiz.
    vi.spyOn(Math, 'random').mockReturnValue(0);
    h.action().click();
    expect(h.lastSent().phase).toBe('question');
    expect(h.lastSent().total).toBe(8);
    expect(h.lastSent().pack).toBeUndefined();
  });

  it('completes with the default (no-op) modal/complete callbacks', () => {
    const h = makeQuiz({ defaults: true });
    expect(h.quiz.startAiQuiz([AI_Q1])).toBe(true);
    h.action().click(); // reveal
    h.action().click(); // finish
    expect(h.action().textContent).toBe('quizNew');
    expect(h.quiz.isActive()).toBe(false);
  });
});

describe('Quiz — host flow with the built-in pack', () => {
  it('starts a shuffled 8-question round and completes to a persistable summary', () => {
    vi.spyOn(Math, 'random').mockReturnValue(0); // deterministic shuffle
    const h = makeQuiz();
    h.action().click(); // no state → start a built-in round
    expect(h.onModal).toHaveBeenCalledWith(true);
    const started = h.lastSent();
    expect(started.phase).toBe('question');
    expect(started.hostId).toBe('me');
    expect(started.total).toBe(8);
    expect(h.question().textContent).toMatch(/^1\/8 · /);

    // Drive reveal → next through all 8 questions (no answers needed).
    for (let i = 0; i < 8; i++) {
      h.action().click(); // reveal
      expect(h.status().textContent).toBe('N(me): 0'); // score line
      h.action().click(); // next / finish
      if (i < 7) expect(h.question().textContent).toMatch(new RegExp(`^${i + 2}/8 · `));
    }

    expect(h.lastSent().phase).toBe('done');
    expect(h.lastSent().history).toHaveLength(8);
    expect(h.onComplete).toHaveBeenCalledTimes(1);
    const summary = h.onComplete.mock.calls[0]?.[0] as QuizSummary;
    expect(summary.questions).toHaveLength(8); // built via the host's round mapping
    for (const q of summary.questions) {
      expect(q.prompt.length).toBeGreaterThan(0);
      expect(q.options).toHaveLength(4);
    }
    expect(summary.results).toEqual([
      { peer_id: 'me', display_name: 'N(me)', score: 0, total: 8 },
    ]);
  });
});

describe('Quiz — cancel (spec 0070 R4.3)', () => {
  it('host cancel broadcasts a terminal cancelled state, resets and closes the modal', () => {
    vi.spyOn(Math, 'random').mockReturnValue(0);
    const h = makeQuiz();
    h.action().click(); // start built-in
    expect(h.cancel().hidden).toBe(false);
    h.cancel().click();
    const sent = h.lastSent();
    expect(sent.phase).toBe('cancelled');
    expect(sent.answeredIds).toEqual([]);
    expect(sent.correct).toBeUndefined();
    expect(h.onModal).toHaveBeenLastCalledWith(false);
    expect(h.quiz.isActive()).toBe(false);
    expect(h.action().textContent).toBe('quizStart');
    expect(h.question().textContent).toBe('');
  });

  it('is ignored with no quiz and for non-hosts', () => {
    const h = makeQuiz();
    h.cancel().click(); // nothing to cancel
    expect(h.send).not.toHaveBeenCalled();
    h.quiz.applyRemote(remoteQ()); // someone else's quiz
    h.send.mockClear();
    h.cancel().click(); // I'm not the host
    expect(h.send).not.toHaveBeenCalled();
  });
});

// A remote (host='host') question state carrying an inline pack.
const remoteQ = (over: Partial<QuizState> = {}): QuizState => ({
  game: 'quiz',
  t: 'state',
  phase: 'question',
  hostId: 'host',
  players: { host: { name: 'Host', score: 0, answered: 1 } },
  qIndex: 0,
  total: 2,
  packIndex: 0,
  answeredIds: ['host'],
  pack: [
    { answer: 1, q: { en: 'RQ1' }, options: { en: ['r1', 'r2', 'r3', 'r4'] } },
    { answer: 0, q: { en: 'RQ2' }, options: { en: ['s1', 's2', 's3', 's4'] } },
  ],
  ...over,
});

describe('Quiz — applyRemote (peer view)', () => {
  it('opens the modal once, renders read-only and relays my answers to the host', () => {
    const h = makeQuiz();
    h.quiz.applyRemote(remoteQ());
    expect(h.onModal).toHaveBeenCalledTimes(1);
    expect(h.onModal).toHaveBeenCalledWith(true);
    expect(h.question().textContent).toBe('1/2 · RQ1');
    expect(h.status().textContent).toBe('answered:1');
    expect(h.action().hidden).toBe(true); // only the host drives the phases
    expect(h.cancel().hidden).toBe(true); // cancel is host-only

    // roster update (same phase/question) does not re-open the modal
    h.quiz.applyRemote(remoteQ({ answeredIds: ['host', 'p2'] }));
    expect(h.status().textContent).toBe('answered:2');
    expect(h.onModal).toHaveBeenCalledTimes(1);

    // my answer goes over the relay to the host
    h.options()[0].click();
    expect(h.send).toHaveBeenCalledWith({
      game: 'quiz',
      t: 'answer',
      q: 0,
      choice: 0,
      by: 'me',
      name: 'N(me)',
    });
    expect(h.status().textContent).toBe('quizWaiting');

    // a non-host ignores others' answer messages (secret until reveal)
    const sends = h.send.mock.calls.length;
    h.quiz.applyRemote({ game: 'quiz', t: 'answer', q: 0, choice: 1, by: 'p2', name: 'P2' });
    expect(h.send.mock.calls.length).toBe(sends);
    expect(h.status().textContent).toBe('quizWaiting');

    // reveal keeps my choice highlighted (I picked 0, correct is 1)
    h.quiz.applyRemote(remoteQ({ phase: 'reveal', correct: 1, choices: { me: 0, host: 1 } }));
    expect(h.options()[0].classList.contains('chosen')).toBe(true);
    expect(h.options()[0].classList.contains('wrong')).toBe(true);
    expect(h.options()[1].classList.contains('correct')).toBe(true);
    h.action().click(); // non-host, quiz still live → no-op
    expect(h.send.mock.calls.length).toBe(sends);

    // moving to the next question clears my choice
    h.quiz.applyRemote(remoteQ({ qIndex: 1 }));
    expect(h.question().textContent).toBe('2/2 · RQ2');
    expect(h.options().some((b) => b.classList.contains('chosen'))).toBe(false);
  });

  it('a cancelled broadcast resets and closes the modal only if a quiz was live', () => {
    const h = makeQuiz();
    // cancelled with nothing live → no modal churn
    h.quiz.applyRemote({ game: 'quiz', t: 'state', phase: 'cancelled' });
    expect(h.onModal).not.toHaveBeenCalled();
    // live quiz → cancelled ends it for everyone
    h.quiz.applyRemote(remoteQ());
    h.quiz.applyRemote({ game: 'quiz', t: 'state', phase: 'cancelled' });
    expect(h.onModal).toHaveBeenLastCalledWith(false);
    expect(h.quiz.isActive()).toBe(false);
    expect(h.action().textContent).toBe('quizStart');
    expect(h.question().textContent).toBe('');
  });

  it('a null / non-object message clears the quiz', () => {
    const h = makeQuiz();
    h.quiz.applyRemote(remoteQ());
    h.quiz.applyRemote(null);
    expect(h.quiz.isActive()).toBe(false);
    expect(h.action().textContent).toBe('quizStart');
    h.quiz.applyRemote(remoteQ());
    h.quiz.applyRemote('junk');
    expect(h.quiz.isActive()).toBe(false);
  });

  it('renders a remote done state: leaderboard + built-in-pack recap', () => {
    const h = makeQuiz();
    const done: QuizState = {
      game: 'quiz',
      t: 'state',
      phase: 'done',
      hostId: 'host',
      players: {
        host: { name: 'Host', score: 1, answered: 1 },
        me: { name: 'Me', score: 0, answered: 1 },
      },
      qIndex: 1,
      total: 2,
      packIndex: 0,
      answeredIds: [],
      history: [
        { packIndex: 0, correct: 1, choices: { host: 1, me: 9 } }, // me: out-of-range → '?'
        { packIndex: 9999, correct: 0, choices: {} }, // unknown item → skipped
      ],
    };
    h.quiz.applyRemote(done);
    expect(h.question().textContent).toBe('🏆');
    const html = h.status().innerHTML;
    expect(html).toContain('Jupiter'); // PACK[0]'s correct option, viewer language en
    expect(html).toContain('?'); // out-of-range choice
    expect(html).toContain('quiz-recap-ok');
    expect(html).toContain('quiz-recap-ko');
    expect((html.match(/quiz-recap-qhead/g) ?? []).length).toBe(1); // bad item skipped

    // a done state without history renders the leaderboard only
    h.quiz.applyRemote({ ...done, history: undefined });
    expect(h.status().innerHTML).toContain('quiz-rank');
    expect(h.status().innerHTML).not.toContain('quiz-recap');
  });

  it('a non-host can take over a finished quiz with a fresh built-in round', () => {
    vi.spyOn(Math, 'random').mockReturnValue(0);
    const h = makeQuiz();
    h.quiz.applyRemote(remoteQ({ phase: 'done', history: [] }));
    expect(h.action().textContent).toBe('quizNew');
    h.action().click(); // done + not host → start a new quiz as host
    const started = h.lastSent();
    expect(started.hostId).toBe('me');
    expect(started.phase).toBe('question');
    expect(started.total).toBe(8);
    expect(h.onModal).toHaveBeenLastCalledWith(true);
  });
});

describe('Quiz — language selection (pick)', () => {
  it("renders the viewer's own language when the pack carries it", () => {
    const h = makeQuiz({ lang: 'it' });
    h.quiz.startAiQuiz([
      { q: { en: 'EN Q', it: 'IT Q' }, options: { en: ['e1', 'e2'], it: ['i1', 'i2'] }, answer: 0 },
    ]);
    expect(h.question().textContent).toBe('1/1 · IT Q');
    expect(h.options()[0].textContent).toBe('i1');
  });

  it('falls back to English when the language is missing', () => {
    const h = makeQuiz({ lang: 'it' });
    h.quiz.startAiQuiz([{ q: { en: 'EN Q' }, options: { en: ['e1', 'e2'] }, answer: 0 }]);
    expect(h.question().textContent).toBe('1/1 · EN Q');
    expect(h.options()[0].textContent).toBe('e1');
  });

  it('falls back to any available language when even English is missing', () => {
    const h = makeQuiz({ lang: 'en' });
    h.quiz.startAiQuiz([{ q: { fr: 'FR Q' }, options: { fr: ['f1', 'f2'] }, answer: 0 }]);
    expect(h.question().textContent).toBe('1/1 · FR Q');
    expect(h.options()[0].textContent).toBe('f1');
  });
});

describe('Quiz — reset', () => {
  it('drops all local state and re-renders the start screen', () => {
    const h = makeQuiz();
    h.quiz.applyRemote(remoteQ());
    expect(h.quiz.isActive()).toBe(true);
    h.quiz.reset();
    expect(h.quiz.isActive()).toBe(false);
    expect(h.status().textContent).toBe('');
    expect(h.question().textContent).toBe('');
    expect(h.action().textContent).toBe('quizStart');
  });
});
