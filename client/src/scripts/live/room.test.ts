// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';

const EN_TEXT = en as Record<string, string>;
const setBalance = vi.fn();
const cachedUser: { balance?: number } | null = { balance: undefined };
vi.mock('../auth', () => ({
  setBalance: (...a: unknown[]) => setBalance(...a),
  getUser: () => cachedUser,
}));
const toast = vi.fn();
vi.mock('../toast', () => ({ toast: (...a: unknown[]) => toast(...a) }));

import en from '../i18n/en.json';
import { mountRoom } from './room';
import { initialContext, type LiveContext } from './session-machine';
import type { LiveConversationOptions, Captions } from './conversation';
import type { StoredSession } from './session-store';

const SESSION: StoredSession = { sessionId: 's1', token: 'T', role: 'host', myLang: 'it', myPeerId: 'a', wsPath: '/ws/live', inviteUrl: 'https://x/lc/C' };

class FakeConvo {
  ctx: LiveContext = initialContext();
  muted = false;
  hp = false;
  started = 0;
  ended = 0;
  released = 0;
  canOutput = true;
  devices = [{ deviceId: 'd1', label: 'Speaker', kind: 'audiooutput' }] as MediaDeviceInfo[];
  outputResult = true;
  setOutput = vi.fn(async (_id: string) => this.outputResult);
  constructor(public opts: LiveConversationOptions) {
    made.push(this);
  }
  async start() {
    this.started += 1;
  }
  end() {
    this.ended += 1;
  }
  release() {
    this.released += 1;
  }
  setMuted(m: boolean) {
    this.muted = m;
    this.opts.onMuteChange?.(m);
  }
  isMuted() {
    return this.muted;
  }
  setHeadphones(on: boolean) {
    this.hp = on;
  }
  headphones() {
    return this.hp;
  }
  canSelectOutput() {
    return this.canOutput;
  }
  async listOutputDevices() {
    return this.devices;
  }
  setOutputDevice(id: string) {
    return this.setOutput(id);
  }
  direction() {
    return { myLang: this.opts.myLang, partnerLangs: this.ctx.partners.map((p) => p.lang) };
  }
  state() {
    return this.ctx;
  }
  emit(ctx: Partial<LiveContext>) {
    this.ctx = { ...this.ctx, ...ctx };
    this.opts.onState(this.ctx);
  }
}
let made: FakeConvo[] = [];
let host: HTMLElement;
const flush = () => new Promise((r) => setTimeout(r, 0));

function mount(over: Partial<Parameters<typeof mountRoom>[0]> = {}) {
  const states: LiveContext[] = [];
  const handle = mountRoom({
    container: host,
    role: 'host',
    session: SESSION,
    onState: (c) => states.push(c),
    createConversation: (o) => new FakeConvo(o) as never,
    ...over,
  });
  return { handle, states, convo: () => made[made.length - 1] };
}

beforeEach(() => {
  document.body.innerHTML = '<div id="host"></div>';
  host = document.getElementById('host') as HTMLElement;
  made = [];
  setBalance.mockReset();
  toast.mockReset();
});

