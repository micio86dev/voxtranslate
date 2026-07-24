// @vitest-environment jsdom
// ChatPanel DOM controller tests (webinar Feature ⑤). Exercises the shared panel logic in
// jsdom: message-row rendering in myLang, de-dup by id (so a WS echo of our optimistic send
// doesn't double up), optimistic render on a 200 send, and notice mapping for 429/422.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ChatPanel, buildChatRow, type ChatEvent } from './webinar-chat';

const strings = {
  send: 'Send',
  hostTag: 'HOST',
  empty: 'No messages yet',
  rateLimited: 'Too fast',
  blocked: 'Blocked',
  genericError: 'Error',
  downloadFile: 'Download file',
};

function dom() {
  document.body.innerHTML = `
    <div id="list"></div>
    <input id="input" />
    <button id="send"></button>
    <p id="notice" class="hidden"></p>`;
  return {
    list: document.getElementById('list')!,
    input: document.getElementById('input') as HTMLInputElement,
    sendBtn: document.getElementById('send') as HTMLButtonElement,
    notice: document.getElementById('notice')!,
  };
}

const ev = (over: Partial<ChatEvent> = {}): ChatEvent => ({
  id: 'm1',
  sender_kind: 'guest',
  display_name: 'Ada',
  original: 'hola',
  lang: 'es',
  translations: { es: 'hola', en: 'hi' },
  created_at: 't',
  ...over,
});

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe('buildChatRow', () => {
  it('renders the name + the myLang translation, tagging a host message', () => {
    const guest = buildChatRow(document, ev(), 'en', 'HOST');
    expect(guest.querySelector('.wv-chat-body')!.textContent).toBe('hi');
    expect(guest.classList.contains('is-host')).toBe(false);
    expect(guest.querySelector('.wv-chat-host-tag')).toBeNull();

    const host = buildChatRow(document, ev({ sender_kind: 'host', display_name: 'H' }), 'fr', 'HOST');
    expect(host.classList.contains('is-host')).toBe(true);
    expect(host.querySelector('.wv-chat-host-tag')!.textContent).toBe('HOST');
    // No 'fr' translation → falls back to the source original.
    expect(host.querySelector('.wv-chat-body')!.textContent).toBe('hola');
  });

  it('renders the send time (HH:MM) from created_at, with the name in its own span', () => {
    const row = buildChatRow(document, ev({ created_at: '2026-07-24T14:32:00Z' }), 'en', 'HOST');
    const time = row.querySelector('.wv-chat-time') as HTMLTimeElement;
    expect(time).not.toBeNull();
    expect(time.getAttribute('datetime')).toBe('2026-07-24T14:32:00Z');
    expect(time.textContent).toMatch(/\d{1,2}:\d{2}/); // locale-formatted HH:MM
    // Name lives in its own span so it can ellipsis instead of wrapping under the avatar.
    expect(row.querySelector('.wv-chat-name')!.textContent).toBe('Ada');
  });

  it('leaves the time empty when created_at is unparseable', () => {
    const row = buildChatRow(document, ev({ created_at: 'not-a-date' }), 'en', 'HOST');
    expect(row.querySelector('.wv-chat-time')!.textContent).toBe('');
  });

  it('guest always shows initials, never an <img>', () => {
    const row = buildChatRow(document, ev(), 'en', 'HOST', 'https://example.com/avatar.png');
    const avatar = row.querySelector('.wv-chat-avatar') as HTMLElement;
    expect(avatar.querySelector('img')).toBeNull();
    expect(avatar.textContent).toBe('A'); // 'Ada'[0]
  });

  it('host without avatar url shows initials', () => {
    const row = buildChatRow(document, ev({ sender_kind: 'host', display_name: 'Bob' }), 'en', 'HOST');
    const avatar = row.querySelector('.wv-chat-avatar') as HTMLElement;
    expect(avatar.querySelector('img')).toBeNull();
    expect(avatar.textContent).toBe('B');
    // jsdom normalizes hex to rgb
    expect(avatar.style.background).toMatch(/rgb\(59,\s*130,\s*246\)|#3b82f6/);
  });

  it('host with avatar url renders <img> in the avatar span', () => {
    const row = buildChatRow(
      document,
      ev({ sender_kind: 'host', display_name: 'Host' }),
      'en',
      'HOST',
      'https://lh3.googleusercontent.com/photo/abc',
    );
    const avatar = row.querySelector('.wv-chat-avatar') as HTMLElement;
    const img = avatar.querySelector('img');
    expect(img).not.toBeNull();
    expect(img!.getAttribute('src')).toContain('abc');
    expect(img!.referrerPolicy).toBe('no-referrer');
    expect(img!.alt).toBe('');
    expect(avatar.textContent).toBe(''); // no initials while image is present
  });

  it('host avatar img onerror falls back to initials', () => {
    const row = buildChatRow(
      document,
      ev({ sender_kind: 'host', display_name: 'Host' }),
      'en',
      'HOST',
      'https://example.com/avatar.png',
    );
    const avatar = row.querySelector('.wv-chat-avatar') as HTMLElement;
    const img = avatar.querySelector('img')!;
    img.dispatchEvent(new Event('error'));
    expect(avatar.querySelector('img')).toBeNull();
    expect(avatar.textContent).toBe('H');
    expect(avatar.style.background).toMatch(/rgb\(59,\s*130,\s*246\)|#3b82f6/);
  });

  it('renders an image attachment as an inline preview (width-constrained via CSS class)', () => {
    const row = buildChatRow(
      document,
      ev({
        attachment: {
          url: 'https://files.example.com/pic.png',
          name: 'pic.png',
          content_type: 'image/png',
          size: 4096,
        },
      }),
      'en',
      'HOST',
    );
    const img = row.querySelector<HTMLImageElement>('.wv-chat-att-img')!;
    expect(img).not.toBeNull();
    expect(img.getAttribute('src')).toBe('https://files.example.com/pic.png');
    expect(row.querySelector('.wv-chat-att-file')).toBeNull();
    // Images are downloadable too (overlaid download button), not just file chips.
    expect(row.querySelector('button.wv-chat-att-dl')).not.toBeNull();
  });

  it('renders a document attachment as a compact chip with a truncated name, size and download button', () => {
    const row = buildChatRow(
      document,
      ev({
        attachment: {
          url: 'https://files.example.com/report.pdf',
          name: 'a-very-long-quarterly-financial-report.pdf',
          content_type: 'application/pdf',
          size: 3_565_158,
        },
      }),
      'en',
      'HOST',
      undefined,
      'Download file',
    );
    // No inline image, no bare new-tab link — a self-contained chip that can't overflow.
    expect(row.querySelector('.wv-chat-att-img')).toBeNull();
    expect(row.querySelector('a.wv-chat-att-file')).toBeNull();
    const chip = row.querySelector<HTMLElement>('.wv-chat-att-file')!;
    expect(chip).not.toBeNull();
    const nameEl = chip.querySelector<HTMLElement>('.wv-chat-att-file-name')!;
    expect(nameEl.textContent).toBe('a-very-long-quarterly-financial-report.pdf');
    expect(nameEl.title).toBe('a-very-long-quarterly-financial-report.pdf');
    expect(chip.querySelector('.wv-chat-att-file-size')?.textContent).toBe('3.4 MB');
    const dl = chip.querySelector<HTMLButtonElement>('button.wv-chat-att-dl')!;
    expect(dl).not.toBeNull();
    expect(dl.getAttribute('aria-label')).toBe('Download file');
    expect(dl.title).toBe('Download file');
  });

  it('document download button fetches the bytes and saves them under the file name', async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      blob: async () => new Blob(['pdf-bytes']),
    } as unknown as Response);
    vi.stubGlobal('fetch', fetchMock);
    URL.createObjectURL = vi.fn(() => 'blob:wv-1');
    URL.revokeObjectURL = vi.fn();
    const clickSpy = vi
      .spyOn(HTMLAnchorElement.prototype, 'click')
      .mockImplementation(() => {});

    const row = buildChatRow(
      document,
      ev({
        attachment: {
          url: 'https://files.example.com/report.pdf',
          name: 'report.pdf',
          content_type: 'application/pdf',
          size: 1024,
        },
      }),
      'en',
      'HOST',
      undefined,
      'Download file',
    );
    const dl = row.querySelector<HTMLButtonElement>('.wv-chat-att-dl')!;
    dl.click();
    await vi.waitFor(() => expect(dl.disabled).toBe(false));

    expect(fetchMock).toHaveBeenCalledWith('https://files.example.com/report.pdf');
    const anchor = clickSpy.mock.contexts[0] as HTMLAnchorElement;
    expect(anchor.getAttribute('href')).toBe('blob:wv-1');
    expect(anchor.download).toBe('report.pdf');
    expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:wv-1');
  });

  it('document download falls back to opening the file when the fetch fails', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue({ ok: false, status: 403 } as unknown as Response),
    );
    const open = vi.spyOn(window, 'open').mockReturnValue(null);
    const row = buildChatRow(
      document,
      ev({
        attachment: {
          url: 'https://files.example.com/report.pdf',
          name: 'report.pdf',
          content_type: 'application/pdf',
          size: 1024,
        },
      }),
      'en',
      'HOST',
      undefined,
      'Download file',
    );
    const dl = row.querySelector<HTMLButtonElement>('.wv-chat-att-dl')!;
    dl.click();
    await vi.waitFor(() => expect(dl.disabled).toBe(false));
    expect(open).toHaveBeenCalledWith('https://files.example.com/report.pdf', '_blank', 'noopener');
  });
});

