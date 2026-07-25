// HLS participant player (webinar phase 1, F1-5). Mocks the public-webinar fetch, a
// fake hls.js, and a fake <video> so format selection, the waiting→live→ended state
// machine, autoplay-blocked handling, and guest_id persistence are covered without a
// real MediaSource or network.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// The player only uses `getPublicWebinar` via an injectable option, so stub the module
// to avoid pulling in ./auth (which reads `location` at import in the node environment).
vi.mock('./webinar', () => ({ getPublicWebinar: vi.fn() }));

import {
  HlsPlayer,
  GUEST_ID_KEY,
  persistGuestId,
  getStoredGuestId,
  selectFormat,
  shouldRetryForVideo,
  stateFromStatus,
  tryAutoplay,
  type PlayerState,
} from './hls-player';
import type { PublicWebinar } from './webinar';

const webinar = (over: Partial<PublicWebinar> = {}): PublicWebinar => ({
  code: 'ab12cd',
  title: 'Launch',
  status: 'scheduled',
  source_language: 'en',
  tier: 'enhanced',
  join_url: 'https://voxtranslate.app/w/ab12cd',
  playback_url: 'https://hls.example/webinar/ab12cd/index.m3u8',
  guest_id: 'guest-1',
  host_avatar_url: null,
  ...over,
});

// A fake <video> with just what the player touches.
function fakeVideo(opts: { canNative?: boolean } = {}) {
  const v: any = {
    src: '',
    muted: false,
    canPlayType: vi.fn((t: string) =>
      opts.canNative && t === 'application/vnd.apple.mpegurl' ? 'maybe' : '',
    ),
    play: vi.fn(async () => {}),
    removeAttribute: vi.fn(() => {
      v.src = '';
    }),
    addEventListener: vi.fn(), // native error recovery + canplay retry
  };
  return v as HTMLVideoElement & { play: any; canPlayType: any; addEventListener: any };
}

// A fake hls.js default export.
function fakeHls(supported = true) {
  const instances: any[] = [];
  const Hls: any = function (cfg: any) {
    const inst = {
      /** The config the player passed in — credentials wiring is asserted on it. */
      cfg,
      loadSource: vi.fn(),
      attachMedia: vi.fn(),
      destroy: vi.fn(),
      startLoad: vi.fn(),
      recoverMediaError: vi.fn(),
      on: vi.fn(), // hlsError recovery + manifest inspection handlers
    };
    instances.push(inst);
    return inst;
  };
  Hls.isSupported = () => supported;
  return { Hls, instances };
}

/** Fire the hlsError handler that was registered via inst.on('hlsError', cb). */
function fireHlsError(inst: any, type: string, fatal: boolean): void {
  const handler = inst.on.mock.calls.find((c: any[]) => c[0] === 'hlsError')?.[1];
  handler?.('hlsError', { fatal, type });
}

/** Fire any hls.js event the player subscribed to, with an arbitrary payload. */
function fireHlsEvent(inst: any, event: string, data: unknown): void {
  const handler = inst.on.mock.calls.find((c: any[]) => c[0] === event)?.[1];
  handler?.(event, data);
}

beforeEach(() => {
  // A fresh fake localStorage per test so guest_id persistence starts clean (the
  // module's own in-memory fallback would leak across tests).
  const map = new Map<string, string>();
  vi.stubGlobal('localStorage', {
    getItem: (k: string) => (map.has(k) ? map.get(k)! : null),
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
  });
  vi.useRealTimers();
});
afterEach(() => {
  vi.unstubAllGlobals();
});

describe('selectFormat', () => {
  it('prefers native HLS on Safari (canPlayType hls)', () => {
    expect(selectFormat({ canPlayType: () => 'maybe' } as any, true)).toBe('native');
  });
  it('uses hls.js when native HLS is unavailable but MediaSource is supported', () => {
    expect(selectFormat({ canPlayType: () => '' } as any, true)).toBe('hlsjs');
  });
  it('is unsupported when neither works', () => {
    expect(selectFormat({ canPlayType: () => '' } as any, false)).toBe('unsupported');
  });
});

