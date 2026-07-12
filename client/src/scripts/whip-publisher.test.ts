// WHIP publisher (webinar phase 1, F1-4). Mocks fetch + a fake RTCPeerConnection
// (like webrtc.test.ts) and the webinar API, so the SDP exchange, the lifecycle calls,
// mic-denied handling, the webcam toggle, and the single reconnect are exercised
// without a real WebRTC stack or network.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const goLive = vi.fn();
const publishStarted = vi.fn();
const publishStopped = vi.fn();
// The publisher builds its capture constraints via webinar.buildPublishConstraints,
// so the mock provides a faithful implementation (mic always on, camera deviceId-pinned).
vi.mock('./webinar', () => ({
  goLive: (id: string) => goLive(id),
  publishStarted: (id: string) => publishStarted(id),
  publishStopped: (id: string) => publishStopped(id),
  buildPublishConstraints: (choice: any = {}) => {
    const audio = choice.audioDeviceId ? { deviceId: { exact: choice.audioDeviceId } } : true;
    let video: any = false;
    if (choice.withCamera) {
      video = choice.videoDeviceId ? { deviceId: { exact: choice.videoDeviceId } } : true;
    }
    return { audio, video };
  },
}));

import {
  WhipPublisher,
  resolveResourceUrl,
  whipDelete,
  whipPublish,
} from './whip-publisher';

// A fake RTCPeerConnection tracking just enough state for the WHIP flow.
const pcs: FakePC[] = [];
class FakePC {
  localDescription: any = null;
  remoteDescription: any = null;
  connectionState = 'new';
  iceGatheringState: string = 'complete';
  onconnectionstatechange: any = null;
  senders: any[] = [];
  transceivers: any[] = [];
  listeners = new Map<string, Set<(e?: any) => void>>();
  constructor(public cfg: any) {
    pcs.push(this);
  }
  addEventListener(type: string, fn: (e?: any) => void) {
    (this.listeners.get(type) ?? this.listeners.set(type, new Set()).get(type)!).add(fn);
  }
  removeEventListener(type: string, fn: (e?: any) => void) {
    this.listeners.get(type)?.delete(fn);
  }
  emit(type: string) {
    this.listeners.get(type)?.forEach((fn) => fn());
  }
  makeSender(track: any) {
    const sender: any = {
      track,
      replaceTrack: vi.fn(async (t: any) => {
        sender.track = t;
      }),
    };
    return sender;
  }
  addTrack(track: any) {
    const sender = this.makeSender(track);
    this.senders.push(sender);
    return sender;
  }
  addTransceiver(_kind: string) {
    const sender = this.makeSender(null);
    const tx = { sender };
    this.transceivers.push(tx);
    return tx;
  }
  async createOffer() {
    return { type: 'offer', sdp: 'offer-sdp' };
  }
  async setLocalDescription(d: any) {
    this.localDescription = d;
  }
  async setRemoteDescription(d: any) {
    this.remoteDescription = d;
  }
  close = vi.fn();
  // Drive the aggregate connection state and fire the handler.
  setConn(state: string) {
    this.connectionState = state;
    this.onconnectionstatechange?.();
  }
}
(globalThis as any).RTCPeerConnection = FakePC;

// A fake WHIP answer Response with a Location header.
function whipAnswer(sdp = 'answer-sdp', status = 201, location = '/whip/session/xyz'): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    text: async () => sdp,
    headers: { get: (k: string) => (k.toLowerCase() === 'location' ? location : null) },
  } as unknown as Response;
}

function fakeTrack(kind: string) {
  return { kind, readyState: 'live', stop: vi.fn() } as any;
}

/** A fake MediaStream that tracks add/remove so the camera toggle is observable. */
function fakeStream(tracks: any[]) {
  const list = [...tracks];
  return {
    getTracks: () => list,
    getVideoTracks: () => list.filter((t) => t.kind === 'video'),
    getAudioTracks: () => list.filter((t) => t.kind === 'audio'),
    addTrack: (t: any) => list.push(t),
    removeTrack: (t: any) => {
      const i = list.indexOf(t);
      if (i >= 0) list.splice(i, 1);
    },
  } as any;
}

