// Talk to Anyone — page wiring (spec 0110).
//
// The thin layer between the DOM in `talk.astro` and the engine in `conversation.ts`:
// setup form, language picker, tier cards, controls, analytics. It owns no protocol
// logic and no audio logic — those live in `conversation.ts` and are tested without a
// browser.

import { initAnalytics, track } from '../analytics';
import { HTTP_BASE, getUser, isLoggedIn } from '../auth';
import {
  engineNeedsPcm,
  formatRate,
  getAvailableTiers,
  engineDescKey,
  loadEnginePref,
  saveEnginePref,
  type EngineInfo,
} from '../engines';
import { applyI18n, loadLocale, resolveStoredLang, setUiLang, t } from '../i18n';
import { LANGUAGES, langMeta, type LangMeta } from '../langmap';
import { TalkConversation, browserSupported, type Exchange, type LiveExchange } from './conversation';
import type { SessionContext } from './session-machine';
import {
  buildExchangeCard,
  failureKey,
  fill,
  langLabel,
  pushHistory,
  renderLive,
  renderPair,
  renderStatus,
  type TalkElements,
} from './view';

const $ = (id: string): HTMLElement | null => document.getElementById(id);

let engines: EngineInfo[] = [];
let userLang = 'en';
let otherLang: string | null = null;
let tierId: string | null = null;
let convo: TalkConversation | null = null;
let live: LiveExchange = { spokenLang: null, targetLang: null, originalText: '', translatedText: '' };
let muted = false;
let startedAt = 0;

function els(): TalkElements {
  return {
    root: $('tk-root'),
    status: $('tk-status-text'),
    detected: $('tk-detected'),
    liveTranslation: $('tk-translation'),
    liveOriginal: $('tk-original'),
    history: $('tk-history'),
    pair: $('tk-pair'),
  };
}

function show(id: string, visible: boolean): void {
  const el = $(id);
  if (el) el.hidden = !visible;
}

// --- setup -----------------------------------------------------------------

