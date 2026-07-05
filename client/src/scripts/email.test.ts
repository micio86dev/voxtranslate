// @vitest-environment jsdom
// Unit tests for the follow-up email composer (spec 0016). The API layer
// (pricing / latest / draft / send) is mocked at the module boundary — the
// async-job polling inside api.ts is that module's concern; here we exercise
// the slot's rendering, recipient chips, generation and send flows.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const api = vi.hoisted(() => ({
  fetchAiPricing: vi.fn(),
  fetchLatestEmail: vi.fn(),
  generateEmailDraft: vi.fn(),
  sendEmail: vi.fn(),
}));
const toastSpy = vi.hoisted(() => vi.fn());
vi.mock('./api', () => api);
vi.mock('./toast', () => ({ toast: toastSpy }));

import type { AiEmail } from './api';
import * as auth from './auth';
import { initEmailSlot, updateEmailContext } from './email';
import { t } from './i18n';

/** Let the slot's fire-and-forget fetch callbacks settle. */
const flush = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

const USER = { id: 'u1', email: 'me@x.co', name: 'Me', balance: 5 };

const DRAFT: AiEmail = {
  id: 'em1',
  status: 'draft',
  subject: 'Recap',
  body_text: 'Hello all',
  recipients: [
    { kind: 'participant', name: 'Anna', cc: false },
    { kind: 'email', email: 'ext@x.com', cc: false },
    { kind: 'email', email: 'boss@x.com', cc: true },
  ],
  tone: 'friendly',
  lang: 'it',
  guidelines: 'keep it short',
  cost: 0.5,
};

function q<T extends HTMLElement>(sel: string): T {
  const node = document.querySelector(sel);
  if (!node) throw new Error(`missing ${sel}`);
  return node as T;
}

const slot = (): HTMLElement => q('#ai-email-slot');
const section = (): HTMLElement => q('#ai-email-slot .ai-section');
const costEl = (): HTMLElement => q('.ai-cost');
const form = (): HTMLElement => q('.ai-report-form');
const genBtn = (): HTMLButtonElement => q('.ai-report-form .ai-generate');
const formStatus = (): HTMLElement => q('.ai-report-form .status-line');
const view = (): HTMLElement => q('.ai-report-view');
const rcpts = (): HTMLElement => q('.ai-email-rcpts');
const subjectIn = (): HTMLInputElement => q('.ai-email-subject');
const bodyIn = (): HTMLTextAreaElement => q('.ai-email-body');
const sendBtn = (): HTMLButtonElement => q('.ai-report-actions .ai-generate');
const regenBtn = (): HTMLButtonElement => q('.ai-report-actions .btn-ghost');
const meta = (): HTMLElement => q('.ai-report-meta');
const viewStatus = (): HTMLElement => q('.ai-report-view .status-line');
const toInput = (): HTMLInputElement =>
  document.querySelectorAll('.ai-email-chips')[0]!.querySelector('input') as HTMLInputElement;
const ccInput = (): HTMLInputElement =>
  document.querySelectorAll('.ai-email-chips')[1]!.querySelector('input') as HTMLInputElement;
const toneSel = (): HTMLSelectElement =>
  document.querySelectorAll('#ai-email-slot select')[0] as HTMLSelectElement;
const langSel = (): HTMLSelectElement =>
  document.querySelectorAll('#ai-email-slot select')[1] as HTMLSelectElement;
const summaryBox = (): HTMLInputElement => q('.ai-check input');
const guide = (): HTMLTextAreaElement => q('.ai-guidelines');

/** Type an address into a chip input and press Enter. */
function typeEmail(input: HTMLInputElement, value: string): void {
  input.value = value;
  input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', cancelable: true }));
}

async function init(id = 's1', events = 12): Promise<void> {
  initEmailSlot({ id, event_count: events });
  await flush();
}

