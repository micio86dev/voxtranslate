// @vitest-environment jsdom
import { describe, it, expect, beforeEach } from 'vitest';
import {
  buildExchangeCard,
  creditMeter,
  failureKey,
  fill,
  HISTORY_LIMIT,
  isActivePhase,
  langLabel,
  pushHistory,
  renderLive,
  renderPair,
  renderStatus,
  statusKey,
  type TalkElements,
} from './view';
import { initialContext, transition, type SessionContext } from './session-machine';
import type { Exchange, LiveExchange } from './conversation';

function ctxWith(patch: Partial<SessionContext>): SessionContext {
  return { ...initialContext(), ...patch };
}

function mountEls(): TalkElements {
  document.body.innerHTML = `
    <div id="root">
      <p id="status"></p>
      <p id="detected" hidden></p>
      <p id="translation"></p>
      <p id="original"></p>
      <div id="pair"></div>
      <div id="history"></div>
    </div>`;
  return {
    root: document.getElementById('root'),
    status: document.getElementById('status'),
    detected: document.getElementById('detected'),
    liveTranslation: document.getElementById('translation'),
    liveOriginal: document.getElementById('original'),
    pair: document.getElementById('pair'),
    history: document.getElementById('history'),
  };
}

const blankLive: LiveExchange = {
  spokenLang: null,
  targetLang: null,
  originalText: '',
  translatedText: '',
};

beforeEach(() => {
  document.body.innerHTML = '';
});

describe('language labels', () => {
  it('always carries a name alongside the flag', () => {
    // A flag alone is unreadable to a screen reader and ambiguous to anyone who does not
    // recognise it (brief §30).
    const it = langLabel('it');
    expect(it.native).toBe('Italiano');
    expect(it.english).toBe('Italian');
    expect(it.flag).toBeTruthy();
    expect(it.rtl).toBe(false);
  });

  it('marks right-to-left languages', () => {
    expect(langLabel('ar').rtl).toBe(true);
    expect(langLabel('he').rtl).toBe(true);
  });

  it('falls back to the raw code rather than rendering nothing', () => {
    const unknown = langLabel('zzz');
    expect(unknown.native).toBe('zzz');
    expect(unknown.flag).toBe('🌐');
  });
});

