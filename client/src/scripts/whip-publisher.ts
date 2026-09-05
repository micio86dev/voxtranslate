// WHIP publisher (webinar phase 1, F1-4). A webinar host "goes live" by publishing
// their microphone (required) and optional webcam to the media server over WHIP —
// WebRTC-HTTP Ingestion Protocol (draft-ietf-wish-whip): a ONE-SHOT HTTP SDP
// exchange, NOT the WebSocket mesh signaling used for peer calls.
//
// Flow: getUserMedia → new RTCPeerConnection → addTrack(mic[, cam]) → createOffer →
// setLocalDescription → wait for ICE gathering → POST the offer SDP to the tokenized
// `publish_url` with `Content-Type: application/sdp` → read the 201 answer SDP →
// setRemoteDescription. The 201's `Location` header is the WHIP resource URL, which we
// DELETE to stop. The `?token=` is already in `publish_url`, so we POST verbatim and
// send NO auth headers (the media server is a different origin from the API).
//
// Lifecycle glue with the API: once the peer connection reaches `connected` we call
// `publishStarted(id)` (status → live); on stop / pagehide we DELETE the WHIP resource
// and call `publishStopped(id)` (status → ended). One automatic reconnect is attempted
// on a connection drop.
//
// The pure logic (URL/SDP handling, the state machine) is kept independent of a real
// RTCPeerConnection so it can be unit-tested with a fake ctor (like webrtc.test.ts).

import { createDenoiser, type Denoiser } from "./denoise";
import { loadNoisyEnv } from "./mic-constraints";
import {
  buildPublishConstraints,
  goLive,
  publishStarted,
  publishStopped,
} from "./webinar";

/** ICE servers for the WHIP ingest connection. STUN only — the media server acts as
 *  the far end, so a TURN relay is generally not needed for host→server ingest. */
const ICE_SERVERS: RTCIceServer[] = [
  { urls: "stun:stun.l.google.com:19302" },
  { urls: "stun:stun1.l.google.com:19302" },
];

/** How long to wait for full ICE gathering before publishing the offer anyway. WHIP
 *  wants a complete offer, but we cap the wait so a host on a slow network still
 *  publishes (any late candidates trickle via the connection itself). */
const ICE_GATHER_TIMEOUT_MS = 3_000;

/** The `RTCPeerConnection` constructor with the legacy webkit fallback, resolved
 *  lazily so a stripped/WebView environment can't crash the bundle on import. */
export function rtcPeerConnectionCtor(): typeof RTCPeerConnection | undefined {
  const g = globalThis as unknown as {
    RTCPeerConnection?: typeof RTCPeerConnection;
    webkitRTCPeerConnection?: typeof RTCPeerConnection;
  };
  return g.RTCPeerConnection ?? g.webkitRTCPeerConnection;
}

/** The publisher's externally-observable state, surfaced to the UI. */
export type WhipState =
  | "idle"
  | "connecting"
  | "on-air"
  | "reconnecting"
  | "mic-denied"
  | "error";

/** Reorder the video codec payload types in a WHIP SDP offer so H264 is preferred over
 *  VP8/VP9. MediaMTX's LL-HLS output requires H264 for video — Chrome negotiates VP8
 *  first by default, which MediaMTX cannot remux into HLS segments playable by all
 *  browsers, resulting in a black video track for participants. Pure, unit-testable. */
