// @vitest-environment jsdom
//
// Session detail screen (specs 0011+): open/close navigation, header +
// transcript/bookmark/quiz rendering, the AI-correction cost preview, and the
// serialized corrected-download pipeline (#222). The module wires its DOM at
// import time, so each test rebuilds the scaffold and imports fresh. The AI
// slots (report/sentiment/email) and all API/auth calls are mocked.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { SessionQuiz, TranscriptDoc } from './api';
import type { SessionRef } from './session-screen';

const api = vi.hoisted(() => ({
  ensureCorrection: vi.fn(),
  fetchAiPricing: vi.fn(),
  fetchCorrectionStatus: vi.fn(),
  fetchSessionQuizzes: vi.fn(),
  fetchTranscript: vi.fn(),
}));
vi.mock('./api', () => api);

const auth = vi.hoisted(() => ({
  formatCredits: vi.fn(),
  setBalance: vi.fn(),
  downloadTranscript: vi.fn(),
}));
vi.mock('./auth', () => auth);

vi.mock('./i18n', () => ({
  getUiLang: () => 'en',
  t: (key: string) => key,
}));

const email = vi.hoisted(() => ({ initEmailSlot: vi.fn(), updateEmailContext: vi.fn() }));
vi.mock('./email', () => email);
const report = vi.hoisted(() => ({ initReportSlot: vi.fn() }));
vi.mock('./report', () => report);
const sentimentSlot = vi.hoisted(() => ({
  initSentimentSlot: vi.fn(),
  updateSentimentContext: vi.fn(),
}));
vi.mock('./sentiment', () => sentimentSlot);

const SCAFFOLD = `
  <div id="home"></div>
  <div id="account" class="hidden"></div>
  <section id="session" class="hidden">
    <button id="session-back"></button>
    <span id="session-room"></span>
    <span id="session-date"></span>
    <span id="session-duration"></span>
    <span id="session-events"></span>
    <span id="session-participants"></span>
    <button id="session-dl-pdf">PDF</button>
    <button id="session-dl-json">JSON</button>
    <button id="session-dl-srt">SRT</button>
    <button id="session-dl-vtt">VTT</button>
    <select id="session-sub-mode">
      <option value="original">original</option>
      <option value="translated">translated</option>
      <option value="both">both</option>
    </select>
    <input id="ai-correct-chk" type="checkbox" />
    <span id="ai-correct-cost"></span>
    <div id="session-quizzes" hidden><div id="session-quizzes-list"></div></div>
    <div id="session-bookmarks" hidden></div>
    <div id="session-transcript"></div>
    <button id="session-transcript-toggle" class="hidden"></button>
    <p id="session-transcript-status"></p>
  </section>
`;

const $ = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;
const btn = (id: string) => $<HTMLButtonElement>(id);
const subMode = () => $<HTMLSelectElement>('session-sub-mode');
const chk = () => $<HTMLInputElement>('ai-correct-chk');
const cost = () => $('ai-correct-cost');
const status = () => $('session-transcript-status');
const transcript = () => $('session-transcript');
const toggle = () => $('session-transcript-toggle');

const flush = async (): Promise<void> => {
  for (let i = 0; i < 12; i++) await Promise.resolve();
};

function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

const ref = (over: Partial<SessionRef> = {}): SessionRef => ({
  id: 's1',
  room: 'demo-room',
  started_at: '2026-01-01T10:00:00.000Z',
  ended_at: '2026-01-01T10:30:05.000Z', // 1805s → "30m 05s"
  event_count: 7,
  ...over,
});

/** 7 events (>6 → collapsible preview); #1 is chat, #2 arrives translated. */
const EVENTS = Array.from({ length: 7 }, (_, i) => ({
  type: i === 1 ? 'chat' : 'speech',
  ts: new Date(Date.UTC(2026, 0, 1, 10, i, 0)).toISOString(),
  speaker_id: i === 1 ? 'p3' : 'p1',
  speaker_name: i === 1 ? 'Bob' : 'Ann',
  lang: i === 2 ? 'it' : 'en',
  original: `line ${i}`,
  translations: i === 2 ? { en: 'translated 2' } : {},
}));

