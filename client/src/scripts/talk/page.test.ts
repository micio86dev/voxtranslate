// @vitest-environment jsdom
import { describe, it, expect, beforeEach, vi } from 'vitest';

const track = vi.fn();
const started: Array<Record<string, unknown>> = [];
const instances: FakeConvo[] = [];

class FakeConvo {
  ended = false;
  paused = false;
  muted: boolean | null = null;
  phase = 'live';
  constructor(public opts: Record<string, unknown>) {
    started.push(opts);
    instances.push(this);
  }
  start = vi.fn(async () => {});
  end = vi.fn(() => {
    this.ended = true;
  });
  pause = vi.fn(() => {
    this.paused = true;
    this.phase = 'paused';
  });
  resume = vi.fn(() => {
    this.paused = false;
    this.phase = 'live';
  });
  setMuted = vi.fn((m: boolean) => {
    this.muted = m;
  });
  state() {
    return { phase: this.phase, activity: 'listening', sessionId: 's', failure: null };
  }
}

let loggedIn = true;
let supported = true;

vi.mock('../analytics', () => ({ initAnalytics: vi.fn(), track: (...a: unknown[]) => track(...a) }));
vi.mock('../auth', () => ({
  HTTP_BASE: 'http://test',
  getUser: () => ({ language: 'it' }),
  isLoggedIn: () => loggedIn,
}));
vi.mock('./conversation', async (orig) => {
  const actual = (await orig()) as Record<string, unknown>;
  return {
    ...actual,
    TalkConversation: FakeConvo,
    browserSupported: () => supported,
  };
});

const ENGINES = [
  {
    id: 'standard',
    display_name: 'Standard',
    tier: 'standard',
    description: 'd',
    rate_per_minute: 0.0045,
    input_languages: ['it', 'es', 'en'],
    output_languages: ['it', 'es', 'en'],
    capabilities: {
      translated_audio: true,
      cost_scales_per_language: true,
      client_direct: false,
      max_room_size: 4,
    },
  },
  {
    id: 'cartesia',
    display_name: 'Enhanced',
    tier: 'enhanced',
    description: 'd',
    rate_per_minute: 0.02,
    input_languages: ['it', 'es', 'en'],
    output_languages: ['it', 'es', 'en'],
    capabilities: {
      translated_audio: false,
      cost_scales_per_language: true,
      client_direct: true,
      max_room_size: 4,
    },
  },
];

const DOM = `
  <main id="tk-root">
    <span id="tk-tier" hidden></span>
    <section id="tk-setup">
      <span id="tk-my-lang"></span>
      <button id="tk-lang-trigger" aria-expanded="false">
        <span id="tk-lang-flag"></span><span id="tk-lang-text" data-i18n="talkOtherLangPick"></span>
      </button>
      <div id="tk-lang-panel" hidden>
        <input id="tk-lang-search" />
        <div id="tk-lang-list"></div>
        <p id="tk-lang-empty" hidden></p>
      </div>
      <div id="tk-tier-field" hidden>
        <div id="tk-tier-options"></div>
        <p id="tk-tier-note" hidden></p>
      </div>
      <button id="tk-start" disabled></button>
      <p id="tk-setup-status"></p>
    </section>
    <section id="tk-live" hidden>
      <div id="tk-pair"></div>
      <span id="tk-status-text"></span>
      <p id="tk-detected" hidden></p>
      <p id="tk-translation"></p>
      <p id="tk-original"></p>
      <h2 id="tk-history-title" hidden></h2>
      <div id="tk-history"></div>
      <button id="tk-mute"></button>
      <button id="tk-pause"></button>
      <button id="tk-end"></button>
    </section>
    <section id="tk-problem" hidden><p id="tk-problem-copy"></p><button id="tk-retry"></button></section>
  </main>`;

const $ = (id: string): HTMLElement => document.getElementById(id)!;
const flush = (): Promise<void> => new Promise((r) => setTimeout(r, 0));

async function mount(): Promise<void> {
  document.body.innerHTML = DOM;
  const { mountTalk } = await import('./page');
  mountTalk();
  await flush();
}

