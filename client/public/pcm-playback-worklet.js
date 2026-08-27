// AudioWorklet processor for translated-audio playback (spec 0093).
//
// The FIFO here is a JITTER BUFFER, not a straight pipe. Translated speech arrives as
// `translated_audio` frames over the room WebSocket, so their spacing is whatever the
// network and the upstream model produced, while this processor drains at exactly
// `sampleRate` and never waits. Draining straight through — which is what it did
// originally, when only the premium tier fed it — turns every inter-arrival gap wider
// than the audio still in hand into silence punched through the middle of a word. The
// day the default tier gained `translated_audio` that became everyone's experience,
// on calls, webinars and the widget alike, on any tier.
//
// So: hold PREBUFFER_MS of cushion before starting, and on an underrun go back to
// buffering instead of interleaving zeros with speech. The cost is up to PREBUFFER_MS
// of added latency on the translated voice — small next to the ear-voice span of the
// translation itself, and paid once per utterance rather than on every hiccup.
//
// `'flush'` clears everything. Served as a static same-origin file (not a blob: URL) so
// the CSP `worker-src 'self'` allows it. Kept byte-identical between the web client
// (`client/public/`) and the Chrome extension (`voxtranslate-chrome-extension/public/`)
// so both drain audio the same way — change one, change the other.
//
// It also posts `{ playing }` on the edges of speech (spec 0110). Only this processor
// knows when translated audio is actually leaving the graph, and Talk to Anyone needs
// that edge exactly: the device's own speaker is feeding its own microphone, so capture
// is gated WHILE audio plays and released the moment it stops. That has to be an event —
// a timer would either clip the start of the next sentence or leave the microphone deaf
// after the voice finished. Nothing listens by default, so this is inert everywhere else.

/** Cushion built before playback starts, and rebuilt after every underrun. */
const PREBUFFER_MS = 250;
/** …but never hold the tail of an utterance hostage longer than this waiting for it. */
const TAIL_TIMEOUT_MS = 120;
/** Decay time constant applied when audio stops mid-waveform, so a gap cannot click. */
const DECAY_MS = 0.5;
/** Below this the decay is done. Snapping to zero also keeps denormals out of the graph. */
const SILENCE_FLOOR = 1e-4;

const PREBUFFER_SAMPLES = Math.round((PREBUFFER_MS / 1000) * sampleRate);
const TAIL_TIMEOUT_SAMPLES = Math.round((TAIL_TIMEOUT_MS / 1000) * sampleRate);
const DECAY_PER_SAMPLE = Math.exp(-1 / ((DECAY_MS / 1000) * sampleRate));

class PcmPlaybackProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this._queue = [];
    this._cur = null;
    this._pos = 0;
    /** Unplayed samples across `_cur` and `_queue` — the depth of the buffer. */
    this._queued = 0;
    /** false = filling the cushion, true = draining it. */
    this._playing = false;
    /** Output samples rendered since the last frame arrived — how quiet the stream is. */
    this._quiet = 0;
    /** Last sample played, decayed toward zero whenever the audio stops. */
    this._tail = 0;
    /** Last `playing` edge reported to the main thread, so each is posted once. */
    this._reported = false;

    this.port.onmessage = (e) => {
      if (e.data === 'flush') {
        this._queue = [];
        this._cur = null;
        this._pos = 0;
        this._queued = 0;
        this._playing = false;
        // A flush is a cancellation — barge-in, or the session ending. Report the edge
        // immediately rather than after the end-of-speech hold: whoever interrupted is
        // talking RIGHT NOW and the microphone has to be listening.
        this._report(false);
      } else {
        this._queue.push(e.data);
        this._queued += e.data.length;
        // The stream is alive, so the tail timeout below restarts. Without this the
        // timeout measures "how long we have been buffering" — which expires during a
        // healthy start-up and begins playback on a fraction of the cushion.
        this._quiet = 0;
      }
    };
  }

  /** Post a `playing` edge once per transition. */
  _report(playing) {
    if (this._reported === playing) return;
    this._reported = playing;
    this.port.postMessage({ playing });
  }

  /** Fill `out` from `start` with the decaying remains of the last sample played. */
  _silence(out, start) {
    for (let i = start; i < out.length; i++) {
      this._tail *= DECAY_PER_SAMPLE;
      if (this._tail < SILENCE_FLOOR && this._tail > -SILENCE_FLOOR) this._tail = 0;
      out[i] = this._tail;
    }
  }

  process(_inputs, outputs) {
    const out = outputs[0] && outputs[0][0];
    if (!out) return true;

    this._quiet += out.length;

    if (!this._playing) {
      // Start once the cushion is there — or, when nothing has arrived for a while and
      // audio is still queued, play out what we have. Without that second arm the last
      // words of an utterance, always fewer than a full cushion, would never come out.
      const ready =
        this._queued >= PREBUFFER_SAMPLES ||
        (this._queued > 0 && this._quiet >= TAIL_TIMEOUT_SAMPLES);
      if (!ready) {
        // Distinguish the end of an utterance from an underrun mid-word. Both leave
        // `_playing` false, but only one means the voice is finished — and un-gating the
        // microphone during a rebuffer would let the phone hear its own translation.
        // Reuse the tail timeout: it is already this file's "nothing is arriving" mark.
        if (this._queued === 0 && this._quiet >= TAIL_TIMEOUT_SAMPLES) this._report(false);
        this._silence(out, 0);
        return true;
      }
      this._playing = true;
      this._report(true);
    }

    for (let i = 0; i < out.length; i++) {
      if (!this._cur || this._pos >= this._cur.length) {
        this._cur = this._queue.shift() || null;
        this._pos = 0;
      }
      if (!this._cur) {
        // Underrun. Rebuild the cushion rather than pour zeros over speech that is
        // still on its way — the gap is the same length either way, but here it lands
        // once, between utterances, instead of shredding the one being spoken.
        this._playing = false;
        this._silence(out, i);
        return true;
      }
      this._tail = this._cur[this._pos++];
      this._queued--;
      out[i] = this._tail;
    }
    return true;
  }
}
registerProcessor('pcm-playback-processor', PcmPlaybackProcessor);
