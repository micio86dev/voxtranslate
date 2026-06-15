// Session detail screen (specs 0011+): full-screen view opened after a call
// ends and from the buy-modal Transcripts tab. Hosts the download buttons, the
// AI tool sections (report/sentiment/email — filled in by later phases), and a
// read-only transcript viewer. Auth-only: guests never reach this screen
// (transcript APIs are auth-gated server-side).

import * as auth from './auth';
import { fetchAiPricing, fetchTranscript, type TranscriptDoc } from './api';
import { getUiLang, t } from './i18n';
import { initEmailSlot, updateEmailContext } from './email';
import { initReportSlot } from './report';
import { initSentimentSlot, updateSentimentContext } from './sentiment';

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;

/** What the screen needs to paint its header before the transcript loads. */
export interface SessionRef {
  id: string;
  room: string;
  started_at: string;
  ended_at?: string | null;
  event_count: number;
}

let current: SessionRef | null = null;
let onCloseCb: (() => void) | null = null;

/** The session currently shown (null when the screen is closed). */
export function currentSession(): SessionRef | null {
  return current;
}

export function openSessionScreen(ref: SessionRef, opts: { onClose?: () => void } = {}): void {
  current = ref;
  onCloseCb = opts.onClose ?? null;
  renderHeader(ref);
  $('home').classList.add('hidden');
  $('session').classList.remove('hidden');
  initReportSlot(ref);
  initSentimentSlot(ref);
  initEmailSlot(ref);
  void renderTranscript(ref);
  $('session-back').focus();
}

export function closeSessionScreen(): void {
  current = null;
  $('session').classList.add('hidden');
  $('home').classList.remove('hidden');
  const cb = onCloseCb;
  onCloseCb = null;
  cb?.();
}

function renderHeader(ref: SessionRef): void {
  $('session-room').textContent = ref.room;
  $('session-date').textContent = new Date(ref.started_at).toLocaleString();
  const ms = ref.ended_at
    ? new Date(ref.ended_at).getTime() - new Date(ref.started_at).getTime()
    : 0;
  $('session-duration').textContent = formatDuration(ms);
  $('session-events').textContent = String(ref.event_count);
  $('session-participants').textContent = '';
  for (const id of ['session-dl-pdf', 'session-dl-json', 'session-dl-srt', 'session-dl-vtt']) {
    const btn = $<HTMLButtonElement>(id);
    btn.disabled = ref.event_count === 0;
    btn.title = ref.event_count === 0 ? t('noTranscriptEvents') : '';
  }
  // AI text correction checkbox + cost estimate
  initAiCorrect(ref);
}

const FALLBACK_CORRECT_COST = { base: 0.05, per_event: 0.001 };

function initAiCorrect(ref: SessionRef): void {
  const chk = $<HTMLInputElement>('ai-correct-chk');
  const costEl = $('ai-correct-cost');
  if (ref.event_count === 0) {
    chk.disabled = true;
    costEl.textContent = '';
    return;
  }
  chk.disabled = false;
  const paintCost = (pricing: { base: number; per_event: number } | null): void => {
    const p = pricing ?? FALLBACK_CORRECT_COST;
    const estimated = p.base + p.per_event * Math.max(1, ref.event_count);
    costEl.textContent = `~${auth.formatCredits(estimated)}`;
  };
  void fetchAiPricing().then((p) => {
    if (current?.id !== ref.id) return;
    paintCost(p?.transcript_correction ?? null);
  });
  // If pricing fetch is slow, show fallback immediately
  paintCost(null);
}

function formatDuration(ms: number): string {
  const total = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  return h > 0
    ? `${h}h ${String(m).padStart(2, '0')}m`
    : `${m}m ${String(s).padStart(2, '0')}s`;
}

