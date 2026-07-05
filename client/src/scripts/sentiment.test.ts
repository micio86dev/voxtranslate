// @vitest-environment jsdom
//
// Sentiment slot (spec 0015): cost preview needs the transcript context, the
// run-once flow (result is server-cached), the mood/speaker/key-moment
// renderers, and the transcript jump. API + auth + chart + toast are mocked;
// requestAnimationFrame is stubbed synchronous.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { AiSentiment, SentimentResult } from './api';
import type { SentimentSlotRef } from './sentiment';

const api = vi.hoisted(() => ({
  fetchAiPricing: vi.fn(),
  fetchSentiment: vi.fn(),
  generateSentiment: vi.fn(),
}));
vi.mock('./api', () => api);

const auth = vi.hoisted(() => ({
  isLoggedIn: vi.fn(),
  getUser: vi.fn(),
  setBalance: vi.fn(),
  formatCredits: vi.fn(),
}));
vi.mock('./auth', () => auth);

vi.mock('./i18n', () => ({
  t: (key: string) => (key === 'aiReportInsufficient' ? 'need {need} have {have}' : key),
}));

const chart = vi.hoisted(() => ({ drawSentimentTimeline: vi.fn() }));
vi.mock('./sentiment-chart', () => chart);

const toastMock = vi.hoisted(() => ({ toast: vi.fn() }));
vi.mock('./toast', () => toastMock);

const SCAFFOLD = `
  <div id="ai-sentiment-slot"></div>
  <span id="account-balance"></span>
  <div id="session-transcript"></div>
`;

const START_ISO = '2026-01-01T10:00:00.000Z';
const START_MS = new Date(START_ISO).getTime();

const $ = (id: string): HTMLElement => document.getElementById(id) as HTMLElement;
const slot = () => $('ai-sentiment-slot');
const runBtn = () => slot().querySelector('.ai-generate') as HTMLButtonElement;
const costEl = () => slot().querySelector('.ai-cost') as HTMLElement;
const statusEl = () => slot().querySelector('.status-line') as HTMLElement;
const form = () => slot().querySelector('.ai-report-form') as HTMLElement;
const view = () => slot().querySelector('.ai-sentiment-view') as HTMLElement;
const overall = () => slot().querySelector('.ai-overall') as HTMLElement;
const speakers = () => slot().querySelectorAll('.ai-speaker-card');
const momentsHead = () => slot().querySelector('.ai-subhead') as HTMLElement;
const moments = () => slot().querySelectorAll<HTMLButtonElement>('.ai-moment');
const meta = () => slot().querySelector('.ai-report-meta') as HTMLElement;

const flush = async (): Promise<void> => {
  for (let i = 0; i < 10; i++) await Promise.resolve();
};

function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

const ref = (over: Partial<SentimentSlotRef> = {}): SentimentSlotRef => ({
  id: 's1',
  started_at: START_ISO,
  ended_at: '2026-01-01T10:20:00.000Z', // 1200s fallback duration
  event_count: 9,
  ...over,
});

const RESULT: SentimentResult = {
  overall: { score: 0.3, mood: 'positive' },
  timeline: [
    { t: 0, score: 0 },
    { t: 60, score: 0.5 },
  ],
  speakers: [
    { name: 'Ann', talk_pct: 60, score: 0.4, mood: 'positive' },
    { name: 'Bob', talk_pct: 40, score: null, mood: null },
  ],
  key_moments: [
    { t: 65, label: 'Deal closed', score: 0.8 },
    { t: 10, label: 'Pushback', score: -0.5 },
  ],
  window_secs: 30,
};

const sentiment = (over: Partial<AiSentiment> = {}): AiSentiment => ({
  result: RESULT,
  model: 'vox-s1',
  cost: 0.4,
  cached: true,
  created_at: '2026-01-02T09:00:00.000Z',
  ...over,
});

async function init(r: SentimentSlotRef = ref()) {
  const mod = await import('./sentiment');
  mod.initSentimentSlot(r);
  await flush();
  return mod;
}