export function preferH264InSdp(sdp: string): string {
  return sdp.replace(/m=video \d+ [\w/]+ ([\d ]+)/, (mLine, ptList: string) => {
    const pts = ptList.trim().split(" ");
    // Collect H264 payload types from rtpmap lines.
    const h264PTs = new Set<string>();
    for (const m of sdp.matchAll(/a=rtpmap:(\d+) H264\//g)) h264PTs.add(m[1]);
    // Include each H264 entry's RTX companion (a=fmtp:PT apt=H264_PT).
    for (const h264PT of [...h264PTs]) {
      for (const m of sdp.matchAll(new RegExp(`a=fmtp:(\\d+) apt=${h264PT}\\b`, "g"))) {
        h264PTs.add(m[1]);
      }
    }
    const preferred = pts.filter((pt) => h264PTs.has(pt));
    const rest = pts.filter((pt) => !h264PTs.has(pt));
    return mLine.replace(ptList, [...preferred, ...rest].join(" "));
  });
}

/** POST the WHIP offer SDP to the tokenized ingest URL and return the 201 answer SDP
 *  plus the `Location` resource URL. NO auth headers — the media server is cross-origin
 *  and authorizes via the `?token=` already baked into `url`. Pure over `fetch`, so it
 *  is unit-testable with a mocked fetch. Throws on a non-2xx or a missing answer body. */
export async function whipPublish(
  url: string,
  offerSdp: string,
): Promise<{ answerSdp: string; resourceUrl: string | null }> {
  const res = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/sdp" },
    body: offerSdp,
  });
  if (!res.ok) {
    throw new Error(`WHIP publish failed (${res.status})`);
  }
  const answerSdp = await res.text();
  if (!answerSdp.trim()) {
    throw new Error("WHIP publish returned an empty answer");
  }
  // The Location header is the WHIP resource to DELETE on stop. It may be relative
  // to the ingest URL — resolve it against `url` so the DELETE hits the right origin.
  const location = res.headers.get("Location");
  const resourceUrl = location ? resolveResourceUrl(location, url) : null;
  return { answerSdp, resourceUrl };
}

/** Resolve a (possibly relative) WHIP resource `Location` against the ingest URL. Pure. */
export function resolveResourceUrl(location: string, base: string): string {
  try {
    return new URL(location, base).toString();
  } catch {
    return location; // already absolute or un-parseable — hand it back as-is
  }
}

/** DELETE the WHIP resource to tear down the publish server-side. Best-effort — a
 *  failure here still lets us mark the webinar ended locally. Pure over `fetch`. */
export async function whipDelete(resourceUrl: string): Promise<void> {
  try {
    await fetch(resourceUrl, { method: "DELETE" });
  } catch {
    /* the server also reaps the session on ICE timeout — best-effort delete */
  }
}

/** Wait until the peer connection finishes ICE gathering, or `timeoutMs` elapses
 *  (whichever comes first). Resolves immediately if gathering is already complete. */
export function waitForIceGathering(
  pc: RTCPeerConnection,
  timeoutMs = ICE_GATHER_TIMEOUT_MS,
): Promise<void> {
  if (pc.iceGatheringState === "complete") return Promise.resolve();
  return new Promise<void>((resolve) => {
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      pc.removeEventListener?.("icegatheringstatechange", check);
      resolve();
    };
    const check = () => {
      if (pc.iceGatheringState === "complete") finish();
    };
    pc.addEventListener?.("icegatheringstatechange", check);
    // Fallback for environments without addEventListener on the pc.
    setTimeout(finish, timeoutMs);
  });
}

/** Options for the publisher — the webinar id it publishes for, whether to start with
 *  the webcam on, the pre-live device choice, and a state callback the UI subscribes to. */
export interface WhipPublisherOptions {
  webinarId: string;
  /** Start capture with the webcam on. Default false (mic-only). */
  withCamera?: boolean;
  /** Microphone `deviceId` chosen in the pre-live step (from `enumerateDevices`).
   *  When set it is pinned with `deviceId: { exact }`; otherwise the browser default. */
  audioDeviceId?: string;
  /** Camera `deviceId` chosen in the pre-live step. Only used when `withCamera` is set
   *  (or when the camera is later toggled on); otherwise the default camera is used. */
  videoDeviceId?: string;
  /** Fired on every state transition so the UI can render connecting / on-air / etc. */
  onState?: (state: WhipState) => void;
  /** Injectable getUserMedia for tests; defaults to `navigator.mediaDevices`. */
  getUserMedia?: (c: MediaStreamConstraints) => Promise<MediaStream>;
}

/**
 * Drives one WHIP publish for a webinar: media capture, the SDP exchange, the API
 * lifecycle calls, a runtime webcam toggle, and one automatic reconnect. Construct,
 * then `start()`; `stop()` (or a `pagehide`) tears it down.
 */
