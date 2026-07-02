// Translated chat. Renders each message in the viewer's language, with the
// original shown small below when it was translated. Tracks unread while closed.

import { avatarUrl } from './auth';
import { t } from './i18n';
import { icon } from './icons';

/** A file attached to a chat message (spec 0018). */
export interface ChatAttachment {
  url: string;
  name: string;
  content_type: string;
  size: number;
}

/** Only allow http(s) attachment URLs before they reach `href`/`src`/`fetch`.
 *  Attachment URLs are server-issued Supabase links today, but this is cheap
 *  defense-in-depth against a `javascript:`/`data:` URL ever reaching a click sink. */
function safeHttpUrl(u: string): string {
  try {
    const parsed = new URL(u, window.location.origin);
    return parsed.protocol === 'https:' || parsed.protocol === 'http:' ? parsed.href : '#';
  } catch {
    return '#';
  }
}

/** File extension for a voice-message download, from its MIME type. The stored
 *  format is already a lightweight audio format (Opus/WebM, 32 kbps mono). */
function audioExt(contentType: string): string {
  const base = contentType.split(';')[0].trim();
  const map: Record<string, string> = {
    'audio/webm': 'webm',
    'audio/ogg': 'ogg',
    'audio/mpeg': 'mp3',
    'audio/mp4': 'm4a',
    'audio/aac': 'aac',
    'audio/wav': 'wav',
  };
  return map[base] ?? 'webm';
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

    // Also show the message in the sender's own language (small, below) when it was
    // actually translated — so you can read the original too, like the subtitles do.
    if (data.original && data.original !== translated) {
      const orig = document.createElement('span');
      orig.className = 'chat-original';
      orig.textContent = data.original;
      msg.appendChild(orig);
    }

    this.container.appendChild(msg);
    this.container.scrollTop = this.container.scrollHeight;

    if (!this.isOpen && !isMine) {
      this.unread++;
      this.onUnread(this.unread);
    }
  }

  /** Build the attachment block: a voice-message player (audio + speed control)
   *  for audio, else a labelled download chip for documents. */
  private renderAttachment(att: ChatAttachment): HTMLElement {
    const wrap = document.createElement('div');
    wrap.className = 'chat-attachment';
    const safeUrl = safeHttpUrl(att.url);

    // Voice message: inline player + a tap-to-cycle 1×/1.5×/2× speed pill. The
    // transcript is the message text, so no download chip is shown for audio.
    if (att.content_type.startsWith('audio/')) {
      const player = document.createElement('div');
      player.className = 'chat-audio-player';
      const audio = document.createElement('audio');
      audio.controls = true;
      audio.preload = 'none';
      audio.src = safeUrl;
      audio.className = 'chat-audio';
      const rates = [1, 1.5, 2];
      let ri = 0;
      const speed = document.createElement('button');
      speed.type = 'button';
      speed.className = 'chat-audio-speed';
      speed.textContent = '1×';
      speed.addEventListener('click', () => {
        ri = (ri + 1) % rates.length;
        audio.playbackRate = rates[ri];
        speed.textContent = `${rates[ri]}×`;
      });
      // Browsers reset playbackRate when a new source plays; reapply on each play.
      audio.addEventListener('play', () => {
        audio.playbackRate = rates[ri];
      });
      // Save the voice note as a file. The signed URL is cross-origin, so the
      // <a download> attribute is ignored — fetch the bytes (supabase.co is in
      // connect-src) and save via an object URL; fall back to opening the file.
      const dl = document.createElement('button');
      dl.type = 'button';
      dl.className = 'chat-audio-dl';
      dl.innerHTML = icon('download', 16);
      dl.title = t('chatVoiceDownload');
      dl.setAttribute('aria-label', t('chatVoiceDownload'));
      dl.addEventListener('click', async () => {
        dl.disabled = true;
        try {
          const res = await fetch(safeUrl);
          if (!res.ok) throw new Error(`download failed: ${res.status}`);
          const blob = await res.blob();
          const objUrl = URL.createObjectURL(blob);
          const a = document.createElement('a');
          a.href = objUrl;
          a.download = `voice-message.${audioExt(att.content_type)}`;
          a.click();
          URL.revokeObjectURL(objUrl);
        } catch {
          window.open(safeUrl, '_blank', 'noopener');
        } finally {
          dl.disabled = false;
        }
      });
      player.appendChild(audio);
      player.appendChild(speed);
      player.appendChild(dl);
      wrap.appendChild(player);
      return wrap;
    }

    // Document: a labelled download link (the file name + size).
    const link = document.createElement('a');
    link.className = 'chat-file-chip';
    link.href = safeUrl;
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
