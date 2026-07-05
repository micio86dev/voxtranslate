// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';

import { ChatManager, formatSize, type ChatAttachment, type ChatPayload } from './chat';

function makeWs(readyState: number = WebSocket.OPEN): {
  ws: WebSocket;
  send: ReturnType<typeof vi.fn>;
} {
  const send = vi.fn();
  return { ws: { readyState, send } as unknown as WebSocket, send };
}

function makeManager(
  myLang = 'en',
  myId = 'me',
): { mgr: ChatManager; container: HTMLElement; send: ReturnType<typeof vi.fn> } {
  const container = document.createElement('div');
  document.body.appendChild(container);
  const { ws, send } = makeWs();
  const mgr = new ChatManager({ myLang, myId, container, ws });
  return { mgr, container, send };
}

function payload(over: Partial<ChatPayload> = {}): ChatPayload {
  return {
    sender_id: 'peer-2',
    sender_name: 'Bruna',
    sender_lang: 'pt',
    original: 'Olá',
    translations: { en: 'Hello', it: 'Ciao' },
    timestamp: 1,
    ...over,
  };
}

function att(over: Partial<ChatAttachment> = {}): ChatAttachment {
  return {
    url: 'https://files.example.com/voice.webm',
    name: 'voice.webm',
    content_type: 'audio/webm',
    size: 2048,
    ...over,
  };
}