beforeEach(() => {
  vi.clearAllMocks();
  document.body.innerHTML = '<div id="ai-email-slot"></div><span id="account-balance"></span>';
  auth.saveSession('tok', { ...USER });
  api.fetchAiPricing.mockResolvedValue({ email_enabled: true, email: { draft: 0.5 } });
  api.fetchLatestEmail.mockResolvedValue(null);
  api.generateEmailDraft.mockResolvedValue({ email: null, insufficient: null, error: '' });
  api.sendEmail.mockResolvedValue({ sent: null, error: '' });
});

afterEach(() => {
  auth.clearSession();
});

describe('initEmailSlot gating', () => {
  it('renders nothing for guests', async () => {
    auth.clearSession();
    await init();
    expect(slot().children.length).toBe(0);
    expect(api.fetchAiPricing).not.toHaveBeenCalled();
  });

  it('renders nothing for empty sessions', async () => {
    await init('s1', 0);
    expect(slot().children.length).toBe(0);
  });

  it('stays hidden when the backend cannot send email', async () => {
    api.fetchAiPricing.mockResolvedValue({ email_enabled: false, email: { draft: 0.5 } });
    await init();
    expect(section().hidden).toBe(true);
  });

  it('stays hidden when pricing is unavailable', async () => {
    api.fetchAiPricing.mockResolvedValue(null);
    await init();
    expect(section().hidden).toBe(true);
  });

  it('shows the section with the draft cost once pricing confirms', async () => {
    await init();
    expect(section().hidden).toBe(false);
    expect(costEl().textContent).toBe('~$0.50');
    expect(genBtn().disabled).toBe(false);
    expect(genBtn().title).toBe('');
  });

  it('disables Draft (with an explanation) when the balance is short', async () => {
    auth.saveSession('tok', { ...USER, balance: 0.1 });
    await init();
    expect(genBtn().disabled).toBe(true);
    expect(genBtn().title).toBe(
      t('aiReportInsufficient').replace('{need}', '$0.50').replace('{have}', '$0.10'),
    );
  });

  it('drops a pricing response that lands after navigating to another session', async () => {
    let resolvePricing!: (v: unknown) => void;
    api.fetchAiPricing.mockReturnValueOnce(
      new Promise((resolve) => {
        resolvePricing = resolve;
      }),
    );
    initEmailSlot({ id: 's1', event_count: 3 });
    const stale = section();
    initEmailSlot({ id: 's2', event_count: 3 });
    resolvePricing({ email_enabled: true, email: { draft: 0.5 } });
    await flush();
    expect(stale.hidden).toBe(true); // the guard kept the stale section hidden
  });

  it('drops a latest-email response that lands after navigating away', async () => {
    let resolveLatest!: (v: unknown) => void;
    api.fetchLatestEmail.mockReturnValueOnce(
      new Promise((resolve) => {
        resolveLatest = resolve;
      }),
    );
    initEmailSlot({ id: 's1', event_count: 3 });
    const staleView = view();
    initEmailSlot({ id: 's2', event_count: 3 });
    resolveLatest(DRAFT);
    await flush();
    expect(staleView.hidden).toBe(true); // never switched to the draft view
  });
});