export class WhipPublisher {
  private id: string;
  private wantCamera: boolean;
  private audioDeviceId?: string;
  /** Live RNNoise stage, when noisy-environment mode is on. */
  private denoiser: Denoiser | null = null;
  /** The raw device stream behind `denoiser` — the thing that actually holds the mic. */
  private rawStream: MediaStream | null = null;
  private videoDeviceId?: string;
  private onState: (s: WhipState) => void;
  private getMedia: (c: MediaStreamConstraints) => Promise<MediaStream>;

  private pc: RTCPeerConnection | null = null;
  private stream: MediaStream | null = null;
  private resourceUrl: string | null = null;
  private cameraTrack: MediaStreamTrack | null = null;
  private audioTrack: MediaStreamTrack | null = null;
  private videoSender: RTCRtpSender | null = null;
  /** Whether the CURRENT publish session offered a real video track. The media server
   *  fixes its HLS renditions from the tracks present when the session starts, so a
   *  camera swapped in later is invisible to viewers unless we republish. */
  private publishedWithVideo = false;

  private state: WhipState = "idle";
  /** Guards the single automatic reconnect so a flapping connection can't loop. */
  private reconnected = false;
  /** True once stop() ran, so late connection-state events don't resurrect us. */
  private stopped = false;
  /** Bound so we can add AND remove the same reference from `pagehide`. */
  private readonly onPageHide = () => void this.stop();

  constructor(opts: WhipPublisherOptions) {
    this.id = opts.webinarId;
    this.wantCamera = !!opts.withCamera;
    this.audioDeviceId = opts.audioDeviceId;
    this.videoDeviceId = opts.videoDeviceId;
    this.onState = opts.onState ?? (() => {});
    this.getMedia =
      opts.getUserMedia ??
      ((c) => navigator.mediaDevices.getUserMedia(c));
  }

  /** The current state (also pushed via the onState callback). */
  getState(): WhipState {
    return this.state;
  }

  /** Whether the webcam is currently capturing + being sent. */
  isCameraOn(): boolean {
    return !!this.cameraTrack && this.cameraTrack.readyState === "live";
  }

  /** Whether the microphone is currently live (not muted). A muted mic keeps the track
   *  (so the WHIP sender + the STT MediaRecorder stay wired) but disables it, yielding
   *  silence. False before `start()` / when there is no captured audio track. */
  isMicrophoneOn(): boolean {
    return !!this.audioTrack && this.audioTrack.enabled;
  }

  /** The captured local media stream (mic [+ camera]) so the studio view can show the
   *  host's own preview. Null before `start()` / after `stop()`. */
  getLocalStream(): MediaStream | null {
    return this.stream;
  }

  private setState(s: WhipState): void {
    this.state = s;
    this.onState(s);
  }

  /**
   * Go live: capture media, fetch a fresh WHIP URL, run the SDP exchange, and — once
   * connected — flag the webinar live. Surfaces `mic-denied` if the microphone
   * permission is refused, and `error` for any other failure.
   */
  async start(): Promise<void> {
    this.stopped = false;
    this.reconnected = false;
    this.setState("connecting");

    // Microphone is REQUIRED; the webcam is optional and requested up front only when
    // the host chose to start with it on (it can be toggled at runtime afterward). The
    // pre-live device choice is honored via deviceId: { exact } (buildPublishConstraints).
    try {
      this.stream = await this.getMedia(
        buildPublishConstraints({
          audioDeviceId: this.audioDeviceId,
          videoDeviceId: this.videoDeviceId,
          withCamera: this.wantCamera,
        }),
      );
    } catch {
      // getUserMedia rejects on a denied permission (or no device) — surface it
      // distinctly so the UI can prompt the host to allow the mic.
      this.setState("mic-denied");
      throw new Error("microphone permission denied");
    }
    // Noisy-environment mode: `buildPublishConstraints` has already turned the browser
    // filter OFF, so RNNoise has to take its place HERE — otherwise the host would go on
    // air with no filtering at all, which is worse than either option alone. The raw
    // device stream is kept separately: the denoised track comes out of an AudioContext,
    // and stopping that would leave the microphone open.
    if (loadNoisyEnv()) {
      this.rawStream = this.stream;
      this.denoiser = await createDenoiser(this.stream);
      this.stream = this.denoiser.stream;
    }
    this.cameraTrack = this.stream.getVideoTracks()[0] ?? null;
    // Keep a handle on the captured mic track so it can be muted at runtime. The SAME
    // track object is ALSO wrapped by the STT bridge's MediaRecorder (audio-capture),
    // so toggling its `.enabled` silences BOTH the WHIP broadcast and the STT ingest.
    this.audioTrack = this.stream.getAudioTracks()[0] ?? null;

    try {
      await this.connect(false);
    } catch (err) {
      this.setState("error");
      this.teardownMedia();
      throw err;
    }

    // Tear the publish down cleanly if the tab is closed / backgrounded mid-stream.
    addEventListener?.("pagehide", this.onPageHide);
  }

