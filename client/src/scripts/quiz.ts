// Trivia quiz mini-game (spec 0047, localized 0048). Multiple players answer at
// once, so it's HOST-authoritative: the starter owns the canonical state and drives
// the phases; others send their answer and the host tallies. State flows over the
// game-agnostic relay (spec 0046), tagged `game:'quiz'` so it coexists with TTT.
//
// The question pack is built-in AND pre-translated into the 8 UI languages — the host
// broadcasts only the question's *index* in the pack, and each client renders it in
// ITS OWN language (no LLM/credits, instant). The correct option sits at the same
// index in every language.

interface PackItem {
  answer: number;
  q: Record<string, string>; // by language (en required = fallback)
  options: Record<string, string[]>; // by language; number/symbol options give only `en`
}

const PACK: PackItem[] = [
  {
    answer: 1,
    q: {
      en: 'Which is the largest planet in the Solar System?',
      it: 'Qual è il pianeta più grande del Sistema Solare?',
      es: '¿Cuál es el planeta más grande del Sistema Solar?',
      fr: 'Quelle est la plus grande planète du système solaire ?',
      de: 'Welcher ist der größte Planet im Sonnensystem?',
      pt: 'Qual é o maior planeta do Sistema Solar?',
      ja: '太陽系で最も大きい惑星はどれ？',
      zh: '太阳系中最大的行星是哪个？',
    },
    options: {
      en: ['Earth', 'Jupiter', 'Saturn', 'Mars'],
      it: ['Terra', 'Giove', 'Saturno', 'Marte'],
      es: ['Tierra', 'Júpiter', 'Saturno', 'Marte'],
      fr: ['Terre', 'Jupiter', 'Saturne', 'Mars'],
      de: ['Erde', 'Jupiter', 'Saturn', 'Mars'],
      pt: ['Terra', 'Júpiter', 'Saturno', 'Marte'],
      ja: ['地球', '木星', '土星', '火星'],
      zh: ['地球', '木星', '土星', '火星'],
    },
  },
  {
    answer: 2,
    q: {
      en: 'What is the capital of Japan?',
      it: 'Qual è la capitale del Giappone?',
      es: '¿Cuál es la capital de Japón?',
      fr: 'Quelle est la capitale du Japon ?',
      de: 'Was ist die Hauptstadt von Japan?',
      pt: 'Qual é a capital do Japão?',
      ja: '日本の首都はどこ？',
      zh: '日本的首都是哪里？',
    },
    options: {
      en: ['Seoul', 'Beijing', 'Tokyo', 'Bangkok'],
      it: ['Seul', 'Pechino', 'Tokyo', 'Bangkok'],
      es: ['Seúl', 'Pekín', 'Tokio', 'Bangkok'],
      fr: ['Séoul', 'Pékin', 'Tokyo', 'Bangkok'],
      de: ['Seoul', 'Peking', 'Tokio', 'Bangkok'],
      pt: ['Seul', 'Pequim', 'Tóquio', 'Bangkok'],
      ja: ['ソウル', '北京', '東京', 'バンコク'],
      zh: ['首尔', '北京', '东京', '曼谷'],
    },
  },
  {
    answer: 3,
    q: {
      en: 'Which is the largest ocean?',
      it: "Qual è l'oceano più grande?",
      es: '¿Cuál es el océano más grande?',
      fr: 'Quel est le plus grand océan ?',
      de: 'Welcher ist der größte Ozean?',
      pt: 'Qual é o maior oceano?',
      ja: '最も大きい海洋はどれ？',
      zh: '最大的海洋是哪个？',
    },
    options: {
      en: ['Atlantic', 'Indian', 'Arctic', 'Pacific'],
      it: ['Atlantico', 'Indiano', 'Artico', 'Pacifico'],
      es: ['Atlántico', 'Índico', 'Ártico', 'Pacífico'],
      fr: ['Atlantique', 'Indien', 'Arctique', 'Pacifique'],
      de: ['Atlantik', 'Indik', 'Arktik', 'Pazifik'],
      pt: ['Atlântico', 'Índico', 'Ártico', 'Pacífico'],
      ja: ['大西洋', 'インド洋', '北極海', '太平洋'],
      zh: ['大西洋', '印度洋', '北冰洋', '太平洋'],
    },
  },
  {
    answer: 2, // 7 — number options are language-neutral
    q: {
      en: 'How many continents are there on Earth?',
      it: 'Quanti continenti ci sono sulla Terra?',
      es: '¿Cuántos continentes hay en la Tierra?',
      fr: 'Combien de continents y a-t-il sur Terre ?',
      de: 'Wie viele Kontinente gibt es auf der Erde?',
      pt: 'Quantos continentes há na Terra?',
      ja: '地球には大陸がいくつある？',
      zh: '地球上有多少个大洲？',
    },
    options: { en: ['5', '6', '7', '8'] },
  },
  {
    answer: 2, // 2
    q: {
      en: 'What is the smallest prime number?',
      it: 'Qual è il numero primo più piccolo?',
      es: '¿Cuál es el número primo más pequeño?',
      fr: 'Quel est le plus petit nombre premier ?',
      de: 'Was ist die kleinste Primzahl?',
      pt: 'Qual é o menor número primo?',
      ja: '最小の素数はいくつ？',
      zh: '最小的质数是多少？',
    },
    options: { en: ['0', '1', '2', '3'] },
  },
  {
    answer: 2, // Au — chemical symbols are language-neutral
    q: {
      en: 'What is the chemical symbol for gold?',
      it: "Qual è il simbolo chimico dell'oro?",
      es: '¿Cuál es el símbolo químico del oro?',
      fr: "Quel est le symbole chimique de l'or ?",
      de: 'Was ist das chemische Symbol für Gold?',
      pt: 'Qual é o símbolo químico do ouro?',
      ja: '金の元素記号は？',
      zh: '黄金的化学符号是什么？',
    },
    options: { en: ['Go', 'Gd', 'Au', 'Ag'] },
  },
];
const ROUND_QS = 4; // questions per game

