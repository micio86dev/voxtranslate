// HLS participant player (webinar phase 1, F1-5). A guest landing on `/w/{code}`
// watches the host's LL-HLS broadcast. Two playback paths:
//   - Safari (+ iOS): NATIVE HLS via `<video src=…>` — it plays the manifest directly.
//   - Everyone else: hls.js (a lazy chunk) attaches to the `<video>` MediaSource.
//
// State machine: `waiting` (webinar scheduled or the manifest isn't live yet) →
// `live` (playing) → `ended` (host stopped). While waiting we poll `GET /api/w/{code}`
// to detect the live transition; while live we poll to detect `ended`. Autoplay
// respects the browser audio policy: we try muted autoplay and fall back to a
// tap-to-start overlay when even that is blocked.
//
// The pure logic (format selection, state transitions from a poll, autoplay-blocked
// handling, guest_id persistence) is kept independent of a real hls.js / <video> so it
// unit-tests with fakes.

import { getPublicWebinar, type PublicWebinar, type WebinarStatus } from "./webinar";

/** Where the participant's stable anonymous id is persisted (survives reloads so the
 *  server can correlate the same guest across polls / a rejoin). */
export const GUEST_ID_KEY = "vox.guest_id";

/** The player's externally-observable state, surfaced to the UI. */
export type PlayerState = "waiting" | "live" | "ended" | "error";

/** Which playback engine to use for the manifest. */
export type PlaybackFormat = "native" | "hlsjs" | "unsupported";

/** How often we re-poll the public webinar endpoint to detect live/ended transitions. */
const POLL_INTERVAL_MS = 5_000;

/** How many times to re-attach when the stream is still audio-only. A guest who joins in
 *  the brief window right after the host goes live — before MediaMTX has remuxed the first
 *  H264 keyframe into an HLS video rendition — otherwise attaches to an audio-only master
 *  and shows a black canvas until a manual refresh. After this many tries we accept the
 *  stream as-is, so a genuinely mic-only broadcast plays its audio instead of looping. */
const MAX_VIDEO_RETRIES = 3;

/** Consecutive health checks (one per poll) with a frozen playhead before we rebuild the
 *  pipeline. 1 ⇒ react after ~POLL_INTERVAL_MS of no progress: long enough not to trip on
 *  a brief buffering hiccup, short enough that a host reload doesn't leave guests frozen. */
const STALL_CHECKS = 1;

/** How far behind the live edge playback may drift before a resume JUMPS forward instead
 *  of draining the stale buffer. A live webinar has no value in old frames, and a guest
 *  who resumes 20 s back keeps that 20 s for the rest of the broadcast. */
const MAX_RESUME_DRIFT_S = 2;

/** Landing exactly on `seekable.end` re-stalls the decoder (the edge moves while the seek
 *  is in flight), so aim just short of it. One LL-HLS part is ~200 ms; half a second is a
 *  safe cushion that is still well inside the low-latency budget. */
const LIVE_EDGE_GUARD_S = 0.5;

/**
 * hls.js live-latency policy. `lowLatencyMode` alone is NOT enough: it makes hls.js honour
 * the playlist's PART-HOLD-BACK on the FIRST load, but hls.js's defaults then let the
 * playhead drift away from the live edge and never bring it back.
 *
 * The drift is unavoidable in this player: `waitForFirstFrame()` blocks up to 10 s before
 * play(), the autoplay policy can park the guest on a tap-to-start overlay, and any
 * rebuffer costs its own duration. With `maxLiveSyncPlaybackRate` at its default of 1,
 * every second lost that way is latency for the WHOLE webinar — which is how a properly
 * configured LL-HLS pipeline still measures ~6 s end to end.
 */