  /** Build the peer connection, run the WHIP exchange, and wire the lifecycle. When
   *  `isRestart` is true this is the single automatic reconnect (no re-capture). */
  private async connect(isRestart: boolean): Promise<void> {
    const Ctor = rtcPeerConnectionCtor();
    if (!Ctor) throw new Error("WebRTC is not supported in this browser");

    // Fetch a FRESH token right before publishing — it is short-lived (~120s).
    const { publish_url } = await goLive(this.id);

    const pc = new Ctor({ iceServers: ICE_SERVERS });
    this.pc = pc;
    this.videoSender = null;

    for (const track of this.stream!.getTracks()) {
      const sender = pc.addTrack(track, this.stream!);
      if (track.kind === "video") this.videoSender = sender;
    }
    this.publishedWithVideo = !!this.videoSender;
    // Guarantee a video m-line even when starting mic-only, so turning the camera on
    // later is a replaceTrack (no renegotiation — WHIP has no re-offer channel). NOTE:
    // an empty m-line is enough for WebRTC but NOT for the server's HLS output, which
    // fixes its renditions per publish session — see toggleCamera().
    if (!this.videoSender) {
      const tx = pc.addTransceiver?.("video", { direction: "sendonly" });
      this.videoSender = tx?.sender ?? null;
    }

    pc.onconnectionstatechange = () => this.onConnState();

    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    await waitForIceGathering(pc);

    const offerSdp = preferH264InSdp(pc.localDescription?.sdp ?? offer.sdp ?? "");
    const { answerSdp, resourceUrl } = await whipPublish(publish_url, offerSdp);
    this.resourceUrl = resourceUrl;
    await pc.setRemoteDescription({ type: "answer", sdp: answerSdp });

    // On the first publish we optimistically show "connecting" until `connected`
    // fires; a restart keeps the UI in "reconnecting" until it recovers.
    if (!isRestart) this.setState("connecting");
  }

  /** React to the peer connection's aggregate state: mark live on `connected`, and
   *  attempt one reconnect on `failed`/`disconnected`. */
  private onConnState(): void {
    if (this.stopped || !this.pc) return;
    const st = this.pc.connectionState;
    if (st === "connected") {
      this.reconnected = false; // a clean connection resets the reconnect budget
      this.setState("on-air");
      void publishStarted(this.id).catch(() => {
        /* the stream is up; a failed status ping is non-fatal to the broadcast */
      });
    } else if (st === "failed" || st === "disconnected") {
      void this.tryReconnect();
    }
  }

  /** Start a NEW publish session with the media captured right now — used when the track
   *  set changed in a way the server only honours at session start (see toggleCamera).
   *  Deliberate, so it does NOT spend the automatic-reconnect budget; a failure surfaces
   *  as `error` exactly like a failed reconnect. */
  private async republish(): Promise<void> {
    if (this.stopped) return;
    if (this.resourceUrl) void whipDelete(this.resourceUrl);
    this.resourceUrl = null;
    this.closePc();
    try {
      await this.connect(true);
    } catch {
      this.setState("error");
    }
  }

  /** One automatic reconnect: DELETE the dead resource, tear the pc down, and
   *  re-run the WHIP exchange with the same captured media. Falls to `error` if the
   *  reconnect itself fails or the budget is already spent. */
  private async tryReconnect(): Promise<void> {
    if (this.stopped || this.reconnected) {
      if (!this.stopped) this.setState("error");
      return;
    }
    this.reconnected = true;
    this.setState("reconnecting");
    // Drop the old server-side resource + pc before re-publishing.
    if (this.resourceUrl) void whipDelete(this.resourceUrl);
    this.resourceUrl = null;
    this.closePc();
    try {
      await this.connect(true);
    } catch {
      this.setState("error");
    }
  }

