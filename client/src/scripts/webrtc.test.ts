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
  addTrack(track: any) {
    const sender: any = { track, replaceTrack: vi.fn(async (t: any) => { sender.track = t; }) };
    this.senders.push(sender);
    this.transceivers.push({ sender, receiver: { track: { kind: track.kind } } });
  }
  addTransceiver(kind: string) {
    const sender: any = { track: null, replaceTrack: vi.fn(async (t: any) => { sender.track = t; }) };
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
  async createOffer(opts?: any) {
    this.offerOpts.push(opts);
    return { type: 'offer', sdp: 'offer-sdp' };
  }
  async createAnswer() {
    return { type: 'answer', sdp: 'answer-sdp' };
  }
  async setLocalDescription(d: any) {
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

    await m.handleIce('p2', { c: 2 });
    expect(pc.addIceCandidate).toHaveBeenCalled();
    pc.addIceCandidate.mockRejectedValueOnce(new Error('bad'));
    await m.handleIce('p2', { c: 3 }); // swallowed, no throw

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
});