describe('recipients', () => {
  it('ignores a participant roster for another session', async () => {
    await init('s1');
    updateEmailContext('other', [{ id: 'p1', name: 'Anna' }]);
    expect(document.querySelectorAll('.ai-chip[data-peer]').length).toBe(0);
  });

  it('paints participant chips, toggles selection, and repaints without duplicates', async () => {
    await init('s1');
    updateEmailContext('s1', [
      { id: 'p1', name: 'Anna' },
      { id: 'p2', name: 'Ben' },
    ]);
    const chips = document.querySelectorAll<HTMLButtonElement>('.ai-chip[data-peer]');
    expect(chips.length).toBe(2);
    expect(chips[0]!.textContent).toBe('Anna');
    expect(chips[0]!.getAttribute('aria-pressed')).toBe('false');
    chips[0]!.click();
    expect(chips[0]!.getAttribute('aria-pressed')).toBe('true');
    // Repaint (e.g. the roster fetch lands again): selection survives, no dupes.
    updateEmailContext('s1', [
      { id: 'p1', name: 'Anna' },
      { id: 'p2', name: 'Ben' },
    ]);
    const repainted = document.querySelectorAll<HTMLButtonElement>('.ai-chip[data-peer]');
    expect(repainted.length).toBe(2);
    expect(repainted[0]!.getAttribute('aria-pressed')).toBe('true');
    repainted[0]!.click();
    expect(repainted[0]!.getAttribute('aria-pressed')).toBe('false');
  });

  it('turns typed addresses into removable chips, rejecting invalid input', async () => {
    await init();
    const input = toInput();
    // Non-Enter keys pass through.
    input.value = 'a';
    input.dispatchEvent(new KeyboardEvent('keydown', { key: 'a', cancelable: true }));
    expect(document.querySelectorAll('.ai-chip.removable').length).toBe(0);
    // Blank Enter is a no-op.
    typeEmail(input, '   ');
    expect(input.classList.contains('invalid')).toBe(false);
    // Invalid address flags the input; typing clears the flag.
    typeEmail(input, 'not-an-email');
    expect(input.classList.contains('invalid')).toBe(true);
    input.dispatchEvent(new Event('input'));
    expect(input.classList.contains('invalid')).toBe(false);
    // A valid address becomes a lowercased chip and clears the input.
    typeEmail(input, 'Ext@X.com');
    const chip = q<HTMLButtonElement>('.ai-chip.removable');
    expect(chip.textContent).toBe('ext@x.com ×');
    expect(input.value).toBe('');
    // Duplicates are ignored.
    typeEmail(input, 'ext@x.com');
    expect(document.querySelectorAll('.ai-chip.removable').length).toBe(1);
    // Clicking the chip removes it.
    chip.click();
    expect(document.querySelectorAll('.ai-chip.removable').length).toBe(0);
  });
});