  /**
   * Toggle the webcam at runtime. Turning it on captures a camera track and swaps it onto
   * the existing video sender (replaceTrack — no renegotiation, mirroring webrtc.ts);
   * turning it off stops the track and clears the sender. Returns the new on/off state.
   * No-op (returns false) before `start()`.
   *
   * Exception — the FIRST camera-on after a mic-only go-live triggers a full republish.
   * replaceTrack is enough for WebRTC, but the media server decides which renditions the
   * HLS master playlist carries when the publish session starts: a video track that
   * arrives later is never remuxed, so every guest sees a black screen for the rest of
   * the broadcast (refreshing doesn't help — only a new publish session does). Once the
   * session carries video, later toggles are plain swaps again.
   */
  async toggleCamera(on: boolean): Promise<boolean> {
    if (!this.pc || !this.stream) return false;
    if (on) {
      if (this.isCameraOn()) return true;
      let camStream: MediaStream;
      try {
        // Reuse the pre-live camera choice when toggling on at runtime.
        const constraints = buildPublishConstraints({
          videoDeviceId: this.videoDeviceId,
          withCamera: true,
        });
        camStream = await this.getMedia({ video: constraints.video });
      } catch {
        return false; // camera denied / unavailable — stay audio-only
      }
      const track = camStream.getVideoTracks()[0] ?? null;
      if (!track) return false;
      this.cameraTrack = track;
      this.stream.addTrack(track);
      if (this.publishedWithVideo) {
        await this.videoSender?.replaceTrack(track);
      } else {
        await this.republish();
      }
      return true;
    }
    // Turning off: stop capture and clear the outgoing video track.
    if (this.cameraTrack) {
      this.cameraTrack.stop();
      this.stream.removeTrack(this.cameraTrack);
      this.cameraTrack = null;
    }
    await this.videoSender?.replaceTrack(null);
    return false;
  }

  /**
   * Mute / unmute the microphone at runtime by toggling the captured audio track's
   * `.enabled`. The track object is shared with the STT bridge's MediaRecorder, so a
   * disabled track yields silence for BOTH the WHIP broadcast and the server STT ingest
   * (no viewer audio, no subtitles) with a single call. Returns the new on/off state.
   * No-op (returns false) before `start()` / when there is no audio track.
   */
  toggleMicrophone(on: boolean): boolean {
    if (!this.audioTrack) return false;
    this.audioTrack.enabled = on;
    return on;
  }

  /**
   * Stop publishing: DELETE the WHIP resource, close the peer connection, stop all
   * capture, tell the server the webinar ended, and reset to idle. Idempotent.
   */
  async stop(): Promise<void> {
    if (this.stopped) return;
    this.stopped = true;
    removeEventListener?.("pagehide", this.onPageHide);

    if (this.resourceUrl) await whipDelete(this.resourceUrl);
    this.resourceUrl = null;
    this.closePc();
    this.teardownMedia();

    void publishStopped(this.id).catch(() => {
      /* the server also ends the webinar on ingest timeout — best-effort ping */
    });
    this.setState("idle");
  }

  /** Close + drop the peer connection without touching media or server state. */
  private closePc(): void {
    if (this.pc) {
      this.pc.onconnectionstatechange = null;
      this.pc.close();
      this.pc = null;
    }
    this.videoSender = null;
  }

  /** Stop + drop every captured track. */
  private teardownMedia(): void {
    this.stream?.getTracks().forEach((t) => t.stop());
    // Stopping the denoised tracks does not release the device — that lives on the raw
    // stream, and its AudioContext has to be closed explicitly.
    this.rawStream?.getTracks().forEach((t) => t.stop());
    this.rawStream = null;
    void this.denoiser?.stop();
    this.denoiser = null;
    this.stream = null;
    this.cameraTrack = null;
    this.audioTrack = null;
  }
}