const LIVE_TUNING = {
  lowLatencyMode: true,
  /** Play up to 50% fast while behind the live edge, until the gap is closed. Imper-
   *  ceptible on speech (the browser pitch-corrects) and the single biggest win here. */
  maxLiveSyncPlaybackRate: 1.5,
  /** Beyond this many target durations behind, stop trying to play the gap away and
   *  seek to the edge instead. */
  liveMaxLatencyDurationCount: 10,
  /** Only used when LL-HLS is NOT available (no EXT-X-PART, or the blocking reloads
   *  fail): plain HLS holds back 3 target durations. With MediaMTX's segments driven by
   *  the publisher's keyframe interval (~2 s), that default IS the ~6 s floor. */
  liveSyncDurationCount: 1,
  /** A webinar guest never seeks backwards; retaining played segments only grows memory
   *  over a long broadcast. */
  backBufferLength: 10,
} as const;

/** The end of `video`'s live window, preferring the seekable range and falling back to the
 *  buffered one. The fallback matters: a browser's NATIVE HLS player exposes no seekable
 *  range on a live stream (Chrome, observed with readyState 4 and playback running), so
 *  without it every drift measurement there silently reads zero. */
function liveEdge(
  video: { seekable?: TimeRanges; buffered?: TimeRanges },
): number | null {
  for (const range of [video.seekable, video.buffered]) {
    if (!range || range.length === 0) continue;
    const end = range.end(range.length - 1);
    if (Number.isFinite(end)) return end;
  }
  return null;
}

/** Seconds `video` is behind the live edge, or 0 when nothing is buffered yet. Pure over
 *  a video-like object so it is unit-testable. */
export function liveEdgeDrift(
  video: Pick<HTMLVideoElement, "currentTime"> & { seekable?: TimeRanges; buffered?: TimeRanges },
): number {
  const edge = liveEdge(video);
  if (edge === null) return 0;
  return Math.max(0, edge - video.currentTime);
}

/** Snap `video` to the live edge when it has drifted more than `maxDriftS` behind it.
 *  Returns the resulting `currentTime` (unchanged when there is nothing to correct).
 *  Pure over a video-like object, so it is unit-testable. */
export function seekToLiveEdge(
  video: Pick<HTMLVideoElement, "currentTime"> & { seekable?: TimeRanges; buffered?: TimeRanges },
  maxDriftS = MAX_RESUME_DRIFT_S,
): number {
  if (liveEdgeDrift(video) <= maxDriftS) return video.currentTime;
  const edge = liveEdge(video)!;
  try {
    video.currentTime = edge - LIVE_EDGE_GUARD_S;
  } catch {
    /* seeking can throw while the media element is detaching — leave it be */
  }
  return video.currentTime;
}

/** localStorage may be blocked (private mode, tests) — fall back to an in-memory map,
 *  mirroring auth.ts so guest_id persistence never throws. */
const mem = new Map<string, string>();
function store(): Pick<Storage, "getItem" | "setItem"> {
  try {
    if (typeof localStorage !== "undefined") return localStorage;
  } catch {
    /* blocked */
  }
  return {
    getItem: (k) => (mem.has(k) ? mem.get(k)! : null),
    setItem: (k, v) => void mem.set(k, v),
  };
}

/** Persist the server-issued `guest_id` locally (first write wins — never clobber an
 *  existing id, so the same visitor keeps one identity across reloads). Returns the
 *  effective stored id. Pure over `store()`. */
export function persistGuestId(guestId: string): string {
  const existing = store().getItem(GUEST_ID_KEY);
  if (existing) return existing;
  if (guestId) store().setItem(GUEST_ID_KEY, guestId);
  return guestId;
}

/** The persisted guest id, or null before the first API response. */
export function getStoredGuestId(): string | null {
  return store().getItem(GUEST_ID_KEY);
}