describe('mountRoom', () => {
  it('builds the room, hands the stored credentials to the engine and starts it', async () => {
    mount();
    expect(host.querySelector('#lc-room')).not.toBeNull();
    const c = made[0];
    expect(c.opts).toMatchObject({ sessionId: 's1', token: 'T', role: 'host', myLang: 'it', myPeerId: 'a', wsPath: '/ws/live', inviteUrl: 'https://x/lc/C' });
    expect(c.started).toBe(1);
  });

  it('paints the phase, the partner and both directions from engine state', () => {
    mount();
    const c = made[0];
    c.emit({ phase: 'live', partners: [{ peerId: 'b', name: 'Bea', lang: 'es', muted: false }] });
    expect(host.querySelector('#lc-live')?.hasAttribute('hidden')).toBe(false);
    expect(host.querySelector('.lc-partner-name')?.textContent).toBe('Bea');
    expect(host.querySelector('#lc-direction')?.textContent).toContain('You speak Italiano, they hear Español');
  });

  it('uses the caller\'s wording for the waiting chip (a desk customer waits for the staff)', () => {
    mount({ role: 'guest', waitingKey: 'liveDeskWaiting' });
    made[0].emit({ phase: 'waiting_peer' });
    expect(host.querySelector('#lc-status-text')?.textContent).toBe('Waiting for the staff…');
  });

  it('mounts a staff seat with no credit line, passing the staff role to the engine', () => {
    mount({ role: 'staff', session: { ...SESSION, role: 'staff' } });
    expect(made[0].opts.role).toBe('staff');
    expect(host.querySelector('#lc-credits')).toBeNull();
    made[0].opts.onBalance?.(3);
    expect(setBalance).not.toHaveBeenCalled();
  });

  it('forwards state changes to the page (it owns the invite panel)', () => {
    const { states } = mount();
    made[0].emit({ phase: 'waiting_peer' });
    expect(states.map((s) => s.phase)).toEqual(['waiting_peer']);
  });

  it('paints captions in the big and small slots', () => {
    mount();
    const captions: Captions = { others: [{ speakerId: 'b', name: 'Bea', text: 'Ciao', original: 'Hola', lang: 'es', settled: false }], mine: null, history: [] };
    made[0].opts.onCaptions(captions);
    expect(host.querySelector('#lc-caption')?.textContent).toBe('Ciao');
    expect(host.querySelector('#lc-caption')?.getAttribute('lang')).toBe('it');
  });

  it('mute toggles the engine, the pressed state, the label and the icon', () => {
    mount();
    const btn = host.querySelector('#lc-mute') as HTMLButtonElement;
    btn.click();
    expect(made[0].muted).toBe(true);
    expect(btn.getAttribute('aria-pressed')).toBe('true');
    expect(btn.textContent).toContain('Unmute');
    btn.click();
    expect(btn.getAttribute('aria-pressed')).toBe('false');
    expect(btn.textContent).toContain('Mute');
    expect(btn.textContent).not.toContain('Unmute');
  });

  it('End asks for confirmation first, and only the confirmation ends it', () => {
    mount();
    (host.querySelector('#lc-end') as HTMLButtonElement).click();
    expect((host.querySelector('#lc-confirm') as HTMLElement).hidden).toBe(false);
    expect(made[0].ended).toBe(0);
    (host.querySelector('#lc-end-no') as HTMLButtonElement).click();
    expect((host.querySelector('#lc-confirm') as HTMLElement).hidden).toBe(true);
    expect(made[0].ended).toBe(0);
    (host.querySelector('#lc-end') as HTMLButtonElement).click();
    (host.querySelector('#lc-end-yes') as HTMLButtonElement).click();
    expect(made[0].ended).toBe(1);
  });

  describe('the end confirmation is a modal over the page', () => {
    const confirm = () => host.querySelector('#lc-confirm') as HTMLElement;
    const open = () => (host.querySelector('#lc-end') as HTMLButtonElement).click();

    it('moves focus to the confirming button when it opens', () => {
      mount();
      open();
      expect(document.activeElement).toBe(host.querySelector('#lc-end-yes'));
    });

    it('Escape cancels it, does not end the conversation, and gives focus back to End', () => {
      mount();
      open();
      document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
      expect(confirm().hidden).toBe(true);
      expect(made[0].ended).toBe(0);
      expect(document.activeElement).toBe(host.querySelector('#lc-end'));
    });

    it('Escape does nothing while the confirmation is closed', () => {
      mount();
      document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
      expect(confirm().hidden).toBe(true);
      expect(document.activeElement).not.toBe(host.querySelector('#lc-end'));
    });

    it('a tap on the dark backdrop cancels, a tap inside the dialog does not', () => {
      mount();
      open();
      (host.querySelector('#lc-confirm-text') as HTMLElement).click();
      expect(confirm().hidden).toBe(false);
      confirm().click();
      expect(confirm().hidden).toBe(true);
      expect(made[0].ended).toBe(0);
    });

    it('keeps Tab inside the two buttons while it is open', () => {
      mount();
      open();
      const yes = host.querySelector('#lc-end-yes') as HTMLButtonElement;
      const no = host.querySelector('#lc-end-no') as HTMLButtonElement;
      yes.focus();
      const tab = (shiftKey: boolean) => {
        const ev = new KeyboardEvent('keydown', { key: 'Tab', shiftKey, bubbles: true, cancelable: true });
        document.dispatchEvent(ev);
        return ev.defaultPrevented;
      };
      // "No" comes first, "Yes" last: Tab on the last wraps to the first, Shift+Tab the other way.
      expect(tab(false)).toBe(true);
      expect(document.activeElement).toBe(no);
      expect(tab(true)).toBe(true);
      expect(document.activeElement).toBe(yes);
    });
  });

  it('the headphones toggle reaches the engine and starts from its saved value', () => {
    mount();
    const box = host.querySelector('#lc-headphones') as HTMLInputElement;
    expect(box.checked).toBe(false);
    box.checked = true;
    box.dispatchEvent(new Event('change'));
    expect(made[0].hp).toBe(true);
  });

  it('shows the output selector only when the browser supports it, and applies a choice', async () => {
    mount();
    const c = made[0];
    c.emit({ phase: 'waiting_peer' });
    await flush();
    expect((host.querySelector('#lc-output-field') as HTMLElement).hidden).toBe(false);
    const sel = host.querySelector('#lc-output') as HTMLSelectElement;
    sel.value = 'd1';
    sel.dispatchEvent(new Event('change'));
    await flush();
    expect(c.setOutput).toHaveBeenCalledWith('d1');
    expect(toast).not.toHaveBeenCalled();
    c.outputResult = false;
    sel.dispatchEvent(new Event('change'));
    await flush();
    expect(toast).toHaveBeenCalledWith('Couldn\'t switch the audio output.', 'err');
  });

  it('hides the output selector on browsers without support (iOS Safari)', async () => {
    mount({ createConversation: (o) => Object.assign(new FakeConvo(o), { canOutput: false }) as never });
    made[0].emit({ phase: 'waiting_peer' });
    await flush();
    expect((host.querySelector('#lc-output-field') as HTMLElement).hidden).toBe(true);
  });

  it('shows the host credit line, and a warning when low', () => {
    mount();
    made[0].opts.onBalance?.(4.2);
    const credits = host.querySelector('#lc-credits') as HTMLElement;
    expect(credits.hidden).toBe(false);
    expect(credits.textContent).toBe('Credits left: $4.2000 · −$0.0000');
    expect(credits.dataset.low).toBe('false');
    expect(setBalance).toHaveBeenCalledWith(4.2);
    made[0].opts.onLowBalance?.(0.4);
    expect(credits.textContent).toBe('Credits are running low: $0.4000 left. · −$0.0000');
    expect(credits.dataset.low).toBe('true');
  });

  it('shows what the conversation has cost so far, growing with each update, to four decimals', () => {
    mount();
    made[0].opts.onBalance?.(4.2);
    made[0].opts.onSpent?.(0.0123);
    const credits = host.querySelector('#lc-credits') as HTMLElement;
    expect(credits.textContent).toBe('Credits left: $4.2000 · −$0.0123');
    made[0].opts.onBalance?.(4.1877);
    made[0].opts.onSpent?.(0.0246);
    expect(credits.textContent).toBe('Credits left: $4.1877 · −$0.0246');
    made[0].opts.onLowBalance?.(0.4);
    expect(credits.textContent).toBe('Credits are running low: $0.4000 left. · −$0.0246');
  });

  it('paints the cached balance as soon as the host starts, before the first charge', () => {
    cachedUser.balance = 3.5;
    mount();
    const credits = host.querySelector('#lc-credits') as HTMLElement;
    expect(credits.hidden).toBe(false);
    expect(credits.textContent).toBe('Credits left: $3.5000 · −$0.0000');
    cachedUser.balance = undefined;
  });

  it('never shows a credit line to a guest (and never touches the account balance)', () => {
    mount({ role: 'guest', session: { ...SESSION, role: 'guest' } });
    made[0].opts.onBalance?.(4.2);
    made[0].opts.onSpent?.(0.5);
    expect(host.querySelector('#lc-credits')).toBeNull();
    expect(setBalance).not.toHaveBeenCalled();
  });

  it('shows the same-language warning once and clears it when the partner leaves', () => {
    mount();
    const c = made[0];
    const bea = { peerId: 'b', name: 'B', lang: 'it', muted: false };
    c.emit({ phase: 'live', partners: [bea] });
    c.opts.onSameLanguage?.('it', 'it', bea);
    const banner = host.querySelector('#lc-same-lang') as HTMLElement;
    expect(banner.hidden).toBe(false);
    expect(banner.textContent).toBe('You and B chose the same language, so nothing is translated between you.');
    c.emit({ phase: 'waiting_peer', partners: [] });
    expect(banner.hidden).toBe(true);
  });

  it('names every partner who shares my language, and drops each one when they leave', () => {
    mount();
    const c = made[0];
    const bea = { peerId: 'b', name: 'Bea', lang: 'it', muted: false };
    const carlo = { peerId: 'c', name: 'Carlo', lang: 'it', muted: false };
    const dina = { peerId: 'd', name: 'Dina', lang: 'es', muted: false };
    c.emit({ phase: 'live', partners: [bea, dina] });
    c.opts.onSameLanguage?.('it', 'it', bea);
    c.emit({ phase: 'live', partners: [bea, dina, carlo] });
    c.opts.onSameLanguage?.('it', 'it', carlo);
    const banner = host.querySelector('#lc-same-lang') as HTMLElement;
    expect(banner.textContent).toBe('You and Bea, Carlo chose the same language, so nothing is translated between you.');
    c.emit({ phase: 'live', partners: [dina, carlo] });
    expect(banner.textContent).toBe('You and Carlo chose the same language, so nothing is translated between you.');
    c.emit({ phase: 'live', partners: [dina] });
    expect(banner.hidden).toBe(true);
  });

  it('falls back to the generic same-language line when the partner has no name', () => {
    mount();
    const c = made[0];
    const anon = { peerId: 'b', name: '', lang: 'it', muted: false };
    c.emit({ phase: 'live', partners: [anon] });
    c.opts.onSameLanguage?.('it', 'it', anon);
    expect((host.querySelector('#lc-same-lang') as HTMLElement).textContent).toBe('You both chose Italiano, so nothing will be translated.');
  });

  it('lists every partner and every distinct language in the direction line', () => {
    mount();
    made[0].emit({
      phase: 'live',
      partners: [
        { peerId: 'b', name: 'Bea', lang: 'es', muted: false },
        { peerId: 'c', name: 'Carlo', lang: 'en', muted: true },
        { peerId: 'd', name: 'Dina', lang: 'es', muted: false },
      ],
    });
    expect([...host.querySelectorAll('.lc-partner-name')].map((n) => n.textContent)).toEqual(['Bea', 'Carlo', 'Dina']);
    expect(host.querySelector('#lc-direction')?.textContent).toContain('You speak Italiano, they hear Español, English');
  });

  it('announces joins and leaves politely, falling back to a generic name', () => {
    mount();
    made[0].opts.onPeerChange?.('joined', { peerId: 'b', name: 'Bea', lang: 'es', muted: false });
    const live = host.querySelector('#lc-announce') as HTMLElement;
    expect(live.getAttribute('aria-live')).toBe('polite');
    expect(live.textContent).toBe('Bea joined the conversation.');
    made[0].opts.onPeerChange?.('left', { peerId: 'b', name: '', lang: 'es', muted: false });
    expect(live.textContent).toBe('Guest left the conversation.');
  });

  it('labels each speaker once the room holds several guests', () => {
    mount({ session: { ...SESSION, maxGuests: 3 } });
    made[0].opts.onCaptions({
      others: [
        { speakerId: 'c', name: 'Carlo', text: 'Ciao', original: 'Hi', lang: 'en', settled: false },
        { speakerId: 'b', name: 'Bea', text: 'Salve', original: 'Hola', lang: 'es', settled: true },
      ],
      mine: null,
      history: [],
    });
    expect([...host.querySelectorAll('.lc-speaker')].map((b) => b.querySelector('.lc-speaker-name')?.textContent)).toEqual(['Carlo', 'Bea']);
  });

  it('a pair shows no name labels, as before', () => {
    mount();
    made[0].opts.onCaptions({ others: [{ speakerId: 'b', name: 'Bea', text: 'x', original: '', lang: 'es', settled: false }], mine: null, history: [] });
    expect((host.querySelector('.lc-speaker-name') as HTMLElement).hidden).toBe(true);
  });

  it('starts labelling when a second partner shows up, even if the stored session predates maxGuests', () => {
    mount();
    made[0].emit({ phase: 'live', partners: [{ peerId: 'b', name: 'Bea', lang: 'es', muted: false }, { peerId: 'c', name: 'Carlo', lang: 'en', muted: false }] });
    made[0].opts.onCaptions({ others: [{ speakerId: 'c', name: 'Carlo', text: 'x', original: '', lang: 'en', settled: false }], mine: null, history: [] });
    expect((host.querySelector('.lc-speaker-name') as HTMLElement).hidden).toBe(false);
  });

  it('a guest of a multi-guest conversation sees "Leave", with the others-carry-on wording', () => {
    mount({ role: 'guest', session: { ...SESSION, role: 'guest', maxGuests: 3 } });
    expect(host.querySelector('#lc-end')?.textContent).toBe('Leave');
    expect(host.querySelector('#lc-confirm-text')?.textContent).toBe('Leave the conversation? The others will carry on without you.');
    expect(host.querySelector('#lc-end-yes')?.textContent).toBe('Leave');
    expect(made[0].opts.maxGuests).toBe(3);
    (host.querySelector('#lc-end') as HTMLButtonElement).click();
    (host.querySelector('#lc-end-yes') as HTMLButtonElement).click();
    expect(made[0].ended).toBe(1);
  });

  it('the host of a multi-guest conversation is warned the end is for everyone', () => {
    mount({ session: { ...SESSION, maxGuests: 2 } });
    expect(host.querySelector('#lc-end')?.textContent).toBe('End');
    expect(host.querySelector('#lc-confirm-text')?.textContent).toBe('End the conversation for everyone?');
  });

  it('shows the "you left" screen for a guest who left, not the ended-by-me one', () => {
    mount({ role: 'guest', session: { ...SESSION, role: 'guest', maxGuests: 3 } });
    made[0].emit({ phase: 'ended', endReason: 'left_by_me' });
    expect((host.querySelector('#lc-ended') as HTMLElement).hidden).toBe(false);
    expect(host.querySelector('#lc-ended .lc-end-copy')?.textContent).toBe('You left the conversation. The others carry on.');
  });

  it('shows a recoverable notice and clears it when a caption arrives', () => {
    mount();
    made[0].opts.onNotice?.('audio_session_closed');
    const notice = host.querySelector('#lc-notice') as HTMLElement;
    expect(notice.hidden).toBe(false);
    expect(notice.textContent).toContain('Translation isn\'t available');
    made[0].opts.onCaptions({ others: [{ speakerId: 'b', name: 'B', text: 'x', original: '', lang: 'es', settled: false }], mine: null, history: [] });
    expect(notice.hidden).toBe(true);
  });

  it('retry builds a NEW engine from the same credentials and starts it', () => {
    mount();
    made[0].emit({ phase: 'failed', failure: 'network' });
    (host.querySelector('#lc-retry') as HTMLButtonElement).click();
    expect(made).toHaveLength(2);
    expect(made[1].opts.token).toBe('T');
    expect(made[1].started).toBe(1);
  });

  it('releases the microphone (but keeps the session) when the page is hidden', () => {
    mount();
    window.dispatchEvent(new Event('pagehide'));
    expect(made[0].released).toBe(1);
    expect(made[0].ended).toBe(0);
  });

  it('destroy releases the engine and stops listening to the page', () => {
    const { handle } = mount();
    handle.destroy();
    expect(made[0].released).toBe(1);
    window.dispatchEvent(new Event('pagehide'));
    expect(made[0].released).toBe(1);
    expect(host.querySelector('#lc-room')).toBeNull();
  });
});

