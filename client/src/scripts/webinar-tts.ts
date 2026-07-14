// Webinar participant TTS. Standard tier = Browser SpeechSynthesis.
// Enhanced tier = Cartesia TTS WebSocket + PCM16 playback.
// Invoked by webinar-view.ts once per final SubtitleEvent when listen mode is 'translated'.

import { PcmPlayback } from './pcm-playback';

export interface WebinarTtsSession {
  token: string;
  expires_at: number; // Unix seconds
  cartesia_version: string;
  tts: { endpoint: string; model: string };
  default_voice_id: string;
}

export class WebinarTts {
  private code: string;
  private tier: 'standard' | 'enhanced';
  private lang: string;
  private httpBase: string;
  private enabled = false;

  // Enhanced-only state
  private session: WebinarTtsSession | null = null;
  // Pending fetch promise; avoids double-fetching when multiple speak() calls
  // arrive before the first fetch completes.
  private sessionFetch: Promise<WebinarTtsSession | null> | null = null;
  private ws: WebSocket | null = null;
  private pcm: PcmPlayback | null = null;
  // Per context_id sequence counter (Cartesia requires monotonically increasing seq).
  private contextSeq = new Map<string, number>();
  private ttsCounter = 0;

  constructor(opts: {
    code: string;
    tier: 'standard' | 'enhanced';
    lang: string;
    httpBase: string;
  }) {
    this.code = opts.code;
    this.tier = opts.tier;
    this.lang = opts.lang;
    this.httpBase = opts.httpBase;
  }

  setEnabled(on: boolean): void {
    this.enabled = on;
    if (!on) this.cancelBrowserSynth();
  }

  speak(text: string): void {
    if (!this.enabled || !text) return;
    if (this.tier === 'standard') {
      this.speakStandard(text);
    } else {
      void this.speakEnhanced(text);
    }
  }

  stop(): void {
    this.enabled = false;
    this.cancelBrowserSynth();
    this.ws?.close();
    this.ws = null;
    this.pcm?.stop();
    this.pcm = null;
    this.session = null;
    this.sessionFetch = null;
    this.contextSeq.clear();
  }

  private cancelBrowserSynth(): void {
    if ('speechSynthesis' in window) window.speechSynthesis.cancel();
  }

  private speakStandard(text: string): void {
    if (!('speechSynthesis' in window)) return;
    const utt = new SpeechSynthesisUtterance(text);
    utt.lang = this.lang;
    window.speechSynthesis.speak(utt);
  }

  private async speakEnhanced(text: string): Promise<void> {
    const session = await this.ensureSession();
    if (!session || !this.enabled) return;

    const ws = await this.ensureWs(session);
    if (!ws || ws.readyState !== WebSocket.OPEN || !this.enabled) return;

    const contextId = `wbr-${this.ttsCounter++}`;
    this.contextSeq.set(contextId, 0);

    try {
      ws.send(JSON.stringify({
        model_id: session.tts.model,
        transcript: text,
        voice: { mode: 'id', id: session.default_voice_id },
        output_format: { container: 'raw', encoding: 'pcm_s16le', sample_rate: 24000 },
        language: this.lang,
        context_id: contextId,
        continue: false,
      }));
    } catch {
      // WS send failure — degrade silently to subtitles-only
    }
  }

  private async ensureSession(): Promise<WebinarTtsSession | null> {
    const now = Math.floor(Date.now() / 1000);
    if (this.session && now + 60 < this.session.expires_at) return this.session;

    // Deduplicate concurrent fetches
    if (this.sessionFetch) return this.sessionFetch;

    this.sessionFetch = (async (): Promise<WebinarTtsSession | null> => {
      try {
        const res = await fetch(`${this.httpBase}/api/w/${encodeURIComponent(this.code)}/tts-session`);
        if (!res.ok) return null;
        const data = (await res.json()) as WebinarTtsSession;
        this.session = data;
        return data;
      } catch {
        return null;
      } finally {
        this.sessionFetch = null;
      }
    })();

    return this.sessionFetch;
  }

  private async ensureWs(session: WebinarTtsSession): Promise<WebSocket | null> {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) return this.ws;
    if (this.ws && this.ws.readyState === WebSocket.CONNECTING) {
      // Wait for it to open (max 5 s)
      return new Promise((resolve) => {
        const ws = this.ws!;
        const onOpen = (): void => { cleanup(); resolve(ws); };
        const onClose = (): void => { cleanup(); resolve(null); };
        const t = setTimeout(() => { cleanup(); resolve(null); }, 5000);
        function cleanup(): void { ws.removeEventListener('open', onOpen); ws.removeEventListener('close', onClose); clearTimeout(t); }
        ws.addEventListener('open', onOpen);
        ws.addEventListener('close', onClose);
      });
    }

    if (!this.pcm) this.pcm = new PcmPlayback();

    const url = new URL(session.tts.endpoint);
    url.searchParams.set('cartesia_version', session.cartesia_version);
    url.searchParams.set('access_token', session.token);

    let ws: WebSocket;
    try {
      ws = new WebSocket(url.toString());
    } catch {
      return null;
    }
    this.ws = ws;

    ws.onmessage = (evt): void => {
      try {
        const msg = JSON.parse(evt.data as string) as {
          type?: string;
          context_id?: string;
          data?: string;
        };
        if (msg.type === 'chunk' && msg.data && msg.context_id) {
          const seq = (this.contextSeq.get(msg.context_id) ?? 0);
          this.contextSeq.set(msg.context_id, seq + 1);
          this.pcm?.enqueue('wbr-host', seq, msg.data);
        }
      } catch {
        // malformed frame — ignore
      }
    };

    ws.onclose = (): void => {
      if (this.ws === ws) this.ws = null;
    };

    // Wait for open (max 5 s)
    return new Promise((resolve) => {
      const onOpen = (): void => { cleanup(); resolve(ws); };
      const onClose = (): void => { cleanup(); resolve(null); };
      const t = setTimeout(() => { cleanup(); resolve(null); }, 5000);
      function cleanup(): void { ws.removeEventListener('open', onOpen); ws.removeEventListener('close', onClose); clearTimeout(t); }
      ws.addEventListener('open', onOpen);
      ws.addEventListener('close', onClose);
    });
  }
}