/**
 * Pick the playback engine for a `<video>` element: prefer hls.js wherever it runs, and
 * fall back to the browser's own HLS player only when it does not (iOS Safari, where
 * MediaSource/Managed MediaSource is unavailable).
 *
 * This order used to be reversed — native first, whenever `canPlayType` said yes. That
 * was written when Safari was the only browser answering. Chrome 150 answers `"maybe"`
 * too, which silently moved every Chrome guest onto the browser's player: no
 * `lowLatencyMode`, no credentialed fetch loader for the session-cookie gate, no error
 * recovery, and no catch-up policy. Measured in the field, that path sits at plain-HLS
 * hold-back — 3 x the 2 s segment duration, oscillating between 5.8 s and 6.9 s with
 * `playbackRate` pinned at 1.00 — while the server was serving LL-HLS parts the whole
 * time with a PART-HOLD-BACK of 0.59 s.
 *
 * Pure — takes the video + an hls.js-supported flag so it is unit-testable with fakes.
 */
export function selectFormat(
  video: Pick<HTMLVideoElement, "canPlayType">,
  hlsjsSupported: boolean,
): PlaybackFormat {
  if (hlsjsSupported) return "hlsjs";
  if (video.canPlayType("application/vnd.apple.mpegurl")) return "native";
  return "unsupported";
}

/** Map a webinar status from a poll to the player state. `scheduled` → waiting;
 *  `live` → live; `ended`/`cancelled` → ended. Pure. */
export function stateFromStatus(status: WebinarStatus): PlayerState {
  if (status === "live") return "live";
  if (status === "ended" || status === "cancelled") return "ended";
  return "waiting"; // scheduled — the host hasn't gone live yet
}

/** Attempt to play a `<video>`, tolerating the browser autoplay policy: try as-is,
 *  and on rejection retry muted. Returns `{ playing, needsTap }` — `needsTap` means
 *  the UI should show a tap overlay: either nothing plays (total block) or the video
 *  plays muted and the user must tap to unmute.
 *  Pure over the passed video-like object, so it is unit-testable. */
export async function tryAutoplay(
  video: Pick<HTMLVideoElement, "play"> & { muted: boolean },
): Promise<{ playing: boolean; needsTap: boolean }> {
  try {
    await video.play();
    return { playing: true, needsTap: false };
  } catch {
    /* unmuted autoplay blocked — retry muted (most browsers allow this) */
  }
  try {
    video.muted = true;
    await video.play();
    // Playing muted: show the tap overlay so the user knows to click to unmute.
    return { playing: true, needsTap: true };
  } catch {
    // Even muted autoplay was blocked — the user must tap to start entirely.
    return { playing: false, needsTap: true };
  }
}

/** Whether to re-attach because the stream still looks audio-only: playable data is
 *  flowing (readyState ≥ HAVE_CURRENT_DATA, checked by the caller) but the `<video>` has
 *  no video track yet (`videoWidth === 0`), and the retry budget isn't spent. This is the
 *  guest who joined during the post-go-live window before MediaMTX remuxed the first H264
 *  keyframe into a video rendition — re-attaching re-reads the master, which lists the
 *  video rendition by then. Pure, so it is unit-testable. */
export function shouldRetryForVideo(
  videoWidth: number,
  retries: number,
  maxRetries: number,
): boolean {
  return videoWidth === 0 && retries < maxRetries;
}

/** Minimal shape of the hls.js instance we drive (subset, so tests can fake it). */
interface HlsLike {
  loadSource(url: string): void;
  attachMedia(video: HTMLVideoElement): void;
  destroy(): void;
  /** Restart segment loading (hls.js recovery for transient network errors). */
  startLoad(): void;
  /** Attempt in-place codec recovery (hls.js recovery for media decode errors). */
  recoverMediaError(): void;
  /** `data` is per-event, so handlers narrow it themselves (hlsError vs hlsManifestParsed). */
  on(event: string, cb: (event: string, data: unknown) => void): void;
}

/** The `hlsError` payload we act on. */
interface HlsErrorData {
  fatal: boolean;
  type?: string;
}

/** The `hlsManifestParsed` payload: one entry per rendition in the master playlist. */
interface HlsManifestParsedData {
  levels?: { videoCodec?: string }[];
}