beforeEach(() => {
  vi.resetModules();
  track.mockClear();
  started.length = 0;
  instances.length = 0;
  loggedIn = true;
  supported = true;
  localStorage.clear();
  vi.stubGlobal(
    'fetch',
    vi.fn(async () => ({ ok: true, json: async () => ENGINES })),
  );
});

describe('setup', () => {
  it('shows the account language and cannot start before a language is chosen', async () => {
    await mount();
    expect($('tk-my-lang').textContent).toContain('Italiano');
    expect(($('tk-start') as HTMLButtonElement).disabled).toBe(true);
    expect(track).toHaveBeenCalledWith('talk_to_anyone_opened');
  });

  it('lists languages, excluding the user’s own', async () => {
    await mount();
    $('tk-lang-trigger').click();
    const options = $('tk-lang-list').querySelectorAll('.lang-opt');
    const labels = [...options].map((o) => o.textContent ?? '');
    expect(labels.some((l) => l.includes('Español'))).toBe(true);
    // Translating Italian into Italian is not a conversation — it must not be offered.
    expect(labels.some((l) => l.includes('Italiano'))).toBe(false);
  });

  it('filters as you type and says so when nothing matches', async () => {
    await mount();
    $('tk-lang-trigger').click();
    const search = $('tk-lang-search') as HTMLInputElement;
    search.value = 'span';
    search.dispatchEvent(new Event('input'));
    expect($('tk-lang-list').querySelectorAll('.lang-opt').length).toBe(1);

    search.value = 'zzzzz';
    search.dispatchEvent(new Event('input'));
    expect($('tk-lang-empty').hidden).toBe(false);
  });

  it('enables Start once a language is picked', async () => {
    await mount();
    $('tk-lang-trigger').click();
    ($('tk-lang-list').querySelector('.lang-opt') as HTMLElement).click();
    expect(($('tk-start') as HTMLButtonElement).disabled).toBe(false);
    expect($('tk-lang-panel').hidden).toBe(true);
    expect(track).toHaveBeenCalledWith(
      'talk_to_anyone_language_selected',
      expect.objectContaining({ language_pair: expect.stringContaining('it-') }),
    );
  });

  it('never offers a client-direct tier', async () => {
    // Enhanced runs the provider in the browser, so the server cannot gate its frames.
    // Offering it here would mean silently swapping the user's choice at start.
    await mount();
    $('tk-lang-trigger').click();
    ($('tk-lang-list').querySelector('.lang-opt') as HTMLElement).click();
    const names = [...$('tk-tier-options').querySelectorAll('.engine-opt')].map(
      (n) => n.textContent ?? '',
    );
    expect(names.some((n) => n.includes('Standard'))).toBe(true);
    expect(names.some((n) => n.includes('Enhanced'))).toBe(false);
  });

  it('survives an engines endpoint that fails', async () => {
    // A dead /api/engines must not leave a blank screen with a dead button.
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => {
        throw new Error('offline');
      }),
    );
    await mount();
    $('tk-lang-trigger').click();
    expect($('tk-lang-list').children.length).toBe(0);
    expect(($('tk-start') as HTMLButtonElement).disabled).toBe(true);
  });
});