describe('stateFromStatus', () => {
  it('maps webinar status to player state', () => {
    expect(stateFromStatus('scheduled')).toBe('waiting');
    expect(stateFromStatus('live')).toBe('live');
    expect(stateFromStatus('ended')).toBe('ended');
    expect(stateFromStatus('cancelled')).toBe('ended');
  });
});

describe('persistGuestId', () => {
  it('stores the guest id, first write wins', () => {
    expect(getStoredGuestId()).toBeNull();
    expect(persistGuestId('g1')).toBe('g1');
    expect(getStoredGuestId()).toBe('g1');
    // A later, different id does NOT overwrite the first identity.
    expect(persistGuestId('g2')).toBe('g1');
    expect(getStoredGuestId()).toBe('g1');
    expect(GUEST_ID_KEY).toBe('vox.guest_id');
  });
});

describe('tryAutoplay', () => {
  it('plays with sound when allowed', async () => {
    const v = { muted: false, play: vi.fn(async () => {}) };
    expect(await tryAutoplay(v)).toEqual({ playing: true, needsTap: false });
    expect(v.muted).toBe(false);
  });
  it('retries muted when sound autoplay is blocked and signals needsTap', async () => {
    const play = vi.fn().mockRejectedValueOnce(new Error('blocked')).mockResolvedValueOnce(undefined);
    const v = { muted: false, play };
    // Muted fallback succeeds but the overlay must appear so the user can unmute.
    expect(await tryAutoplay(v)).toEqual({ playing: true, needsTap: true });
    expect(v.muted).toBe(true);
  });
  it('needs a tap when even muted autoplay is blocked', async () => {
    const v = { muted: false, play: vi.fn().mockRejectedValue(new Error('blocked')) };
    expect(await tryAutoplay(v)).toEqual({ playing: false, needsTap: true });
  });
});

describe('shouldRetryForVideo', () => {
  it('retries only while audio-only and the budget remains', () => {
    expect(shouldRetryForVideo(0, 0, 3)).toBe(true); // audio-only, budget left → re-attach
    expect(shouldRetryForVideo(640, 0, 3)).toBe(false); // video present → stop retrying
    expect(shouldRetryForVideo(0, 3, 3)).toBe(false); // budget spent → accept audio-only
  });
});