/** Options for the player. `code` is the webinar's public code; the callbacks surface
 *  state + the tap-to-start requirement to the page UI. */
export interface HlsPlayerOptions {
  code: string;
  video: HTMLVideoElement;
  onState?: (state: PlayerState) => void;
  /** Called with `true` when autoplay was blocked and the UI must show a start button. */
  onTapToStart?: (needsTap: boolean) => void;
  /** Called once after the initial fetch with the raw webinar info (e.g. source language,
   *  tier) so the UI can adapt before the player enters its first state. */
  onInfo?: (info: PublicWebinar) => void;
  /** Injectable hls.js loader for tests. Defaults to a dynamic `import('hls.js')`. */
  loadHls?: () => Promise<{ Hls: HlsFactory }>;
  /** Injectable public-webinar fetch for tests. Defaults to `getPublicWebinar`. */
  fetchWebinar?: (code: string) => Promise<PublicWebinar>;
  /** Milliseconds to wait for the first decodable video frame before calling play().
   *  Defaults to 10 000. Pass 0 in tests to skip the wait entirely (no setTimeout). */
  firstFrameTimeoutMs?: number;
}

/** The bits of the hls.js default export we use: the static `isSupported()` guard and
 *  the constructor. */
export interface HlsFactory {
  isSupported(): boolean;
  new (config?: object): HlsLike;
}

/**
 * Drives HLS playback for a participant: format selection, autoplay policy, and the
 * waiting→live→ended state machine (polling the public endpoint to detect transitions).
 * Construct with the `<video>` element, then `start()`; `destroy()` stops everything.
 */
export class HlsPlayer {
  private code: string;
  private video: HTMLVideoElement;
  private onState: (s: PlayerState) => void;
  private onTapToStart: (needsTap: boolean) => void;
  private loadHls: () => Promise<{ Hls: HlsFactory }>;
  private fetchWebinar: (code: string) => Promise<PublicWebinar>;

  private onInfo: (info: PublicWebinar) => void;

  private hls: HlsLike | null = null;
  private state: PlayerState = "waiting";
  private playbackUrl: string | null = null;
  /** True once the manifest has been attached (native src set or hls.js loaded), so a
   *  later live poll doesn't attach it twice. */
  private attached = false;
  private pollTimer: ReturnType<typeof setInterval> | null = null;
  private destroyed = false;
  /** Counts re-attaches triggered by an audio-only stream (see MAX_VIDEO_RETRIES). */
  private videoRetries = 0;
  private readonly firstFrameTimeoutMs: number;
  /** Whether the attached master playlist advertises a video rendition. `null` until a
   *  manifest is parsed (and on Safari's native path, where we can't inspect it). */
  private manifestHasVideo: boolean | null = null;
  /** `currentTime` at the previous health check, to detect a frozen playhead. -1 = none yet. */
  private lastCurrentTime = -1;
  /** Consecutive health checks that saw no playback progress. */
  private stalledChecks = 0;

  constructor(opts: HlsPlayerOptions) {
    this.code = opts.code;
    this.video = opts.video;
    this.onState = opts.onState ?? (() => {});
    this.onTapToStart = opts.onTapToStart ?? (() => {});
    this.onInfo = opts.onInfo ?? (() => {});
    this.loadHls =
      opts.loadHls ??
      (() => import("hls.js").then((m) => ({ Hls: m.default as unknown as HlsFactory })));
    this.fetchWebinar = opts.fetchWebinar ?? getPublicWebinar;
    this.firstFrameTimeoutMs = opts.firstFrameTimeoutMs ?? 10_000;
  }

  getState(): PlayerState {
    return this.state;
  }

  private setState(s: PlayerState): void {
    if (this.state === s) return;
    this.state = s;
    this.onState(s);
  }