describe('draft generation', () => {
  it('requires at least one To recipient', async () => {
    await init();
    genBtn().click();
    await flush();
    expect(formStatus().textContent).toBe(t('aiEmailNoRecipients'));
    expect(api.generateEmailDraft).not.toHaveBeenCalled();
  });

  it('sends the full request and shows the draft + new balance on success', async () => {
    await init('s1');
    updateEmailContext('s1', [{ id: 'p1', name: 'Anna' }]);
    q<HTMLButtonElement>('.ai-chip[data-peer="p1"]').click();
    typeEmail(toInput(), 'to@x.co');
    typeEmail(ccInput(), 'cc@x.co');
    toneSel().value = 'concise';
    langSel().value = 'it';
    guide().value = 'focus';
    summaryBox().checked = false;
    api.generateEmailDraft.mockResolvedValue({
      email: { ...DRAFT, balance: 0.25 },
      insufficient: null,
      error: '',
    });

    genBtn().click();
    // In-flight state is synchronous.
    expect(genBtn().disabled).toBe(true);
    expect(genBtn().classList.contains('btn-loading')).toBe(true);
    expect(formStatus().textContent).toBe(t('aiEmailGenerating'));
    await flush();

    expect(api.generateEmailDraft).toHaveBeenCalledWith('s1', {
      recipients: [
        { kind: 'participant', peer_id: 'p1' },
        { kind: 'email', email: 'to@x.co', cc: false },
        { kind: 'email', email: 'cc@x.co', cc: true },
      ],
      tone: 'concise',
      guidelines: 'focus',
      lang: 'it',
      includeSummary: false,
    });
    // The charged balance lands in the account bar + cached user.
    expect(auth.getUser()?.balance).toBe(0.25);
    const bal = q<HTMLElement>('#account-balance');
    expect(bal.textContent).toBe('$0.25');
    expect(bal.classList.contains('low')).toBe(true);
    // The draft view replaces the form, prefilled from the server echo.
    expect(form().hidden).toBe(true);
    expect(view().hidden).toBe(false);
    expect(rcpts().textContent).toBe(`${t('aiEmailTo')}: Anna, ext@x.com · CC: boss@x.com`);
    expect(subjectIn().value).toBe('Recap');
    expect(bodyIn().value).toBe('Hello all');
    expect(sendBtn().hidden).toBe(false);
    expect(sendBtn().disabled).toBe(false);
    expect(meta().textContent).toBe('$0.50');
    expect(toastSpy).toHaveBeenCalledWith(`✓ ${t('aiEmailTitle')}`, 'ok');
  });

  it('shows the insufficient-credits message from a 402', async () => {
    await init();
    typeEmail(toInput(), 'a@x.co');
    api.generateEmailDraft.mockResolvedValue({
      email: null,
      insufficient: { error: 'insufficient_credits', required: 0.5, available: 0.1, feature: 'email' },
      error: '',
    });
    genBtn().click();
    await flush();
    const msg = t('aiReportInsufficient').replace('{need}', '$0.50').replace('{have}', '$0.10');
    expect(formStatus().textContent).toBe(msg);
    expect(toastSpy).toHaveBeenCalledWith(msg, 'err');
    expect(genBtn().classList.contains('btn-loading')).toBe(false);
  });

  it('surfaces a server error message', async () => {
    await init();
    typeEmail(toInput(), 'a@x.co');
    api.generateEmailDraft.mockResolvedValue({ email: null, insufficient: null, error: 'boom' });
    genBtn().click();
    await flush();
    expect(formStatus().textContent).toBe('boom');
    expect(toastSpy).toHaveBeenCalledWith('boom', 'err');
  });

  it('falls back to the localized failure message', async () => {
    await init();
    typeEmail(toInput(), 'a@x.co');
    genBtn().click(); // default mock: { email: null, insufficient: null, error: '' }
    await flush();
    expect(formStatus().textContent).toBe(t('aiEmailFailed'));
    expect(genBtn().disabled).toBe(false);
  });

  it('drops a result that lands after navigating to another session', async () => {
    await init('s1');
    typeEmail(toInput(), 'a@x.co');
    let resolveGen!: (v: unknown) => void;
    api.generateEmailDraft.mockReturnValueOnce(
      new Promise((resolve) => {
        resolveGen = resolve;
      }),
    );
    const staleBtn = genBtn();
    const staleStatus = formStatus();
    staleBtn.click();
    await init('s2');
    resolveGen({ email: { ...DRAFT }, insufficient: null, error: '' });
    await flush();
    expect(staleBtn.disabled).toBe(true); // guard returned before re-enabling
    expect(staleStatus.textContent).toBe(t('aiEmailGenerating'));
    expect(toastSpy).not.toHaveBeenCalled();
  });

  it('ignores clicks while Draft is disabled (broke)', async () => {
    auth.saveSession('tok', { ...USER, balance: 0 });
    await init();
    typeEmail(toInput(), 'a@x.co');
    genBtn().click();
    await flush();
    expect(api.generateEmailDraft).not.toHaveBeenCalled();
  });
});

