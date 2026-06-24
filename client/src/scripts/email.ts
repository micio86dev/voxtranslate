// Follow-up email composer (spec 0016): rendered into #ai-email-slot on the
// session detail screen. POST /api/sessions/{id}/email-draft charges a flat
// credit price (from /api/billing/ai-pricing); sending the draft is free.
// The whole section stays hidden when the backend has no Resend credentials
// (`email_enabled: false` — the endpoints would 503).

import * as auth from './auth';
import {
  fetchAiPricing,
  fetchLatestEmail,
  generateEmailDraft,
  sendEmail,
  type AiEmail,
  type EmailRecipient,
} from './api';
import { buildRecipientRefs, validEmail } from './email-utils';
import { ENDONYM, SUPPORTED, getUiLang, t } from './i18n';
import { toast } from './toast';

const $ = (id: string) => document.getElementById(id) as HTMLElement;

function el(tag: string, className: string): HTMLElement {
  const node = document.createElement(tag);
  node.className = className;
  return node;
}

function insufficientMsg(required: number, available: number): string {
  return t('aiReportInsufficient')
    .replace('{need}', auth.formatCredits(required))
    .replace('{have}', auth.formatCredits(available));
}

/** Patch the cached balance + account-bar display (we run outside app.ts). */
function applyBalance(balance: number): void {
  auth.setBalance(balance);
  const bal = document.getElementById('account-balance');
  if (bal) {
    bal.textContent = auth.formatCredits(balance);
    bal.classList.toggle('low', balance < 0.5);
  }
}

/** Session id the slot currently belongs to — guards late fetch callbacks. */
let active = '';
/** Set by `updateEmailContext` once the transcript doc is loaded. */
let participants: { id: string; name: string }[] = [];
let repaint: (() => void) | null = null;

/** What the slot needs from the session screen. */
export interface EmailSlotRef {
  id: string;
  event_count: number;
}

/**
 * The To-chips need the participant roster (peer id + name), which only the
 * transcript doc knows — the session screen calls this once that fetch lands.
 */
export function updateEmailContext(
  sessionId: string,
  list: { id: string; name: string }[],
): void {
  if (active !== sessionId) return;
  participants = list;
  repaint?.();
}

/** (Re)build the email section for a session. */
export function initEmailSlot(ref: EmailSlotRef): void {
  const slot = $('ai-email-slot');
  slot.innerHTML = '';
  active = ref.id;
  participants = [];
  repaint = null;
  // The whole #session-ai card is hidden by the report slot for guests/empty
  // sessions; mirror the check so this module never depends on that ordering.
  if (!auth.isLoggedIn() || ref.event_count === 0) return;
  build(slot, ref.id);
}

/** `To: Anna, ext@x.com · CC: boss@x.com` from the server's sanitized echo. */
function recipientsLine(recipients: EmailRecipient[]): string {
  const fmt = (r: EmailRecipient): string => (r.kind === 'participant' ? r.name : r.email);
  const to = recipients.filter((r) => !r.cc).map(fmt);
  const cc = recipients.filter((r) => r.cc).map(fmt);
  const parts = [`${t('aiEmailTo')}: ${to.join(', ')}`];
  if (cc.length) parts.push(`CC: ${cc.join(', ')}`);
  return parts.join(' · ');
}

