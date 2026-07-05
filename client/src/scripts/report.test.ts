// @vitest-environment jsdom
//
// AI session report slot (spec 0014): form/viewer lifecycle, cost preview and
// balance gating, the generate → job outcome branches (success / 402 / error),
// and the stale-session guards. API + auth + toast are mocked; report-md stays
// real (pure).
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { AiReport } from './api';
import type { ReportSlotRef } from './report';

const api = vi.hoisted(() => ({
  fetchAiPricing: vi.fn(),
  fetchLatestReport: vi.fn(),
  generateReport: vi.fn(),
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
  getUiLang: () => 'en',
  SUPPORTED: ['en', 'it'],
  ENDONYM: { en: 'English', it: 'Italiano' },
  t: (key: string) => (key === 'aiReportInsufficient' ? 'need {need} have {have}' : key),
}));

const toastMock = vi.hoisted(() => ({ toast: vi.fn() }));
vi.mock('./toast', () => toastMock);

const SCAFFOLD = `
  <div id="session-ai" class="hidden"></div>
  <div id="ai-report-slot"></div>
  <span id="account-balance"></span>
`;

const $ = (id: string): HTMLElement => document.getElementById(id) as HTMLElement;
const slot = () => $('ai-report-slot');
const card = () => $('session-ai');
const genBtn = () => slot().querySelector('.ai-generate') as HTMLButtonElement;
const costEl = () => slot().querySelector('.ai-cost') as HTMLElement;
const statusEl = () => slot().querySelector('.status-line') as HTMLElement;
const form = () => slot().querySelector('.ai-report-form') as HTMLElement;
const view = () => slot().querySelector('.ai-report-view') as HTMLElement;
const md = () => slot().querySelector('.ai-report-md') as HTMLElement;
const meta = () => slot().querySelector('.ai-report-meta') as HTMLElement;
const selects = () => slot().querySelectorAll('select');
const fmtSel = () => selects()[0] as HTMLSelectElement;
const langSel = () => selects()[1] as HTMLSelectElement;
const guide = () => slot().querySelector('textarea') as HTMLTextAreaElement;
const actionBtns = () => view().querySelectorAll('button');
const copyBtn = () => actionBtns()[0] as HTMLButtonElement;
const regenBtn = () => actionBtns()[1] as HTMLButtonElement;

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

const ref = (over: Partial<ReportSlotRef> = {}): ReportSlotRef => ({
  id: 'r1',
  started_at: '2026-01-01T10:00:00.000Z',
  ended_at: '2026-01-01T10:30:00.000Z', // 1800s → 30 min
  event_count: 5,
  ...over,
});

const REPORT: AiReport = {
  id: 'rep1',
  format: 'freeform',
  lang: 'it',
  guidelines: 'brief',
  markdown: '## Summary\n\nAll good.',
  model: 'vox-r1',
  cost: 0.2,
  created_at: '2026-01-02T09:00:00.000Z',
};

async function init(r: ReportSlotRef = ref()) {
  const mod = await import('./report');
  mod.initReportSlot(r);
  await flush();
  return mod;
}

beforeEach(() => {
  vi.resetModules();
  vi.clearAllMocks();
  document.body.innerHTML = SCAFFOLD;
  auth.isLoggedIn.mockReturnValue(true);
  auth.getUser.mockReturnValue({ balance: 10 });
  auth.formatCredits.mockImplementation((n: number) => n.toFixed(3));
  api.fetchAiPricing.mockResolvedValue({ report: { base: 0.05, per_minute: 0.002 } });
  api.fetchLatestReport.mockResolvedValue(null);
  api.generateReport.mockResolvedValue({ report: null, insufficient: null, error: '' });
});

afterEach(() => {
  vi.useRealTimers();
});