  /** Fetch the webinar, persist the guest id, then react to its status: attach + play
   *  when already live, otherwise start polling until it goes live. */
  async start(): Promise<void> {
    let info: PublicWebinar;
    try {
      info = await this.fetchWebinar(this.code);
    } catch {
      this.setState("error");
      return;
    }
    this.onInfo(info);
    persistGuestId(info.guest_id);
    this.playbackUrl = info.playback_url;
    await this.applyStatus(info);
    // Poll while waiting (detect the live transition) or while live (detect ended).
    if (this.state === "waiting" || this.state === "live") this.startPolling();
  }

  /** Move the player to the state implied by `info`, attaching the manifest the first
   *  time we see it live. */
  private async applyStatus(info: PublicWebinar): Promise<void> {
    if (info.playback_url) this.playbackUrl = info.playback_url;
    const next = stateFromStatus(info.status);
    if (next === "live") {
      if (!this.attached && this.playbackUrl) {
        const ok = await this.attach(this.playbackUrl);
        // No playable engine (neither native HLS nor MediaSource) — attach() already
        // set `error`; don't override it with `live`.
        if (!ok) return;
      }
      this.setState("live");
    } else if (next === "ended") {
      this.setState("ended");
      this.stopPolling();
    } else {
      this.setState("waiting");
    }
  }