beforeEach(() => {
  vi.resetModules();
  vi.clearAllMocks();
  document.body.innerHTML = SCAFFOLD;
  vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => {
    cb(0);
    return 1;
  });
  HTMLElement.prototype.scrollIntoView = vi.fn();
  auth.isLoggedIn.mockReturnValue(true);
  auth.getUser.mockReturnValue({ balance: 10 });
  auth.formatCredits.mockImplementation((n: number) => n.toFixed(3));
  api.fetchAiPricing.mockResolvedValue({
    sentiment: { base: 0.02, per_participant: 0.01, per_minute: 0.001 },
  });
  api.fetchSentiment.mockResolvedValue(null);
  api.generateSentiment.mockResolvedValue({ sentiment: null, insufficient: null, error: '' });
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('initSentimentSlot', () => {
  it('renders nothing for guests or empty sessions', async () => {
    auth.isLoggedIn.mockReturnValue(false);
    const mod = await init();
    expect(slot().children).toHaveLength(0);
    // Context updates for the never-built slot must not crash (repaint is null).
    mod.updateSentimentContext('s1', 2, 600, []);

    auth.isLoggedIn.mockReturnValue(true);
    mod.initSentimentSlot(ref({ event_count: 0 }));
    await flush();
    expect(slot().children).toHaveLength(0);
  });

  it('prices only once the transcript context (participants) is known', async () => {
    const mod = await init();
    expect(costEl().textContent).toBe(''); // pricing there, context missing
    mod.updateSentimentContext('s1', 2, 600, []);
    // base 0.02 + 2 × 0.01 + 10 min × 0.001 = 0.050
    expect(costEl().textContent).toBe('~0.050');
    expect(runBtn().disabled).toBe(false);
  });

  it('ignores context for a different session', async () => {
    const mod = await init();
    mod.updateSentimentContext('other', 2, 600, []);
    expect(costEl().textContent).toBe('');
  });

  it('gates Run on the cached balance', async () => {
    auth.getUser.mockReturnValue({ balance: 0.01 });
    const mod = await init();
    mod.updateSentimentContext('s1', 2, 600, []);
    expect(runBtn().disabled).toBe(true);
    expect(runBtn().title).toBe('need 0.050 have 0.010');
  });
});

describe('existing (cached) analysis', () => {
  it('shows the viewer with mood, speakers, moments, meta and the chart', async () => {
    api.fetchSentiment.mockResolvedValue(sentiment());
    await init();
    expect(form().hidden).toBe(true);
    expect(view().hidden).toBe(false);
    expect(costEl().textContent).toBe(''); // no price once shown
    expect(overall().textContent).toContain('😊');
    expect(overall().textContent).toContain('aiMoodPositive');
    expect(overall().textContent).toContain('+0.30');
    expect(speakers()).toHaveLength(2);
    expect(speakers()[0].textContent).toContain('😊 Ann');
    expect(speakers()[0].textContent).toContain('+0.40 · 60% aiSentimentTalk');
    expect(speakers()[1].textContent).toContain('Bob');
    expect(speakers()[1].textContent).toContain('— · 40% aiSentimentTalk');
    expect(momentsHead().hidden).toBe(false);
    expect(moments()).toHaveLength(2);
    expect(moments()[0].textContent).toContain('01:05');
    expect(moments()[0].textContent).toContain('Deal closed');
    expect(moments()[0].querySelector('.pos')).not.toBeNull();
    expect(moments()[1].querySelector('.neg')).not.toBeNull();
    expect(meta().textContent).toBe('vox-s1 · ' + new Date('2026-01-02T09:00:00.000Z').toLocaleString() + ' · aiSentimentCached');
    // No context yet → the ref-derived fallback duration feeds the chart.
    expect(chart.drawSentimentTimeline).toHaveBeenCalledWith(
      expect.any(HTMLCanvasElement),
      { points: RESULT.timeline, durationSeconds: 1200, bookmarks: [], keyMoments: RESULT.key_moments },
    );
  });

  it('redraws with bookmark markers once the transcript context lands', async () => {
    api.fetchSentiment.mockResolvedValue(sentiment());
    const mod = await init();
    chart.drawSentimentTimeline.mockClear();
    mod.updateSentimentContext('s1', 2, 600, [30, 90]);
    await flush();
    expect(chart.drawSentimentTimeline).toHaveBeenCalledWith(
      expect.any(HTMLCanvasElement),
      expect.objectContaining({ durationSeconds: 600, bookmarks: [30, 90] }),
    );
  });

  it('falls back for unknown moods and hides the moments head when none', async () => {
    api.fetchSentiment.mockResolvedValue(
      sentiment({
        cached: false,
        created_at: undefined,
        result: {
          ...RESULT,
          overall: { score: -0.1, mood: 'weird' },
          speakers: [{ name: 'Cara', talk_pct: 100, score: -0.2, mood: 'weird' }],
          key_moments: [],
        },
      }),
    );
    await init();
    expect(overall().textContent).toContain('😐'); // emoji fallback
    expect(overall().textContent).toContain('weird'); // raw mood label
    expect(overall().textContent).toContain('-0.10');
    expect(speakers()[0].textContent).toContain('Cara');
    expect(momentsHead().hidden).toBe(true);
    expect(moments()).toHaveLength(0);
    expect(meta().textContent).toBe('vox-s1 · 0.400'); // paid, no date
  });

  it('skips the chart when the frame lands after a session switch', async () => {
    let raf: FrameRequestCallback | null = null;
    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => {
      raf = cb;
      return 1;
    });
    api.fetchSentiment.mockResolvedValueOnce(sentiment()).mockResolvedValueOnce(null);
    const mod = await init();
    mod.initSentimentSlot(ref({ id: 's2' }));
    await flush();
    (raf as unknown as FrameRequestCallback)(0);
    expect(chart.drawSentimentTimeline).not.toHaveBeenCalled();
  });
});