describe('initReportSlot', () => {
  it('hides the whole AI card for guests', async () => {
    auth.isLoggedIn.mockReturnValue(false);
    await init();
    expect(card().classList.contains('hidden')).toBe(true);
    expect(slot().children).toHaveLength(0);
  });

  it('hides the card for an empty session', async () => {
    await init(ref({ event_count: 0 }));
    expect(card().classList.contains('hidden')).toBe(true);
  });

  it('builds the form with format + language selectors and a cost preview', async () => {
    await init();
    expect(card().classList.contains('hidden')).toBe(false);
    expect(Array.from(fmtSel().options).map((o) => o.value)).toEqual(['structured', 'freeform']);
    expect(Array.from(langSel().options).map((o) => o.value)).toEqual(['en', 'it']);
    expect(langSel().options[1].textContent).toBe('Italiano');
    expect(langSel().value).toBe('en');
    // base 0.05 + 0.002 × 30 min = 0.110
    expect(costEl().textContent).toBe('~0.110');
    expect(genBtn().disabled).toBe(false);
    expect(genBtn().title).toBe('');
  });

  it('estimates from now when the session has not ended', async () => {
    await init(ref({ started_at: new Date(Date.now() - 30_000).toISOString(), ended_at: null }));
    expect(costEl().textContent).toBe('~0.052'); // ≤1 min floor
  });

  it('disables Generate when the balance cannot cover the estimate', async () => {
    auth.getUser.mockReturnValue({ balance: 0.05 });
    await init();
    expect(genBtn().disabled).toBe(true);
    expect(genBtn().title).toBe('need 0.110 have 0.050');
  });

  it('leaves the cost blank when pricing is unavailable', async () => {
    api.fetchAiPricing.mockResolvedValue(null);
    api.fetchLatestReport.mockResolvedValue(REPORT);
    await init();
    expect(costEl().textContent).toBe('');
    // A repaint without pricing (Regenerate) must also stay blank.
    regenBtn().click();
    expect(costEl().textContent).toBe('');
    expect(genBtn().disabled).toBe(false);
  });

  it('shows the latest stored report and prefills the form from it', async () => {
    api.fetchLatestReport.mockResolvedValue(REPORT);
    await init();
    expect(form().hidden).toBe(true);
    expect(view().hidden).toBe(false);
    expect(md().innerHTML).toContain('<h3>Summary</h3>');
    expect(meta().textContent).toContain('vox-r1');
    expect(meta().textContent).toContain('0.200');
    expect(fmtSel().value).toBe('freeform');
    expect(langSel().value).toBe('it');
    expect(guide().value).toBe('brief');
  });

  it('ignores unknown format/lang and missing fields when prefilling', async () => {
    api.fetchLatestReport.mockResolvedValue({
      format: 'weird',
      lang: 'xx',
      guidelines: null,
      markdown: 'x',
      model: '',
      cost: 0,
    });
    await init();
    expect(fmtSel().value).toBe('structured');
    expect(langSel().value).toBe('en');
    expect(guide().value).toBe('');
    expect(meta().textContent).toBe('0.000'); // model + date filtered out
  });

  it('drops a stale latest-report response after switching sessions', async () => {
    const d = deferred<AiReport | null>();
    api.fetchLatestReport.mockReturnValueOnce(d.promise).mockResolvedValueOnce(null);
    const mod = await import('./report');
    mod.initReportSlot(ref());
    mod.initReportSlot(ref({ id: 'r2' }));
    d.resolve(REPORT);
    await flush();
    expect(view().hidden).toBe(true); // r2's fresh form, not r1's report
  });
});