  /** Attach the manifest to the `<video>` via the right engine, then try to autoplay.
   *  Returns whether a playable engine was found (false → unsupported, state set to
   *  `error` and the caller must not fall through to `live`). */
  private async attach(url: string): Promise<boolean> {
    if (this.attached) return true;
    const format = selectFormat(this.video, await this.hlsjsSupported());
    if (format === "native") {
      // MediaMTX gates HLS behind a session cookie (Set-Cookie on the first request),
      // so Safari's native player must send credentials or the playlist/segment
      // requests 401 and playback stalls. CORS returns a per-origin ACAO +
      // allow-credentials:true, so a credentialed request is valid.
      this.video.crossOrigin = "use-credentials";
      this.video.src = url;
      // Safari: when the stream dies (host webcam off), the video element fires an
      // error. Reset attached so the poll loop can reload the src when the host returns.
      this.video.addEventListener('error', () => {
        if (this.attached && this.state !== 'ended') {
          this.video.removeAttribute('src');
          this.attached = false;
          this.setState('waiting');
        }
      }, { once: true });
    } else if (format === "hlsjs") {
      const { Hls } = await this.loadHls();
      // MediaMTX serves LL-HLS behind a session cookie. Send credentials on every
      // request (xhrSetup) so the BLOCKING low-latency playlist reloads carry the
      // cookie — without it they 401, hls.js abandons LL-HLS, and end-to-end latency
      // degrades from ~1-2 s to full-segment (~6-7 s). CORS returns a per-origin
      // ACAO + allow-credentials:true, so credentialed requests are valid.
      const hls = new Hls({
        ...LIVE_TUNING,
        xhrSetup: (xhr: XMLHttpRequest) => {
          xhr.withCredentials = true;
        },
        // hls.js loads low-latency PARTS through fetch, not XHR — `xhrSetup` never runs
        // for them. Without credentials there, the blocking playlist reloads are
        // rejected, hls.js abandons LL-HLS, and latency degrades to full-segment
        // (~6-7 s) with the cookie-gated requests silently failing.
        fetchSetup: (context: { url: string }, initParams: RequestInit) =>
          new Request(context.url, { ...initParams, credentials: 'include' }),
      });
      this.hls = hls;
      // Learn whether the master playlist actually offers a video rendition. That tells a
      // genuinely mic-only broadcast (nothing to wait for) apart from a broken one where
      // video is advertised but never decodes — the two need opposite recovery policies.
      hls.on('hlsManifestParsed', (_evt: string, data: unknown) => {
        if (this.hls !== hls) return;
        const levels = (data as HlsManifestParsedData).levels ?? [];
        this.manifestHasVideo = levels.some((l) => !!l.videoCodec);
      });
      // hls.js error recovery (recommended pattern from the hls.js docs):
      //   - Network errors: call startLoad() to re-issue the manifest/segment
      //     request. This recovers in seconds when the host stream is just
      //     starting (manifest temporarily 404), without waiting the full 5 s
      //     poll interval. We allow MAX_NETWORK_RETRIES outer retries before
      //     falling back to a full destroy (hls.js also has its own internal
      //     retries before it fires the fatal event).
      //   - Media decode errors: try recoverMediaError() once; if it fires
      //     again, destroy.
      //   - Anything else: destroy and let the poll re-attach.
      const MAX_NETWORK_RETRIES = 3;
      let networkRetries = 0;
      let mediaErrorRecovered = false;
      hls.on('hlsError', (_evt: string, raw: unknown) => {
        const data = raw as HlsErrorData;
        if (!data.fatal || this.hls !== hls) return;
        if (data.type === 'networkError' && networkRetries < MAX_NETWORK_RETRIES) {
          networkRetries++;
          hls.startLoad();
        } else if (data.type === 'mediaError' && !mediaErrorRecovered) {
          mediaErrorRecovered = true;
          hls.recoverMediaError();
        } else {
          // Unrecoverable — tear down and let the poll re-attach on the next tick.
          hls.destroy();
          this.hls = null;
          this.attached = false;
          networkRetries = 0;
          mediaErrorRecovered = false;
          if (this.state !== 'ended') this.setState('waiting');
        }
      });
      hls.loadSource(url);
      hls.attachMedia(this.video);
    } else {
      // Neither native HLS nor MediaSource — nothing we can play here.
      this.setState("error");
      return false;
    }
    // Set attached early so a concurrent poll doesn't start a second attach() while
    // we wait for the first decodable frame below.
    this.attached = true;
    // Keep the waiting overlay visible until the video actually has a frame to show.
    // Without this, setState("live") removes the overlay while the video canvas is
    // still black — the classic "tap shows nothing" black-screen bug.
    await this.waitForFirstFrame();
    // A fatal error during the wait (native <video> error or hls.js destroy) resets
    // this.attached — abort so the caller does not override the waiting/error state.
    if (!this.attached || this.destroyed) return false;
    // If the 10-second timeout fired before canplay (stream not ready yet — e.g. the
    // host's HLS manifest isn't serving segments), readyState is still below
    // HAVE_CURRENT_DATA (2). Calling play() on an empty MediaSource succeeds silently in
    // Chrome, producing a black-silent canvas. Instead, tear down and let the poll retry
    // so the waiting overlay stays visible rather than showing a black screen.
    if (this.video.readyState < 2 /* HAVE_CURRENT_DATA */) {
      if (this.hls) {
        this.hls.destroy();
        this.hls = null;
      } else {
        try { this.video.removeAttribute('src'); } catch { /* best-effort */ }
      }
      this.attached = false;
      return false;
    }
    // Data is flowing but there's no video track yet (videoWidth 0) — the guest joined
    // during the audio-only window right after go-live, before MediaMTX remuxed the first
    // H264 keyframe into a video rendition. Tear down and let the poll re-attach (the
    // master lists the video rendition by then), which is what a manual refresh does.
    // Bounded by MAX_VIDEO_RETRIES so a genuinely mic-only broadcast plays its audio.
    if (shouldRetryForVideo(this.video.videoWidth, this.videoRetries, MAX_VIDEO_RETRIES)) {
      this.videoRetries++;
      if (this.hls) {
        this.hls.destroy();
        this.hls = null;
      } else {
        try { this.video.removeAttribute('src'); } catch { /* best-effort */ }
      }
      this.attached = false;
      return false;
    }
    // The first-frame wait above can have cost up to `firstFrameTimeoutMs` while the live
    // edge kept moving. Start AT the edge instead of wherever the buffer happens to begin,
    // or that startup cost becomes the guest's permanent latency.
    seekToLiveEdge(this.video);
    const { needsTap } = await tryAutoplay(this.video);
    this.onTapToStart(needsTap);
    return true;
  }