describe('run', () => {
  it('runs the analysis, patches the balance, shows the result and toasts', async () => {
    api.generateSentiment.mockResolvedValue({
      sentiment: sentiment({ cached: false, balance: 0.3 }),
      insufficient: null,
      error: '',
    });
    const mod = await init();
    mod.updateSentimentContext('s1', 2, 600, []);
    runBtn().click();
    expect(runBtn().disabled).toBe(true);
    expect(statusEl().textContent).toBe('aiSentimentRunning');
    await flush();
    expect(api.generateSentiment).toHaveBeenCalledWith('s1');
    expect(statusEl().textContent).toBe('');
    expect(view().hidden).toBe(false);
    expect(auth.setBalance).toHaveBeenCalledWith(0.3);
    expect($('account-balance').textContent).toBe('0.300');
    expect($('account-balance').classList.contains('low')).toBe(true);
    expect(toastMock.toast).toHaveBeenCalledWith('✓ aiSentimentTitle', 'ok');
    expect(meta().textContent).toContain('0.400'); // charged cost, not "cached"
  });

  it('surfaces a 402 with the need/have message', async () => {
    api.generateSentiment.mockResolvedValue({
      sentiment: null,
      insufficient: { required: 2, available: 1 },
      error: '',
    });
    const mod = await init();
    mod.updateSentimentContext('s1', 2, 600, []);
    runBtn().click();
    await flush();
    expect(statusEl().textContent).toBe('need 2.000 have 1.000');
    expect(toastMock.toast).toHaveBeenCalledWith('need 2.000 have 1.000', 'err');
    expect(view().hidden).toBe(true);
  });

  it('shows the server error text, or the localized fallback when empty', async () => {
    api.generateSentiment.mockResolvedValueOnce({ sentiment: null, insufficient: null, error: 'boom' });
    const mod = await init();
    mod.updateSentimentContext('s1', 2, 600, []);
    runBtn().click();
    await flush();
    expect(statusEl().textContent).toBe('boom');

    api.generateSentiment.mockResolvedValueOnce({ sentiment: null, insufficient: null, error: '' });
    runBtn().click();
    await flush();
    expect(statusEl().textContent).toBe('aiSentimentFailed');
  });

  it('ignores a result that lands after switching sessions', async () => {
    const d = deferred<{ sentiment: AiSentiment | null; insufficient: null; error: string }>();
    api.generateSentiment.mockReturnValueOnce(d.promise);
    const mod = await init();
    mod.updateSentimentContext('s1', 2, 600, []);
    runBtn().click();
    mod.initSentimentSlot(ref({ id: 's2' }));
    await flush();
    d.resolve({ sentiment: sentiment(), insufficient: null, error: '' });
    await flush();
    expect(toastMock.toast).not.toHaveBeenCalled();
  });

  it('a synthetic click on the disabled button never charges', async () => {
    auth.getUser.mockReturnValue({ balance: 0 });
    const mod = await init();
    mod.updateSentimentContext('s1', 2, 600, []);
    expect(runBtn().disabled).toBe(true);
    runBtn().dispatchEvent(new MouseEvent('click'));
    await flush();
    expect(api.generateSentiment).not.toHaveBeenCalled();
  });
});

describe('key-moment transcript jump', () => {
  const addRow = (offsetMs: number): HTMLElement => {
    const row = document.createElement('div');
    row.className = 'tr-event';
    row.dataset.ts = String(START_MS + offsetMs);
    $('session-transcript').appendChild(row);
    return row;
  };

  it('scrolls + flashes the row closest to the moment offset', async () => {
    const far = addRow(0);
    addRow(50_000);
    const best = addRow(64_000); // moment t=65s → closest
    api.fetchSentiment.mockResolvedValue(sentiment());
    await init();
    vi.useFakeTimers();
    moments()[0].click(); // t = 65
    expect(best.classList.contains('tr-flash')).toBe(true);
    expect(far.classList.contains('tr-flash')).toBe(false);
    expect(best.scrollIntoView).toHaveBeenCalledWith({ behavior: 'smooth', block: 'center' });
    await vi.advanceTimersByTimeAsync(2000);
    expect(best.classList.contains('tr-flash')).toBe(false);
  });

  it('is a no-op with an empty transcript', async () => {
    api.fetchSentiment.mockResolvedValue(sentiment());
    await init();
    expect(() => moments()[0].click()).not.toThrow();
  });

  it('ignores clicks from a stale slot after a session switch', async () => {
    const row = addRow(64_000);
    api.fetchSentiment.mockResolvedValueOnce(sentiment()).mockResolvedValueOnce(null);
    const mod = await init();
    const staleMoment = moments()[0];
    mod.initSentimentSlot(ref({ id: 's2' }));
    await flush();
    staleMoment.click();
    expect(row.classList.contains('tr-flash')).toBe(false);
  });
});