describe('ChatPanel', () => {
  it('shows the empty state, then replaces it on the first appended message', () => {
    const d = dom();
    const panel = new ChatPanel({
      ...d,
      httpBase: 'https://h',
      code: 'c',
      myLang: () => 'en',
      senderLang: () => 'en',
      displayName: () => 'Ada',
      token: () => null,
      strings,
    });
    expect(d.list.querySelector('.wv-chat-empty')).not.toBeNull();
    panel.append(ev());
    expect(d.list.querySelector('.wv-chat-empty')).toBeNull();
    expect(d.list.querySelectorAll('.wv-chat-msg')).toHaveLength(1);
  });

  it('de-dupes by id so a WS echo of an already-rendered message is a no-op', () => {
    const d = dom();
    const panel = new ChatPanel({
      ...d,
      httpBase: 'https://h',
      code: 'c',
      myLang: () => 'en',
      senderLang: () => 'en',
      displayName: () => 'Ada',
      token: () => null,
      strings,
    });
    // append returns true for a genuinely new row, false for a dedup'd id — the unread
    // badge relies on this to count only new live messages (never the echoed own send).
    expect(panel.append(ev({ id: 'x' }))).toBe(true);
    expect(panel.append(ev({ id: 'x' }))).toBe(false); // duplicate id — ignored
    expect(d.list.querySelectorAll('.wv-chat-msg')).toHaveLength(1);
  });

  it('optimistically renders a sent message on a 200 and clears the input', async () => {
    const d = dom();
    d.input.value = 'hey there';
    const fetchImpl = (async () => ({
      ok: true,
      status: 200,
      json: async () => ({ id: 'srv-9', created_at: 't9' }),
    })) as unknown as typeof fetch;
    const panel = new ChatPanel({
      ...d,
      httpBase: 'https://h',
      code: 'c',
      myLang: () => 'en',
      senderLang: () => 'en',
      displayName: () => 'Ada',
      token: () => null,
      strings,
      fetchImpl,
    });
    const ok = await panel.send();
    expect(ok).toBe(true);
    expect(d.input.value).toBe(''); // cleared
    const rows = d.list.querySelectorAll('.wv-chat-msg');
    expect(rows).toHaveLength(1);
    expect(rows[0].querySelector('.wv-chat-body')!.textContent).toBe('hey there');
    // A later WS echo carrying the SAME server id must not duplicate the message.
    panel.append(ev({ id: 'srv-9', original: 'hey there' }));
    expect(d.list.querySelectorAll('.wv-chat-msg')).toHaveLength(1);
  });

  it('shows the rate-limit notice on a 429 and keeps the input intact', async () => {
    const d = dom();
    d.input.value = 'spam';
    const fetchImpl = (async () => ({
      ok: false,
      status: 429,
      json: async () => ({}),
    })) as unknown as typeof fetch;
    const panel = new ChatPanel({
      ...d,
      httpBase: 'https://h',
      code: 'c',
      myLang: () => 'en',
      senderLang: () => 'en',
      displayName: () => 'Ada',
      token: () => null,
      strings,
      fetchImpl,
    });
    const ok = await panel.send();
    expect(ok).toBe(false);
    expect(d.notice.textContent).toBe('Too fast');
    expect(d.notice.classList.contains('hidden')).toBe(false);
    expect(d.input.value).toBe('spam'); // not cleared — the user can retry
    expect(d.list.querySelectorAll('.wv-chat-msg')).toHaveLength(0);
  });

  it('maps a 422 to the blocked notice', async () => {
    const d = dom();
    d.input.value = 'bad words';
    const fetchImpl = (async () => ({
      ok: false,
      status: 422,
      json: async () => ({ error: 'moderated' }),
    })) as unknown as typeof fetch;
    const panel = new ChatPanel({
      ...d,
      httpBase: 'https://h',
      code: 'c',
      myLang: () => 'en',
      senderLang: () => 'en',
      displayName: () => 'Ada',
      token: () => null,
      strings,
      fetchImpl,
    });
    await panel.send();
    expect(d.notice.textContent).toBe('Blocked');
  });
});