  /** Wait for the first decodable video frame, or give up after firstFrameTimeoutMs.
   *  Returns immediately (no setTimeout) when firstFrameTimeoutMs === 0 (test mode). */
  private waitForFirstFrame(): Promise<void> {
    if (this.firstFrameTimeoutMs === 0) return Promise.resolve();
    return new Promise<void>((resolve) => {
      const timer = setTimeout(resolve, this.firstFrameTimeoutMs);
      this.video.addEventListener(
        'canplay',
        () => {
          clearTimeout(timer);
          resolve();
        },
        { once: true },
      );
    });
  }

  /** Whether hls.js reports MediaSource support in this browser (loads the chunk to ask).
   *  Answering this costs one lazy chunk on iOS, where the answer is always false — worth
   *  it, because the previous fast path ("native HLS available ⇒ we never need hls.js")
   *  short-circuited to `false` on every Chrome guest and forced `selectFormat` down the
   *  native path regardless of what it preferred. */
  private async hlsjsSupported(): Promise<boolean> {
    try {
      const { Hls } = await this.loadHls();
      return Hls.isSupported();
    } catch {
      return false;
    }
  }

  /**
   * Mute or unmute the HLS audio (webinar Fase 2 audio controls). The `<video>` starts
   * muted so muted-autoplay is allowed; the participant opts into sound via the audio
   * buttons. Unmuting also (best-effort) resumes playback if the element is paused —
   * this runs inside the button's click handler, a user gesture the autoplay policy
   * honours, so the previously-blocked sound can start. Returns whether the element is
   * muted after the call.
   */
  muteAudio(muted: boolean): boolean {
    this.video.muted = muted;
    if (!muted && this.video.paused) {
      // Resume at the live edge, not where the pause left off — the buffered gap is
      // stale in a live webinar and would otherwise be carried for the rest of it.
      seekToLiveEdge(this.video);
      // A user gesture is driving this — try to (re)start playback with sound.
      void this.video.play?.().catch(() => {
        /* still blocked (rare) — the tap-to-start overlay remains the fallback */
      });
    }
    return this.video.muted;
  }

  /** Whether the HLS audio is currently muted. */
  isMuted(): boolean {
    return this.video.muted;
  }

  /** Seconds the guest is behind the live edge (0 when nothing is buffered). Exposed so
   *  the studio/QA can measure the real end-to-end delay instead of eyeballing it. */
  getLiveLatency(): number {
    return liveEdgeDrift(this.video);
  }

  /** User tapped the start overlay: unmute and/or play (a user gesture always allows
   *  playback). Returns whether playback started.
   *
   *  Handles two scenarios:
   *  1. Video is already playing muted (muted autoplay worked): just unmute.
   *  2. Video is paused (autoplay fully blocked): call play() under the user gesture.
   *     If play() rejects because hls.js has no data yet (host just went live),
   *     register a one-shot `canplay` listener so the video auto-starts when ready. */
  async userStart(): Promise<boolean> {
    this.video.muted = false;
    if (!this.video.paused) {
      // Already playing muted — unmuting alone is enough.
      this.onTapToStart(false);
      return true;
    }
    try {
      // The overlay may have sat there for a while — start at the live edge, not at the
      // frame that was buffered when the tap prompt appeared.
      seekToLiveEdge(this.video);
      await this.video.play();
      this.onTapToStart(false);
      return true;
    } catch {
      // play() failed — hls.js probably has no data yet (race between the host
      // going live and the first HLS part arriving).  Auto-retry on canplay so
      // the participant doesn't have to tap a second time.
      this.video.addEventListener(
        'canplay',
        () => {
          void this.video
            .play()
            .then(() => this.onTapToStart(false))
            .catch(() => {
              /* still blocked — tap button stays for another manual attempt */
            });
        },
        { once: true },
      );
      return false;
    }
  }

