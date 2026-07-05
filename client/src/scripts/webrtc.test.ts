import { describe, it, expect, vi, beforeEach } from 'vitest';
import { MeshManager } from './webrtc';

const pcs: any[] = [];

// A fake RTCPeerConnection that tracks signalingState the way a real one does,
// so the perfect-negotiation logic (glare/rollback/stray-answer guards) is
// exercised for real rather than no-op'd.
class FakePC {
  localDescription: any;
  remoteDescription: any;
  signalingState = 'stable';
  connectionState = 'new';
  iceConnectionState: string = 'new';
  ontrack: any = () => {};
  onicecandidate: any = () => {};
  onconnectionstatechange: any = () => {};
  oniceconnectionstatechange: any = () => {};
  senders: any[] = [];
  transceivers: any[] = [];
  offerOpts: any[] = [];
  constructor(public cfg: any) {
    pcs.push(this);
  }
  // getStats fixtures: reports the adaptive-bitrate monitor sees, or a hard failure.
  statsRecords: any[] = [];
  getStatsFail = false;
  // negotiate() knobs: throw from createOffer, or run a hook that can flip
  // signalingState mid-negotiation (simulating a raced remote offer).
  failOffer = false;
  onCreateOffer: (() => void) | null = null;
  makeSender(track: any) {
    const sender: any = {
      track,
      params: {} as any,
      replaceTrack: vi.fn(async (t: any) => { sender.track = t; }),
      getParameters: vi.fn(() => sender.params),
      setParameters: vi.fn(async (p: any) => { sender.params = p; }),
    };
    return sender;
  }
  addTrack(track: any) {
    const sender = this.makeSender(track);
    this.senders.push(sender);
    this.transceivers.push({ sender, receiver: { track: { kind: track.kind } } });
  }
  addTransceiver(kind: string) {
    const sender = this.makeSender(null);
    const tx = { sender, receiver: { track: { kind } } };
    this.transceivers.push(tx);
    return tx;
  }
  getSenders() {
    return this.senders;
  }
  getTransceivers() {
    return this.transceivers;
  }
  async getStats() {
    if (this.getStatsFail) throw new Error('stats unavailable');
    return { forEach: (fn: (r: unknown) => void) => this.statsRecords.forEach(fn) };
  }
  async createOffer(opts?: any) {
    this.offerOpts.push(opts);
    if (this.failOffer) throw new Error('createOffer failed');
    this.onCreateOffer?.();
    return { type: 'offer', sdp: 'offer-sdp' };
  }
  async createAnswer() {
    return { type: 'answer', sdp: 'answer-sdp' };
  }
  failRollback = false; // browsers without rollback support throw here
  async setLocalDescription(d: any) {
    if (d?.type === 'rollback' && this.failRollback) throw new Error('rollback unsupported');
    this.localDescription = d;
    if (d?.type === 'offer') this.signalingState = 'have-local-offer';
    else if (d?.type === 'answer' || d?.type === 'rollback') this.signalingState = 'stable';
  }
  async setRemoteDescription(d: any) {
    this.remoteDescription = d;
    if (d?.type === 'offer') this.signalingState = 'have-remote-offer';
    else if (d?.type === 'answer') this.signalingState = 'stable';
  }
  addIceCandidate = vi.fn(async () => {});
  close = vi.fn();
}
(globalThis as any).RTCPeerConnection = FakePC;

// Drain the microtask queue so an async negotiate() chain (createOffer →
// setLocalDescription → send) settles before we assert on it.
const flush = async () => {
  for (let i = 0; i < 6; i++) await Promise.resolve();
};

function fakeStream() {
  const tracks = [
    { kind: 'audio', enabled: true },
    { kind: 'video', enabled: true },
  ];
  return {
    getTracks: () => tracks,
    getAudioTracks: () => tracks.filter((t) => t.kind === 'audio'),
    getVideoTracks: () => tracks.filter((t) => t.kind === 'video'),
  } as any;
}

function fakeAudioOnlyStream() {
  const tracks = [{ kind: 'audio', enabled: true }];
  return {
    getTracks: () => tracks,
    getAudioTracks: () => tracks.filter((t) => t.kind === 'audio'),
    getVideoTracks: () => tracks.filter((t) => t.kind === 'video'),
  } as any;
}

