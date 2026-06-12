// Translated chat. Renders each message in the viewer's language, with the
// original shown small below when it was translated. Tracks unread while closed.

import { avatarUrl } from './auth';
import { icon } from './icons';

/** A file attached to a chat message (spec 0018). */
export interface ChatAttachment {
  url: string;
  name: string;
  content_type: string;
  size: number;
}

export interface ChatPayload {
  sender_id: string;
  sender_name: string;
  sender_lang: string;
  sender_avatar?: string | null;
  original: string;
  translations: Record<string, string>;
  timestamp: number;
  /** Present when the message carries an uploaded file (spec 0018). */
  attachment?: ChatAttachment | null;
}

/** Human-readable size, e.g. "3.4 MB". */
export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export class ChatManager {
  private myLang: string;
  private myId: string;
  private container: HTMLElement;
  private ws: WebSocket;
  private unread = 0;
  private isOpen = false;

  onUnread: (count: number) => void = () => {};

  constructor(opts: { myLang: string; myId: string; container: HTMLElement; ws: WebSocket }) {
    this.myLang = opts.myLang;
    this.myId = opts.myId;
    this.container = opts.container;
    this.ws = opts.ws;
  }

  addMessage(data: ChatPayload): void {
    // Every user reads incoming messages only in their own language.
    const translated = data.translations[this.myLang] ?? data.original;
    const isMine = data.sender_id === this.myId;

    const msg = document.createElement('div');
    msg.className = `chat-msg ${isMine ? 'chat-msg-mine' : 'chat-msg-other'}`;

    // Sender avatar + name (bold), message rendered inline on the same line.
    if (!isMine) {
      const av = avatarUrl(data.sender_avatar, 36);
      if (av) {
        const img = document.createElement('img');
        img.className = 'chat-avatar';
        img.referrerPolicy = 'no-referrer';
        img.alt = '';
        img.src = av;
        img.addEventListener('error', () => img.remove());
        msg.appendChild(img);
      }
      const sender = document.createElement('span');
      sender.className = 'chat-sender';
      sender.textContent = data.sender_name;
      msg.appendChild(sender);
      msg.appendChild(document.createTextNode(' '));
    }
    // File attachment (spec 0018): an inline audio player for audio, otherwise a
    // download chip. Rendered before the (translated) transcription/extracted text.
    if (data.attachment) {
      msg.appendChild(this.renderAttachment(data.attachment));
    }

    if (translated) {
      const text = document.createElement('span');
      text.className = 'chat-text';
      text.textContent = translated;
      msg.appendChild(text);
    }

    this.container.appendChild(msg);
    this.container.scrollTop = this.container.scrollHeight;

    if (!this.isOpen && !isMine) {
      this.unread++;
      this.onUnread(this.unread);
    }
  }

  /** Build the attachment block: audio player for audio, else a download chip. */
  private renderAttachment(att: ChatAttachment): HTMLElement {
    const wrap = document.createElement('div');
    wrap.className = 'chat-attachment';

    if (att.content_type.startsWith('audio/')) {
      const audio = document.createElement('audio');
      audio.controls = true;
      audio.preload = 'none';
      audio.src = att.url;
      audio.className = 'chat-audio';
      wrap.appendChild(audio);
    }

    // Always offer a labelled download link (the file name + size).
    const link = document.createElement('a');
    link.className = 'chat-file-chip';
    link.href = att.url;
    link.target = '_blank';
    link.rel = 'noopener noreferrer';
    link.innerHTML = icon('file', 18);
    const meta = document.createElement('span');
    meta.className = 'chat-file-meta';
    const nameEl = document.createElement('span');
    nameEl.className = 'chat-file-name';
    nameEl.textContent = att.name;
    const sizeEl = document.createElement('span');
    sizeEl.className = 'chat-file-size';
    sizeEl.textContent = formatSize(att.size);
    meta.appendChild(nameEl);
    meta.appendChild(sizeEl);
    link.appendChild(meta);
    wrap.appendChild(link);

    return wrap;
  }

  sendMessage(text: string): void {
    const trimmed = text.trim();
    if (!trimmed) return;
    if (this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({ type: 'chat', text: trimmed }));
    }
  }

  setOpen(open: boolean): void {
    this.isOpen = open;
    if (open) {
      this.unread = 0;
      this.onUnread(0);
    }
  }

  setMyLang(lang: string): void {
    this.myLang = lang;
  }
}