  private startPolling(): void {
    if (this.pollTimer != null) return;
    this.pollTimer = setInterval(() => void this.poll(), POLL_INTERVAL_MS);
  }

  private stopPolling(): void {
    if (this.pollTimer != null) {
      clearInterval(this.pollTimer);
      this.pollTimer = null;
    }
  }

  /** One poll: re-fetch the public webinar, apply any status change, then make sure what
   *  we are showing is actually alive. A transient fetch failure is ignored — we keep
   *  polling (and still run the health check, since a dead playlist is the more likely
   *  reason a poll failed mid-broadcast). */
  private async poll(): Promise<void> {
    if (this.destroyed) return;
    let info: PublicWebinar | null = null;
    try {
      info = await this.fetchWebinar(this.code);
    } catch {
      /* transient — try again on the next tick */
    }
    if (info) await this.applyStatus(info);
    if (!this.destroyed && this.state === 'live' && this.attached) {
      await this.checkPlaybackHealth();
    }
  }

  /**
   * Watchdog for a stream that is "live" but not actually playing. Two failures happen in
   * the field, neither of which raises a fatal hls.js error, so nothing else recovers them:
   *
   *  - **Frozen playhead.** The host reloads the studio page, so MediaMTX ends that publish
   *    session and starts a new muxer. The guest keeps polling a playlist that will never
   *    advance again and sits on a frozen frame until they refresh by hand.
   *  - **Advertised-but-absent video.** The master lists a video rendition yet no frame ever
   *    decodes (`videoWidth === 0`). The initial attach budget (MAX_VIDEO_RETRIES) gives up
   *    and leaves a black canvas labelled "live", permanently.
   *
   * A mic-only broadcast (`manifestHasVideo === false`) is NOT a fault: its audio plays and
   * we leave it alone rather than restarting the pipeline every few seconds. When the
   * manifest can't be inspected (Safari's native player) we only act on a frozen playhead.
   */
  private async checkPlaybackHealth(): Promise<void> {
    const t = this.video.currentTime ?? 0;
    const progressed = this.lastCurrentTime < 0 || t > this.lastCurrentTime;
    this.lastCurrentTime = t;
    // A viewer who deliberately paused is not stalled.
    const frozen = !progressed && !this.video.paused;
    this.stalledChecks = frozen ? this.stalledChecks + 1 : 0;

    const videoMissing = this.manifestHasVideo === true && this.video.videoWidth === 0;
    if (this.stalledChecks >= STALL_CHECKS || videoMissing) {
      await this.rebuildPlayback();
    }
  }

  /** Tear the playback pipeline down and immediately rebuild it from the manifest —
   *  what a manual page refresh does, minus the refresh. */
  private async rebuildPlayback(): Promise<void> {
    this.teardownPlayback();
    if (this.destroyed || !this.playbackUrl) return;
    // A rebuild is a fresh chance at the video rendition, so the audio-only budget resets
    // too — otherwise one bad startup window poisons the rest of the broadcast.
    this.videoRetries = 0;
    const ok = await this.attach(this.playbackUrl);
    if (this.destroyed) return;
    this.setState(ok ? 'live' : 'waiting');
  }

  /** Detach whichever engine is attached and reset the per-attachment health state. */
  private teardownPlayback(): void {
    if (this.hls) {
      this.hls.destroy();
      this.hls = null;
    } else {
      try {
        this.video.removeAttribute?.('src');
      } catch {
        /* detaching src is best-effort */
      }
    }
    this.attached = false;
    this.manifestHasVideo = null;
    this.lastCurrentTime = -1;
    this.stalledChecks = 0;
  }

  /** Stop polling, tear down hls.js, and detach the media. Idempotent. */
  destroy(): void {
    if (this.destroyed) return;
    this.destroyed = true;
    this.stopPolling();
    if (this.hls) {
      this.hls.destroy();
      this.hls = null;
    }
    try {
      this.video.removeAttribute?.("src");
    } catch {
      /* detaching src is best-effort */
    }
  }
}
