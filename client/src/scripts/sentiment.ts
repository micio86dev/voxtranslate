// Sentiment analysis (spec 0015): card rendered into #ai-sentiment-slot on the
// session detail screen. POST /api/sessions/{id}/sentiment charges credits
// once per session — the result is cached server-side (UNIQUE(session_id)), so
// the UI offers Run only while no analysis exists and never a regenerate.

import * as auth from './auth';
import {
  fetchAiPricing,
  fetchSentiment,
  generateSentiment,
  type AiSentiment,
  type SentimentResult,
} from './api';
import { t } from './i18n';
import { drawSentimentTimeline } from './sentiment-chart';
import { toast } from './toast';

const $ = (id: string) => document.getElementById(id) as HTMLElement;

function el(tag: string, className: string): HTMLElement {
  const node = document.createElement(tag);
  node.className = className;
  return node;
}

const MOOD_EMOJI: Record<string, string> = {
  positive: '😊',
  neutral: '😐',
  negative: '😟',
  mixed: '🌗',
};

function moodLabel(mood: string): string {
  const key: Record<string, string> = {
    positive: 'aiMoodPositive',
    neutral: 'aiMoodNeutral',
    negative: 'aiMoodNegative',
    mixed: 'aiMoodMixed',
  };
  return key[mood] ? t(key[mood]) : mood;
}

function fmtScore(score: number): string {
  return (score > 0 ? '+' : '') + score.toFixed(2);
}