afterEach(() => {
  document.body.innerHTML = '';
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe('formatSize', () => {
  it('formats bytes / KB / MB with sensible precision', () => {
    expect(formatSize(0)).toBe('0 B');
    expect(formatSize(512)).toBe('512 B');
    expect(formatSize(1023)).toBe('1023 B');
    expect(formatSize(1024)).toBe('1 KB');
    expect(formatSize(2048)).toBe('2 KB');
    expect(formatSize(1024 * 1024)).toBe('1.0 MB');
    expect(formatSize(3_565_158)).toBe('3.4 MB');
  });
});

describe('addMessage', () => {
  it('renders a peer message in MY language with the original below', () => {
    const { mgr, container } = makeManager('en');
    mgr.addMessage(payload());
    const msg = container.querySelector('.chat-msg')!;
    expect(msg.classList.contains('chat-msg-other')).toBe(true);
    expect(msg.querySelector('.chat-sender')?.textContent).toBe('Bruna');
    expect(msg.querySelector('.chat-text')?.textContent).toBe('Hello');
    expect(msg.querySelector('.chat-original')?.textContent).toBe('Olá');
  });

  it('falls back to the original when no translation exists for my language — and hides the duplicate', () => {
    const { mgr, container } = makeManager('fr');
    mgr.addMessage(payload());
    const msg = container.querySelector('.chat-msg')!;
    expect(msg.querySelector('.chat-text')?.textContent).toBe('Olá');
    expect(msg.querySelector('.chat-original')).toBeNull();
  });

  it('renders my own message without sender/avatar and never counts it unread', () => {
    const { mgr, container } = makeManager('en', 'me');
    const onUnread = vi.fn();
    mgr.onUnread = onUnread;
    mgr.addMessage(payload({ sender_id: 'me' }));
    const msg = container.querySelector('.chat-msg')!;
    expect(msg.classList.contains('chat-msg-mine')).toBe(true);
    expect(msg.querySelector('.chat-sender')).toBeNull();
    expect(msg.querySelector('img')).toBeNull();
    expect(onUnread).not.toHaveBeenCalled();
  });

  it('shows a peer avatar (Google-resized) and drops it on load error', () => {
    const { mgr, container } = makeManager();
    mgr.addMessage(
      payload({ sender_avatar: 'https://lh3.googleusercontent.com/a/photo=s96-c' }),
    );
    const img = container.querySelector<HTMLImageElement>('img.chat-avatar')!;
    expect(img.getAttribute('src')).toBe('https://lh3.googleusercontent.com/a/photo=s36');
    expect(img.referrerPolicy).toBe('no-referrer');
    img.dispatchEvent(new Event('error'));
    expect(container.querySelector('img.chat-avatar')).toBeNull();
  });

  it('omits the avatar element when the sender has none', () => {
    const { mgr, container } = makeManager();
    mgr.addMessage(payload({ sender_avatar: null }));
    expect(container.querySelector('img.chat-avatar')).toBeNull();
  });

  it('tracks unread while closed and resets on open', () => {
    const { mgr } = makeManager();
    const onUnread = vi.fn();
    mgr.onUnread = onUnread;

    mgr.addMessage(payload());
    mgr.addMessage(payload());
    expect(onUnread).toHaveBeenNthCalledWith(1, 1);
    expect(onUnread).toHaveBeenNthCalledWith(2, 2);

    mgr.setOpen(true);
    expect(onUnread).toHaveBeenLastCalledWith(0);

    mgr.addMessage(payload()); // open → no unread
    expect(onUnread).toHaveBeenCalledTimes(3);

    mgr.setOpen(false); // closing alone does not re-count
    mgr.addMessage(payload());
    expect(onUnread).toHaveBeenLastCalledWith(1);
  });

  it('setMyLang switches the render language for later messages', () => {
    const { mgr, container } = makeManager('en');
    mgr.setMyLang('it');
    mgr.addMessage(payload());
    expect(container.querySelector('.chat-text')?.textContent).toBe('Ciao');
  });

  it('renders an attachment-only message without empty text spans', () => {
    const { mgr, container } = makeManager('en');
    mgr.addMessage(
      payload({ original: '', translations: {}, attachment: att({ content_type: 'application/pdf' }) }),
    );
    const msg = container.querySelector('.chat-msg')!;
    expect(msg.querySelector('.chat-text')).toBeNull();
    expect(msg.querySelector('.chat-original')).toBeNull();
    expect(msg.querySelector('.chat-attachment')).not.toBeNull();
  });
});

describe('document attachments', () => {
  it('renders a download chip with name, human size and a safe new-tab link', () => {
    const { mgr, container } = makeManager();
    mgr.addMessage(
      payload({
        attachment: att({
          url: 'https://files.example.com/report.pdf',
          name: 'report.pdf',
          content_type: 'application/pdf',
          size: 3_565_158,
        }),
      }),
    );
    const link = container.querySelector<HTMLAnchorElement>('a.chat-file-chip')!;
    expect(link.getAttribute('href')).toBe('https://files.example.com/report.pdf');
    expect(link.target).toBe('_blank');
    expect(link.rel).toBe('noopener noreferrer');
    expect(link.querySelector('svg')).not.toBeNull(); // file glyph
    expect(link.querySelector('.chat-file-name')?.textContent).toBe('report.pdf');
    expect(link.querySelector('.chat-file-size')?.textContent).toBe('3.4 MB');
  });

  it('resolves a relative URL against the page origin', () => {
    const { mgr, container } = makeManager();
    mgr.addMessage(
      payload({ attachment: att({ url: '/files/report.pdf', content_type: 'application/pdf' }) }),
    );
    const link = container.querySelector<HTMLAnchorElement>('a.chat-file-chip')!;
    expect(link.getAttribute('href')).toBe(`${window.location.origin}/files/report.pdf`);
  });

  it('neutralises a javascript: URL to "#" (defense-in-depth)', () => {
    const { mgr, container } = makeManager();
    mgr.addMessage(
      payload({
        // eslint-disable-next-line no-script-url
        attachment: att({ url: 'javascript:alert(1)', content_type: 'application/pdf' }),
      }),
    );
    expect(container.querySelector('a.chat-file-chip')?.getAttribute('href')).toBe('#');
  });

  it('neutralises an unparsable URL to "#"', () => {
    const { mgr, container } = makeManager();
    mgr.addMessage(
      payload({ attachment: att({ url: 'http://', content_type: 'application/pdf' }) }),
    );
    expect(container.querySelector('a.chat-file-chip')?.getAttribute('href')).toBe('#');
  });
});

describe('voice-message attachments', () => {
  it('renders an inline player (no download chip) with a lazy audio element', () => {
    const { mgr, container } = makeManager();
    mgr.addMessage(payload({ attachment: att() }));
    const audio = container.querySelector<HTMLAudioElement>('audio.chat-audio')!;
    expect(audio.controls).toBe(true);
    expect(audio.preload).toBe('none');
    expect(audio.getAttribute('src')).toBe('https://files.example.com/voice.webm');
    expect(container.querySelector('a.chat-file-chip')).toBeNull();
  });

  it('cycles the speed pill 1× → 1.5× → 2× → 1× and applies the rate', () => {
    const { mgr, container } = makeManager();
    mgr.addMessage(payload({ attachment: att() }));
    const audio = container.querySelector<HTMLAudioElement>('audio.chat-audio')!;
    const speed = container.querySelector<HTMLButtonElement>('.chat-audio-speed')!;
    expect(speed.textContent).toBe('1×');
    speed.click();
    expect(speed.textContent).toBe('1.5×');
    expect(audio.playbackRate).toBe(1.5);
    speed.click();
    expect(speed.textContent).toBe('2×');
    expect(audio.playbackRate).toBe(2);
    speed.click();
    expect(speed.textContent).toBe('1×');
    expect(audio.playbackRate).toBe(1);
  });

  it('reapplies the chosen rate on play (browsers reset it per source)', () => {
    const { mgr, container } = makeManager();
    mgr.addMessage(payload({ attachment: att() }));
    const audio = container.querySelector<HTMLAudioElement>('audio.chat-audio')!;
    container.querySelector<HTMLButtonElement>('.chat-audio-speed')!.click(); // 1.5×
    audio.playbackRate = 1; // simulated browser reset
    audio.dispatchEvent(new Event('play'));
    expect(audio.playbackRate).toBe(1.5);
  });

  it.each([
    ['audio/mpeg', 'voice-message.mp3'],
    ['audio/mp4;codecs=mp4a.40.2', 'voice-message.m4a'],
    ['audio/flac', 'voice-message.webm'], // unknown type → safe default
  ])('downloads the note via an object URL (%s → %s)', async (contentType, filename) => {
    const { mgr, container } = makeManager();
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        blob: async () => new Blob(['audio-bytes']),
      } as unknown as Response),
    );
    URL.createObjectURL = vi.fn(() => 'blob:vox-1');
    URL.revokeObjectURL = vi.fn();
    const clickSpy = vi
      .spyOn(HTMLAnchorElement.prototype, 'click')
      .mockImplementation(() => {});

    mgr.addMessage(payload({ attachment: att({ content_type: contentType }) }));
    const dl = container.querySelector<HTMLButtonElement>('.chat-audio-dl')!;
    expect(dl.title).toBe('Download voice message');
    dl.click();
    expect(dl.disabled).toBe(true); // guarded while in flight
    await vi.waitFor(() => expect(dl.disabled).toBe(false));

    expect(fetch).toHaveBeenCalledWith('https://files.example.com/voice.webm');
    const anchor = clickSpy.mock.contexts[0] as HTMLAnchorElement;
    expect(anchor.getAttribute('href')).toBe('blob:vox-1');
    expect(anchor.download).toBe(filename);
    expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:vox-1');
  });

  it('falls back to opening the file when the download fetch fails', async () => {
    const { mgr, container } = makeManager();
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue({ ok: false, status: 403 } as unknown as Response),
    );
    const open = vi.spyOn(window, 'open').mockReturnValue(null);

    mgr.addMessage(payload({ attachment: att() }));
    const dl = container.querySelector<HTMLButtonElement>('.chat-audio-dl')!;
    dl.click();
    await vi.waitFor(() => expect(dl.disabled).toBe(false));

    expect(open).toHaveBeenCalledWith(
      'https://files.example.com/voice.webm',
      '_blank',
      'noopener',
    );
  });

  it('also falls back when fetch itself rejects (offline)', async () => {
    const { mgr, container } = makeManager();
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new Error('net down')));
    const open = vi.spyOn(window, 'open').mockReturnValue(null);

    mgr.addMessage(payload({ attachment: att() }));
    const dl = container.querySelector<HTMLButtonElement>('.chat-audio-dl')!;
    dl.click();
    await vi.waitFor(() => expect(dl.disabled).toBe(false));
    expect(open).toHaveBeenCalledOnce();
  });
});

describe('sendMessage', () => {
  it('trims and sends over an OPEN socket', () => {
    const { mgr, send } = makeManager();
    mgr.sendMessage('  hi there  ');
    expect(send).toHaveBeenCalledWith(JSON.stringify({ type: 'chat', text: 'hi there' }));
  });

  it('ignores empty / whitespace-only input', () => {
    const { mgr, send } = makeManager();
    mgr.sendMessage('');
    mgr.sendMessage('   \n ');
    expect(send).not.toHaveBeenCalled();
  });

  it('drops the message when the socket is not open', () => {
    const container = document.createElement('div');
    const { ws, send } = makeWs(WebSocket.CLOSED);
    const mgr = new ChatManager({ myLang: 'en', myId: 'me', container, ws });
    mgr.sendMessage('hello');
    expect(send).not.toHaveBeenCalled();
  });
});
