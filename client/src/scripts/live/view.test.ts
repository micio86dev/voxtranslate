// @vitest-environment jsdom
import { beforeEach, describe, expect, it } from 'vitest';
import {
  HISTORY_RENDER_LIMIT,
  buildExchangeCard,
  buildRoom,
  fill,
  formatCredits,
  langLabel,
  renderCaptions,
  renderDirection,
  renderOutputOptions,
  renderPartner,
  renderPhase,
  type RoomEls,
} from './view';
import { initialContext, type LiveContext } from './session-machine';
import type { Exchange } from './conversation';

let els: RoomEls;
const ctx = (over: Partial<LiveContext>): LiveContext => ({ ...initialContext(), ...over });
const partner = { peerId: 'b', name: 'Bea', lang: 'es', muted: false };

beforeEach(() => {
  document.body.innerHTML = '';
  els = buildRoom(document, { role: 'host' });
  document.body.append(els.root);
});

describe('helpers', () => {
  it('fill substitutes every placeholder occurrence', () => {
    expect(fill('{a} and {a} to {b}', { a: 'x', b: 'y' })).toBe('x and x to y');
  });
  it('langLabel names a language, never a bare flag, and survives an unknown code', () => {
    expect(langLabel('it')).toMatchObject({ native: 'Italiano', english: 'Italian', rtl: false });
    expect(langLabel('zz')).toMatchObject({ native: 'zz', flag: '🌐' });
    expect(langLabel('ar').rtl).toBe(true);
  });
  it('formatCredits shows two decimals and never a negative amount', () => {
    expect(formatCredits(12.5)).toBe('$12.50');
    expect(formatCredits(-1)).toBe('$0.00');
  });
});

describe('buildRoom', () => {
  it('is accessible: a polite live region for the caption, labelled controls, tap-sized buttons', () => {
    expect(els.caption.closest('[aria-live="polite"]')).not.toBeNull();
    expect(els.status.getAttribute('role')).toBe('status');
    expect(els.mute.getAttribute('aria-pressed')).toBe('false');
    expect(els.history.getAttribute('aria-live')).toBe('off');
    for (const b of [els.mute, els.end, els.endYes, els.endNo]) expect(b.type).toBe('button');
  });

  it('keeps a page heading for assistive tech once the visible one is hidden', () => {
    expect(els.root.querySelector('h1')?.textContent).toBe('Live Connect');
  });

  it('hides everything a conversation does not have yet', () => {
    expect(els.live.hidden).toBe(true);
    expect(els.ended.hidden).toBe(true);
    expect(els.problem.hidden).toBe(true);
    expect(els.outputField.hidden).toBe(true);
    expect(els.confirm.hidden).toBe(true);
  });

  it('shows the credit line only to the host', () => {
    expect(els.credits).not.toBeNull();
    const guest = buildRoom(document, { role: 'guest' });
    expect(guest.credits).toBeNull();
    // The organisation pays for a desk conversation: the staff seat has no credit line either.
    expect(buildRoom(document, { role: 'staff' }).credits).toBeNull();
  });

  it('sends a staff member back to the desk list, and a guest or host where they came from', () => {
    const staff = buildRoom(document, { role: 'staff' });
    expect(staff.endedAction.getAttribute('href')).toBe('/live/desks');
    expect(staff.endedAction.textContent).toBe('Back to desks');
    expect(buildRoom(document, { role: 'guest' }).endedAction.getAttribute('href')).toBe('/');
    expect(els.endedAction.getAttribute('href')).toBe('/live');
  });
});