function fmtOffset(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = Math.floor(secs % 60);
  return `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
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
/** Session start (epoch ms) — anchors key-moment offsets to transcript rows. */
let startedAtMs = 0;
/** Set by `updateSentimentContext` once the transcript doc is loaded. */
let context: { participants: number; durationSeconds: number; bookmarks: number[] } | null = null;
let repaint: (() => void) | null = null;

/** What the slot needs from the session screen. */
export interface SentimentSlotRef {
  id: string;
  started_at: string;
  ended_at?: string | null;
  event_count: number;
}

/**
 * The cost preview needs the participant count (and the chart wants bookmark
 * offsets), which only the transcript doc knows — the session screen calls
 * this once that fetch lands.
 */
export function updateSentimentContext(
  sessionId: string,
  participants: number,
  durationSeconds: number,
  bookmarks: number[],
): void {
  if (active !== sessionId) return;
  context = { participants, durationSeconds, bookmarks };
  repaint?.();
}

/** (Re)build the sentiment section for a session. */
export function initSentimentSlot(ref: SentimentSlotRef): void {
  const slot = $('ai-sentiment-slot');
  slot.innerHTML = '';
  active = ref.id;
  startedAtMs = new Date(ref.started_at).getTime();
  context = null;
  repaint = null;
  // The whole #session-ai card is hidden by the report slot for guests/empty
  // sessions; mirror the check so this module never depends on that ordering.
  if (!auth.isLoggedIn() || ref.event_count === 0) return;
  const endMs = ref.ended_at ? new Date(ref.ended_at).getTime() : Date.now();
  const durationSeconds = Math.max(
    0,
    Math.round((endMs - new Date(ref.started_at).getTime()) / 1000),
  );
  build(slot, ref.id, durationSeconds);
}

function build(slot: HTMLElement, sessionId: string, fallbackDuration: number): void {
  const section = el('div', 'ai-section');

  const head = el('div', 'ai-section-head');
  const title = el('span', 'ai-section-title');
  title.textContent = `📊 ${t('aiSentimentTitle')}`;
  const costEl = el('span', 'ai-cost mono');
  head.append(title, costEl);

  // ---- run form (replaced by the viewer once a result exists) ---------------
  const form = el('div', 'ai-report-form');
  const runBtn = document.createElement('button');
  runBtn.className = 'btn-primary ai-generate';
  runBtn.textContent = t('aiSentimentRun');
  const status = el('p', 'status-line');
  status.setAttribute('role', 'status');
  form.append(runBtn, status);

  // ---- viewer ----------------------------------------------------------------
  const view = el('div', 'ai-sentiment-view');
  view.hidden = true;
  const overall = el('div', 'ai-overall');
  const canvas = document.createElement('canvas');
  canvas.className = 'ai-timeline';
  canvas.setAttribute('role', 'img');
  canvas.setAttribute('aria-label', t('aiSentimentTimeline'));
  const speakers = el('div', 'ai-speakers');
  const momentsHead = el('div', 'ai-subhead');
  momentsHead.textContent = t('aiSentimentMoments');
  momentsHead.hidden = true;
  const moments = el('div', 'ai-moments');
  const meta = el('span', 'ai-report-meta');
  view.append(overall, canvas, speakers, momentsHead, moments, meta);

  section.append(head, form, view);
  slot.appendChild(section);

  // ---- behavior ----------------------------------------------------------------
  let pricing: { base: number; per_participant: number; per_minute: number } | null = null;
  let shown: AiSentiment | null = null;

  const estimate = (): number | null => {
    if (!pricing || !context) return null;
    const minutes = Math.max(1, Math.ceil(Math.max(0, context.durationSeconds) / 60));
    return (
      pricing.base + pricing.per_participant * context.participants + pricing.per_minute * minutes
    );
  };

  const paintCost = (): void => {
    if (shown) {
      costEl.textContent = '';
      return;
    }
    const est = estimate();
    if (est === null) {
      costEl.textContent = '';
      return;
    }
    costEl.textContent = `~${auth.formatCredits(est)}`;
    const balance = auth.getUser()?.balance ?? 0;
    const broke = balance < est;
    runBtn.disabled = broke;
    runBtn.title = broke ? insufficientMsg(est, balance) : '';
  };

  const showResult = (s: AiSentiment): void => {
    shown = s;
    form.hidden = true;
    view.hidden = false;
    paintCost();
    renderOverall(overall, s.result);
    renderSpeakers(speakers, s.result);
    renderMoments(momentsHead, moments, s.result, sessionId);
    const when = s.created_at ? new Date(s.created_at).toLocaleString() : '';
    meta.textContent = [
      s.model,
      when,
      s.cached ? t('aiSentimentCached') : auth.formatCredits(s.cost),
    ]
      .filter(Boolean)
      .join(' · ');
    // Canvas needs layout for clientWidth — draw on the next frame.
    requestAnimationFrame(() => {
      if (active !== sessionId) return;
      drawSentimentTimeline(canvas, {
        points: s.result.timeline,
        durationSeconds: context?.durationSeconds || fallbackDuration,
        bookmarks: context?.bookmarks ?? [],
        keyMoments: s.result.key_moments,
      });
    });
  };

  repaint = () => {
    paintCost();
    if (shown) showResult(shown); // redraw the chart with bookmark markers
  };

  void fetchAiPricing().then((p) => {
    if (active !== sessionId || !p) return;
    pricing = p.sentiment;
    paintCost();
  });
  void fetchSentiment(sessionId).then((s) => {
    if (active !== sessionId || !s) return;
    showResult(s);
  });

  runBtn.addEventListener('click', async () => {
    if (runBtn.disabled) return;
    runBtn.disabled = true;
    runBtn.classList.add('btn-loading');
    status.textContent = t('aiSentimentRunning');
    const result = await generateSentiment(sessionId);
    if (active !== sessionId) return; // navigated to another session meanwhile
    runBtn.disabled = false;
    runBtn.classList.remove('btn-loading');
    if (result.sentiment) {
      status.textContent = '';
      if (typeof result.sentiment.balance === 'number') applyBalance(result.sentiment.balance);
      showResult(result.sentiment);
      toast(`✓ ${t('aiSentimentTitle')}`, 'ok');
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
    const err = result.error || t('aiSentimentFailed');
    status.textContent = err;
    toast(err, 'err');
  });
}

function renderOverall(box: HTMLElement, r: SentimentResult): void {
  box.innerHTML = '';
  const emoji = el('span', 'ai-mood-emoji');
  emoji.textContent = MOOD_EMOJI[r.overall.mood] ?? '😐';
  const label = el('span', 'ai-mood-label');
  label.textContent = moodLabel(r.overall.mood);
  const score = el('span', 'ai-mood-score mono');
  score.textContent = fmtScore(r.overall.score);
  box.append(emoji, label, score);
}

function renderSpeakers(box: HTMLElement, r: SentimentResult): void {
  box.innerHTML = '';
  for (const sp of r.speakers) {
    const card = el('div', 'ai-speaker-card');
    const name = el('span', 'ai-speaker-name');
    name.textContent = `${sp.mood ? MOOD_EMOJI[sp.mood] ?? '' : ''} ${sp.name}`.trim();
    const detail = el('span', 'ai-speaker-detail mono');
    const score = sp.score !== null ? fmtScore(sp.score) : '—';
    detail.textContent = `${score} · ${sp.talk_pct}% ${t('aiSentimentTalk')}`;
    card.append(name, detail);
    box.appendChild(card);
  }
}

/** Key moments are clickable: jump the transcript viewer to that time. */
function renderMoments(
  head: HTMLElement,
  box: HTMLElement,
  r: SentimentResult,
  sessionId: string,
): void {
  box.innerHTML = '';
  head.hidden = r.key_moments.length === 0;
  for (const m of r.key_moments) {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'ai-moment';
    const time = el('span', 'ai-moment-time mono');
    time.textContent = fmtOffset(m.t);
    const label = el('span', 'ai-moment-label');
    label.textContent = m.label;
    const score = el('span', `ai-moment-score mono ${m.score < 0 ? 'neg' : 'pos'}`);
    score.textContent = fmtScore(m.score);
    row.append(time, label, score);
    row.addEventListener('click', () => {
      if (active !== sessionId) return;
      scrollTranscriptTo(m.t);
    });
    box.appendChild(row);
  }
}

/** Scroll the transcript list to the event closest to `offsetSecs`. */
function scrollTranscriptTo(offsetSecs: number): void {
  const rows = document.querySelectorAll<HTMLElement>('#session-transcript .tr-event[data-ts]');
  if (!rows.length) return;
  const target = startedAtMs + offsetSecs * 1000;
  let best: HTMLElement = rows[0];
  let bestDelta = Infinity;
  for (const row of rows) {
    const delta = Math.abs(Number(row.dataset.ts) - target);
    if (delta < bestDelta) {
      bestDelta = delta;
      best = row;
    }
  }
  best.scrollIntoView({ behavior: 'smooth', block: 'center' });
  best.classList.add('tr-flash');
  setTimeout(() => best.classList.remove('tr-flash'), 2000);
}