async function fetchEngines(): Promise<EngineInfo[]> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/engines`, { cache: 'no-store' });
    if (!res.ok) return [];
    const data = (await res.json()) as EngineInfo[] | { engines?: EngineInfo[] };
    // The endpoint answers with a bare array on older deployments and `{engines, flags}`
    // on current ones; app.ts tolerates both and so must this.
    return Array.isArray(data) ? data : (data.engines ?? []);
  } catch {
    return [];
  }
}

/** Languages the picker offers: everything at least one tier can speak, minus our own. */
function pickable(): LangMeta[] {
  const offered = new Set<string>();
  for (const e of engines) for (const code of e.output_languages) offered.add(code);
  return LANGUAGES.filter((l) => offered.has(l.code) && l.code !== userLang);
}

function renderLangList(query: string): void {
  const list = $('tk-lang-list');
  if (!list) return;
  const q = query.trim().toLowerCase();
  const matches = pickable().filter(
    (l) =>
      !q ||
      l.code.includes(q) ||
      l.english.toLowerCase().includes(q) ||
      l.native.toLowerCase().includes(q),
  );
  list.textContent = '';
  for (const meta of matches) {
    const opt = document.createElement('button');
    opt.type = 'button';
    opt.className = 'lang-opt';
    opt.setAttribute('role', 'option');
    opt.setAttribute('aria-selected', String(meta.code === otherLang));
    if (meta.rtl) opt.dir = 'rtl';
    opt.innerHTML = '';
    const flag = document.createElement('span');
    flag.className = 'lang-opt-flag';
    flag.setAttribute('aria-hidden', 'true');
    flag.textContent = meta.flag;
    const native = document.createElement('span');
    native.className = 'lang-opt-native';
    native.textContent = meta.native;
    const english = document.createElement('span');
    english.className = 'lang-opt-en';
    english.textContent = meta.english;
    opt.append(flag, native, english);
    opt.addEventListener('click', () => selectOther(meta.code));
    list.appendChild(opt);
  }
  show('tk-lang-empty', matches.length === 0);
}

function selectOther(code: string): void {
  otherLang = code;
  const label = langLabel(code);
  const flag = $('tk-lang-flag');
  const text = $('tk-lang-text');
  if (flag) flag.textContent = label.flag;
  if (text) {
    // Endonym and English name together — a flag alone tells a screen reader nothing.
    text.textContent = `${label.native} (${label.english})`;
    text.removeAttribute('data-i18n');
  }
  closePanel();
  renderTiers();
  track('talk_to_anyone_language_selected', { language_pair: `${userLang}-${code}` });
  const start = $('tk-start') as HTMLButtonElement | null;
  if (start) start.disabled = !isLoggedIn();
}

function openPanel(): void {
  show('tk-lang-panel', true);
  $('tk-lang-trigger')?.setAttribute('aria-expanded', 'true');
  ($('tk-lang-search') as HTMLInputElement | null)?.focus();
}

function closePanel(): void {
  show('tk-lang-panel', false);
  $('tk-lang-trigger')?.setAttribute('aria-expanded', 'false');
}

function renderTiers(): void {
  const box = $('tk-tier-options');
  if (!box || !otherLang) return;
  // Enhanced is client-direct: the browser talks to the provider itself, so the server
  // never sees the audio it would have to gate. Filtering it out here means the user is
  // never offered a tier the conversation would silently swap under them.
  const tiers = getAvailableTiers(otherLang, engines).filter((e) => !e.capabilities.client_direct);
  show('tk-tier-field', tiers.length > 1);
  if (!tiers.length) return;
  if (!tierId || !tiers.some((e) => e.id === tierId)) tierId = tiers[0].id;

  box.textContent = '';
  for (const engine of tiers) {
    const card = document.createElement('button');
    card.type = 'button';
    card.className = 'engine-opt';
    card.setAttribute('role', 'radio');
    card.setAttribute('aria-checked', String(engine.id === tierId));
    if (engine.id === tierId) card.classList.add('active');

    const head = document.createElement('span');
    head.className = 'engine-opt-head';
    head.textContent = `${engine.display_name} · ${formatRate(engine.rate_per_minute)}`;

    const desc = document.createElement('span');
    desc.className = 'engine-opt-desc';
    const key = engineDescKey(engine.tier);
    desc.textContent = key ? t(key) : engine.description;

    card.append(head, desc);
    card.addEventListener('click', () => {
      tierId = engine.id;
      saveEnginePref(engine.id);
      renderTiers();
    });
    box.appendChild(card);
  }
  const tierEl = $('tk-tier');
  const chosen = tiers.find((e) => e.id === tierId);
  if (tierEl && chosen) {
    tierEl.textContent = chosen.display_name;
    tierEl.hidden = false;
  }
}

// --- conversation ----------------------------------------------------------

function onState(ctx: SessionContext): void {
  renderStatus(els(), ctx, live);

  const inConversation = ctx.phase !== 'idle' && ctx.phase !== 'ended' && ctx.phase !== 'error';
  show('tk-setup', !inConversation && ctx.phase !== 'error');
  show('tk-live', inConversation);
  show('tk-problem', ctx.phase === 'error');

  if (ctx.phase === 'error') {
    const copy = $('tk-problem-copy');
    if (copy) {
      copy.textContent = t(failureKey(ctx.failure));
      copy.removeAttribute('data-i18n');
    }
    track('talk_to_anyone_error', { error_code: ctx.failure ?? 'unknown', tier: tierId ?? '' });
  }

  const pause = $('tk-pause');
  if (pause) pause.textContent = ctx.phase === 'paused' ? t('talkResume') : t('talkPause');
}

function onExchange(exchange: Exchange): void {
  const history = $('tk-history');
  if (!history) return;
  show('tk-history-title', true);
  pushHistory(history, buildExchangeCard(exchange, document));
  track('talk_to_anyone_translation_completed', {
    tier: tierId ?? '',
    language_pair: `${exchange.spokenLang}-${exchange.targetLang}`,
  });
}

async function start(): Promise<void> {
  if (!otherLang) return;
  const status = $('tk-setup-status');
  if (!browserSupported()) {
    if (status) status.textContent = t('talkErrorUnsupported');
    return;
  }
  if (!isLoggedIn()) {
    if (status) status.textContent = t('talkSignInRequired');
    return;
  }

  const engineId = tierId ?? loadEnginePref() ?? 'standard';
  startedAt = Date.now();
  muted = false;
  convo = new TalkConversation({
    userLang,
    otherLang,
    engineId,
    needsPcm: engineNeedsPcm(engineId, engines),
    onState,
    onLive: (l) => {
      live = l;
      renderLive(els(), l);
      renderStatus(els(), convo?.state() ?? ({} as SessionContext), l);
      if (l.spokenLang) {
        track('talk_to_anyone_language_detected', { language_pair: `${l.spokenLang}-${l.targetLang}` });
      }
    },
    onExchange,
    onNotice: (code) => {
      const el = $('tk-status-text');
      // Recoverable: say what happened in plain language and keep listening. Never a
      // provider's error text (brief §26).
      if (el) el.textContent = code === 'provider_unavailable' ? t('talkErrorProvider') : t('talkErrorGeneric');
    },
    onEngineChanged: (to, reason) => {
      const note = $('tk-tier-note');
      if (note && reason === 'talk_client_direct_unsupported') {
        note.textContent = t('talkTierClientDirect');
        note.hidden = false;
      }
      const tierEl = $('tk-tier');
      const engine = engines.find((e) => e.id === to);
      if (tierEl && engine) tierEl.textContent = engine.display_name;
    },
    onTimeToTranslatedSpeech: (ms) => {
      // The headline product metric (brief §33). Bucketed, never per-utterance text.
      track('talk_to_anyone_latency', { tier: tierId ?? '', ms: Math.round(ms) });
    },
  });

  renderPair(els(), userLang, otherLang);
  track('talk_to_anyone_started', { tier: engineId, language_pair: `${userLang}-${otherLang}` });
  track('talk_to_anyone_tier_used', { tier: engineId });
  await convo.start();
}

function end(): void {
  if (!convo) return;
  track('talk_to_anyone_ended', {
    tier: tierId ?? '',
    duration_seconds: Math.round((Date.now() - startedAt) / 1000),
  });
  convo.end();
  convo = null;
  const history = $('tk-history');
  if (history) history.textContent = '';
  show('tk-history-title', false);
  live = { spokenLang: null, targetLang: null, originalText: '', translatedText: '' };
  renderLive(els(), live);
}

// --- mount -----------------------------------------------------------------

export function mountTalk(): void {
  const lang = resolveStoredLang();
  setUiLang(lang);
  applyI18n();
  void loadLocale(lang).then(applyI18n);
  initAnalytics();
  track('talk_to_anyone_opened');

  // The account's language is authoritative for a signed-in user (the same priority
  // `applyUserLanguage` uses in app.ts); the stored UI language is the guest fallback.
  userLang = getUser()?.language || lang || 'en';
  const mine = $('tk-my-lang');
  if (mine) {
    const label = langLabel(userLang);
    mine.textContent = `${label.flag} ${label.native}`;
  }

  const start$ = $('tk-start') as HTMLButtonElement | null;
  if (start$ && !isLoggedIn()) {
    const status = $('tk-setup-status');
    if (status) status.textContent = t('talkSignInRequired');
  }

  $('tk-lang-trigger')?.addEventListener('click', () => {
    const panel = $('tk-lang-panel');
    if (panel?.hidden) {
      renderLangList('');
      openPanel();
    } else {
      closePanel();
    }
  });
  $('tk-lang-search')?.addEventListener('input', (e) => {
    renderLangList((e.target as HTMLInputElement).value);
  });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') closePanel();
  });
  document.addEventListener('click', (e) => {
    const panel = $('tk-lang-panel');
    if (!panel || panel.hidden) return;
    const target = e.target as Node;
    if (!panel.contains(target) && !$('tk-lang-trigger')?.contains(target)) closePanel();
  });

  start$?.addEventListener('click', () => void start());
  $('tk-end')?.addEventListener('click', end);
  $('tk-retry')?.addEventListener('click', () => {
    show('tk-problem', false);
    show('tk-setup', true);
  });
  $('tk-mute')?.addEventListener('click', () => {
    muted = !muted;
    convo?.setMuted(muted);
    const btn = $('tk-mute');
    if (btn) btn.textContent = muted ? t('talkUnmute') : t('talkMute');
  });
  $('tk-pause')?.addEventListener('click', () => {
    if (!convo) return;
    if (convo.state().phase === 'paused') convo.resume();
    else convo.pause();
  });

  // Leaving the page must not leave a microphone track live.
  window.addEventListener('pagehide', () => convo?.end());

  void fetchEngines().then((list) => {
    engines = list;
    const preferred = loadEnginePref();
    if (preferred) tierId = preferred;
    if (otherLang) renderTiers();
  });
}

export { fill };