describe('renderPhase', () => {
  it('shows the conversation while connecting/waiting/live, with the right chip', () => {
    for (const phase of ['requesting_mic', 'connecting', 'waiting_peer', 'live', 'reconnecting'] as const) {
      renderPhase(els, ctx({ phase }), 'host');
      expect(els.live.hidden, phase).toBe(false);
      expect(els.ended.hidden).toBe(true);
      expect(els.problem.hidden).toBe(true);
      expect(els.root.dataset.phase).toBe(phase);
    }
    renderPhase(els, ctx({ phase: 'waiting_peer' }), 'host');
    expect(els.statusText.textContent).toBe('Waiting for the other person…');
    renderPhase(els, ctx({ phase: 'reconnecting' }), 'host');
    expect(els.statusText.textContent).toContain('reconnecting');
  });

  it('a caller may say what "waiting" means: a desk customer waits for the staff, not for "the other person"', () => {
    renderPhase(els, ctx({ phase: 'waiting_peer' }), 'guest', { waitingKey: 'liveDeskWaiting' });
    expect(els.statusText.textContent).toBe('Waiting for the staff…');
    // Only that phase: the other chips are unchanged.
    renderPhase(els, ctx({ phase: 'live' }), 'guest', { waitingKey: 'liveDeskWaiting' });
    expect(els.statusText.textContent).toBe('Connected');
  });

  it('ended shows why, from this role\'s point of view', () => {
    renderPhase(els, ctx({ phase: 'ended', endReason: 'ended_by_guest' }), 'host');
    expect(els.live.hidden).toBe(true);
    expect(els.ended.hidden).toBe(false);
    expect(els.endedText.textContent).toBe('The other person ended the conversation.');
    renderPhase(els, ctx({ phase: 'ended', endReason: 'ended_by_guest' }), 'guest');
    expect(els.endedText.textContent).toBe('You ended the conversation.');
  });

  it('failed shows the problem, with retry for connection problems and buy-credits for credits', () => {
    renderPhase(els, ctx({ phase: 'failed', failure: 'mic_denied' }), 'host');
    expect(els.problem.hidden).toBe(false);
    expect(els.problemText.textContent).toContain('Microphone access is blocked');
    expect(els.retry.hidden).toBe(false);
    expect(els.buy.hidden).toBe(true);

    renderPhase(els, ctx({ phase: 'failed', failure: 'credits' }), 'host');
    expect(els.buy.hidden).toBe(false);
    expect(els.buy.getAttribute('href')).toBe('/?buy=1');
    expect(els.retry.hidden).toBe(true);

    renderPhase(els, ctx({ phase: 'failed', failure: 'credits' }), 'guest');
    expect(els.buy.hidden).toBe(true);

    renderPhase(els, ctx({ phase: 'failed', failure: 'busy' }), 'guest');
    expect(els.retry.hidden).toBe(false);
  });

  it('collapses the confirm bar when the phase changes', () => {
    els.confirm.hidden = false;
    renderPhase(els, ctx({ phase: 'ended', endReason: 'ended_by_me' }), 'host');
    expect(els.confirm.hidden).toBe(true);
  });
});

describe('renderPartner / renderDirection', () => {
  it('shows the partner name, language and connected state', () => {
    renderPartner(els, partner, 'live');
    expect(els.partnerName.textContent).toBe('Bea');
    expect(els.partnerMeta.textContent).toContain('Español');
    expect(els.partnerCard.dataset.online).toBe('true');
    expect(els.partnerMuted.hidden).toBe(true);
    renderPartner(els, { ...partner, muted: true }, 'live');
    expect(els.partnerMuted.hidden).toBe(false);
  });

  it('shows "not connected yet" while nobody is in the room, and a reconnecting partner as offline', () => {
    renderPartner(els, null, 'waiting_peer');
    expect(els.partnerMeta.textContent).toBe('Not connected yet');
    expect(els.partnerCard.dataset.online).toBe('false');
    renderPartner(els, partner, 'reconnecting');
    expect(els.partnerCard.dataset.online).toBe('false');
  });

  it('states both directions explicitly, with endonyms', () => {
    renderDirection(els, 'it', 'es');
    expect(els.dirOut.textContent).toBe('You speak Italiano, they hear Español');
    expect(els.dirIn.textContent).toBe('They speak Español, you hear Italiano');
  });

  it('says the partner language is not known yet instead of inventing one', () => {
    renderDirection(els, 'it', null);
    expect(els.dirOut.textContent).toBe('You speak Italiano');
    expect(els.dirIn.textContent).toBe('Their language appears when they join.');
  });
});

