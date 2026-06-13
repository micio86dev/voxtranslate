// Trivia quiz mini-game (spec 0047). Multiple players answer concurrently, so —
// unlike turn-based Tic-Tac-Toe — it's HOST-authoritative: the player who starts
// owns the canonical state and drives the phases (question → reveal → … → done);
// others send their answer and the host tallies. State flows over the same
// game-agnostic relay (spec 0046), tagged `game:'quiz'` so it coexists with TTT.
//
// Questions come from a built-in pack (free, no LLM/credits). The host picks a
// random subset and broadcasts each question's text/options; the correct index
// stays in the host's memory until the reveal, so it isn't sent early.

const PACK: { q: string; options: string[]; answer: number }[] = [
  { q: 'Which planet is the largest in the Solar System?', options: ['Earth', 'Jupiter', 'Saturn', 'Mars'], answer: 1 },
  { q: 'What is the capital of Japan?', options: ['Seoul', 'Beijing', 'Tokyo', 'Bangkok'], answer: 2 },
  { q: 'How many continents are there on Earth?', options: ['5', '6', '7', '8'], answer: 2 },
  { q: 'Who painted the Mona Lisa?', options: ['Van Gogh', 'Picasso', 'Da Vinci', 'Monet'], answer: 2 },
  { q: 'What is the chemical symbol for gold?', options: ['Go', 'Gd', 'Au', 'Ag'], answer: 2 },
  { q: 'Which ocean is the largest?', options: ['Atlantic', 'Indian', 'Arctic', 'Pacific'], answer: 3 },
  { q: 'In which year did the first iPhone launch?', options: ['2005', '2007', '2009', '2010'], answer: 1 },
  { q: 'How many strings does a standard guitar have?', options: ['4', '5', '6', '7'], answer: 2 },
  { q: 'What language has the most native speakers?', options: ['English', 'Hindi', 'Spanish', 'Mandarin Chinese'], answer: 3 },
  { q: 'What is the smallest prime number?', options: ['0', '1', '2', '3'], answer: 2 },
];
const ROUND_QS = 5; // questions per game

interface Player { name: string; score: number }
export interface QuizState {
  game: 'quiz';
  t: 'state';
  phase: 'question' | 'reveal' | 'done';
  hostId: string;
  players: Record<string, Player>;
  qIndex: number;
  total: number;
  question: string;
  options: string[];
  answeredIds: string[]; // who has answered THIS question (no choices leaked)
  correct?: number; // reveal only
  choices?: Record<string, number>; // reveal only
}
interface AnswerMsg { game: 'quiz'; t: 'answer'; q: number; choice: number; by: string; name: string }

function shuffle<T>(a: T[]): T[] {
  const r = a.slice();
  for (let i = r.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [r[i], r[j]] = [r[j], r[i]];
  }
  return r;
}

export class Quiz {
  private state: QuizState | null = null;
  private myChoice: number | null = null; // local: my pick for the current question
  // Host-only:
  private questions: { q: string; options: string[]; answer: number }[] = [];
  private pending: Record<string, number> = {}; // answers for the current question

  private optionEls: HTMLButtonElement[] = [];

  constructor(
    private root: HTMLElement,
    private myId: string,
    private nameOf: (id: string) => string,
    private send: (state: unknown) => void,
    private t: (k: string) => string,
  ) {
    const opts = root.querySelector('#quiz-options') as HTMLElement;
    for (let i = 0; i < 4; i++) {
      const b = document.createElement('button');
      b.className = 'quiz-option';
      b.addEventListener('click', () => this.choose(i));
      opts.appendChild(b);
      this.optionEls.push(b);
    }
    (root.querySelector('#quiz-action') as HTMLButtonElement).addEventListener('click', () =>
      this.onAction(),
    );
    this.render();
  }

  private isHost(): boolean {
    return !!this.state && this.state.hostId === this.myId;
  }

  // ---- actions ----------------------------------------------------------------

  private choose(i: number): void {
    const s = this.state;
    if (!s || s.phase !== 'question' || this.myChoice !== null) return;
    this.myChoice = i;
    this.render();
    if (this.isHost()) {
      this.recordAnswer({ game: 'quiz', t: 'answer', q: s.qIndex, choice: i, by: this.myId, name: this.nameOf(this.myId) });
    } else {
      this.send({ game: 'quiz', t: 'answer', q: s.qIndex, choice: i, by: this.myId, name: this.nameOf(this.myId) });
    }
  }

  private onAction(): void {
    const s = this.state;
    if (!s) return this.startNew();
    if (!this.isHost()) {
      // Non-host: only useful action is to take over a stuck/finished quiz.
      if (s.phase === 'done') this.startNew();
      return;
    }
    if (s.phase === 'question') this.reveal();
    else if (s.phase === 'reveal') this.next();
    else this.startNew(); // done → new quiz
  }

  private startNew(): void {
    this.questions = shuffle(PACK).slice(0, ROUND_QS);
    this.pending = {};
    this.myChoice = null;
    const q = this.questions[0];
    this.setState({
      game: 'quiz',
      t: 'state',
      phase: 'question',
      hostId: this.myId,
      players: { [this.myId]: { name: this.nameOf(this.myId), score: 0 } },
      qIndex: 0,
      total: this.questions.length,
      question: q.q,
      options: q.options,
      answeredIds: [],
    });
  }

