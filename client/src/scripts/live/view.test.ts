// @vitest-environment jsdom
import { beforeEach, describe, expect, it } from 'vitest';
import {
  HISTORY_RENDER_LIMIT,
  buildExchangeCard,
  buildRoom,
  renderTier,
  fill,
  formatCredits,
  langLabel,
  renderCaptions,
  renderDirection,
  renderOutputOptions,
  renderPartners,
  renderPhase,
  renderSpeaking,
  renderVoiceNote,
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
    expect(els.stage.getAttribute('aria-live')).toBe('polite');
    expect(els.announce.getAttribute('aria-live')).toBe('polite');
    expect(els.partners.getAttribute('aria-labelledby')).toBe('lc-partners-title');
    expect(document.getElementById('lc-partners-title')?.textContent).toBe('In this conversation');
    expect(els.status.getAttribute('role')).toBe('status');
    expect(els.mute.getAttribute('aria-pressed')).toBe('false');
    expect(els.history.getAttribute('aria-live')).toBe('off');
    for (const b of [els.mute, els.end, els.endYes, els.endNo]) expect(b.type).toBe('button');
  });

  it('tells everybody in the room that translations and any translated voice are AI-generated (AI Act art. 50)', () => {
    const note = els.root.querySelector('.lc-ai-notice');
    expect(note?.textContent).toBe('Translations, captions and any translated voice are AI-generated and may contain errors.');
    expect(note?.getAttribute('data-i18n')).toBe('aiGeneratedNotice');
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

  it('has a tier badge inside the status chip and a polite, dismissible downgrade notice, both hidden at first', () => {
    expect(els.tier.hidden).toBe(true);
    expect(els.status.contains(els.tier)).toBe(true);
    expect(els.tierNotice.hidden).toBe(true);
    expect(els.tierNotice.getAttribute('aria-live')).toBe('polite');
    expect(els.tierNoticeClose.type).toBe('button');
    expect(els.tierNoticeClose.textContent).toBe('Dismiss');
  });

  it('renderTier shows the tier name, or hides the badge when the tier is unknown', () => {
    renderTier(els, 'gemini_live_translate');
    expect(els.tier.hidden).toBe(false);
    expect(els.tier.textContent).toBe('Quality: Premium');
    renderTier(els, undefined);
    expect(els.tier.hidden).toBe(true);
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

describe('buildRoom: ending words follow the size of the conversation', () => {
  const text = (e: RoomEls) => [e.end.textContent, e.confirmText.textContent, e.endYes.textContent];

  it('a pair keeps "End" and the both-of-you confirmation', () => {
    expect(text(buildRoom(document, { role: 'host' }))).toEqual(['End', 'End the conversation for both of you?', 'End conversation']);
    expect(text(buildRoom(document, { role: 'guest', maxGuests: 1 }))).toEqual(['End', 'End the conversation for both of you?', 'End conversation']);
  });

  it('the host of several guests is told it ends for everyone', () => {
    expect(text(buildRoom(document, { role: 'host', maxGuests: 3 }))).toEqual(['End', 'End the conversation for everyone?', 'End conversation']);
  });

  it('a guest of several guests leaves instead', () => {
    expect(text(buildRoom(document, { role: 'guest', maxGuests: 2 }))).toEqual([
      'Leave',
      'Leave the conversation? The others will carry on without you.',
      'Leave',
    ]);
  });

  it('a desk is untouched by maxGuests', () => {
    expect(text(buildRoom(document, { role: 'staff', maxGuests: 3 }))[0]).toBe('End');
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

  it('a guest who left a conversation that carries on sees that, not "ended"', () => {
    renderPhase(els, ctx({ phase: 'ended', endReason: 'left_by_me' }), 'guest');
    expect(els.endedText.textContent).toBe('You left the conversation. The others carry on.');
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

  it('has a distance tip next to the headphones setting, translatable and visible at first', () => {
    const tip = els.root.querySelector('#lc-distance-tip') as HTMLElement;
    expect(tip).not.toBeNull();
    expect(tip.dataset.i18n).toBe('liveDistanceTip');
    expect(tip.hidden).toBe(false);
    expect(els.headphones.closest('.lc-settings')?.contains(tip)).toBe(true);
  });

  it('shows the end confirmation as a centred modal, not as a card at the bottom of the page', () => {
    // On a phone the sticky control bar sits at the bottom of the screen: a card that follows it in
    // the page flow opens below the fold and nobody sees it. The overlay is fixed over everything.
    expect(els.confirm.classList.contains('lc-confirm-overlay')).toBe(true);
    const dialog = els.confirm.querySelector('[role="alertdialog"]');
    expect(dialog).not.toBeNull();
    expect(dialog?.getAttribute('aria-modal')).toBe('true');
    expect(dialog?.getAttribute('aria-labelledby')).toBe('lc-confirm-text');
    expect(dialog?.contains(els.confirmText)).toBe(true);
    expect(dialog?.contains(els.endYes)).toBe(true);
    expect(dialog?.contains(els.endNo)).toBe(true);
    expect(els.confirm.parentElement).toBe(els.live);
  });

  it('collapses the confirm bar when the phase changes', () => {
    els.confirm.hidden = false;
    renderPhase(els, ctx({ phase: 'ended', endReason: 'ended_by_me' }), 'host');
    expect(els.confirm.hidden).toBe(true);
  });
});

describe('renderPartners / renderDirection', () => {
  const carlo = { peerId: 'c', name: 'Carlo', lang: 'en', muted: true };
  const rows = () => [...els.partners.querySelectorAll<HTMLElement>('.lc-partner')];

  it('shows one row per partner with name, language and connected state', () => {
    renderPartners(els, [partner], 'live');
    expect(rows()).toHaveLength(1);
    expect(rows()[0].querySelector('.lc-partner-name')?.textContent).toBe('Bea');
    expect(rows()[0].querySelector('.lc-partner-meta')?.textContent).toContain('Español');
    expect(rows()[0].dataset.online).toBe('true');
    expect(rows()[0].querySelector('.lc-muted-badge')).toBeNull();
  });

  it('lists three partners, each with their own language and their own muted icon', () => {
    renderPartners(els, [partner, carlo, { peerId: 'd', name: 'Dina', lang: 'fr', muted: false }], 'live');
    expect(rows().map((r) => r.querySelector('.lc-partner-name')?.textContent)).toEqual(['Bea', 'Carlo', 'Dina']);
    expect(rows().map((r) => !!r.querySelector('.lc-muted-badge'))).toEqual([false, true, false]);
    expect(rows()[1].querySelector('.lc-muted-badge')?.textContent).toBe('Muted');
    expect(rows()[2].querySelector('.lc-partner-meta')?.textContent).toContain('Français');
  });

  it('shows "not connected yet" while nobody is in the room, and a reconnecting partner as offline', () => {
    renderPartners(els, [], 'waiting_peer');
    expect(rows()).toHaveLength(1);
    expect(rows()[0].querySelector('.lc-partner-meta')?.textContent).toBe('Not connected yet');
    expect(rows()[0].dataset.online).toBe('false');
    renderPartners(els, [partner], 'reconnecting');
    expect(rows()[0].dataset.online).toBe('false');
  });

  it('falls back to a generic name, and rebuilds rather than accumulates', () => {
    renderPartners(els, [{ ...partner, name: '' }], 'live');
    expect(rows()[0].querySelector('.lc-partner-name')?.textContent).toBe('Guest');
    renderPartners(els, [partner], 'live');
    expect(rows()).toHaveLength(1);
  });

  it('states both directions explicitly, with endonyms', () => {
    renderDirection(els, 'it', ['es']);
    expect(els.dirOut.textContent).toBe('You speak Italiano, they hear Español');
    expect(els.dirIn.textContent).toBe('They speak Español, you hear Italiano');
  });

  it('lists every distinct partner language once (IT to ES, EN)', () => {
    renderDirection(els, 'it', ['es', 'en', 'es-MX']);
    expect(els.dirOut.textContent).toBe('You speak Italiano, they hear Español, English');
    expect(els.dirIn.textContent).toBe('They speak Español, English, you hear Italiano');
  });

  it('says the partner language is not known yet instead of inventing one', () => {
    renderDirection(els, 'it', []);
    expect(els.dirOut.textContent).toBe('You speak Italiano');
    expect(els.dirIn.textContent).toBe('Their language appears when they join.');
  });
});

describe('renderCaptions', () => {
  const ex = (id: number, who: Exchange['who']): Exchange => ({ id, who, original: `o${id}`, originalLang: 'es', translated: `t${id}`, translatedLang: 'it' });
  const sp = (id: string, text: string, over: Record<string, unknown> = {}) => ({
    speakerId: id, name: id.toUpperCase(), text, original: `orig-${text}`, lang: 'es', settled: false, ...over,
  });
  const blocks = () => [...els.stage.querySelectorAll<HTMLElement>('.lc-speaker')];

  it('puts the speaker translation big (with language and direction) and their original small', () => {
    renderCaptions(els, { others: [sp('b', 'Ciao', { original: 'Hola' })], mine: null, history: [] }, 'it');
    const [b] = blocks();
    expect(b.querySelector('.lc-caption')?.textContent).toBe('Ciao');
    expect(b.querySelector('.lc-caption')?.getAttribute('lang')).toBe('it');
    expect(b.querySelector('.lc-caption-original')?.textContent).toBe('Hola');
    expect(b.querySelector('.lc-caption-original')?.getAttribute('lang')).toBe('es');
    expect(els.mine.hidden).toBe(true);
  });

  it('stacks one block per speaker, in the order given, and keeps ids on the first only', () => {
    renderCaptions(els, { others: [sp('c', 'due'), sp('b', 'uno'), sp('d', 'tre')], mine: null, history: [] }, 'it', { showNames: true });
    expect(blocks().map((x) => x.dataset.speaker)).toEqual(['c', 'b', 'd']);
    expect(blocks().map((x) => x.querySelector('.lc-speaker-name')?.textContent)).toEqual(['C', 'B', 'D']);
    expect(els.stage.querySelectorAll('#lc-caption')).toHaveLength(1);
    expect(blocks()[0].querySelector('#lc-caption')?.textContent).toBe('due');
    expect(els.stage.querySelectorAll('#lc-caption-original')).toHaveLength(1);
  });

  it('reuses a speaker block when only their text changes, and reorders without rebuilding', () => {
    renderCaptions(els, { others: [sp('b', 'uno'), sp('c', 'due')], mine: null, history: [] }, 'it');
    const [first, second] = blocks();
    renderCaptions(els, { others: [sp('c', 'due bis'), sp('b', 'uno')], mine: null, history: [] }, 'it');
    expect(blocks()[0]).toBe(second);
    expect(blocks()[1]).toBe(first);
    expect(blocks()[0].querySelector('.lc-caption')?.textContent).toBe('due bis');
    expect(blocks()[1].querySelector('#lc-caption')).toBeNull();
  });

  it('removes the block of a speaker who is gone', () => {
    renderCaptions(els, { others: [sp('b', 'uno'), sp('c', 'due')], mine: null, history: [] }, 'it');
    renderCaptions(els, { others: [sp('c', 'due')], mine: null, history: [] }, 'it');
    expect(blocks().map((x) => x.dataset.speaker)).toEqual(['c']);
    renderCaptions(els, { others: [], mine: null, history: [] }, 'it');
    expect(blocks()).toHaveLength(0);
  });

  it('shows a speaker name only when asked to and when there is one', () => {
    renderCaptions(els, { others: [sp('b', 'x', { name: '' }), sp('c', 'y')], mine: null, history: [] }, 'it', { showNames: true });
    expect(blocks().map((x) => x.querySelector<HTMLElement>('.lc-speaker-name')?.hidden)).toEqual([true, false]);
    renderCaptions(els, { others: [sp('c', 'y')], mine: null, history: [] }, 'it');
    expect(blocks()[0].querySelector<HTMLElement>('.lc-speaker-name')?.hidden).toBe(true);
  });

  it('collapses the stage until somebody has said something', () => {
    expect(els.stage.dataset.empty).toBe('true');
    renderCaptions(els, { others: [sp('b', 'Ciao')], mine: null, history: [] });
    expect(els.stage.dataset.empty).toBe('false');
  });

  it('shows what I said small, under a "You said" label', () => {
    renderCaptions(els, { others: [], mine: { text: 'Buongiorno', original: '', lang: 'it', settled: true }, history: [] });
    expect(els.mine.hidden).toBe(false);
    expect(els.mineText.textContent).toBe('Buongiorno');
    expect(els.mine.textContent).toContain('You said');
  });

  it('marks RTL text with dir="rtl"', () => {
    renderCaptions(els, { others: [sp('b', 'مرحبا', { original: 'مرحبا', lang: 'ar' })], mine: null, history: [] });
    expect(blocks()[0].querySelector('.lc-caption-original')?.getAttribute('dir')).toBe('rtl');
  });

  it('sets speaker text via textContent only (a stranger\'s words are never HTML)', () => {
    renderCaptions(els, { others: [sp('b', '<img src=x onerror=alert(1)>', { name: '<b>n</b>' })], mine: null, history: [] }, 'it', { showNames: true });
    expect(els.stage.querySelector('img')).toBeNull();
    expect(els.stage.querySelector('b')).toBeNull();
  });

  it('renders the history newest first and never beyond the render limit', () => {
    const history = Array.from({ length: HISTORY_RENDER_LIMIT + 5 }, (_, i) => ex(i + 1, i % 2 ? 'me' : 'partner'));
    renderCaptions(els, { others: [], mine: null, history });
    expect(els.history.children).toHaveLength(HISTORY_RENDER_LIMIT);
    expect((els.history.firstElementChild as HTMLElement).dataset.exchange).toBe(String(HISTORY_RENDER_LIMIT + 5));
    expect(els.historyTitle.hidden).toBe(false);
    renderCaptions(els, { others: [], mine: null, history: [] });
    expect(els.historyTitle.hidden).toBe(true);
  });

  it('names the speaker on history cards in a room with several people', () => {
    const history = [{ ...ex(1, 'partner'), speakerId: 'b', speakerName: 'Bea' }, ex(2, 'me')];
    renderCaptions(els, { others: [], mine: null, history }, 'it', { showNames: true });
    const cards = [...els.history.querySelectorAll('.lc-card')];
    expect(cards.find((c) => (c as HTMLElement).dataset.who === 'partner')?.querySelector('.lc-card-label')?.textContent).toBe('Bea');
    renderCaptions(els, { others: [], mine: null, history }, 'it');
    expect(els.history.querySelector('[data-who="partner"] .lc-card-label')).toBeNull();
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

describe('Enhanced room pieces', () => {
  it('the speaking line and the host-voice line are hidden quiet statuses until asked for', () => {
    const els = buildRoom(document, { role: 'guest' });
    expect(els.speaking.hidden).toBe(true);
    expect(els.voiceNote.hidden).toBe(true);
    expect(els.speaking.getAttribute('aria-live')).toBe('polite');
    expect(els.voiceNote.getAttribute('role')).toBe('status');
    renderSpeaking(els, true);
    renderVoiceNote(els, true);
    expect(els.speaking.hidden).toBe(false);
    expect(els.voiceNote.hidden).toBe(false);
    renderSpeaking(els, false);
    renderVoiceNote(els, false);
    expect(els.speaking.hidden).toBe(true);
    expect(els.voiceNote.hidden).toBe(true);
  });

  it('names Enhanced in the tier badge', () => {
    const els = buildRoom(document, { role: 'host' });
    renderTier(els, 'cartesia');
    expect(els.tier.textContent).toBe('Quality: Enhanced');
  });
});