const DOC: TranscriptDoc = {
  session: {
    id: 's1',
    room_name: 'demo-room',
    started_at: '2026-01-01T10:00:00.000Z',
    ended_at: '2026-01-01T10:30:00.000Z',
    duration_seconds: 1800,
    participants: [
      { id: 'p1', name: 'Ann', language: 'en' },
      { id: 'p2', name: ' ann ', language: 'it' }, // rejoin duplicate → collapsed
      { id: 'p3', name: 'Bob', language: 'it' },
    ],
  },
  events: EVENTS,
  bookmarks: [
    { ts: '2026-01-01T10:05:00.000Z', label: 'Key point', by: 'Ann' }, // +300s
    { ts: '2026-01-01T10:10:00.000Z', by: 'Bob' }, // +600s, unlabelled
  ],
  exported_at: '2026-01-01T11:00:00.000Z',
};

async function load() {
  return import('./session-screen');
}

async function open(over: Partial<SessionRef> = {}, opts: { onClose?: () => void } = {}) {
  const mod = await load();
  mod.openSessionScreen(ref(over), opts);
  await flush();
  return mod;
}

beforeEach(() => {
  vi.resetModules();
  vi.clearAllMocks();
  document.body.innerHTML = SCAFFOLD;
  auth.formatCredits.mockImplementation((n: number) => n.toFixed(3));
  auth.downloadTranscript.mockResolvedValue({ ok: true, status: 200 });
  api.ensureCorrection.mockResolvedValue({ ok: true, cached: true });
  api.fetchAiPricing.mockResolvedValue(null);
  api.fetchCorrectionStatus.mockResolvedValue(null);
  api.fetchSessionQuizzes.mockResolvedValue([]);
  api.fetchTranscript.mockResolvedValue(DOC);
});

afterEach(() => {
  vi.useRealTimers();
});