describe('existing email + sending', () => {
  it('shows an editable existing draft with the form prefilled for Regenerate', async () => {
    api.fetchLatestEmail.mockResolvedValue(DRAFT);
    await init();
    expect(form().hidden).toBe(true);
    expect(view().hidden).toBe(false);
    expect(subjectIn().value).toBe('Recap');
    expect(subjectIn().readOnly).toBe(false);
    expect(toneSel().value).toBe('friendly');
    expect(langSel().value).toBe('it');
    expect(guide().value).toBe('keep it short');
    expect(sendBtn().disabled).toBe(false);
    expect(meta().textContent).toBe('$0.50'); // what the draft cost
  });

  it('renders a sent email read-only with the send date', async () => {
    const sentAt = '2026-07-01T10:00:00Z';
    api.fetchLatestEmail.mockResolvedValue({
      ...DRAFT,
      status: 'sent',
      sent_at: sentAt,
      lang: 'xx', // unsupported → the language select keeps its default
      recipients: [{ kind: 'participant', name: 'Anna', cc: false }],
    });
    await init();
    expect(rcpts().textContent).toBe(`${t('aiEmailTo')}: Anna`); // no CC part
    expect(subjectIn().readOnly).toBe(true);
    expect(bodyIn().readOnly).toBe(true);
    expect(sendBtn().hidden).toBe(true);
    expect(langSel().value).toBe('en');
    expect(meta().textContent).toBe(
      `✓ ${t('aiEmailSent')} · ${new Date(sentAt).toLocaleString()}`,
    );
    expect(costEl().textContent).toBe(''); // no price tag on a sent email
  });

  it('cannot send an unsaved draft (no id)', async () => {
    api.fetchLatestEmail.mockResolvedValue({ ...DRAFT, id: undefined });
    await init();
    expect(sendBtn().disabled).toBe(true);
    sendBtn().click();
    await flush();
    expect(api.sendEmail).not.toHaveBeenCalled();
  });

  it('sends only the edited fields and flips to the sent view', async () => {
    api.fetchLatestEmail.mockResolvedValue(DRAFT);
    await init('s1');
    subjectIn().value = 'New subject';
    bodyIn().value = 'New body';
    api.sendEmail.mockResolvedValue({
      sent: { id: 'em1', status: 'sent', resend_id: 'r1', sent_at: '2026-07-02T09:30:00Z' },
      error: '',
    });
    sendBtn().click();
    expect(sendBtn().disabled).toBe(true);
    expect(viewStatus().textContent).toBe(t('aiEmailSending'));
    await flush();
    expect(api.sendEmail).toHaveBeenCalledWith('s1', 'em1', {
      subject: 'New subject',
      body_text: 'New body',
    });
    expect(viewStatus().textContent).toBe('');
    expect(subjectIn().value).toBe('New subject');
    expect(subjectIn().readOnly).toBe(true);
    expect(sendBtn().hidden).toBe(true);
    expect(meta().textContent).toContain(`✓ ${t('aiEmailSent')}`);
    expect(toastSpy).toHaveBeenCalledWith(`✓ ${t('aiEmailSent')}`, 'ok');
  });

  it('does not send a blank subject or body', async () => {
    api.fetchLatestEmail.mockResolvedValue(DRAFT);
    await init();
    subjectIn().value = '   ';
    sendBtn().click();
    await flush();
    expect(api.sendEmail).not.toHaveBeenCalled();
  });

  it('shows the server send error and keeps the draft editable', async () => {
    api.fetchLatestEmail.mockResolvedValue(DRAFT);
    await init();
    api.sendEmail.mockResolvedValue({ sent: null, error: 'smtp down' });
    sendBtn().click();
    await flush();
    expect(viewStatus().textContent).toBe('smtp down');
    expect(toastSpy).toHaveBeenCalledWith('smtp down', 'err');
    expect(sendBtn().disabled).toBe(false);
  });

  it('falls back to the localized send failure on a network blip', async () => {
    api.fetchLatestEmail.mockResolvedValue(DRAFT);
    await init();
    sendBtn().click(); // default mock: { sent: null, error: '' }
    await flush();
    expect(viewStatus().textContent).toBe(t('aiEmailSendFailed'));
  });

  it('drops a send result that lands after navigating away', async () => {
    api.fetchLatestEmail.mockResolvedValueOnce(DRAFT);
    await init('s1');
    let resolveSend!: (v: unknown) => void;
    api.sendEmail.mockReturnValueOnce(
      new Promise((resolve) => {
        resolveSend = resolve;
      }),
    );
    const staleSend = sendBtn();
    const staleStatus = viewStatus();
    staleSend.click();
    await init('s2');
    resolveSend({
      sent: { id: 'em1', status: 'sent', resend_id: 'r1', sent_at: '2026-07-02T09:30:00Z' },
      error: '',
    });
    await flush();
    expect(staleSend.disabled).toBe(true);
    expect(staleStatus.textContent).toBe(t('aiEmailSending'));
    expect(toastSpy).not.toHaveBeenCalled();
  });

  it('Regenerate reopens the prefilled form and restores the price tag', async () => {
    api.fetchLatestEmail.mockResolvedValue(DRAFT);
    await init();
    regenBtn().click();
    expect(form().hidden).toBe(false);
    expect(view().hidden).toBe(true);
    expect(toneSel().value).toBe('friendly'); // prefill survives
    expect(costEl().textContent).toBe('~$0.50');
  });
});
