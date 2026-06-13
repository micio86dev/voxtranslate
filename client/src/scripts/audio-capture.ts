// Dual audio path: the same mic track that WebRTC sends to peers is also
// captured by MediaRecorder and streamed to the server for Deepgram STT.
//
// `start`/`stop` also send control frames so the server opens/closes a fresh
// Deepgram session per capture (clean WebM stream each time → reliable STT).

// MediaRecorder timeslice: how often a WebM chunk is emitted + sent. This is pure
// buffering latency BEFORE audio reaches Deepgram, so smaller = lower end-to-end
// delay (same audio → identical STT/translation quality). 100 ms shaves ~150 ms off
// the old 250 ms with negligible overhead (~10 sends/s) — spec 0043.
const CHUNK_MS = 100;

export class AudioCapture {
  private recorder: MediaRecorder | null = null;
  private stream: MediaStream;
  private ws: WebSocket;
  private active = false;

  constructor(stream: MediaStream, ws: WebSocket) {
    this.stream = stream;
    this.ws = ws;
  }

  /** Point at a new MediaStream after a device change (call while stopped). */
  setStream(stream: MediaStream): void {
    this.stream = stream;
  }

  start(): void {
    if (this.active) return;
    const audioTrack = this.stream.getAudioTracks()[0];
    if (!audioTrack) return;
    const sttStream = new MediaStream([audioTrack]);

    const mime = 'audio/webm;codecs=opus';
    try {
      this.recorder = new MediaRecorder(sttStream, {
        mimeType: MediaRecorder.isTypeSupported(mime) ? mime : 'audio/webm',
        audioBitsPerSecond: 32000,
      });
    } catch {
      return;
    }
    this.recorder.ondataavailable = (e) => {
      if (e.data.size > 0 && this.ws.readyState === WebSocket.OPEN) this.ws.send(e.data);
    };
    this.recorder.onstop = () => this.sendControl('stop');

    this.sendControl('start'); // open Deepgram before audio flows
    this.recorder.start(CHUNK_MS);
    this.active = true;
  }

  stop(): void {
    this.active = false;
    if (this.recorder && this.recorder.state !== 'inactive') {
      this.recorder.stop(); // triggers onstop → sends 'stop'
    }
  }

  /**
   * Stop the current capture and immediately begin a fresh one (spec 0012:
   * after a language change the server opens a new Deepgram stream, which
   * needs a header-bearing first WebM chunk — only a new MediaRecorder
   * produces that).
   *
   * MediaRecorder's `onstop` fires asynchronously, so a naive `stop(); start()`
   * would put the 'start' control frame on the wire BEFORE the old session's
   * 'stop', killing the new session. Chain the restart inside `onstop` instead.
   */
  restart(): void {
    const old = this.recorder;
    this.active = false;
    if (old && old.state !== 'inactive') {
      old.onstop = () => {
        this.sendControl('stop');
        this.start();
      };
      old.stop();
    } else {
      this.start();
    }
  }

  /** Mute = stop sending audio to STT; unmute = resume. */
  setMuted(muted: boolean): void {
    if (muted) this.stop();
    else this.start();
  }

  private sendControl(type: 'start' | 'stop'): void {
    if (this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify({ type }));
  }
}