describe('status', () => {
  it('says "listening" while the direction is undecided', () => {
    // The single most important copy decision on this screen (brief §16): an undecided
    // direction is normal, so it must never read as an error or expose a confidence.
    const detecting = ctxWith({ phase: 'live', activity: 'detecting' });
    expect(statusKey(detecting)).toBe('talkStatusListening');
    expect(statusKey(ctxWith({ phase: 'live', activity: 'listening' }))).toBe(
      'talkStatusListening',
    );
  });

  it('distinguishes translating, speaking, paused and reconnecting', () => {
    expect(statusKey(ctxWith({ phase: 'live', activity: 'translating' }))).toBe(
      'talkStatusTranslating',
    );
    expect(statusKey(ctxWith({ phase: 'live', activity: 'speaking' }))).toBe('talkStatusSpeaking');
    expect(statusKey(ctxWith({ phase: 'paused' }))).toBe('talkStatusPaused');
    expect(statusKey(ctxWith({ phase: 'reconnecting' }))).toBe('talkStatusReconnecting');
    expect(statusKey(ctxWith({ phase: 'connecting' }))).toBe('talkStatusConnecting');
  });

  it('maps every failure to its own copy', () => {
    // Generic copy would tell a user with no microphone to check their signal.
    const kinds = ['mic_denied', 'mic_blocked', 'mic_missing', 'unsupported', 'connection', 'credits', 'provider'] as const;
    const keys = kinds.map((k) => failureKey(k));
    expect(new Set(keys).size).toBe(kinds.length);
    expect(failureKey(null)).toBe('talkErrorGeneric');
    expect(statusKey(ctxWith({ phase: 'error', failure: 'mic_denied' }))).toBe(
      'talkErrorMicDenied',
    );
  });

  it('hides the detected chip until a direction is committed', () => {
    // A chip flickering between two flags on every partial is exactly the visual noise
    // brief §15 warns against.
    const els = mountEls();
    renderStatus(els, ctxWith({ phase: 'live' }), blankLive);
    expect(els.detected!.hidden).toBe(true);

    renderStatus(els, ctxWith({ phase: 'live' }), { ...blankLive, spokenLang: 'it', targetLang: 'es' });
    expect(els.detected!.hidden).toBe(false);
    expect(els.detected!.textContent).toContain('Italiano');
    expect(els.detected!.getAttribute('data-lang')).toBe('it');
  });

  it('drops the chip when the session leaves live', () => {
    const els = mountEls();
    const spoken = { ...blankLive, spokenLang: 'it', targetLang: 'es' };
    renderStatus(els, ctxWith({ phase: 'live' }), spoken);
    renderStatus(els, ctxWith({ phase: 'paused' }), spoken);
    expect(els.detected!.hidden).toBe(true);
  });

  it('exposes the phase and activity as data attributes for styling', () => {
    const els = mountEls();
    renderStatus(els, ctxWith({ phase: 'live', activity: 'speaking' }), blankLive);
    expect(els.root!.dataset.phase).toBe('live');
    expect(els.root!.dataset.activity).toBe('speaking');
    // Outside a live session the activity is meaningless and must not linger.
    renderStatus(els, ctxWith({ phase: 'paused', activity: 'speaking' }), blankLive);
    expect(els.root!.dataset.activity).toBe('listening');
  });

  it('knows which phases are active', () => {
    expect(isActivePhase(ctxWith({ phase: 'live' }))).toBe(true);
    expect(isActivePhase(ctxWith({ phase: 'reconnecting' }))).toBe(true);
    expect(isActivePhase(ctxWith({ phase: 'idle' }))).toBe(false);
  });

  it('covers every reachable phase', () => {
    // A missing case would render an empty status line — the screen would look broken
    // while the conversation carried on perfectly.
    let ctx = initialContext();
    const seen = new Set<string>();
    for (const ev of [
      { type: 'START_REQUESTED' },
      { type: 'MIC_GRANTED' },
      { type: 'SOCKET_OPEN' },
      { type: 'PAUSE_REQUESTED' },
      { type: 'RESUME_REQUESTED' },
      { type: 'SOCKET_CLOSED', recoverable: true },
      { type: 'RECONNECT_SUCCEEDED' },
      { type: 'STOP_REQUESTED' },
      { type: 'TEARDOWN_COMPLETE' },
    ] as const) {
      seen.add(statusKey(ctx));
      ctx = transition(ctx, ev, 's').context;
      expect(statusKey(ctx)).toBeTruthy();
    }
    expect(seen.size).toBeGreaterThan(4);
  });
});

describe('live card', () => {
  it('renders translation and original with the right direction', () => {
    const els = mountEls();
    renderLive(els, {
      spokenLang: 'ar',
      targetLang: 'it',
      originalText: 'أين محطة القطار',
      translatedText: "Dov'è la stazione",
    });
    expect(els.liveTranslation!.textContent).toBe("Dov'è la stazione");
    expect(els.liveTranslation!.getAttribute('dir')).toBe('ltr');
    expect(els.liveTranslation!.lang).toBe('it');
    // The Arabic original must render right-to-left or it is unreadable.
    expect(els.liveOriginal!.getAttribute('dir')).toBe('rtl');
    expect(els.liveOriginal!.lang).toBe('ar');
  });

  it('clears direction when there is nothing to show', () => {
    const els = mountEls();
    renderLive(els, blankLive);
    expect(els.liveTranslation!.hasAttribute('dir')).toBe(false);
    expect(els.liveTranslation!.textContent).toBe('');
  });
});