describe('translation tier in the room', () => {
  const PREMIUM: StoredSession = { ...SESSION, engine: 'gemini_live_translate' };
  const badge = () => host.querySelector('#lc-tier') as HTMLElement;
  const notice = () => host.querySelector('#lc-tier-notice') as HTMLElement;
  const downgrade = () => made[0].opts.onEngineDowngraded?.({ from: 'gemini_live_translate', to: 'standard', reason: 'premium_at_capacity' });

  it('shows the tier next to the connection state, for host and guest, and hands it to the engine', () => {
    for (const role of ['host', 'guest'] as const) {
      host.innerHTML = '';
      made = [];
      mount({ role, session: PREMIUM });
      expect(badge().hidden).toBe(false);
      expect(badge().textContent).toBe('Quality: Premium');
      expect(badge().parentElement?.id).toBe('lc-status');
      expect(made[0].opts.engine).toBe('gemini_live_translate');
    }
  });

  it('shows Standard for a Standard conversation and nothing when the tier is unknown (a desk)', () => {
    mount({ session: { ...SESSION, engine: 'standard' } });
    expect(badge().textContent).toBe('Quality: Standard');
    host.innerHTML = '';
    mount();
    expect(badge().hidden).toBe(true);
  });

  it('on a downgrade: polite notice that can be dismissed, badge follows, conversation untouched', () => {
    mount({ session: PREMIUM });
    made[0].emit({ phase: 'live', partners: [{ peerId: 'b', name: 'Bea', lang: 'es', muted: false }] });
    expect(notice().hidden).toBe(true);
    downgrade();
    expect(notice().hidden).toBe(false);
    expect(notice().getAttribute('aria-live')).toBe('polite');
    expect(notice().getAttribute('role')).toBe('status');
    expect(notice().textContent).toContain('Premium is busy. Continuing on Standard, billed at the Standard rate.');
    expect(badge().textContent).toBe('Quality: Standard');
    expect(made[0].ended).toBe(0);
    expect(made[0].released).toBe(0);
    expect(host.querySelector('#lc-live')?.hasAttribute('hidden')).toBe(false);
    // A caption does not wipe it (it is not a "translation is down" notice) ...
    made[0].opts.onCaptions({ others: [{ speakerId: 'b', name: 'Bea', lang: 'es', translated: 'hola', interim: false, seq: 1 }], mine: null, history: [] } as never);
    expect(notice().hidden).toBe(false);
    // ... the person does.
    (notice().querySelector('button') as HTMLButtonElement).click();
    expect(notice().hidden).toBe(true);
    expect(badge().textContent).toBe('Quality: Standard');
  });

  it('a guest is told about the tier but never about money', () => {
    mount({ role: 'guest', session: PREMIUM });
    downgrade();
    expect(notice().textContent).toContain('Premium is busy, so this conversation continues on Standard.');
    expect(notice().textContent).not.toMatch(/billed|rate|\$/i);
  });
});