function pick<T>(d: Record<string, T>, lang: string): T {
  return d[lang] ?? d.en;
}
function shuffle<T>(a: T[]): T[] {
  const r = a.slice();
  for (let i = r.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [r[i], r[j]] = [r[j], r[i]];
  }
  return r;
}

interface Player { name: string; score: number }
export interface QuizState {
  game: 'quiz';
  t: 'state';
  phase: 'question' | 'reveal' | 'done';
  hostId: string;
  players: Record<string, Player>;
  qIndex: number;
  total: number;
  packIndex: number; // which PACK question — each client renders it in its own language
  answeredIds: string[]; // who answered THIS question (no choices leaked)
  correct?: number; // reveal only
  choices?: Record<string, number>; // reveal only
}
interface AnswerMsg { game: 'quiz'; t: 'answer'; q: number; choice: number; by: string; name: string }

export class Quiz {
  private state: QuizState | null = null;
  private myChoice: number | null = null;
  private round: number[] = []; // host-only: chosen PACK indices
  private pending: Record<string, number> = {}; // host-only: answers for the current question
  private optionEls: HTMLButtonElement[] = [];

  constructor(
    private root: HTMLElement,
    private myId: string,
    private nameOf: (id: string) => string,
    private myLang: () => string,
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
    const a: AnswerMsg = { game: 'quiz', t: 'answer', q: s.qIndex, choice: i, by: this.myId, name: this.nameOf(this.myId) };
    if (this.isHost()) this.recordAnswer(a);
    else this.send(a);
  }

  private onAction(): void {
    const s = this.state;
    if (!s) return this.startNew();
    if (!this.isHost()) {
      if (s.phase === 'done') this.startNew(); // take over a finished/stuck quiz
      return;
    }
    if (s.phase === 'question') this.reveal();
    else if (s.phase === 'reveal') this.next();
    else this.startNew();
  }

  private startNew(): void {
    this.round = shuffle(PACK.map((_, i) => i)).slice(0, Math.min(ROUND_QS, PACK.length));
    this.pending = {};
    this.myChoice = null;
    this.setState({
      game: 'quiz',
      t: 'state',
      phase: 'question',
      hostId: this.myId,
      players: { [this.myId]: { name: this.nameOf(this.myId), score: 0 } },
      qIndex: 0,
      total: this.round.length,
      packIndex: this.round[0],
      answeredIds: [],
    });
  }

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
    const correct = PACK[s.packIndex].answer;
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
    this.setState({
      ...s,
      phase: 'question',
      qIndex,
      packIndex: this.round[qIndex],
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

  applyRemote(msg: unknown): void {
    if (!msg || typeof msg !== 'object') {
      this.state = null;
      this.render();
      return;
    }
    const m = msg as { t?: string; phase?: string; qIndex?: number };
    if (m.t === 'answer') {
      if (this.isHost()) this.recordAnswer(msg as AnswerMsg);
      return; // non-host ignores others' answers (secret until reveal)
    }
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
    this.round = [];
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
      action.hidden = false;
      return;
    }

    const lang = this.myLang();
    const pq = PACK[s.packIndex];
    const options = pick(pq.options, lang);
    optsWrap.hidden = false;
    qEl.textContent = `${s.qIndex + 1}/${s.total} · ${pick(pq.q, lang)}`;
    const revealing = s.phase === 'reveal';
    this.optionEls.forEach((b, i) => {
      b.textContent = options[i] ?? '';
      b.hidden = i >= options.length;
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
    return Object.values(s.players)
      .sort((a, b) => b.score - a.score)
      .map((p, i) => `<div class="quiz-rank"><span>${i + 1}. ${escapeText(p.name)}</span><strong>${p.score}</strong></div>`)
      .join('');
  }
}

function escapeText(s: string): string {
  const d = document.createElement('div');
  d.textContent = s;
  return d.innerHTML;
}