describe('renderCaptions', () => {
  const ex = (id: number, who: Exchange['who']): Exchange => ({ id, who, original: `o${id}`, originalLang: 'es', translated: `t${id}`, translatedLang: 'it' });

  it('puts the partner translation big (with language and direction) and their original small', () => {
    renderCaptions(els, { partner: { text: 'Ciao', original: 'Hola', lang: 'es', settled: false }, mine: null, history: [] });
    expect(els.caption.textContent).toBe('Ciao');
    expect(els.captionOriginal.textContent).toBe('Hola');
    expect(els.captionOriginal.getAttribute('lang')).toBe('es');
    expect(els.mine.hidden).toBe(true);
  });

  it('collapses the stage until the partner has said something', () => {
    const stage = els.caption.parentElement as HTMLElement;
    expect(stage.dataset.empty).toBe('true');
    renderCaptions(els, { partner: { text: 'Ciao', original: '', lang: 'es', settled: false }, mine: null, history: [] });
    expect(stage.dataset.empty).toBe('false');
  });

  it('shows what I said small, under a "You said" label', () => {
    renderCaptions(els, { partner: null, mine: { text: 'Buongiorno', original: '', lang: 'it', settled: true }, history: [] });
    expect(els.mine.hidden).toBe(false);
    expect(els.mineText.textContent).toBe('Buongiorno');
    expect(els.mine.textContent).toContain('You said');
  });

  it('marks RTL text with dir="rtl"', () => {
    renderCaptions(els, { partner: { text: 'مرحبا', original: 'مرحبا', lang: 'ar', settled: false }, mine: null, history: [] });
    expect(els.captionOriginal.getAttribute('dir')).toBe('rtl');
  });

  it('renders the history newest first and never beyond the render limit', () => {
    const history = Array.from({ length: HISTORY_RENDER_LIMIT + 5 }, (_, i) => ex(i + 1, i % 2 ? 'me' : 'partner'));
    renderCaptions(els, { partner: null, mine: null, history });
    expect(els.history.children).toHaveLength(HISTORY_RENDER_LIMIT);
    expect((els.history.firstElementChild as HTMLElement).dataset.exchange).toBe(String(HISTORY_RENDER_LIMIT + 5));
    expect(els.historyTitle.hidden).toBe(false);
    renderCaptions(els, { partner: null, mine: null, history: [] });
    expect(els.historyTitle.hidden).toBe(true);
  });
});

describe('buildExchangeCard', () => {
  it('shows the translation prominently for the partner and the original beneath', () => {
    const card = buildExchangeCard({ id: 1, who: 'partner', original: 'Hola', originalLang: 'es', translated: 'Ciao', translatedLang: 'it' }, document);
    expect(card.dataset.who).toBe('partner');
    expect(card.querySelector('.lc-card-main')?.textContent).toContain('Ciao');
    expect(card.querySelector('.lc-card-sub')?.textContent).toContain('Hola');
  });

  it('leaves a missing translation out rather than render a hollow row', () => {
    const card = buildExchangeCard({ id: 2, who: 'me', original: 'Ciao', originalLang: 'it', translated: '', translatedLang: 'es' }, document);
    expect(card.querySelector('.lc-card-sub')).toBeNull();
    expect(card.querySelector('.lc-card-main')?.textContent).toContain('Ciao');
  });

  it('sets text via textContent only (no HTML injection from a transcript)', () => {
    const card = buildExchangeCard({ id: 3, who: 'partner', original: '<img src=x onerror=alert(1)>', originalLang: 'es', translated: '<b>x</b>', translatedLang: 'it' }, document);
    expect(card.querySelector('img')).toBeNull();
    expect(card.querySelector('b')).toBeNull();
  });
});

describe('renderOutputOptions', () => {
  it('stays hidden with no devices and shows a select with them', () => {
    renderOutputOptions(els, []);
    expect(els.outputField.hidden).toBe(true);
    renderOutputOptions(els, [
      { deviceId: 'default', label: '' },
      { deviceId: 'abc', label: 'Speaker' },
    ] as MediaDeviceInfo[]);
    expect(els.outputField.hidden).toBe(false);
    expect([...els.output.options].map((o) => o.textContent)).toEqual(['Default', 'Speaker']);
    expect([...els.output.options].map((o) => o.value)).toEqual(['default', 'abc']);
  });
});