function fakeVideoOnlyStream() {
  const tracks = [{ kind: 'video', enabled: true }];
  return {
    getTracks: () => tracks,
    getAudioTracks: () => tracks.filter((t) => t.kind === 'audio'),
    getVideoTracks: () => tracks.filter((t) => t.kind === 'video'),
  } as any;
}

const videoSenderOf = (pc: any) => pc.senders.find((s: any) => s.track?.kind === 'video');

describe('MeshManager', () => {
  beforeEach(() => {
    pcs.length = 0;
  });

  // localId '' is < any real id, so the manager is always the impolite peer and
  // sends the first offer — matching the legacy "initiator" behaviour.
  it('impolite peer creates offer, adds tracks, replaces a reconnecting peer', async () => {
    const send = vi.fn();
    const m = new MeshManager(fakeStream(), send);
    await m.addPeer('p1', true);
    expect(pcs.length).toBe(1);
    expect(pcs[0].senders.length).toBe(2);
    expect(send).toHaveBeenCalledWith(expect.objectContaining({ type: 'offer', to: 'p1', sdp: 'offer-sdp' }));
    // Re-adding an existing peer = their socket reconnected: close the dead pc and
    // build a fresh one (rather than ignoring it and leaving the tile black).
    await m.addPeer('p1', true);
    expect(pcs.length).toBe(2);
    expect(pcs[0].close).toHaveBeenCalled();
  });

  it('polite peer waits for the offer instead of sending one', async () => {
    const send = vi.fn();
    // localId 'zzzz' > peer 'aaaa' → polite.
    const m = new MeshManager(fakeStream(), send, undefined, undefined, 'zzzz');
    await m.addPeer('aaaa');
    expect(pcs.length).toBe(1);
    expect(send).not.toHaveBeenCalledWith(expect.objectContaining({ type: 'offer' }));
  });

  it('answers offers, relays ice/track events', async () => {
    const send = vi.fn();
    const m = new MeshManager(fakeStream(), send);
    const onRemote = vi.fn();
    m.onRemoteStream = onRemote;

    await m.handleOffer('p2', 'remote-offer');
    const pc = pcs[0];
    expect(pc.remoteDescription).toEqual({ type: 'offer', sdp: 'remote-offer' });
    expect(send).toHaveBeenCalledWith(expect.objectContaining({ type: 'answer', to: 'p2', sdp: 'answer-sdp' }));
    expect(pc.signalingState).toBe('stable');

    pc.onicecandidate({ candidate: { toJSON: () => ({ c: 1 }) } });
    expect(send).toHaveBeenCalledWith(expect.objectContaining({ type: 'ice', to: 'p2', candidate: { c: 1 } }));
    pc.onicecandidate({ candidate: null }); // nothing sent

    pc.ontrack({ streams: [{ id: 's' }] });
    expect(onRemote).toHaveBeenCalledWith('p2', { id: 's' });

    await m.handleIce('p2', { c: 2 } as RTCIceCandidateInit);
    expect(pc.addIceCandidate).toHaveBeenCalled();
    pc.addIceCandidate.mockRejectedValueOnce(new Error('bad'));
    await m.handleIce('p2', { c: 3 } as RTCIceCandidateInit); // swallowed, no throw

    // unknown peers are no-ops
    await m.handleAnswer('ghost', 'x');
    await m.handleIce('ghost', {});
  });

  it('applies an answer only when an offer is pending (no wrong-state throw)', async () => {
    const send = vi.fn();
    const m = new MeshManager(fakeStream(), send);
    await m.addPeer('p1'); // impolite → sends offer → have-local-offer
    const pc = pcs[0];
    expect(pc.signalingState).toBe('have-local-offer');

    await m.handleAnswer('p1', 'ans'); // expected answer → applied
    expect(pc.remoteDescription).toEqual({ type: 'answer', sdp: 'ans' });
    expect(pc.signalingState).toBe('stable');

    // A second / stray answer arrives while already stable: it must be ignored,
    // not throw `setRemoteDescription ... wrong state: stable`.
    pc.remoteDescription = null;
    await expect(m.handleAnswer('p1', 'late')).resolves.toBeUndefined();
    expect(pc.remoteDescription).toBeNull(); // not applied
  });

  it('glare: impolite peer ignores the colliding offer, then takes the answer', async () => {
    const send = vi.fn();
    // localId 'aaaa' < peer 'zzzz' → impolite.
    const m = new MeshManager(fakeStream(), send, undefined, undefined, 'aaaa');
    await m.addPeer('zzzz'); // impolite → sends our offer → have-local-offer
    const pc = pcs[0];
    send.mockClear();

    await m.handleOffer('zzzz', 'their-offer'); // collides with our in-flight offer
    expect(pc.remoteDescription).toBeUndefined(); // we kept ours, ignored theirs
    expect(send).not.toHaveBeenCalledWith(expect.objectContaining({ type: 'answer' }));

    await m.handleAnswer('zzzz', 'their-answer'); // their answer to OUR offer
    expect(pc.remoteDescription).toEqual({ type: 'answer', sdp: 'their-answer' });
    expect(pc.signalingState).toBe('stable');
  });

  it('glare: polite peer rolls back its offer and answers theirs', async () => {
    const send = vi.fn();
    // localId 'zzzz' > peer 'aaaa' → polite.
    const m = new MeshManager(fakeStream(), send, undefined, undefined, 'zzzz');
    await m.addPeer('aaaa'); // polite → no offer yet
    const pc = pcs[0];
    // Drive the polite side into an in-flight offer via an ICE restart.
    pc.iceConnectionState = 'failed';
    pc.oniceconnectionstatechange();
    await flush();
    expect(pc.signalingState).toBe('have-local-offer');
    send.mockClear();

    await m.handleOffer('aaaa', 'their-offer'); // collision → polite yields
    expect(pc.remoteDescription).toEqual({ type: 'offer', sdp: 'their-offer' });
    expect(send).toHaveBeenCalledWith(expect.objectContaining({ type: 'answer', to: 'aaaa' }));
    expect(pc.signalingState).toBe('stable');
  });

  it('glare: polite peer still answers when rollback is unsupported', async () => {
    const send = vi.fn();
    const m = new MeshManager(fakeStream(), send, undefined, undefined, 'zzzz');
    await m.addPeer('aaaa'); // polite → no offer yet
    const pc = pcs[0];
    pc.iceConnectionState = 'failed';
    pc.oniceconnectionstatechange(); // drive an in-flight offer via ICE restart
    await flush();
    expect(pc.signalingState).toBe('have-local-offer');
    send.mockClear();

    pc.failRollback = true; // e.g. an engine without rollback support
    await m.handleOffer('aaaa', 'their-offer');
    // The rejection is absorbed and the polite peer still answers their offer.
    expect(pc.remoteDescription).toEqual({ type: 'offer', sdp: 'their-offer' });
    expect(send).toHaveBeenCalledWith(expect.objectContaining({ type: 'answer', to: 'aaaa' }));
  });

  it('restarts when the peer degraded to failed during the grace period', async () => {
    vi.useFakeTimers();
    try {
      const send = vi.fn();
      const m = new MeshManager(fakeStream(), send);
      await m.addPeer('p1');
      const pc = pcs[0];
      await m.handleAnswer('p1', 'ans'); // → stable
      pc.offerOpts.length = 0;

      pc.iceConnectionState = 'disconnected';
      pc.oniceconnectionstatechange();
      pc.iceConnectionState = 'failed'; // got worse while we waited
      await vi.advanceTimersByTimeAsync(2_500);
      expect(pc.offerOpts).toContainEqual({ iceRestart: true });
      m.destroy();
    } finally {
      vi.useRealTimers();
    }
  });

  it('ICE restart recovers a failed peer instead of removing it', async () => {
    const send = vi.fn();
    const onRemoved = vi.fn();
    const m = new MeshManager(fakeStream(), send);
    m.onPeerRemoved = onRemoved;
    await m.addPeer('p1'); // offer → have-local-offer
    const pc = pcs[0];
    await m.handleAnswer('p1', 'ans'); // → stable
    send.mockClear();

    pc.iceConnectionState = 'failed';
    pc.oniceconnectionstatechange();
    await flush();

    // A fresh offer goes out WITH iceRestart, and the peer is kept (not dropped).
    expect(send).toHaveBeenCalledWith(expect.objectContaining({ type: 'offer', to: 'p1' }));
    expect(pc.offerOpts).toContainEqual({ iceRestart: true });
    expect(onRemoved).not.toHaveBeenCalled();
  });

  it('ICE restarts a still-disconnected peer after the grace period', async () => {
    vi.useFakeTimers();
    try {
      const send = vi.fn();
      const m = new MeshManager(fakeStream(), send);
      await m.addPeer('p1');
      const pc = pcs[0];
      await m.handleAnswer('p1', 'ans'); // → stable
      send.mockClear();

      pc.iceConnectionState = 'disconnected';
      pc.oniceconnectionstatechange();
      // Still down when the grace timer fires → restart.
      await vi.advanceTimersByTimeAsync(2_500);
      expect(pc.offerOpts).toContainEqual({ iceRestart: true });
    } finally {
      vi.useRealTimers();
    }
  });

  it('skips the restart when the peer recovered during the grace period', async () => {
    vi.useFakeTimers();
    try {
      const send = vi.fn();
      const m = new MeshManager(fakeStream(), send);
      await m.addPeer('p1');
      const pc = pcs[0];
      await m.handleAnswer('p1', 'ans');
      pc.offerOpts.length = 0;

      pc.iceConnectionState = 'disconnected';
      pc.oniceconnectionstatechange();
      pc.iceConnectionState = 'connected'; // self-healed before the timer
      await vi.advanceTimersByTimeAsync(2_500);
      expect(pc.offerOpts).not.toContainEqual({ iceRestart: true });
    } finally {
      vi.useRealTimers();
    }
  });

  it('toggles tracks, replaces stream, destroys, removes', async () => {
    const send = vi.fn();
    const stream = fakeStream();
    const m = new MeshManager(stream, send);
    await m.addPeer('p1');

    m.setAudioEnabled(false);
    expect(stream.getAudioTracks()[0].enabled).toBe(false);
    m.setVideoEnabled(false);
    expect(stream.getVideoTracks()[0].enabled).toBe(false);

    m.setLocalStream(fakeStream());
    expect(pcs[0].senders.some((s: any) => s.replaceTrack.mock.calls.length > 0)).toBe(true);

    const onRemoved = vi.fn();
    m.onPeerRemoved = onRemoved;
    m.removePeer('p1');
    expect(pcs[0].close).toHaveBeenCalled();
    expect(onRemoved).toHaveBeenCalledWith('p1');

    m.removePeer('ghost'); // unknown peer still fires the callback
    expect(onRemoved).toHaveBeenCalledWith('ghost');

    m.destroy();
  });

  it('negotiates a video m-line on audio-only joins so screen share needs no camera', async () => {
    const m = new MeshManager(fakeAudioOnlyStream(), vi.fn());
    await m.addPeer('p1');
    const txs = pcs[0].transceivers;
    // One audio sender (addTrack) + an added video transceiver (addTransceiver).
    expect(pcs[0].senders.length).toBe(1);
    const videoTx = txs.find((t: any) => t.receiver.track.kind === 'video');
    expect(videoTx).toBeTruthy();
    // The screen track lands on that video sender even though we have no camera.
    const screen = { kind: 'video' } as any;
    m.replaceVideoTrack(screen);
    expect(videoTx.sender.replaceTrack).toHaveBeenCalledWith(screen);
  });

  it('replaceVideoTrack swaps the video sender and can clear it', async () => {
    const m = new MeshManager(fakeStream(), vi.fn());
    await m.addPeer('p1');
    const videoTx = pcs[0].transceivers.find((t: any) => t.receiver.track.kind === 'video');
    const screen = { kind: 'video' } as any;
    m.replaceVideoTrack(screen);
    expect(videoTx.sender.replaceTrack).toHaveBeenCalledWith(screen);
    m.replaceVideoTrack(null); // stop sharing with the camera off → clear video
    expect(videoTx.sender.replaceTrack).toHaveBeenCalledWith(null);
  });

  it('replaceAudioTrack swaps the audio sender and can revert it', async () => {
    const m = new MeshManager(fakeStream(), vi.fn());
    await m.addPeer('p1');
    const audioSender = pcs[0].senders.find((s: any) => s.track?.kind === 'audio');
    expect(audioSender).toBeTruthy();
    const mix = { kind: 'audio' } as any; // mic+screen-audio mix while sharing (spec 0085)
    m.replaceAudioTrack(mix);
    expect(audioSender.replaceTrack).toHaveBeenCalledWith(mix);
    m.replaceAudioTrack(null); // stop sharing → revert to the plain mic track
    expect(audioSender.replaceTrack).toHaveBeenCalledWith(null);
  });

  // Great-Firewall hardening: a restricted client is constructed with
  // iceTransportPolicy 'relay' so every peer connection goes straight to TURN.
  it('omits iceTransportPolicy by default (unrestricted → browser default all)', async () => {
    const m = new MeshManager(fakeStream(), vi.fn());
    await m.addPeer('p1', true);
    expect(pcs[0].cfg.iceTransportPolicy).toBeUndefined();
    expect('iceTransportPolicy' in pcs[0].cfg).toBe(false);
  });

  it('forces relay when iceTransportPolicy is set (China-side peer)', async () => {
    const m = new MeshManager(fakeStream(), vi.fn(), undefined, undefined, '', 'relay');
    await m.addPeer('p1', true);
    expect(pcs[0].cfg.iceTransportPolicy).toBe('relay');
  });

  it('falls back to webkitRTCPeerConnection when the unprefixed ctor is missing', async () => {
    const orig = (globalThis as any).RTCPeerConnection;
    delete (globalThis as any).RTCPeerConnection;
    (globalThis as any).webkitRTCPeerConnection = FakePC;
    try {
      const m = new MeshManager(fakeStream(), vi.fn());
      await m.addPeer('p1');
      expect(pcs.length).toBe(1);
      m.destroy();
    } finally {
      (globalThis as any).RTCPeerConnection = orig;
      delete (globalThis as any).webkitRTCPeerConnection;
    }
  });

  it('throws a clear error when WebRTC is unsupported entirely', async () => {
    const orig = (globalThis as any).RTCPeerConnection;
    delete (globalThis as any).RTCPeerConnection;
    try {
      const m = new MeshManager(fakeStream(), vi.fn());
      await expect(m.addPeer('p1')).rejects.toThrow('WebRTC is not supported');
    } finally {
      (globalThis as any).RTCPeerConnection = orig;
    }
  });

  it('negotiates an audio m-line on mic-less joins (#229)', async () => {
    const m = new MeshManager(fakeVideoOnlyStream(), vi.fn());
    await m.addPeer('p1');
    // One video sender (addTrack) + an added audio transceiver (addTransceiver).
    expect(pcs[0].senders.length).toBe(1);
    const audioTx = pcs[0].transceivers.find((t: any) => t.receiver.track.kind === 'audio');
    expect(audioTx).toBeTruthy();
    // No sender currently CARRIES an audio track, so replaceAudioTrack finds
    // nothing to swap (see the report on #229) — it must at least not throw.
    m.replaceAudioTrack({ kind: 'audio' } as any);
    // setLocalStream only swaps kinds that have a matching live sender: the
    // new stream's audio track has no home here and is skipped.
    m.setLocalStream(fakeStream());
    expect(videoSenderOf(pcs[0]).replaceTrack).toHaveBeenCalled();
  });

  it('ignores receiver tracks that arrive without a stream', async () => {
    const m = new MeshManager(fakeStream(), vi.fn());
    const onRemote = vi.fn();
    m.onRemoteStream = onRemote;
    await m.addPeer('p1');
    pcs[0].ontrack({ streams: [] }); // inactive m-line before its msid is known
    expect(onRemote).not.toHaveBeenCalled();
  });

  it('default callbacks are safe no-ops until the app overrides them', async () => {
    const m = new MeshManager(fakeStream(), vi.fn());
    await m.addPeer('p1');
    pcs[0].ontrack({ streams: [{ id: 's' }] }); // default onRemoteStream
    m.removePeer('p1'); // default onPeerRemoved
    m.destroy();
    m.destroy(); // idempotent: statsTimer already cleared
  });

  it('expects trailing ICE failures for an ignored (glare) offer', async () => {
    const m = new MeshManager(fakeStream(), vi.fn(), undefined, undefined, 'aaaa');
    await m.addPeer('zzzz'); // impolite → our offer is in flight
    const pc = pcs[0];
    await m.handleOffer('zzzz', 'their-offer'); // collision → deliberately ignored
    pc.addIceCandidate.mockRejectedValueOnce(new Error('stale candidate'));
    // Candidates trailing the dropped offer can't apply — expected, not an error.
    await expect(m.handleIce('zzzz', { c: 9 } as RTCIceCandidateInit)).resolves.toBeUndefined();
  });

  it('negotiate swallows createOffer failures and yields to a raced remote offer', async () => {
    const send = vi.fn();
    const m = new MeshManager(fakeStream(), send);
    await m.addPeer('p1');
    const pc = pcs[0];
    await m.handleAnswer('p1', 'ans'); // → stable
    send.mockClear();

    pc.failOffer = true; // e.g. transient / torn-down transceiver
    pc.iceConnectionState = 'failed';
    pc.oniceconnectionstatechange();
    await flush();
    expect(send).not.toHaveBeenCalled(); // failure swallowed, nothing sent

    pc.failOffer = false;
    // A remote offer races in while ours is being created → leave it to
    // handleOffer instead of forcing a bad local description.
    pc.onCreateOffer = () => {
      pc.signalingState = 'have-remote-offer';
    };
    pc.oniceconnectionstatechange();
    await flush();
    expect(send).not.toHaveBeenCalled();
  });

  // Spec 0031: per-stream cap = upload budget ÷ peer count, floored at 200 kbps.
  it('splits the video budget across peers and floors at 200 kbps', async () => {
    const m = new MeshManager(fakeStream(), vi.fn());
    await m.addPeer('p1');
    await m.addPeer('p2');
    await flush();
    // 2.4 Mbps default budget ÷ 2 peers → 1.2 Mbps per stream, on every peer.
    expect(videoSenderOf(pcs[0]).params.encodings[0].maxBitrate).toBe(1_200_000);
    expect(videoSenderOf(pcs[1]).params.encodings[0].maxBitrate).toBe(1_200_000);

    m.setVideoBudget(5_000_000); // screen-share boost (spec 0088)
    await flush();
    expect(videoSenderOf(pcs[0]).params.encodings[0].maxBitrate).toBe(2_500_000);

    m.setVideoBudget(100_000); // below the floor → clamped to MIN_VIDEO_BITRATE
    await flush();
    expect(videoSenderOf(pcs[0]).params.encodings[0].maxBitrate).toBe(200_000);
    m.destroy();
  });

  it('videoSender falls back to getSenders when getTransceivers is unavailable', async () => {
    const proto = FakePC.prototype as any;
    const orig = proto.getTransceivers;
    delete proto.getTransceivers;
    try {
      const m = new MeshManager(fakeStream(), vi.fn());
      await m.addPeer('p1');
      const vs = videoSenderOf(pcs[0]);
      const screen = { kind: 'video', id: 'screen' } as any;
      m.replaceVideoTrack(screen);
      expect(vs.replaceTrack).toHaveBeenCalledWith(screen);
      m.destroy();
    } finally {
      proto.getTransceivers = orig;
    }
  });

  it('skips the bitrate cap for peers with no video sender at all', async () => {
    const proto = FakePC.prototype as any;
    const origTx = proto.addTransceiver;
    const origGetTx = proto.getTransceivers;
    delete proto.addTransceiver; // environment without addTransceiver
    delete proto.getTransceivers;
    try {
      const m = new MeshManager(fakeAudioOnlyStream(), vi.fn());
      await m.addPeer('p1');
      await flush();
      // Audio-only pc, no video m-line anywhere → applyBitrate finds no sender.
      expect(pcs[0].senders.some((s: any) => s.params.encodings)).toBe(false);
      m.replaceVideoTrack({ kind: 'video' } as any); // nothing to swap → no throw
      m.setVideoBudget(1_000_000); // still nothing to cap → no throw
      await flush();
      m.destroy();
    } finally {
      proto.addTransceiver = origTx;
      proto.getTransceivers = origGetTx;
    }
  });

  it('swallows setParameters failures (pre-negotiation / unsupported)', async () => {
    const m = new MeshManager(fakeStream(), vi.fn());
    await m.addPeer('p1');
    videoSenderOf(pcs[0]).getParameters = () => {
      throw new Error('not negotiated yet');
    };
    m.setVideoBudget(3_000_000); // hits the applyBitrate catch — must not throw
    await flush();
    m.destroy();
  });

  // Spec 0030/0032: the 5s stats poll adapts the budget and nudges the UI.
  it('backs off the budget and fires onNetworkWeak on a sustained weak uplink', async () => {
    vi.useFakeTimers();
    try {
      const m = new MeshManager(fakeStream(), vi.fn());
      const onWeak = vi.fn();
      m.onNetworkWeak = onWeak;
      await m.addPeer('p1');
      const pc = pcs[0];
      const vs = videoSenderOf(pc);
      pc.statsRecords = [
        { type: 'outbound-rtp', kind: 'video', qualityLimitationReason: 'bandwidth' },
      ];

      await vi.advanceTimersByTimeAsync(5_000); // weak #1 → 2.4 → 1.8 Mbps
      expect(vs.params.encodings[0].maxBitrate).toBe(1_800_000);
      expect(onWeak).not.toHaveBeenCalled(); // one bad check isn't a streak

      await vi.advanceTimersByTimeAsync(5_000); // weak #2 → notify + 1.35 Mbps
      expect(onWeak).toHaveBeenCalledTimes(1);
      expect(vs.params.encodings[0].maxBitrate).toBe(1_350_000);

      pc.statsRecords = []; // healthy again → gentle recovery toward the max
      await vi.advanceTimersByTimeAsync(5_000);
      expect(vs.params.encodings[0].maxBitrate).toBe(1_620_000); // 1.35 × 1.2
      expect(onWeak).toHaveBeenCalledTimes(1); // streak was reset
      m.destroy();
    } finally {
      vi.useRealTimers();
    }
  });

  it('treats high remote packet loss as weak; a missing fractionLost does not', async () => {
    vi.useFakeTimers();
    try {
      const m = new MeshManager(fakeStream(), vi.fn());
      await m.addPeer('p1');
      const pc = pcs[0];
      const vs = videoSenderOf(pc);
      pc.statsRecords = [{ type: 'remote-inbound-rtp', fractionLost: 0.2 }];

      await vi.advanceTimersByTimeAsync(5_000);
      expect(vs.params.encodings[0].maxBitrate).toBe(1_800_000);
      // Second weak tick exercises the DEFAULT onNetworkWeak (harmless no-op).
      await vi.advanceTimersByTimeAsync(5_000);
      expect(vs.params.encodings[0].maxBitrate).toBe(1_350_000);

      pc.statsRecords = [{ type: 'remote-inbound-rtp' }]; // no fractionLost → healthy
      await vi.advanceTimersByTimeAsync(5_000);
      expect(vs.params.encodings[0].maxBitrate).toBe(1_620_000);
      m.destroy();
    } finally {
      vi.useRealTimers();
    }
  });

  it('survives getStats failures and holds the budget at the max', async () => {
    vi.useFakeTimers();
    try {
      const m = new MeshManager(fakeStream(), vi.fn());
      const onWeak = vi.fn();
      m.onNetworkWeak = onWeak;
      await m.addPeer('p1');
      const pc = pcs[0];
      pc.getStatsFail = true;

      await vi.advanceTimersByTimeAsync(10_000); // two failing polls
      // Failure ≠ weak: budget stays at the initial 2.4 Mbps cap, no UI nudge.
      expect(videoSenderOf(pc).params.encodings[0].maxBitrate).toBe(2_400_000);
      expect(onWeak).not.toHaveBeenCalled();
      m.destroy();
    } finally {
      vi.useRealTimers();
    }
  });
});