function build(slot: HTMLElement, sessionId: string): void {
  const section = el('div', 'ai-section');
  section.hidden = true; // until pricing confirms the feature is enabled

  const head = el('div', 'ai-section-head');
  const title = el('span', 'ai-section-title');
  title.textContent = `✉️ ${t('aiEmailTitle')}`;
  const costEl = el('span', 'ai-cost mono');
  head.append(title, costEl);

  // ---- composer form (hidden once a draft exists; Regenerate reopens it) ----
  const form = el('div', 'ai-report-form');

  // To: participant toggle chips + raw addresses typed into the inline input.
  const selected = new Set<string>(); // participant peer ids
  const toEmails: string[] = [];
  const ccEmails: string[] = [];

  const toChips = el('div', 'ai-email-chips');
  const ccChips = el('div', 'ai-email-chips');

  const chipInput = (list: string[], host: HTMLElement): HTMLInputElement => {
    const input = document.createElement('input');
    input.type = 'email';
    input.className = 'ai-email-input';
    input.placeholder = t('aiEmailEmailPlaceholder');
    input.addEventListener('keydown', (e) => {
      if (e.key !== 'Enter') return;
      e.preventDefault();
      const email = input.value.trim().toLowerCase();
      if (!email) return;
      if (!validEmail(email)) {
        input.classList.add('invalid');
        return;
      }
      input.classList.remove('invalid');
      input.value = '';
      if (list.includes(email)) return;
      list.push(email);
      const chip = document.createElement('button');
      chip.type = 'button';
      chip.className = 'ai-chip removable';
      chip.textContent = `${email} ×`;
      chip.addEventListener('click', () => {
        list.splice(list.indexOf(email), 1);
        chip.remove();
      });
      host.insertBefore(chip, input);
    });
    input.addEventListener('input', () => input.classList.remove('invalid'));
    return input;
  };
  const toInput = chipInput(toEmails, toChips);
  const ccInput = chipInput(ccEmails, ccChips);
  toChips.appendChild(toInput);
  ccChips.appendChild(ccInput);

  // Participant chips land before the To input once the roster is known.
  const paintParticipants = (): void => {
    toChips.querySelectorAll('.ai-chip[data-peer]').forEach((c) => c.remove());
    for (const p of participants) {
      const chip = document.createElement('button');
      chip.type = 'button';
      chip.className = 'ai-chip';
      chip.dataset.peer = p.id;
      chip.textContent = p.name;
      chip.setAttribute('aria-pressed', String(selected.has(p.id)));
      chip.addEventListener('click', () => {
        if (selected.has(p.id)) selected.delete(p.id);
        else selected.add(p.id);
        chip.setAttribute('aria-pressed', String(selected.has(p.id)));
      });
      toChips.insertBefore(chip, toInput);
    }
  };

  const row = el('div', 'ai-form-row');
  const toneSel = document.createElement('select');
  for (const [tone, key] of [
    ['professional', 'aiToneProfessional'],
    ['friendly', 'aiToneFriendly'],
    ['concise', 'aiToneConcise'],
  ] as const) {
    const opt = document.createElement('option');
    opt.value = tone;
    opt.textContent = t(key);
    toneSel.appendChild(opt);
  }
  const langSel = document.createElement('select');
  for (const code of SUPPORTED) {
    const opt = document.createElement('option');
    opt.value = code;
    opt.textContent = ENDONYM[code] ?? code;
    langSel.appendChild(opt);
  }
  langSel.value = getUiLang();
  row.append(field(t('aiEmailTone'), toneSel), field(t('aiReportLangLabel'), langSel));

  const summaryWrap = el('label', 'ai-check');
  const summaryBox = document.createElement('input');
  summaryBox.type = 'checkbox';
  summaryBox.checked = true;
  const summaryText = el('span', '');
  summaryText.textContent = t('aiEmailIncludeSummary');
  summaryWrap.append(summaryBox, summaryText);

  const guide = document.createElement('textarea');
  guide.className = 'ai-guidelines';
  guide.maxLength = 2000;
  guide.rows = 2;
  guide.placeholder = t('aiReportGuidelinesPh');

  const genBtn = document.createElement('button');
  genBtn.className = 'btn-primary ai-generate';
  genBtn.textContent = t('aiEmailGenerate');

  const status = el('p', 'status-line');
  status.setAttribute('role', 'status');

  form.append(
    chipsField(t('aiEmailTo'), toChips),
    chipsField(t('aiEmailCc'), ccChips),
    row,
    summaryWrap,
    guide,
    genBtn,
    status,
  );

  // ---- draft editor / sent view ----------------------------------------------
  const view = el('div', 'ai-report-view');
  view.hidden = true;
  const rcptsEl = el('p', 'ai-email-rcpts');
  const subjectIn = document.createElement('input');
  subjectIn.className = 'ai-email-subject';
  subjectIn.maxLength = 200;
  subjectIn.setAttribute('aria-label', t('aiEmailSubject'));
  const bodyIn = document.createElement('textarea');
  bodyIn.className = 'ai-email-body';
  bodyIn.maxLength = 20000;
  const actions = el('div', 'ai-report-actions');
  const sendBtn = document.createElement('button');
  sendBtn.className = 'btn-primary ai-generate';
  sendBtn.textContent = t('aiEmailSend');
  const regenBtn = document.createElement('button');
  regenBtn.className = 'btn-ghost';
  regenBtn.textContent = t('aiReportRegenerate');
  const meta = el('span', 'ai-report-meta');
  const viewStatus = el('p', 'status-line');
  viewStatus.setAttribute('role', 'status');
  actions.append(sendBtn, regenBtn, meta);
  view.append(rcptsEl, subjectIn, bodyIn, actions, viewStatus);

  section.append(head, form, view);
  slot.appendChild(section);

  // ---- behavior ----------------------------------------------------------------
  let cost: number | null = null;
  let shown: AiEmail | null = null;

  const paintCost = (): void => {
    if (cost === null || shown?.status === 'sent') {
      costEl.textContent = '';
      return;
    }
    costEl.textContent = `~${auth.formatCredits(cost)}`;
    const balance = auth.getUser()?.balance ?? 0;
    const broke = balance < cost;
    genBtn.disabled = broke;
    genBtn.title = broke ? insufficientMsg(cost, balance) : '';
  };

  const showEmail = (m: AiEmail): void => {
    shown = m;
    form.hidden = true;
    view.hidden = false;
    rcptsEl.textContent = recipientsLine(m.recipients);
    subjectIn.value = m.subject;
    bodyIn.value = m.body_text;
    // Prefill the form so Regenerate starts from the last request.
    if (m.tone) toneSel.value = m.tone;
    if (m.lang && (SUPPORTED as readonly string[]).includes(m.lang)) langSel.value = m.lang;
    if (m.guidelines) guide.value = m.guidelines;
    const sent = m.status === 'sent';
    subjectIn.readOnly = sent;
    bodyIn.readOnly = sent;
    sendBtn.hidden = sent;
    // An unsaved draft (insert failed server-side) has no id — nothing to send.
    sendBtn.disabled = !m.id;
    const when = sent && m.sent_at ? new Date(m.sent_at).toLocaleString() : '';
    meta.textContent = sent
      ? `✓ ${t('aiEmailSent')}${when ? ` · ${when}` : ''}`
      : typeof m.cost === 'number'
        ? auth.formatCredits(m.cost)
        : '';
    paintCost();
  };

  repaint = paintParticipants;
  paintParticipants();

  void fetchAiPricing().then((p) => {
    if (active !== sessionId || !p) return;
    if (!p.email_enabled) return; // backend can't send — keep the section hidden
    section.hidden = false;
    cost = p.email.draft;
    paintCost();
  });
  void fetchLatestEmail(sessionId).then((m) => {
    if (active !== sessionId || !m) return;
    showEmail(m);
  });

  genBtn.addEventListener('click', async () => {
    if (genBtn.disabled) return;
    const refs = buildRecipientRefs({
      participants: [...selected],
      emails: toEmails,
      cc: ccEmails,
    });
    if (selected.size + toEmails.length === 0) {
      status.textContent = t('aiEmailNoRecipients');
      return;
    }
    genBtn.disabled = true;
    genBtn.classList.add('btn-loading');
    status.textContent = t('aiEmailGenerating');
    const result = await generateEmailDraft(sessionId, {
      recipients: refs,
      tone: toneSel.value,
      guidelines: guide.value,
      lang: langSel.value,
      includeSummary: summaryBox.checked,
    });
    if (active !== sessionId) return; // navigated to another session meanwhile
    genBtn.disabled = false;
    genBtn.classList.remove('btn-loading');
    if (result.email) {
      status.textContent = '';
      if (typeof result.email.balance === 'number') applyBalance(result.email.balance);
      showEmail(result.email);
      toast(`✓ ${t('aiEmailTitle')}`, 'ok');
      return;
    }
    if (result.insufficient) {
      const msg = insufficientMsg(
        result.insufficient.required,
        result.insufficient.available,
      );
      status.textContent = msg;
      paintCost();
      toast(msg, 'err');
      return;
    }
    const err = result.error || t('aiEmailFailed');
    status.textContent = err;
    toast(err, 'err');
  });

  sendBtn.addEventListener('click', async () => {
    if (sendBtn.disabled || !shown?.id) return;
    const subject = subjectIn.value.trim();
    const body = bodyIn.value.trim();
    if (!subject || !body) return;
    sendBtn.disabled = true;
    viewStatus.textContent = t('aiEmailSending');
    // Only ship fields the user actually edited — the server keeps the rest.
    const edits: { subject?: string; body_text?: string } = {};
    if (subject !== shown.subject) edits.subject = subject;
    if (body !== shown.body_text) edits.body_text = body;
    const result = await sendEmail(sessionId, shown.id, edits);
    if (active !== sessionId) return;
    sendBtn.disabled = false;
    if (result.sent) {
      viewStatus.textContent = '';
      showEmail({
        ...shown,
        subject,
        body_text: body,
        status: 'sent',
        sent_at: result.sent.sent_at,
      });
      toast(`✓ ${t('aiEmailSent')}`, 'ok');
      return;
    }
    const err = result.error || t('aiEmailSendFailed');
    viewStatus.textContent = err;
    toast(err, 'err');
  });

  // Regenerate just reopens the (prefilled) form — the charge only happens on
  // Draft, which shows the cost, so no extra confirm step is needed.
  regenBtn.addEventListener('click', () => {
    form.hidden = false;
    view.hidden = true;
    shown = null;
    paintCost();
  });
}

/** `<label class="ai-field"><span>caption</span>{control}</label>` */
function field(caption: string, control: HTMLElement): HTMLElement {
  const label = el('label', 'ai-field');
  const span = el('span', 'ai-field-label');
  span.textContent = caption;
  label.append(span, control);
  return label;
}

/** Like [`field`] but a `<div>` — chip groups hold several interactive
 *  controls, which must not nest inside a single `<label>`. */
function chipsField(caption: string, host: HTMLElement): HTMLElement {
  const wrap = el('div', 'ai-field');
  const span = el('span', 'ai-field-label');
  span.textContent = caption;
  wrap.append(span, host);
  return wrap;
}