describe('starting', () => {
  async function pickAndStart(): Promise<void> {
    await mount();
    $('tk-lang-trigger').click();
    const es = [...$('tk-lang-list').querySelectorAll('.lang-opt')].find((o) =>
      (o.textContent ?? '').includes('Español'),
    ) as HTMLElement;
    es.click();
    $('tk-start').click();
    await flush();
  }

  it('passes both languages and the capability-derived capture format', async () => {
    await pickAndStart();
    expect(started).toHaveLength(1);
    expect(started[0].userLang).toBe('it');
    expect(started[0].otherLang).toBe('es');
    // Standard is speech-to-speech, so PCM — decided by capability, never by id.
    expect(started[0].needsPcm).toBe(true);
    expect(track).toHaveBeenCalledWith(
      'talk_to_anyone_started',
      expect.objectContaining({ language_pair: 'it-es' }),
    );
  });

  it('refuses on an unsupported browser without touching the microphone', async () => {
    supported = false;
    await pickAndStart();
    expect(started).toHaveLength(0);
    expect($('tk-setup-status').textContent).toBeTruthy();
  });

  it('refuses when signed out', async () => {
    loggedIn = false;
    await mount();
    // The message is up front, before a language is even chosen.
    expect($('tk-setup-status').textContent).toBeTruthy();
    $('tk-lang-trigger').click();
    ($('tk-lang-list').querySelector('.lang-opt') as HTMLElement).click();
    $('tk-start').click();
    await flush();
    expect(started).toHaveLength(0);
  });

  it('wires mute, pause/resume and end', async () => {
    await pickAndStart();
    const convo = instances[0];

    $('tk-mute').click();
    expect(convo.setMuted).toHaveBeenCalledWith(true);
    $('tk-mute').click();
    expect(convo.setMuted).toHaveBeenLastCalledWith(false);

    $('tk-pause').click();
    expect(convo.pause).toHaveBeenCalled();
    $('tk-pause').click();
    expect(convo.resume).toHaveBeenCalled();

    $('tk-end').click();
    expect(convo.end).toHaveBeenCalled();
    expect(track).toHaveBeenCalledWith(
      'talk_to_anyone_ended',
      expect.objectContaining({ duration_seconds: expect.any(Number) }),
    );
  });

  it('ends the session when the page goes away', async () => {
    // A leaked microphone track leaves the browser's recording dot on after the user
    // has navigated away — a trust problem before it is a bug.
    await pickAndStart();
    window.dispatchEvent(new Event('pagehide'));
    expect(instances[0].end).toHaveBeenCalled();
  });
});

describe('live rendering', () => {
  async function running() {
    await mount();
    $('tk-lang-trigger').click();
    ([...$('tk-lang-list').querySelectorAll('.lang-opt')].find((o) =>
      (o.textContent ?? '').includes('Español'),
    ) as HTMLElement).click();
    $('tk-start').click();
    await flush();
    return started[0] as Record<string, (arg: unknown) => void>;
  }

  it('shows the conversation screen and the language pair', async () => {
    const opts = await running();
    opts.onState({ phase: 'live', activity: 'listening', sessionId: 's', failure: null });
    expect($('tk-live').hidden).toBe(false);
    expect($('tk-setup').hidden).toBe(true);
    expect($('tk-pair').textContent).toContain('Español');
  });

  it('appends finished exchanges and reveals the history heading', async () => {
    const opts = await running();
    opts.onExchange({
      id: 1,
      spokenLang: 'it',
      originalText: 'Vorrei andare alla stazione',
      targetLang: 'es',
      translatedText: 'Quiero ir a la estación',
    });
    expect($('tk-history').children.length).toBe(1);
    expect($('tk-history-title').hidden).toBe(false);
    expect(track).toHaveBeenCalledWith(
      'talk_to_anyone_translation_completed',
      expect.objectContaining({ language_pair: 'it-es' }),
    );
  });

  it('shows failure copy on the problem screen and lets the user retry', async () => {
    const opts = await running();
    opts.onState({ phase: 'error', activity: 'listening', sessionId: null, failure: 'mic_denied' });
    expect($('tk-problem').hidden).toBe(false);
    expect($('tk-problem-copy').textContent).toContain('Microphone');
    expect(track).toHaveBeenCalledWith(
      'talk_to_anyone_error',
      expect.objectContaining({ error_code: 'mic_denied' }),
    );

    $('tk-retry').click();
    expect($('tk-problem').hidden).toBe(true);
    expect($('tk-setup').hidden).toBe(false);
  });

  it('explains a tier the server had to change', async () => {
    const opts = await running();
    (opts.onEngineChanged as unknown as (to: string, r: string) => void)(
      'standard',
      'talk_client_direct_unsupported',
    );
    expect($('tk-tier-note').hidden).toBe(false);
    expect($('tk-tier-note').textContent).toContain('Enhanced');
  });

  it('keeps a recoverable notice out of the error screen', async () => {
    const opts = await running();
    (opts.onNotice as unknown as (c: string) => void)('provider_unavailable');
    expect($('tk-status-text').textContent).toContain('Translation is unavailable');
    expect($('tk-problem').hidden).toBe(true);
  });
});