const fetchMock = vi.fn<(url: string, init?: RequestInit) => Promise<Response>>();
const flush = async () => {
  for (let i = 0; i < 8; i++) await Promise.resolve();
};

beforeEach(() => {
  pcs.length = 0;
  fetchMock.mockReset();
  goLive.mockReset();
  publishStarted.mockReset();
  publishStopped.mockReset();
  goLive.mockResolvedValue({ publish_url: 'https://ingest.example/webinar/ab12/whip?token=t', expires_in: 120 });
  publishStarted.mockResolvedValue({});
  publishStopped.mockResolvedValue({});
  vi.stubGlobal('fetch', fetchMock);
  // addEventListener/removeEventListener are used for the pagehide hook.
  vi.stubGlobal('addEventListener', vi.fn());
  vi.stubGlobal('removeEventListener', vi.fn());
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('resolveResourceUrl', () => {
  it('resolves a relative Location against the ingest URL', () => {
    expect(resolveResourceUrl('/whip/s/1', 'https://ingest.example/webinar/ab12/whip?token=t')).toBe(
      'https://ingest.example/whip/s/1',
    );
  });
  it('keeps an absolute Location as-is', () => {
    expect(resolveResourceUrl('https://other.example/r/9', 'https://ingest.example/whip')).toBe(
      'https://other.example/r/9',
    );
  });
});

describe('whipPublish', () => {
  it('POSTs application/sdp and returns the answer + resolved resource url', async () => {
    fetchMock.mockResolvedValue(whipAnswer('ans', 201, '/whip/s/7'));
    const out = await whipPublish('https://ingest.example/webinar/ab12/whip?token=t', 'off');
    expect(out).toEqual({ answerSdp: 'ans', resourceUrl: 'https://ingest.example/whip/s/7' });
    const [, init] = fetchMock.mock.calls[0];
    expect(init?.headers).toEqual({ 'Content-Type': 'application/sdp' });
  });

  it('throws on a non-2xx', async () => {
    fetchMock.mockResolvedValue(whipAnswer('', 403, ''));
    await expect(whipPublish('https://ingest/x', 'o')).rejects.toThrow('WHIP publish failed (403)');
  });

  it('throws on an empty answer body', async () => {
    fetchMock.mockResolvedValue(whipAnswer('   ', 201, ''));
    await expect(whipPublish('https://ingest/x', 'o')).rejects.toThrow('empty answer');
  });

  it('null resource url when there is no Location header', async () => {
    fetchMock.mockResolvedValue(whipAnswer('ans', 201, ''));
    const out = await whipPublish('https://ingest/x', 'o');
    expect(out.resourceUrl).toBeNull();
  });
});

describe('whipDelete', () => {
  it('DELETEs the resource and swallows a network failure', async () => {
    fetchMock.mockResolvedValue({ ok: true } as Response);
    await whipDelete('https://ingest/whip/s/1');
    expect(fetchMock).toHaveBeenCalledWith('https://ingest/whip/s/1', { method: 'DELETE' });
    fetchMock.mockRejectedValueOnce(new Error('offline'));
    await expect(whipDelete('https://ingest/whip/s/1')).resolves.toBeUndefined();
  });
});

describe('WhipPublisher.start', () => {
  it('errors when WebRTC is unsupported', async () => {
    fetchMock.mockResolvedValue(whipAnswer());
    const orig = (globalThis as any).RTCPeerConnection;
    delete (globalThis as any).RTCPeerConnection;
    try {
      const p = new WhipPublisher({
        webinarId: 'w1',
        getUserMedia: async () => fakeStream([fakeTrack('audio')]),
      });
      await expect(p.start()).rejects.toThrow('WebRTC is not supported');
      expect(p.getState()).toBe('error');
    } finally {
      (globalThis as any).RTCPeerConnection = orig;
    }
  });


  it('captures mic-only, POSTs the WHIP offer, applies the answer, goes on-air', async () => {
    fetchMock.mockResolvedValue(whipAnswer());
    const states: string[] = [];
    const getUserMedia = vi.fn(async () => fakeStream([fakeTrack('audio')]));
    const p = new WhipPublisher({ webinarId: 'w1', onState: (s) => states.push(s), getUserMedia });

    await p.start();
    // Mic required, no video requested up front.
    expect(getUserMedia).toHaveBeenCalledWith({ audio: true, video: false });
    // A fresh publish token is fetched right before publishing.
    expect(goLive).toHaveBeenCalledWith('w1');

    // The WHIP POST: verbatim tokenized URL, application/sdp, the offer SDP body, NO auth.
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('https://ingest.example/webinar/ab12/whip?token=t');
    expect(init?.method).toBe('POST');
    expect(init?.headers).toEqual({ 'Content-Type': 'application/sdp' });
    expect(init?.body).toBe('offer-sdp');
    expect((init?.headers as any).Authorization).toBeUndefined();

    // The 201 answer is applied to the pc.
    expect(pcs[0].remoteDescription).toEqual({ type: 'answer', sdp: 'answer-sdp' });
    // A video m-line is negotiated up front so a later camera toggle needs no re-offer.
    expect(pcs[0].transceivers.length).toBe(1);

    // connected → on-air + publishStarted.
    pcs[0].setConn('connected');
    await flush();
    expect(p.getState()).toBe('on-air');
    expect(publishStarted).toHaveBeenCalledWith('w1');
    expect(states).toContain('connecting');
    expect(states).toContain('on-air');
  });

  it('requests the webcam up front when withCamera is set', async () => {
    fetchMock.mockResolvedValue(whipAnswer());
    const getUserMedia = vi.fn(async () => fakeStream([fakeTrack('audio'), fakeTrack('video')]));
    const p = new WhipPublisher({ webinarId: 'w1', withCamera: true, getUserMedia });
    await p.start();
    expect(getUserMedia).toHaveBeenCalledWith({ audio: true, video: true });
    // The video track is on a real sender (addTrack), not a placeholder transceiver.
    expect(pcs[0].senders.some((s: any) => s.track?.kind === 'video')).toBe(true);
    expect(pcs[0].transceivers.length).toBe(0);
  });

  it('honors the pre-live device choice via deviceId: { exact } (mic + camera)', async () => {
    fetchMock.mockResolvedValue(whipAnswer());
    const getUserMedia = vi.fn(async () => fakeStream([fakeTrack('audio'), fakeTrack('video')]));
    const p = new WhipPublisher({
      webinarId: 'w1',
      withCamera: true,
      audioDeviceId: 'mic-2',
      videoDeviceId: 'cam-2',
      getUserMedia,
    });
    await p.start();
    expect(getUserMedia).toHaveBeenCalledWith({
      audio: { deviceId: { exact: 'mic-2' } },
      video: { deviceId: { exact: 'cam-2' } },
    });
  });

  it('pins the mic device but stays mic-only when withCamera is false', async () => {
    fetchMock.mockResolvedValue(whipAnswer());
    const getUserMedia = vi.fn(async () => fakeStream([fakeTrack('audio')]));
    const p = new WhipPublisher({ webinarId: 'w1', audioDeviceId: 'mic-9', getUserMedia });
    await p.start();
    // Camera stays off (video: false) even though a videoDeviceId was not provided.
    expect(getUserMedia).toHaveBeenCalledWith({
      audio: { deviceId: { exact: 'mic-9' } },
      video: false,
    });
  });

  it('surfaces mic-denied when getUserMedia rejects', async () => {
    const states: string[] = [];
    const getUserMedia = vi.fn(async () => {
      throw new Error('NotAllowedError');
    });
    const p = new WhipPublisher({ webinarId: 'w1', onState: (s) => states.push(s), getUserMedia });
    await expect(p.start()).rejects.toThrow('microphone permission denied');
    expect(p.getState()).toBe('mic-denied');
    expect(states).toContain('mic-denied');
    // Never reached the network / peer connection.
    expect(goLive).not.toHaveBeenCalled();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('goes to error and cleans media when the WHIP POST fails', async () => {
    fetchMock.mockResolvedValue(whipAnswer('', 500, ''));
    const track = fakeTrack('audio');
    const getUserMedia = vi.fn(async () => fakeStream([track]));
    const p = new WhipPublisher({ webinarId: 'w1', getUserMedia });
    await expect(p.start()).rejects.toThrow();
    expect(p.getState()).toBe('error');
    expect(track.stop).toHaveBeenCalled(); // media torn down on a failed start
  });
});

describe('WhipPublisher.toggleCamera', () => {
  it('captures + replaceTracks the camera on, stops + clears it off', async () => {
    fetchMock.mockResolvedValue(whipAnswer());
    const audio = fakeTrack('audio');
    const getUserMedia = vi
      .fn()
      .mockResolvedValueOnce(fakeStream([audio])) // start: mic-only
      .mockResolvedValueOnce(fakeStream([fakeTrack('video')])); // toggleCamera(true)
    const p = new WhipPublisher({ webinarId: 'w1', getUserMedia });
    await p.start();
    // The up-front video transceiver's sender is where the camera track lands.
    const videoSender = pcs[0].transceivers[0].sender;

    const on = await p.toggleCamera(true);
    expect(on).toBe(true);
    expect(p.isCameraOn()).toBe(true);
    expect(videoSender.replaceTrack).toHaveBeenCalledWith(expect.objectContaining({ kind: 'video' }));

    const off = await p.toggleCamera(false);
    expect(off).toBe(false);
    expect(p.isCameraOn()).toBe(false);
    expect(videoSender.replaceTrack).toHaveBeenLastCalledWith(null);
  });

  it('stays audio-only when the camera is denied', async () => {
    fetchMock.mockResolvedValue(whipAnswer());
    const getUserMedia = vi
      .fn()
      .mockResolvedValueOnce(fakeStream([fakeTrack('audio')]))
      .mockRejectedValueOnce(new Error('camera denied'));
    const p = new WhipPublisher({ webinarId: 'w1', getUserMedia });
    await p.start();
    expect(await p.toggleCamera(true)).toBe(false);
    expect(p.isCameraOn()).toBe(false);
  });

  it('is a no-op before start()', async () => {
    const p = new WhipPublisher({ webinarId: 'w1' });
    expect(await p.toggleCamera(true)).toBe(false);
  });
});

describe('WhipPublisher.toggleMicrophone', () => {
  it('captures the mic track live at start (isMicrophoneOn true)', async () => {
    fetchMock.mockResolvedValue(whipAnswer());
    const audio = { ...fakeTrack('audio'), enabled: true };
    const getUserMedia = vi.fn(async () => fakeStream([audio]));
    const p = new WhipPublisher({ webinarId: 'w1', getUserMedia });
    await p.start();
    expect(p.isMicrophoneOn()).toBe(true);
  });

  it('mutes + unmutes by toggling the captured audio track enabled flag', async () => {
    fetchMock.mockResolvedValue(whipAnswer());
    const audio = { ...fakeTrack('audio'), enabled: true };
    const getUserMedia = vi.fn(async () => fakeStream([audio]));
    const p = new WhipPublisher({ webinarId: 'w1', getUserMedia });
    await p.start();

    // Mute: the SAME track the STT MediaRecorder wraps is disabled → silence for both paths.
    expect(p.toggleMicrophone(false)).toBe(false);
    expect(audio.enabled).toBe(false);
    expect(p.isMicrophoneOn()).toBe(false);

    // Unmute: track re-enabled.
    expect(p.toggleMicrophone(true)).toBe(true);
    expect(audio.enabled).toBe(true);
    expect(p.isMicrophoneOn()).toBe(true);
  });

  it('is a no-op before start() (no captured track)', () => {
    const p = new WhipPublisher({ webinarId: 'w1' });
    expect(p.isMicrophoneOn()).toBe(false);
    expect(p.toggleMicrophone(false)).toBe(false);
  });
});

describe('WhipPublisher.stop', () => {
  it('DELETEs the WHIP resource, closes the pc, stops media, ends the webinar', async () => {
    fetchMock.mockResolvedValue(whipAnswer('answer-sdp', 201, 'https://ingest.example/whip/s/1'));
    const audio = fakeTrack('audio');
    const getUserMedia = vi.fn(async () => fakeStream([audio]));
    const p = new WhipPublisher({ webinarId: 'w1', getUserMedia });
    await p.start();
    pcs[0].setConn('connected');
    await flush();

    fetchMock.mockClear();
    await p.stop();

    // The resource is DELETEd (best-effort teardown).
    expect(fetchMock).toHaveBeenCalledWith('https://ingest.example/whip/s/1', { method: 'DELETE' });
    expect(pcs[0].close).toHaveBeenCalled();
    expect(audio.stop).toHaveBeenCalled();
    expect(publishStopped).toHaveBeenCalledWith('w1');
    expect(p.getState()).toBe('idle');

    // Idempotent: a second stop does nothing more.
    publishStopped.mockClear();
    await p.stop();
    expect(publishStopped).not.toHaveBeenCalled();
  });
});

describe('WhipPublisher reconnect', () => {
  it('reconnects once on a connection drop, then errors on a second drop', async () => {
    fetchMock.mockResolvedValue(whipAnswer());
    const getUserMedia = vi.fn(async () => fakeStream([fakeTrack('audio')]));
    const states: string[] = [];
    const p = new WhipPublisher({ webinarId: 'w1', getUserMedia, onState: (s) => states.push(s) });
    await p.start();
    pcs[0].setConn('connected');
    await flush();

    // First drop → single automatic reconnect (a NEW pc + a fresh go-live token).
    goLive.mockClear();
    pcs[0].setConn('failed');
    await flush();
    expect(states).toContain('reconnecting');
    expect(goLive).toHaveBeenCalledWith('w1'); // re-published with a fresh token
    expect(pcs.length).toBe(2);

    // The reconnected pc recovers → on-air again (reconnect budget resets).
    pcs[1].setConn('connected');
    await flush();
    expect(p.getState()).toBe('on-air');

    // A later drop after recovery uses the (reset) budget for one more reconnect.
    pcs[1].setConn('failed');
    await flush();
    expect(pcs.length).toBe(3);
  });

  it('waits for ICE gathering before publishing when not yet complete', async () => {
    fetchMock.mockResolvedValue(whipAnswer());
    // Force the pc to start "gathering" so waitForIceGathering must await the event.
    const getUserMedia = vi.fn(async () => fakeStream([fakeTrack('audio')]));
    const p = new WhipPublisher({ webinarId: 'w1', getUserMedia });
    const orig = (globalThis as any).RTCPeerConnection;
    class Gathering extends FakePC {
      constructor(cfg: any) {
        super(cfg);
        this.iceGatheringState = 'gathering';
      }
    }
    (globalThis as any).RTCPeerConnection = Gathering;
    try {
      const startP = p.start();
      await flush();
      // Not published yet — still gathering.
      expect(fetchMock).not.toHaveBeenCalled();
      // Complete gathering → the wait resolves → the WHIP POST fires.
      pcs[0].iceGatheringState = 'complete';
      pcs[0].emit('icegatheringstatechange');
      await startP;
      expect(fetchMock).toHaveBeenCalled();
    } finally {
      (globalThis as any).RTCPeerConnection = orig;
    }
  });
});