describe('history', () => {
  const exchange = (id: number): Exchange => ({
    id,
    spokenLang: 'it',
    originalText: 'Vorrei andare alla stazione',
    targetLang: 'es',
    translatedText: 'Quiero ir a la estación',
  });

  it('builds a card with both languages named, flags decorative', () => {
    const card = buildExchangeCard(exchange(1), document);
    expect(card.dataset.exchange).toBe('1');
    expect(card.textContent).toContain('Vorrei andare alla stazione');
    expect(card.textContent).toContain('Quiero ir a la estación');
    expect(card.textContent).toContain('Italiano');
    expect(card.textContent).toContain('Español');
    // Flags are decoration; the name next to them carries the meaning.
    for (const flag of card.querySelectorAll('.tk-flag')) {
      expect(flag.getAttribute('aria-hidden')).toBe('true');
    }
  });

  it('puts the newest exchange first', () => {
    const els = mountEls();
    pushHistory(els.history!, buildExchangeCard(exchange(1), document));
    pushHistory(els.history!, buildExchangeCard(exchange(2), document));
    expect((els.history!.firstElementChild as HTMLElement).dataset.exchange).toBe('2');
  });

  it('bounds the DOM', () => {
    // A ten-minute conversation is hundreds of sentences; unbounded growth would make
    // the page crawl on the phone it is meant to run on.
    const els = mountEls();
    for (let i = 1; i <= HISTORY_LIMIT + 10; i++) {
      pushHistory(els.history!, buildExchangeCard(exchange(i), document));
    }
    expect(els.history!.children.length).toBe(HISTORY_LIMIT);
    expect((els.history!.firstElementChild as HTMLElement).dataset.exchange).toBe(
      String(HISTORY_LIMIT + 10),
    );
  });
});

describe('language pair header', () => {
  it('names both languages and labels the region', () => {
    const els = mountEls();
    renderPair(els, 'it', 'es');
    expect(els.pair!.textContent).toContain('Italiano');
    expect(els.pair!.textContent).toContain('Español');
    expect(els.pair!.textContent).toContain('⇄');
    expect(els.pair!.getAttribute('aria-label')).toBeTruthy();
  });

  it('repaints rather than appending', () => {
    const els = mountEls();
    renderPair(els, 'it', 'es');
    renderPair(els, 'it', 'fr');
    expect(els.pair!.textContent).not.toContain('Español');
    expect(els.pair!.textContent).toContain('Français');
  });
});

describe('credit meter', () => {
  it('shows the spend with enough precision to visibly move', () => {
    // A Standard conversation burns ~$0.0009 every five seconds. At the app's usual two
    // decimals the spend would read "$0.00" for the first half-minute and the meter would
    // look broken, which is the opposite of showing credits draining in real time.
    const m = creditMeter(12.5108, 12.5099)!;
    expect(m.used).toBe('$0.0009');
    // The remaining balance keeps two decimals, matching everywhere else it is shown.
    expect(m.left).toBe('$12.51');
  });

  it('tracks a whole conversation', () => {
    expect(creditMeter(12.535, 12.511)!.used).toBe('$0.0240');
  });

  it('never renders a negative spend', () => {
    // Topping up mid-conversation raises the balance above where it started.
    expect(creditMeter(10, 25)!.used).toBe('$0.0000');
  });

  it('starts from zero when the opening balance is unknown', () => {
    // The profile refresh failed; the first `balance_update` is all we have. Better a
    // meter that starts at zero than one that invents a spend.
    const m = creditMeter(null, 8.25)!;
    expect(m.used).toBe('$0.0000');
    expect(m.left).toBe('$8.25');
  });

  it('renders nothing when billing is off', () => {
    // A backend in guest-only mode sends no balance at all; a "$0.0000" meter would be
    // a lie about a system that is not counting.
    expect(creditMeter(12, null)).toBeNull();
  });
});

describe('fill', () => {
  it('substitutes every occurrence and leaves unknown placeholders alone', () => {
    // `t()` takes no parameters, so interpolation is done at the call site — and a
    // translator who drops `{lang}` must not crash the status line.
    expect(fill('{lang} detected', { lang: 'Italiano' })).toBe('Italiano detected');
    expect(fill('{a} and {a}', { a: 'x' })).toBe('x and x');
    expect(fill('no placeholder', { lang: 'x' })).toBe('no placeholder');
    expect(fill('{from} → {to}', { from: 'A', to: 'B' })).toBe('A → B');
  });
});