async function renderTranscript(ref: SessionRef): Promise<void> {
  const list = $('session-transcript');
  const status = $('session-transcript-status');
  list.innerHTML = '';
  $('session-bookmarks').hidden = true;
  if (ref.event_count === 0) {
    status.textContent = t('noTranscriptEvents');
    return;
  }
  status.textContent = t('processing');
  const doc = await fetchTranscript(ref.id);
  if (current?.id !== ref.id) return; // navigated away while loading
  if (!doc) {
    status.textContent = t('loadFailed');
    return;
  }
  status.textContent = '';
  // The export is authoritative — refresh duration + participants from it. Collapse
  // duplicates first: rejoining/refreshing mints a new peer_id, so the same person
  // can appear several times in the roster (and inflate the count the sentiment
  // estimate is billed on). Dedupe by display name, keeping the first occurrence.
  const seen = new Set<string>();
  const participants = doc.session.participants.filter((p) => {
    const key = (p.name || '').trim().toLowerCase();
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
  $('session-duration').textContent = formatDuration(doc.session.duration_seconds * 1000);
  $('session-participants').textContent = participants.map((p) => p.name).join(', ');
  // Sentiment cost preview needs the participant count; the chart wants
  // bookmark offsets (seconds from session start).
  const startMs = new Date(doc.session.started_at).getTime();
  updateSentimentContext(
    ref.id,
    participants.length,
    doc.session.duration_seconds,
    (doc.bookmarks ?? []).map((bm) => Math.max(0, (new Date(bm.ts).getTime() - startMs) / 1000)),
  );
  // The email composer's To-chips come from the same (deduped) roster.
  updateEmailContext(
    ref.id,
    participants.map((p) => ({ id: p.id, name: p.name })),
  );
  renderBookmarks(doc);
  renderEvents(list, doc);
}

/** Pinned moments (spec 0013) above the transcript; hidden when none exist. */
function renderBookmarks(doc: TranscriptDoc): void {
  const box = $('session-bookmarks');
  box.innerHTML = '';
  const pins = doc.bookmarks ?? [];
  box.hidden = pins.length === 0;
  if (!pins.length) return;
  const head = document.createElement('div');
  head.className = 'bm-head';
  head.textContent = `🔖 ${t('bookmarksTitle')}`;
  box.appendChild(head);
  for (const bm of pins) {
    const row = document.createElement('div');
    row.className = 'bm-item';
    const time = document.createElement('span');
    time.className = 'bm-time mono';
    time.textContent = new Date(bm.ts).toLocaleTimeString([], {
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
    });
    const by = document.createElement('span');
    by.className = 'bm-by';
    by.textContent = bm.by;
    row.append(time, by);
    if (bm.label) {
      const label = document.createElement('span');
      label.className = 'bm-label';
      label.textContent = bm.label;
      row.appendChild(label);
    }
    box.appendChild(row);
  }
}

function renderEvents(list: HTMLElement, doc: TranscriptDoc): void {
  const ui = getUiLang();
  for (const ev of doc.events) {
    const row = document.createElement('div');
    row.className = ev.type === 'chat' ? 'tr-event tr-chat' : 'tr-event';
    // Epoch ms, so sentiment key moments can jump to the closest row.
    row.dataset.ts = String(new Date(ev.ts).getTime());
    const time = document.createElement('span');
    time.className = 'tr-time mono';
    time.textContent = new Date(ev.ts).toLocaleTimeString([], {
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
    });
    const body = document.createElement('div');
    body.className = 'tr-body';
    const name = document.createElement('span');
    name.className = 'tr-speaker';
    name.textContent = ev.type === 'chat' ? `${ev.speaker_name} 💬` : ev.speaker_name;
    const text = document.createElement('span');
    text.className = 'tr-text';
    // Show the viewer's language when a translation exists; original below it.
    const translated = ev.lang !== ui ? ev.translations?.[ui] : undefined;
    text.textContent = translated || ev.original;
    body.append(name, text);
    if (translated) {
      const orig = document.createElement('span');
      orig.className = 'tr-orig';
      orig.textContent = ev.original;
      body.appendChild(orig);
    }
    row.append(time, body);
    list.appendChild(row);
  }
}

// ---- One-time wiring (DOM is ready when modules execute) --------------------

$('session-back').addEventListener('click', closeSessionScreen);

for (const format of ['pdf', 'json', 'srt', 'vtt'] as const) {
  const btn = $<HTMLButtonElement>(`session-dl-${format}`);
  btn.addEventListener('click', async () => {
    if (!current || btn.disabled) return;
    const prev = btn.textContent;
    btn.disabled = true;
    btn.textContent = t('processing');
    // SRT/VTT also carry the lang-mode dropdown (original/translated/both).
    const mode = $<HTMLSelectElement>('session-sub-mode').value as auth.SubtitleMode;
    const ok = await auth.downloadTranscript(current.id, format, getUiLang(), mode);
    btn.textContent = prev;
    btn.disabled = (current?.event_count ?? 0) === 0;
    if (!ok) {
      const status = $('session-transcript-status');
      status.textContent = t('downloadFailed');
      setTimeout(() => {
        if (status.textContent === t('downloadFailed')) status.textContent = '';
      }, 3500);
    }
  });
}