describe('HlsPlayer state machine', () => {
  it('starts waiting, then polls to live and attaches via hls.js', async () => {
    vi.useFakeTimers();
    const { Hls, instances } = fakeHls(true);
    const video = fakeVideo({ canNative: false });
    const states: PlayerState[] = [];
    const fetchWebinar = vi
      .fn()
      .mockResolvedValueOnce(webinar({ status: 'scheduled' }))
      .mockResolvedValueOnce(webinar({ status: 'live' }));
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      onState: (s) => states.push(s),
      loadHls: async () => ({ Hls }),
      fetchWebinar,
      firstFrameTimeoutMs: 0,
    });

    await p.start();
    expect(p.getState()).toBe('waiting');
    expect(getStoredGuestId()).toBe('guest-1'); // guest id persisted from the API

    // Advance the poll interval → sees "live" → attaches + plays via hls.js.
    await vi.advanceTimersByTimeAsync(5_000);
    await Promise.resolve();
    expect(p.getState()).toBe('live');
    expect(instances[0].loadSource).toHaveBeenCalledWith('https://hls.example/webinar/ab12cd/index.m3u8');
    expect(instances[0].attachMedia).toHaveBeenCalledWith(video);

    // A later poll flips it to ended → polling stops, hls torn down on destroy.
    fetchWebinar.mockResolvedValue(webinar({ status: 'ended' }));
    await vi.advanceTimersByTimeAsync(5_000);
    await Promise.resolve();
    expect(p.getState()).toBe('ended');
    p.destroy();
    expect(instances[0].destroy).toHaveBeenCalled();
    vi.useRealTimers();
  });

  it('re-attaches when a guest joins during the audio-only startup window', async () => {
    vi.useFakeTimers();
    const { Hls, instances } = fakeHls(true);
    const video = fakeVideo({ canNative: false }) as any;
    video.videoWidth = 0; // data flows but MediaMTX hasn't added the H264 rendition yet
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      loadHls: async () => ({ Hls }),
      fetchWebinar: vi.fn().mockResolvedValue(webinar({ status: 'live' })),
      firstFrameTimeoutMs: 0,
    });

    await p.start();
    // First attach saw audio-only → tore down; the guest stays on the waiting overlay
    // instead of being shown a live-but-black canvas.
    expect(p.getState()).toBe('waiting');
    expect(instances[0].destroy).toHaveBeenCalled();

    // The host's video rendition appears; the next poll re-attaches and goes live.
    video.videoWidth = 640;
    await vi.advanceTimersByTimeAsync(5_000);
    await Promise.resolve();
    expect(p.getState()).toBe('live');
    expect(instances[1].attachMedia).toHaveBeenCalledWith(video);
    p.destroy();
    vi.useRealTimers();
  });

  it('stops retrying and accepts a mic-only (audio-only) broadcast after the budget', async () => {
    vi.useFakeTimers();
    const { Hls } = fakeHls(true);
    const video = fakeVideo({ canNative: false }) as any;
    video.videoWidth = 0; // stays 0 forever — the host never turns the camera on
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      loadHls: async () => ({ Hls }),
      fetchWebinar: vi.fn().mockResolvedValue(webinar({ status: 'live' })),
      firstFrameTimeoutMs: 0,
    });

    await p.start(); // attempt 1 → still audio-only, retry
    // Three polls exhaust MAX_VIDEO_RETRIES; the stream is then accepted as-is → live.
    await vi.advanceTimersByTimeAsync(5_000);
    await Promise.resolve();
    await vi.advanceTimersByTimeAsync(5_000);
    await Promise.resolve();
    await vi.advanceTimersByTimeAsync(5_000);
    await Promise.resolve();
    expect(p.getState()).toBe('live');
    p.destroy();
    vi.useRealTimers();
  });

  it('attaches natively on Safari (no hls.js chunk loaded)', async () => {
    const video = fakeVideo({ canNative: true });
    const loadHls = vi.fn();
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      loadHls,
      fetchWebinar: vi.fn().mockResolvedValue(webinar({ status: 'live' })),
      firstFrameTimeoutMs: 0,
    });
    await p.start();
    expect(p.getState()).toBe('live');
    expect(video.src).toBe('https://hls.example/webinar/ab12cd/index.m3u8');
    expect(loadHls).not.toHaveBeenCalled(); // native path never loads the chunk
  });

  it('reports tap-to-start when autoplay is fully blocked', async () => {
    const video = fakeVideo({ canNative: true }) as any;
    video.paused = true; // autoplay fully blocked → video never started playing
    (video.play as any).mockRejectedValue(new Error('blocked')); // sound + muted both blocked
    let needsTap = false;
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      onTapToStart: (n) => (needsTap = n),
      fetchWebinar: vi.fn().mockResolvedValue(webinar({ status: 'live' })),
      firstFrameTimeoutMs: 0,
    });
    await p.start();
    expect(needsTap).toBe(true);

    // userStart() unmutes + plays under the tap gesture.
    (video.play as any).mockResolvedValue(undefined);
    expect(await p.userStart()).toBe(true);
    expect(video.muted).toBe(false);
    expect(needsTap).toBe(false);
  });

  it('userStart just unmutes when video is already playing muted', async () => {
    const video = fakeVideo({ canNative: true }) as any;
    video.paused = false; // video already playing (muted autoplay worked)
    const play = vi.fn().mockResolvedValue(undefined);
    video.play = play;
    let needsTap = true;
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      onTapToStart: (n) => (needsTap = n),
      fetchWebinar: vi.fn().mockResolvedValue(webinar({ status: 'live' })),
      firstFrameTimeoutMs: 0,
    });
    expect(await p.userStart()).toBe(true);
    expect(video.muted).toBe(false);
    expect(needsTap).toBe(false);
    expect(play).not.toHaveBeenCalled(); // no play() call needed — already playing
  });

  it('muteAudio toggles the video mute flag and resumes on unmute', () => {
    const video = fakeVideo({ canNative: true }) as any;
    video.paused = true; // element is paused (autoplay was muted → user opts into sound)
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      fetchWebinar: vi.fn().mockResolvedValue(webinar({ status: 'live' })),
      firstFrameTimeoutMs: 0,
    });
    // Unmute: clears the flag AND (since paused) tries to (re)start playback.
    expect(p.muteAudio(false)).toBe(false);
    expect(video.muted).toBe(false);
    expect(p.isMuted()).toBe(false);
    expect(video.play).toHaveBeenCalled();
    // Mute again: sets the flag, never calls play().
    video.play.mockClear();
    expect(p.muteAudio(true)).toBe(true);
    expect(video.muted).toBe(true);
    expect(video.play).not.toHaveBeenCalled();
  });

  it('muteAudio does not call play() when the video is already playing', () => {
    const video = fakeVideo({ canNative: true }) as any;
    video.paused = false; // already playing
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      fetchWebinar: vi.fn().mockResolvedValue(webinar({ status: 'live' })),
      firstFrameTimeoutMs: 0,
    });
    p.muteAudio(false);
    expect(video.play).not.toHaveBeenCalled();
  });

  it('goes to error when the initial fetch fails', async () => {
    const video = fakeVideo();
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      fetchWebinar: vi.fn().mockRejectedValue(new Error('boom')),
      firstFrameTimeoutMs: 0,
    });
    await p.start();
    expect(p.getState()).toBe('error');
  });

  it('goes to error when neither playback engine is supported', async () => {
    const { Hls } = fakeHls(false); // hls.js reports NOT supported
    const video = fakeVideo({ canNative: false }); // and no native HLS
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      loadHls: async () => ({ Hls }),
      fetchWebinar: vi.fn().mockResolvedValue(webinar({ status: 'live' })),
      firstFrameTimeoutMs: 0,
    });
    await p.start();
    expect(p.getState()).toBe('error');
  });

  it('recovers a fatal network error with startLoad() instead of destroying', async () => {
    const { Hls, instances } = fakeHls(true);
    const video = fakeVideo({ canNative: false });
    const states: PlayerState[] = [];
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      onState: (s) => states.push(s),
      loadHls: async () => ({ Hls }),
      fetchWebinar: vi.fn().mockResolvedValue(webinar({ status: 'live' })),
      firstFrameTimeoutMs: 0,
    });
    await p.start();
    expect(p.getState()).toBe('live');

    // First fatal network error → startLoad(), NOT destroy
    fireHlsError(instances[0], 'networkError', true);
    expect(instances[0].startLoad).toHaveBeenCalledTimes(1);
    expect(instances[0].destroy).not.toHaveBeenCalled();
    expect(p.getState()).toBe('live'); // state unchanged

    // Second and third → still startLoad()
    fireHlsError(instances[0], 'networkError', true);
    fireHlsError(instances[0], 'networkError', true);
    expect(instances[0].startLoad).toHaveBeenCalledTimes(3);
    expect(instances[0].destroy).not.toHaveBeenCalled();
    expect(p.getState()).toBe('live');
  });

  it('destroys hls.js and resets to waiting after MAX_NETWORK_RETRIES are exhausted', async () => {
    const { Hls, instances } = fakeHls(true);
    const video = fakeVideo({ canNative: false });
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      loadHls: async () => ({ Hls }),
      fetchWebinar: vi.fn().mockResolvedValue(webinar({ status: 'live' })),
      firstFrameTimeoutMs: 0,
    });
    await p.start();

    // Exhaust the 3 network retries
    fireHlsError(instances[0], 'networkError', true);
    fireHlsError(instances[0], 'networkError', true);
    fireHlsError(instances[0], 'networkError', true);
    expect(instances[0].startLoad).toHaveBeenCalledTimes(3);
    expect(instances[0].destroy).not.toHaveBeenCalled();

    // 4th fatal error exceeds the limit → destroy + state reset
    fireHlsError(instances[0], 'networkError', true);
    expect(instances[0].destroy).toHaveBeenCalledTimes(1);
    expect(p.getState()).toBe('waiting');
  });

  it('recovers a fatal media error with recoverMediaError() on the first attempt', async () => {
    const { Hls, instances } = fakeHls(true);
    const video = fakeVideo({ canNative: false });
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      loadHls: async () => ({ Hls }),
      fetchWebinar: vi.fn().mockResolvedValue(webinar({ status: 'live' })),
      firstFrameTimeoutMs: 0,
    });
    await p.start();

    // First media error → recoverMediaError(), not destroy
    fireHlsError(instances[0], 'mediaError', true);
    expect(instances[0].recoverMediaError).toHaveBeenCalledTimes(1);
    expect(instances[0].destroy).not.toHaveBeenCalled();
    expect(p.getState()).toBe('live');

    // Second media error → unrecoverable → destroy
    fireHlsError(instances[0], 'mediaError', true);
    expect(instances[0].destroy).toHaveBeenCalledTimes(1);
    expect(p.getState()).toBe('waiting');
  });

  it('ignores a transient poll failure and keeps waiting', async () => {
    vi.useFakeTimers();
    const video = fakeVideo({ canNative: true });
    const fetchWebinar = vi
      .fn()
      .mockResolvedValueOnce(webinar({ status: 'scheduled' }))
      .mockRejectedValueOnce(new Error('flaky'))
      .mockResolvedValueOnce(webinar({ status: 'live' }));
    const p = new HlsPlayer({ code: 'ab12cd', video, fetchWebinar, firstFrameTimeoutMs: 0 });
    await p.start();
    await vi.advanceTimersByTimeAsync(5_000); // poll rejects → still waiting
    await Promise.resolve();
    expect(p.getState()).toBe('waiting');
    await vi.advanceTimersByTimeAsync(5_000); // next poll → live
    await Promise.resolve();
    expect(p.getState()).toBe('live');
    p.destroy();
    vi.useRealTimers();
  });

  it('stays waiting and tears down hls.js when firstFrame timeout fires with no data', async () => {
    // Regression: Chrome resolves play() on an empty MediaSource silently — without
    // this guard the player would call setState('live'), hide the overlay, and show
    // a permanently black-silent canvas (case C black screen).
    vi.useFakeTimers();
    const { Hls, instances } = fakeHls(true);
    const video = fakeVideo({ canNative: false }) as any;
    video.readyState = 0; // HAVE_NOTHING — canplay never fired before the timeout
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      loadHls: async () => ({ Hls }),
      fetchWebinar: vi.fn().mockResolvedValue(webinar({ status: 'live' })),
      firstFrameTimeoutMs: 50,
    });

    const startP = p.start();
    await vi.advanceTimersByTimeAsync(200); // past the 50 ms firstFrame timeout
    await startP;

    expect(p.getState()).toBe('waiting'); // overlay must stay up, not go live
    expect(instances[0]?.destroy).toHaveBeenCalled(); // stale hls.js torn down
    p.destroy();
    vi.useRealTimers();
  });
});