describe('generate', () => {
  it('posts the form, shows the report, patches the balance, and toasts', async () => {
    api.generateReport.mockResolvedValue({
      report: { ...REPORT, balance: 0.4 },
      insufficient: null,
      error: '',
    });
    await init();
    fmtSel().value = 'freeform';
    langSel().value = 'it';
    guide().value = 'focus on pricing';
    genBtn().click();
    expect(genBtn().disabled).toBe(true); // in flight
    expect(statusEl().textContent).toBe('aiReportGenerating');
    await flush();
    expect(api.generateReport).toHaveBeenCalledWith('r1', {
      format: 'freeform',
      lang: 'it',
      guidelines: 'focus on pricing',
    });
    expect(genBtn().disabled).toBe(false);
    expect(statusEl().textContent).toBe('');
    expect(view().hidden).toBe(false);
    expect(auth.setBalance).toHaveBeenCalledWith(0.4);
    expect($('account-balance').textContent).toBe('0.400');
    expect($('account-balance').classList.contains('low')).toBe(true); // < 0.5
    expect(toastMock.toast).toHaveBeenCalledWith('✓ aiReportTitle', 'ok');
  });

  it('survives a missing account-balance element and skips a missing balance', async () => {
    $('account-balance').remove();
    api.generateReport.mockResolvedValue({
      report: { ...REPORT, balance: 2 },
      insufficient: null,
      error: '',
    });
    await init();
    genBtn().click();
    await flush();
    expect(auth.setBalance).toHaveBeenCalledWith(2);

    // No balance on the report → no patch at all.
    auth.setBalance.mockClear();
    api.generateReport.mockResolvedValue({ report: REPORT, insufficient: null, error: '' });
    regenBtn().click();
    genBtn().click();
    await flush();
    expect(auth.setBalance).not.toHaveBeenCalled();
  });

  it('surfaces a 402 with the need/have message', async () => {
    api.generateReport.mockResolvedValue({
      report: null,
      insufficient: { required: 1, available: 0.25 },
      error: '',
    });
    await init();
    genBtn().click();
    await flush();
    expect(statusEl().textContent).toBe('need 1.000 have 0.250');
    expect(toastMock.toast).toHaveBeenCalledWith('need 1.000 have 0.250', 'err');
    expect(view().hidden).toBe(true);
  });

  it('shows the server error text, or the localized fallback when empty', async () => {
    api.generateReport.mockResolvedValueOnce({ report: null, insufficient: null, error: 'boom' });
    await init();
    genBtn().click();
    await flush();
    expect(statusEl().textContent).toBe('boom');
    expect(toastMock.toast).toHaveBeenCalledWith('boom', 'err');

    api.generateReport.mockResolvedValueOnce({ report: null, insufficient: null, error: '' });
    genBtn().click();
    await flush();
    expect(statusEl().textContent).toBe('aiReportFailed');
  });

  it('ignores a result that lands after switching sessions', async () => {
    const d = deferred<{ report: AiReport | null; insufficient: null; error: string }>();
    api.generateReport.mockReturnValueOnce(d.promise);
    const mod = await init();
    genBtn().click();
    mod.initReportSlot(ref({ id: 'r2' }));
    await flush();
    d.resolve({ report: REPORT, insufficient: null, error: '' });
    await flush();
    expect(toastMock.toast).not.toHaveBeenCalled();
  });

  it('a synthetic click on the disabled button never charges', async () => {
    auth.getUser.mockReturnValue({ balance: 0 });
    await init();
    expect(genBtn().disabled).toBe(true);
    genBtn().dispatchEvent(new MouseEvent('click'));
    await flush();
    expect(api.generateReport).not.toHaveBeenCalled();
  });
});

describe('viewer actions', () => {
  it('Regenerate reopens the prefilled form and focuses the guidelines', async () => {
    api.fetchLatestReport.mockResolvedValue(REPORT);
    await init();
    expect(form().hidden).toBe(true);
    regenBtn().click();
    expect(form().hidden).toBe(false);
    expect(document.activeElement).toBe(guide());
  });

  it('Copy writes the markdown and flips the label temporarily', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    api.fetchLatestReport.mockResolvedValue(REPORT);
    await init();
    vi.useFakeTimers();
    copyBtn().click();
    await flush();
    expect(writeText).toHaveBeenCalledWith(REPORT.markdown);
    expect(copyBtn().textContent).toBe('copied');
    await vi.advanceTimersByTimeAsync(1500);
    expect(copyBtn().textContent).toBe('copy');
  });

  it('Copy is a no-op before any report exists', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    await init();
    copyBtn().click();
    await flush();
    expect(writeText).not.toHaveBeenCalled();
  });

  it('a clipboard failure leaves the button label alone', async () => {
    const writeText = vi.fn().mockRejectedValue(new Error('denied'));
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    api.fetchLatestReport.mockResolvedValue(REPORT);
    await init();
    copyBtn().click();
    await flush();
    expect(copyBtn().textContent).toBe('copy');
  });
});
