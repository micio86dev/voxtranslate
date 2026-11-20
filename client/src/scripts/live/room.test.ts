// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';

const setBalance = vi.fn();
const cachedUser: { balance?: number } | null = { balance: undefined };
vi.mock('../auth', () => ({
  setBalance: (...a: unknown[]) => setBalance(...a),
  getUser: () => cachedUser,
}));
const toast = vi.fn();
vi.mock('../toast', () => ({ toast: (...a: unknown[]) => toast(...a) }));

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
    return { myLang: this.opts.myLang, partnerLang: this.ctx.partner?.lang ?? null };
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
    c.emit({ phase: 'live', partner: { peerId: 'b', name: 'Bea', lang: 'es', muted: false } });
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
    const captions: Captions = { partner: { text: 'Ciao', original: 'Hola', lang: 'es', settled: false }, mine: null, history: [] };
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
    c.emit({ phase: 'live', partner: { peerId: 'b', name: 'B', lang: 'it', muted: false } });
    c.opts.onSameLanguage?.('it', 'it');
    const banner = host.querySelector('#lc-same-lang') as HTMLElement;
    expect(banner.hidden).toBe(false);
    expect(banner.textContent).toBe('You both chose Italiano, so nothing will be translated.');
    c.emit({ phase: 'waiting_peer', partner: null });
    expect(banner.hidden).toBe(true);
  });

  it('shows a recoverable notice and clears it when a caption arrives', () => {
    mount();
    made[0].opts.onNotice?.('audio_session_closed');
    const notice = host.querySelector('#lc-notice') as HTMLElement;
    expect(notice.hidden).toBe(false);
    expect(notice.textContent).toContain('Translation isn\'t available');
    made[0].opts.onCaptions({ partner: { text: 'x', original: '', lang: 'es', settled: false }, mine: null, history: [] });
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