// ---- live-stream recovery (webinar black screen / freeze) --------------------
//
// Three field failures this pins against, all reported together:
//   1. The host reloads the studio page → MediaMTX starts a NEW publish session and
//      a new HLS muxer. hls.js sees no *fatal* error, so nothing recovers: the guest
//      stares at a frozen frame until they manually refresh.
//   2. The manifest advertises a video rendition but no frames ever decode
//      (videoWidth stays 0) — the guest gets a black canvas labelled "live". The
//      initial 3-strike budget gives up and leaves it black forever.
//   3. LL-HLS latency degraded to full-segment (~6 s) because the credentialed
//      playlist reloads were rejected — hls.js uses fetch (not XHR) for low-latency
//      part loading, so wiring credentials into `xhrSetup` alone is not enough.
describe('HlsPlayer live recovery', () => {
  /** Drive the player to `live` with a decodable video track. */
  async function livePlayer(over: { videoWidth?: number } = {}) {
    const { Hls, instances } = fakeHls(true);
    const video = fakeVideo({ canNative: false }) as any;
    video.videoWidth = over.videoWidth ?? 640;
    video.currentTime = 10;
    video.paused = false;
    const p = new HlsPlayer({
      code: 'ab12cd',
      video,
      loadHls: async () => ({ Hls }),
      fetchWebinar: vi.fn().mockResolvedValue(webinar({ status: 'live' })),
      firstFrameTimeoutMs: 0,
    });
    await p.start();
    return { p, video, instances };
  }

  it('sends credentials on the fetch loader, not just XHR (LL-HLS latency)', async () => {
    vi.useFakeTimers();
    const { p, instances } = await livePlayer();
    const cfg = instances[0].cfg;

    // XHR path (kept for the non-low-latency loader).
    const xhr: any = {};
    cfg.xhrSetup(xhr);
    expect(xhr.withCredentials).toBe(true);

    // Fetch path — the one LL-HLS part loading actually uses.
    expect(typeof cfg.fetchSetup).toBe('function');
    const req = cfg.fetchSetup({ url: 'https://hls.example/index.m3u8' }, { method: 'GET' });
    expect(req.credentials).toBe('include');

    p.destroy();
    vi.useRealTimers();
  });

  it('re-attaches when playback freezes (the host republished)', async () => {
    vi.useFakeTimers();
    const { p, video, instances } = await livePlayer();
    expect(p.getState()).toBe('live');

    // currentTime never advances again: the muxer the guest is reading is dead.
    await vi.advanceTimersByTimeAsync(5_000); // poll 1 — notices the freeze
    await vi.advanceTimersByTimeAsync(5_000); // poll 2 — still frozen → recover
    await Promise.resolve();

    expect(instances[0].destroy).toHaveBeenCalled();
    expect(instances.length).toBeGreaterThan(1);
    expect(instances[1].attachMedia).toHaveBeenCalledWith(video);

    p.destroy();
    vi.useRealTimers();
  });

  it('leaves healthy playback alone', async () => {
    vi.useFakeTimers();
    const { p, video, instances } = await livePlayer();

    for (let i = 0; i < 4; i++) {
      video.currentTime += 5; // playhead keeps moving
      await vi.advanceTimersByTimeAsync(5_000);
    }
    await Promise.resolve();

    expect(instances[0].destroy).not.toHaveBeenCalled();
    expect(instances).toHaveLength(1);
    expect(p.getState()).toBe('live');

    p.destroy();
    vi.useRealTimers();
  });

  /** Poll until the initial audio-only attach budget is spent and the player has settled
   *  into `live` (so later assertions measure the watchdog, not the startup retries). */
  async function settleAudioOnly(p: HlsPlayer, video: any): Promise<void> {
    for (let i = 0; i < 6 && p.getState() !== 'live'; i++) {
      video.currentTime += 5;
      await vi.advanceTimersByTimeAsync(5_000);
      await Promise.resolve();
    }
    expect(p.getState()).toBe('live'); // settled with audio, no picture
  }

  it('keeps recovering while the manifest promises video that never decodes', async () => {
    vi.useFakeTimers();
    // Data flows (audio plays, playhead advances) but the picture never appears.
    const { p, video, instances } = await livePlayer({ videoWidth: 0 });
    await settleAudioOnly(p, video);

    // The master lists an H264 rendition — a black canvas is a FAULT here, not a
    // mic-only broadcast, so giving up permanently is wrong.
    const announceVideo = () =>
      fireHlsEvent(instances[instances.length - 1], 'hlsManifestParsed', {
        levels: [{ videoCodec: 'avc1.42e01e' }],
      });
    announceVideo();

    const before = instances.length;
    for (let i = 0; i < 4; i++) {
      video.currentTime += 5;
      await vi.advanceTimersByTimeAsync(5_000);
      await Promise.resolve();
      announceVideo(); // each fresh attachment parses the same master
    }

    // Still rebuilding on every poll, long past the 3-strike startup budget.
    expect(instances.length).toBeGreaterThanOrEqual(before + 4);

    p.destroy();
    vi.useRealTimers();
  });

  it('settles on an audio-only manifest instead of looping forever', async () => {
    vi.useFakeTimers();
    const { p, video, instances } = await livePlayer({ videoWidth: 0 });
    await settleAudioOnly(p, video);

    // A genuinely mic-only webinar: no video rendition in the master at all.
    fireHlsEvent(instances[instances.length - 1], 'hlsManifestParsed', {
      levels: [{ videoCodec: undefined }],
    });

    const before = instances.length;
    for (let i = 0; i < 5; i++) {
      video.currentTime += 5;
      await vi.advanceTimersByTimeAsync(5_000);
      await Promise.resolve();
    }

    expect(instances.length).toBe(before); // no churn — the audio just plays

    p.destroy();
    vi.useRealTimers();
  });
});
