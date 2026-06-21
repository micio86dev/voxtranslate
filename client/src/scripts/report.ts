// AI session report (spec 0014): form + viewer rendered into #ai-report-slot
// on the session detail screen. POST /api/sessions/{id}/report charges
// credits, so the form shows a cost preview (from /api/billing/ai-pricing)
// and disables Generate when the cached balance can't cover it — the server's
// atomic deduction remains the real gate.

import * as auth from './auth';
import { fetchAiPricing, fetchLatestReport, generateReport, type AiReport } from './api';
import { ENDONYM, SUPPORTED, getUiLang, t } from './i18n';
import { estimateReportCost, mdToHtml } from './report-md';

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

/** What the slot needs from the session screen. */
export interface ReportSlotRef {
  id: string;
  started_at: string;
  ended_at?: string | null;
  event_count: number;
}

/**
 * (Re)build the AI report section for a session. Hides the whole AI card for
 * empty sessions (the server would 422 anyway — nothing to report on).
 */
export function initReportSlot(ref: ReportSlotRef): void {
  const card = $('session-ai');
  const slot = $('ai-report-slot');
  slot.innerHTML = '';
  active = ref.id;
  if (!auth.isLoggedIn() || ref.event_count === 0) {
    card.classList.add('hidden');
    return;
  }
  card.classList.remove('hidden');
  const endMs = ref.ended_at ? new Date(ref.ended_at).getTime() : Date.now();
  const durationSeconds = Math.max(
    0,
    Math.round((endMs - new Date(ref.started_at).getTime()) / 1000),
  );
  build(slot, ref.id, durationSeconds);
}

function build(slot: HTMLElement, sessionId: string, durationSeconds: number): void {
  const section = el('div', 'ai-section');

  const head = el('div', 'ai-section-head');
  const title = el('span', 'ai-section-title');
  title.textContent = `📋 ${t('aiReportTitle')}`;
  const costEl = el('span', 'ai-cost mono');
  head.append(title, costEl);

  // ---- form (hidden once a report is shown; Regenerate brings it back) ------
  const form = el('div', 'ai-report-form');

  const row = el('div', 'ai-form-row');
  const fmtSel = document.createElement('select');
  for (const f of ['structured', 'freeform'] as const) {
    const opt = document.createElement('option');
    opt.value = f;
    opt.textContent = t(f === 'structured' ? 'aiReportStructured' : 'aiReportFreeform');
    fmtSel.appendChild(opt);
  }
  const langSel = document.createElement('select');
  for (const code of SUPPORTED) {
    const opt = document.createElement('option');
    opt.value = code;
    opt.textContent = ENDONYM[code] ?? code;
    langSel.appendChild(opt);
  }
  langSel.value = getUiLang();
  row.append(
    field(t('aiReportFormatLabel'), fmtSel),
    field(t('aiReportLangLabel'), langSel),
  );

  const guide = document.createElement('textarea');
  guide.className = 'ai-guidelines';
  guide.maxLength = 2000;
  guide.rows = 2;
  guide.placeholder = t('aiReportGuidelinesPh');

  const genBtn = document.createElement('button');
  genBtn.className = 'btn-primary ai-generate';
  genBtn.textContent = t('aiReportGenerate');

  const status = el('p', 'status-line');
  status.setAttribute('role', 'status');

  form.append(row, guide, genBtn, status);

  // ---- viewer ----------------------------------------------------------------
  const view = el('div', 'ai-report-view');
  view.hidden = true;
  const md = el('div', 'ai-report-md');
  const actions = el('div', 'ai-report-actions');
  const copyBtn = document.createElement('button');
  copyBtn.className = 'btn-ghost';
  copyBtn.textContent = t('copy');
  const regenBtn = document.createElement('button');
  regenBtn.className = 'btn-ghost';
  regenBtn.textContent = t('aiReportRegenerate');
  const meta = el('span', 'ai-report-meta');
  actions.append(copyBtn, regenBtn, meta);
  view.append(md, actions);

  section.append(head, form, view);
  slot.appendChild(section);

  // ---- behavior ----------------------------------------------------------------
  let pricing: { base: number; per_minute: number } | null = null;
  let lastMd = '';

  const paintCost = (): void => {
    if (!pricing) {
      costEl.textContent = '';
      return;
    }
    const estimated = estimateReportCost(pricing, durationSeconds);
    costEl.textContent = `~${auth.formatCredits(estimated)}`;
    const balance = auth.getUser()?.balance ?? 0;
    const broke = balance < estimated;
    genBtn.disabled = broke;
    genBtn.title = broke ? insufficientMsg(estimated, balance) : '';
  };

  const showReport = (r: AiReport): void => {
    form.hidden = true;
    view.hidden = false;
    lastMd = r.markdown;
    md.innerHTML = mdToHtml(r.markdown);
    const when = r.created_at ? new Date(r.created_at).toLocaleString() : '';
    meta.textContent = [r.model, when, auth.formatCredits(r.cost)]
      .filter(Boolean)
      .join(' · ');
    // Prefill the form so Regenerate starts from the last request.
    if (r.format === 'structured' || r.format === 'freeform') fmtSel.value = r.format;
    if ((SUPPORTED as readonly string[]).includes(r.lang)) langSel.value = r.lang;
    if (r.guidelines) guide.value = r.guidelines;
  };

  void fetchAiPricing().then((p) => {
    if (active !== sessionId || !p) return;
    pricing = p.report;
    paintCost();
  });
  void fetchLatestReport(sessionId).then((r) => {
    if (active !== sessionId || !r) return;
    showReport(r);
  });

  genBtn.addEventListener('click', async () => {
    if (genBtn.disabled) return;
    genBtn.disabled = true;
    genBtn.classList.add('btn-loading');
    status.textContent = t('aiReportGenerating');
    const result = await generateReport(sessionId, {
      format: fmtSel.value,
      lang: langSel.value,
      guidelines: guide.value,
    });
    if (active !== sessionId) return; // navigated to another session meanwhile
    genBtn.disabled = false;
    genBtn.classList.remove('btn-loading');
    if (result.report) {
      status.textContent = '';
      if (typeof result.report.balance === 'number') applyBalance(result.report.balance);
      showReport(result.report);
      paintCost();
      return;
    }
    if (result.insufficient) {
      status.textContent = insufficientMsg(
        result.insufficient.required,
        result.insufficient.available,
      );
      paintCost();
      return;
    }
    status.textContent = result.error || t('aiReportFailed');
  });

  // Regenerate just reopens the (prefilled) form — the charge only happens on
  // Generate, which shows the cost, so no extra confirm step is needed.
  regenBtn.addEventListener('click', () => {
    form.hidden = false;
    paintCost();
    guide.focus();
  });

  copyBtn.addEventListener('click', async () => {
    if (!lastMd) return;
    try {
      await navigator.clipboard.writeText(lastMd);
      const prev = copyBtn.textContent;
      copyBtn.textContent = t('copied');
      setTimeout(() => {
        copyBtn.textContent = prev;
      }, 1500);
    } catch {
      /* clipboard unavailable — leave the button as is */
    }
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