describe('open / close navigation', () => {
  it('opens over home, paints the header, inits the AI slots, and focuses Back', async () => {
    const mod = await open();
    expect(mod.currentSession()?.id).toBe('s1');
    expect($('home').classList.contains('hidden')).toBe(true);
    expect($('session').classList.contains('hidden')).toBe(false);
    expect($('session-room').textContent).toBe('demo-room');
    expect($('session-date').textContent).not.toBe('');
    expect($('session-events').textContent).toBe('7');
    expect(report.initReportSlot).toHaveBeenCalledWith(expect.objectContaining({ id: 's1' }));
    expect(sentimentSlot.initSentimentSlot).toHaveBeenCalledWith(expect.objectContaining({ id: 's1' }));
    expect(email.initEmailSlot).toHaveBeenCalledWith(expect.objectContaining({ id: 's1' }));
    expect(document.activeElement).toBe($('session-back'));

    const onClose = vi.fn();
    mod.openSessionScreen(ref(), { onClose });
    await flush();
    btn('session-back').click();
    expect(mod.currentSession()).toBeNull();
    expect($('session').classList.contains('hidden')).toBe(true);
    expect($('home').classList.contains('hidden')).toBe(false);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('returns to the account screen when opened from Billing → Transcripts', async () => {
    $('home').classList.add('hidden');
    $('account').classList.remove('hidden');
    const mod = await open();
    expect($('account').classList.contains('hidden')).toBe(true);
    mod.closeSessionScreen();
    expect($('account').classList.contains('hidden')).toBe(false);
    expect($('home').classList.contains('hidden')).toBe(true);
  });
});

describe('header + empty sessions', () => {
  it('formats sub-hour durations as Xm YYs and enables downloads', async () => {
    api.fetchTranscript.mockResolvedValue(null); // keep the header value
    await open();
    expect($('session-duration').textContent).toBe('30m 05s');
    expect(btn('session-dl-pdf').disabled).toBe(false);
    expect(status().textContent).toBe('loadFailed');
  });

  it('formats hour-plus durations as Xh MMm', async () => {
    api.fetchTranscript.mockResolvedValue(null);
    await open({ ended_at: '2026-01-01T11:01:05.000Z' });
    expect($('session-duration').textContent).toBe('1h 01m');
  });

  it('treats a still-open session as zero duration', async () => {
    api.fetchTranscript.mockResolvedValue(null);
    await open({ ended_at: null });
    expect($('session-duration').textContent).toBe('0m 00s');
  });

  it('disables everything for a session with no events', async () => {
    await open({ event_count: 0 });
    for (const f of ['pdf', 'json', 'srt', 'vtt']) {
      expect(btn(`session-dl-${f}`).disabled).toBe(true);
      expect(btn(`session-dl-${f}`).title).toBe('noTranscriptEvents');
    }
    expect(chk().disabled).toBe(true);
    expect(chk().checked).toBe(false);
    expect(cost().textContent).toBe('');
    expect(status().textContent).toBe('noTranscriptEvents');
    expect(api.fetchTranscript).not.toHaveBeenCalled();
    expect(toggle().classList.contains('hidden')).toBe(true);
  });
});

describe('transcript rendering', () => {
  it('renders events, dedupes the roster, and feeds the sentiment/email slots', async () => {
    await open();
    expect(status().textContent).toBe('');
    // The export is authoritative for duration + (deduped) participants.
    expect($('session-duration').textContent).toBe('30m 00s');
    expect($('session-participants').textContent).toBe('Ann, Bob');
    expect(sentimentSlot.updateSentimentContext).toHaveBeenCalledWith('s1', 2, 1800, [300, 600]);
    expect(email.updateEmailContext).toHaveBeenCalledWith('s1', [
      { id: 'p1', name: 'Ann' },
      { id: 'p3', name: 'Bob' },
    ]);

    const rows = transcript().querySelectorAll('.tr-event');
    expect(rows).toHaveLength(7);
    expect(rows[0].querySelector('.tr-text')?.textContent).toBe('line 0');
    expect(rows[0].querySelector('.tr-speaker')?.textContent).toBe('Ann');
    expect((rows[0] as HTMLElement).dataset.ts).toBe(String(Date.UTC(2026, 0, 1, 10, 0, 0)));
    // Chat events are marked and suffixed.
    expect(rows[1].classList.contains('tr-chat')).toBe(true);
    expect(rows[1].querySelector('.tr-speaker')?.textContent).toBe('Bob 💬');
    // Foreign-language speech shows the viewer's translation + the original.
    expect(rows[2].querySelector('.tr-text')?.textContent).toBe('translated 2');
    expect(rows[2].querySelector('.tr-orig')?.textContent).toBe('line 2');
    expect(rows[0].querySelector('.tr-orig')).toBeNull();

    // 7 > 6 rows → collapsed preview with the expand toggle.
    expect(transcript().classList.contains('collapsed')).toBe(true);
    expect(toggle().classList.contains('hidden')).toBe(false);
    expect(toggle().textContent).toBe('viewFullTranscript');
    expect(toggle().getAttribute('aria-expanded')).toBe('false');

    // Pinned moments above the transcript.
    const box = $('session-bookmarks');
    expect(box.hidden).toBe(false);
    const pins = box.querySelectorAll('.bm-item');
    expect(pins).toHaveLength(2);
    expect(pins[0].querySelector('.bm-by')?.textContent).toBe('Ann');
    expect(pins[0].querySelector('.bm-label')?.textContent).toBe('Key point');
    expect(pins[1].querySelector('.bm-label')).toBeNull();
  });

  it('keeps a short transcript expanded and hides empty bookmarks', async () => {
    api.fetchTranscript.mockResolvedValue({
      ...DOC,
      session: { ...DOC.session, duration_seconds: 65, participants: [DOC.session.participants[0]] },
      events: EVENTS.slice(0, 2),
      bookmarks: [],
    });
    await open();
    expect($('session-duration').textContent).toBe('1m 05s');
    expect(transcript().classList.contains('collapsed')).toBe(false);
    expect(toggle().classList.contains('hidden')).toBe(true);
    expect($('session-bookmarks').hidden).toBe(true);
    expect(sentimentSlot.updateSentimentContext).toHaveBeenCalledWith('s1', 1, 65, []);
  });

  it('drops a transcript that lands after navigating to another session', async () => {
    const d = deferred<TranscriptDoc | null>();
    api.fetchTranscript.mockReturnValueOnce(d.promise);
    const mod = await load();
    mod.openSessionScreen(ref());
    mod.openSessionScreen(ref({ id: 's2', event_count: 0 }));
    await flush();
    d.resolve(DOC);
    await flush();
    expect(status().textContent).toBe('noTranscriptEvents'); // s2's state, not s1's doc
    expect(transcript().querySelectorAll('.tr-event')).toHaveLength(0);
  });

  it('the toggle expands and collapses the preview', async () => {
    await open();
    toggle().click();
    expect(transcript().classList.contains('collapsed')).toBe(false);
    expect(toggle().textContent).toBe('hideTranscript');
    expect(toggle().getAttribute('aria-expanded')).toBe('true');
    toggle().click();
    expect(transcript().classList.contains('collapsed')).toBe(true);
    expect(toggle().textContent).toBe('viewFullTranscript');
  });

  it('renders quiz history with scores, winner and average (#221)', async () => {
    const quizzes: SessionQuiz[] = [
      {
        id: 'q1',
        title: 'Trivia',
        questions: [{ prompt: 'Q1', options: ['a', 'b'], correct_index: 0 }],
        created_at: '2026-01-01T10:20:00.000Z',
        results: [
          { peer_id: 'p1', display_name: 'Ann', score: 2, total: 2 },
          { peer_id: 'p3', display_name: 'Bob', score: 1, total: 2 },
        ],
      },
      { id: 'q2', title: null, questions: [], created_at: '2026-01-01T10:25:00.000Z', results: [] },
    ];
    api.fetchSessionQuizzes.mockResolvedValue(quizzes);
    await open();
    const box = $('session-quizzes');
    expect(box.hidden).toBe(false);
    const cards = $('session-quizzes-list').querySelectorAll('.quiz-card');
    expect(cards).toHaveLength(2);
    expect(cards[0].querySelector('.quiz-card-title')?.textContent).toBe('Trivia');
    const scoreRows = cards[0].querySelectorAll('.quiz-score-row');
    expect(scoreRows).toHaveLength(2);
    expect(scoreRows[0].textContent).toContain('Ann');
    expect(scoreRows[0].textContent).toContain('2/2');
    expect(cards[0].querySelector('.quiz-card-summary')?.textContent).toBe(
      '🏆 Ann · quizAverage: 1.5',
    );
    // Untitled quiz falls back to the generic label; no summary without results.
    expect(cards[1].querySelector('.quiz-card-title')?.textContent).toBe('quizzesLabel');
    expect(cards[1].querySelector('.quiz-card-summary')).toBeNull();
  });
});

describe('AI-correction cost preview (spec 0068)', () => {
  it('estimates from the fallback price, then repaints from server pricing', async () => {
    await open();
    // Fallback: 0.05 + 0.001 × 7 = 0.057
    expect(cost().textContent).toBe('~0.057');
    api.fetchAiPricing.mockResolvedValue({ transcript_correction: { base: 0.1, per_event: 0.01 } });
    const mod = await load(); // same registry — reuse via a fresh open below
    mod.openSessionScreen(ref());
    await flush();
    expect(cost().textContent).toBe('~0.170');
    // "both" runs two passes: 0.1 + 0.01 × 7 × 2 = 0.240
    subMode().value = 'both';
    subMode().dispatchEvent(new Event('change'));
    await flush();
    expect(cost().textContent).toBe('~0.240');
  });

  it('labels an already-cached (mode, lang) shape as free', async () => {
    api.fetchCorrectionStatus.mockResolvedValue({ cached: true });
    await open();
    expect(cost().textContent).toBe('aiCorrectFree');
    expect(api.fetchCorrectionStatus).toHaveBeenCalledWith('s1', 'original', '');
  });

  it('ignores a stale cached-probe after the sub-mode changed', async () => {
    const pending: Array<{ mode: string; resolve: (v: { cached: boolean } | null) => void }> = [];
    api.fetchCorrectionStatus.mockImplementation(
      (_id: string, mode: string) =>
        new Promise<{ cached: boolean } | null>((resolve) => pending.push({ mode, resolve })),
    );
    await open();
    subMode().value = 'both';
    subMode().dispatchEvent(new Event('change'));
    pending.filter((p) => p.mode === 'original').forEach((p) => p.resolve({ cached: true }));
    await flush();
    expect(cost().textContent).toBe('~0.064'); // stale probe ignored, estimate stands
    pending.filter((p) => p.mode === 'both').forEach((p) => p.resolve({ cached: true }));
    await flush();
    expect(cost().textContent).toBe('aiCorrectFree');
  });

  it('drops pricing that lands after switching sessions', async () => {
    const d = deferred<{ transcript_correction: { base: number; per_event: number } } | null>();
    api.fetchAiPricing.mockReturnValueOnce(d.promise);
    const mod = await load();
    mod.openSessionScreen(ref());
    mod.openSessionScreen(ref({ id: 's2', event_count: 3 }));
    await flush();
    d.resolve({ transcript_correction: { base: 9, per_event: 9 } });
    await flush();
    expect(cost().textContent).toBe('~0.053'); // s2's fallback estimate, not 9-credit pricing
  });
});

describe('downloads (#222)', () => {
  it('downloads without correction, forwarding the subtitle mode', async () => {
    await open();
    subMode().value = 'translated';
    btn('session-dl-pdf').click();
    await flush();
    expect(auth.downloadTranscript).toHaveBeenCalledWith('s1', 'pdf', 'en', 'translated', false);
    expect(api.ensureCorrection).not.toHaveBeenCalled();
    expect(btn('session-dl-pdf').textContent).toBe('PDF');
    expect(btn('session-dl-pdf').disabled).toBe(false);
  });

  it('ensures the JSON correction (source text) before a corrected download', async () => {
    api.ensureCorrection.mockResolvedValue({ ok: true, cached: false, balance: 5 });
    await open();
    chk().checked = true;
    btn('session-dl-json').click();
    await flush();
    expect(api.ensureCorrection).toHaveBeenCalledWith('s1', 'original', '');
    expect(auth.setBalance).toHaveBeenCalledWith(5);
    expect(auth.downloadTranscript).toHaveBeenCalledWith('s1', 'json', 'en', 'original', true);
  });

  it('PDF corrects both texts; SRT follows the dropdown', async () => {
    await open();
    chk().checked = true;
    btn('session-dl-pdf').click();
    await flush();
    expect(api.ensureCorrection).toHaveBeenCalledWith('s1', 'both', 'en');
    subMode().value = 'translated';
    btn('session-dl-srt').click();
    await flush();
    expect(api.ensureCorrection).toHaveBeenCalledWith('s1', 'translated', 'en');
    expect(auth.downloadTranscript).toHaveBeenCalledTimes(2);
  });

  it('maps correction failures to rate-limit / no-credits / generic messages', async () => {
    await open();
    chk().checked = true;
    api.ensureCorrection.mockResolvedValueOnce({ ok: false, cached: false, rateLimited: true });
    btn('session-dl-vtt').click();
    await flush();
    expect(status().textContent).toBe('downloadRateLimited');

    api.ensureCorrection.mockResolvedValueOnce({
      ok: false,
      cached: false,
      insufficient: { required: 1, available: 0 },
    });
    btn('session-dl-vtt').click();
    await flush();
    expect(status().textContent).toBe('aiCorrectNoCredits');

    api.ensureCorrection.mockResolvedValueOnce({ ok: false, cached: false });
    btn('session-dl-vtt').click();
    await flush();
    expect(status().textContent).toBe('aiCorrectFailed');
    expect(auth.downloadTranscript).not.toHaveBeenCalled();
  });

  it('flags a rate-limited or failed download', async () => {
    await open();
    auth.downloadTranscript.mockResolvedValueOnce({ ok: false, status: 429 });
    btn('session-dl-srt').click();
    await flush();
    expect(status().textContent).toBe('downloadRateLimited');

    auth.downloadTranscript.mockResolvedValueOnce({ ok: false, status: 500 });
    btn('session-dl-srt').click();
    await flush();
    expect(status().textContent).toBe('downloadFailed');
  });

  it('serializes downloads: one in flight disables all four buttons', async () => {
    const d = deferred<{ ok: boolean; status: number }>();
    auth.downloadTranscript.mockReturnValueOnce(d.promise);
    await open();
    btn('session-dl-pdf').click();
    await flush();
    expect(btn('session-dl-pdf').textContent).toBe('processing');
    expect(btn('session-dl-pdf').classList.contains('btn-loading')).toBe(true);
    for (const f of ['pdf', 'json', 'srt', 'vtt']) {
      expect(btn(`session-dl-${f}`).disabled).toBe(true);
    }
    btn('session-dl-json').dispatchEvent(new MouseEvent('click')); // rapid second click
    await flush();
    d.resolve({ ok: true, status: 200 });
    await flush();
    expect(auth.downloadTranscript).toHaveBeenCalledTimes(1);
    expect(btn('session-dl-pdf').textContent).toBe('PDF');
    expect(btn('session-dl-pdf').classList.contains('btn-loading')).toBe(false);
    for (const f of ['pdf', 'json', 'srt', 'vtt']) {
      expect(btn(`session-dl-${f}`).disabled).toBe(false);
    }
  });

  it('abandons a correction that finishes after leaving the screen', async () => {
    const d = deferred<{ ok: boolean; cached: boolean }>();
    api.ensureCorrection.mockReturnValueOnce(d.promise);
    const mod = await open();
    chk().checked = true;
    btn('session-dl-pdf').click();
    await flush();
    mod.closeSessionScreen();
    d.resolve({ ok: true, cached: true });
    await flush();
    expect(auth.downloadTranscript).not.toHaveBeenCalled();
    expect(btn('session-dl-pdf').textContent).toBe('PDF'); // finally restored
  });

  it('a click with no open session is inert', async () => {
    const mod = await open();
    mod.closeSessionScreen();
    btn('session-dl-pdf').dispatchEvent(new MouseEvent('click'));
    await flush();
    expect(auth.downloadTranscript).not.toHaveBeenCalled();
  });

  it('auto-clears the transient status unless something replaced it', async () => {
    await open();
    vi.useFakeTimers();
    auth.downloadTranscript.mockResolvedValueOnce({ ok: false, status: 500 });
    btn('session-dl-pdf').click();
    await flush();
    expect(status().textContent).toBe('downloadFailed');
    await vi.advanceTimersByTimeAsync(4000);
    expect(status().textContent).toBe('');

    auth.downloadTranscript.mockResolvedValueOnce({ ok: false, status: 500 });
    btn('session-dl-pdf').click();
    await flush();
    status().textContent = 'newer message';
    await vi.advanceTimersByTimeAsync(4000);
    expect(status().textContent).toBe('newer message'); // unchanged guard
  });
});
