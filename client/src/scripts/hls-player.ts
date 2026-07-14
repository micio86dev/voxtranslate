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

/** Pick the playback engine for a `<video>` element: prefer NATIVE HLS when the browser
 *  can play `application/vnd.apple.mpegurl` (Safari/iOS), else hls.js if MediaSource is
 *  supported, else `unsupported`. Pure — takes the video + an hls.js-supported flag so
 *  it is unit-testable with fakes. */
export function selectFormat(
  video: Pick<HTMLVideoElement, "canPlayType">,
  hlsjsSupported: boolean,
): PlaybackFormat {
  if (video.canPlayType("application/vnd.apple.mpegurl")) return "native";
  if (hlsjsSupported) return "hlsjs";
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

/** Minimal shape of the hls.js instance we drive (subset, so tests can fake it). */
interface HlsLike {
  loadSource(url: string): void;
  attachMedia(video: HTMLVideoElement): void;
  destroy(): void;
  on(event: string, cb: (event: string, data: { fatal: boolean }) => void): void;
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
      const hls = new Hls({ lowLatencyMode: true });
      this.hls = hls;
      // When the HLS stream dies (host disables webcam / disconnects), hls.js fires a
      // fatal network error. Reset `attached` so the poll loop can re-attach as soon as
      // the host comes back — without this the participant must reload.
      hls.on('hlsError', (_evt: string, data: { fatal: boolean }) => {
        if (data.fatal && this.hls === hls) {
          hls.destroy();
          this.hls = null;
          this.attached = false;
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
    this.attached = true;
    const { needsTap } = await tryAutoplay(this.video);
    this.onTapToStart(needsTap);
    return true;
  }

  /** Whether hls.js reports MediaSource support in this browser (loads the chunk to
   *  ask). Safari answers `native` before this is ever called, so the chunk only loads
   *  where it is actually needed. */
  private async hlsjsSupported(): Promise<boolean> {
    // Fast path: if the video can play HLS natively we never need hls.js at all.
    if (this.video.canPlayType("application/vnd.apple.mpegurl")) return false;
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

  /** One poll: re-fetch the public webinar and apply any status change. A transient
   *  fetch failure is ignored — we keep polling. */
  private async poll(): Promise<void> {
    if (this.destroyed) return;
    let info: PublicWebinar;
    try {
      info = await this.fetchWebinar(this.code);
    } catch {
      return; // transient — try again on the next tick
    }
    await this.applyStatus(info);
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