  /** Host: a player's answer arrived. */
  private recordAnswer(a: AnswerMsg): void {
    const s = this.state;
    if (!s || !this.isHost() || s.phase !== 'question' || a.q !== s.qIndex) return;
    if (this.pending[a.by] !== undefined) return; // first answer wins
    this.pending[a.by] = a.choice;
    if (!s.players[a.by]) s.players[a.by] = { name: a.name, score: 0 };
    this.setState({ ...s, answeredIds: Object.keys(this.pending) });
  }

  private reveal(): void {
    const s = this.state;
    if (!s) return;
    const correct = this.questions[s.qIndex].answer;
    const players = { ...s.players };
    for (const [id, choice] of Object.entries(this.pending)) {
      if (choice === correct) players[id] = { ...players[id], score: (players[id]?.score ?? 0) + 1 };
    }
    this.setState({ ...s, phase: 'reveal', correct, choices: { ...this.pending }, players });
  }

  private next(): void {
    const s = this.state;
    if (!s) return;
    const qIndex = s.qIndex + 1;
    this.myChoice = null;
    this.pending = {};
    if (qIndex >= s.total) {
      this.setState({ ...s, phase: 'done', answeredIds: [], correct: undefined, choices: undefined });
      return;
    }
    const q = this.questions[qIndex];
    this.setState({
      ...s,
      phase: 'question',
      qIndex,
      question: q.q,
      options: q.options,
      answeredIds: [],
      correct: undefined,
      choices: undefined,
    });
  }

  private setState(s: QuizState | null): void {
    this.state = s;
    this.render();
    this.send(s ?? null);
  }

  // ---- remote -----------------------------------------------------------------

  /** Apply a peer's broadcast / the join snapshot. Routes answers to the host. */
  applyRemote(msg: unknown): void {
    if (!msg || typeof msg !== 'object') {
      this.state = null;
      this.render();
      return;
    }
    const m = msg as { t?: string; phase?: string; qIndex?: number };
    if (m.t === 'answer') {
      if (this.isHost()) this.recordAnswer(msg as AnswerMsg);
      return; // non-host ignores others' answers (kept secret until reveal)
    }
    // A canonical state. If the question changed, my previous pick no longer applies.
    if (this.state && (this.state.qIndex !== m.qIndex || this.state.phase !== m.phase)) {
      if (m.phase === 'question') this.myChoice = null;
    }
    this.state = msg as QuizState;
    this.render();
  }

  reset(): void {
    this.state = null;
    this.myChoice = null;
    this.pending = {};
    this.questions = [];
    this.render();
  }

  // ---- rendering --------------------------------------------------------------

  private render(): void {
    const s = this.state;
    const statusEl = this.root.querySelector('#quiz-status') as HTMLElement;
    const qEl = this.root.querySelector('#quiz-question') as HTMLElement;
    const action = this.root.querySelector('#quiz-action') as HTMLButtonElement;
    const optsWrap = this.root.querySelector('#quiz-options') as HTMLElement;

    if (!s) {
      statusEl.textContent = '';
      qEl.textContent = '';
      optsWrap.hidden = true;
      action.textContent = this.t('quizStart');
      action.hidden = false;
      return;
    }

    if (s.phase === 'done') {
      optsWrap.hidden = true;
      qEl.textContent = '🏆';
      statusEl.innerHTML = this.leaderboard(s);
      action.textContent = this.t('quizNew');
      action.hidden = false; // anyone can start a new quiz (also recovers a left host)
      return;
    }

    optsWrap.hidden = false;
    qEl.textContent = `${s.qIndex + 1}/${s.total} · ${s.question}`;
    const revealing = s.phase === 'reveal';
    this.optionEls.forEach((b, i) => {
      b.textContent = s.options[i] ?? '';
      b.hidden = i >= s.options.length;
      b.classList.toggle('correct', revealing && s.correct === i);
      b.classList.toggle('chosen', this.myChoice === i);
      b.classList.toggle('wrong', revealing && this.myChoice === i && s.correct !== i);
      b.disabled = revealing || this.myChoice !== null;
    });

    if (revealing) {
      statusEl.textContent = this.scoreLine(s);
      action.hidden = !this.isHost();
      action.textContent = s.qIndex + 1 >= s.total ? this.t('quizFinish') : this.t('quizNext');
    } else {
      // question
      statusEl.textContent =
        this.myChoice !== null
          ? this.t('quizWaiting')
          : this.t('quizAnswered').replace('{n}', String(s.answeredIds.length));
      action.hidden = !this.isHost();
      action.textContent = this.t('quizReveal');
    }
  }

  private scoreLine(s: QuizState): string {
    return Object.values(s.players)
      .sort((a, b) => b.score - a.score)
      .map((p) => `${p.name}: ${p.score}`)
      .join(' · ');
  }

  private leaderboard(s: QuizState): string {
    const rows = Object.values(s.players).sort((a, b) => b.score - a.score);
    return rows.map((p, i) => `<div class="quiz-rank"><span>${i + 1}. ${escapeText(p.name)}</span><strong>${p.score}</strong></div>`).join('');
  }
}

function escapeText(s: string): string {
  const d = document.createElement('div');
  d.textContent = s;
  return d.innerHTML;
}