describe('Enhanced in the room', () => {
  const ENHANCED: StoredSession = { ...SESSION, engine: 'cartesia' };
  const badge = () => host.querySelector('#lc-tier') as HTMLElement;
  const notice = () => host.querySelector('#lc-tier-notice') as HTMLElement;
  const speaking = () => host.querySelector('#lc-speaking') as HTMLElement;
  const voiceNote = () => host.querySelector('#lc-voice-note') as HTMLElement;
  const downgrade = (reason: string) => made[0].opts.onEngineDowngraded?.({ from: 'cartesia', to: 'standard', reason });

  it('shows Enhanced in the badge, for host and guest', () => {
    for (const role of ['host', 'guest'] as const) {
      host.innerHTML = '';
      made = [];
      mount({ role, session: ENHANCED });
      expect(badge().textContent).toBe('Quality: Enhanced');
    }
  });

  it('the speaking line is a quiet polite status, hidden until a translation is being spoken', () => {
    mount({ session: ENHANCED });
    expect(speaking().hidden).toBe(true);
    expect(speaking().getAttribute('aria-live')).toBe('polite');
    expect(speaking().textContent).toBe(EN_TEXT.talkStatusSpeaking);
    made[0].opts.onSpeaking?.(true);
    expect(speaking().hidden).toBe(false);
    made[0].opts.onSpeaking?.(false);
    expect(speaking().hidden).toBe(true);
  });

  it('a fallback to Standard gets Enhanced-specific words (not "busy"), polite, and no money talk for a guest', () => {
    mount({ session: ENHANCED });
    downgrade('enhanced_unavailable');
    expect(notice().hidden).toBe(false);
    expect(notice().getAttribute('aria-live')).toBe('polite');
    expect(notice().textContent).toContain('Enhanced isn\'t available right now. Continuing on Standard, billed at the Standard rate.');
    expect(badge().textContent).toBe('Quality: Standard');
    host.innerHTML = '';
    made = [];
    mount({ role: 'guest', session: ENHANCED });
    downgrade('enhanced_unavailable');
    expect(notice().textContent).toContain('Enhanced isn\'t available right now, so this conversation continues on Standard.');
    expect(notice().textContent).not.toMatch(/billed|rate|\$/i);
  });

  it('not_enhanced (a downgrade this phone missed) is told the same way', () => {
    mount({ session: ENHANCED });
    downgrade('not_enhanced');
    expect(notice().textContent).toContain('Enhanced isn\'t available');
  });

  it('a guest is told the host\'s voice is imitated, only while Enhanced with the host\'s clone is what they hear', () => {
    mount({ role: 'guest', session: { ...ENHANCED, voiceClone: true } });
    expect(voiceNote().hidden).toBe(false);
    expect(voiceNote().textContent).toBe(EN_TEXT.liveJoinNoticeVoiceClone);
    downgrade('enhanced_unavailable');
    expect(voiceNote().hidden).toBe(true);
  });

  it('nobody else sees that line: not the host, not a guest without the clone, not a Standard conversation', () => {
    mount({ role: 'host', session: { ...ENHANCED, voiceClone: true } });
    expect(voiceNote().hidden).toBe(true);
    host.innerHTML = '';
    mount({ role: 'guest', session: ENHANCED });
    expect(voiceNote().hidden).toBe(true);
    host.innerHTML = '';
    mount({ role: 'guest', session: { ...SESSION, engine: 'standard', voiceClone: true } });
    expect(voiceNote().hidden).toBe(true);
  });

  it('hands the voice flag to the engine so it is kept in the stored session', () => {
    mount({ role: 'guest', session: { ...ENHANCED, voiceClone: true } });
    expect(made[0].opts.voiceClone).toBe(true);
  });
});
