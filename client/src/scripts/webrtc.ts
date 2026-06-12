// WebRTC full-mesh manager: one RTCPeerConnection per remote peer. Existing
// peers initiate offers toward a newcomer (avoids offer glare).

const ICE_SERVERS: RTCIceServer[] = [
  { urls: 'stun:stun.l.google.com:19302' },
  { urls: 'stun:stun1.l.google.com:19302' },
];

/** Floor for the per-stream video cap (spec 0031): below this, video is too
 *  degraded to be worth more dividing — we just send the floor to each peer. */
const MIN_VIDEO_BITRATE = 200_000;

type Signal =
  | { type: 'offer'; to: string; sdp: string }
  | { type: 'answer'; to: string; sdp: string }
  | { type: 'ice'; to: string; candidate: RTCIceCandidateInit };

export class MeshManager {
  private peers = new Map<string, RTCPeerConnection>();
  private localStream: MediaStream;
  private send: (s: Signal) => void;
  private iceServers: RTCIceServer[];
  private videoBudget: number;
  private statsTimer: ReturnType<typeof setInterval> | null = null;

  onRemoteStream: (peerId: string, stream: MediaStream) => void = () => {};
  onPeerRemoved: (peerId: string) => void = () => {};
  /** Fired when getStats reports the uplink can't keep up (bandwidth-limited or
   *  high packet loss) for a sustained window — the UI nudges the camera off. */
  onNetworkWeak: () => void = () => {};

  constructor(
    localStream: MediaStream,
    send: (s: Signal) => void,
    iceServers: RTCIceServer[] = ICE_SERVERS,
    videoBudget = 2_400_000,
  ) {
    this.localStream = localStream;
    this.send = send;
    this.iceServers = iceServers;
    this.videoBudget = videoBudget;
  }

  /** Replace the local stream's tracks on all peers (e.g. after a device change). */
  setLocalStream(stream: MediaStream): void {
    this.localStream = stream;
    for (const pc of this.peers.values()) {
      const senders = pc.getSenders();
      for (const track of stream.getTracks()) {
        const sender = senders.find((s) => s.track && s.track.kind === track.kind);
        if (sender) void sender.replaceTrack(track);
      }
    }
  }

  /**
   * Swap the outgoing video track on every peer (pass null to clear it). Works
   * even when the camera was never on, because addPeer() always negotiates a
   * video m-line — so screen sharing no longer depends on the camera being
   * active. No renegotiation needed (replaceTrack reuses the existing sender).
   */
  replaceVideoTrack(track: MediaStreamTrack | null): void {
    for (const pc of this.peers.values()) {
      const sender = this.videoSender(pc);
      if (sender) void sender.replaceTrack(track);
    }
  }

  /** The RTCRtpSender for our outgoing video, even if it has no track yet. */
  private videoSender(pc: RTCPeerConnection): RTCRtpSender | null {
    const tx = pc.getTransceivers?.().find(
      (t) => (t.sender.track?.kind ?? t.receiver?.track?.kind) === 'video',
    );
    if (tx) return tx.sender;
    // Fallback for environments without getTransceivers: a sender that
    // currently carries a video track.
    return pc.getSenders().find((s) => s.track?.kind === 'video') ?? null;
  }

  async addPeer(peerId: string, isInitiator: boolean): Promise<void> {
    if (this.peers.has(peerId)) return;
    const pc = new RTCPeerConnection({ iceServers: this.iceServers });
    this.peers.set(peerId, pc);

    for (const track of this.localStream.getTracks()) {
      pc.addTrack(track, this.localStream);
    }
    // Guarantee an outgoing video m-line even on audio-only joins, so screen
    // share (or turning the camera on later) only needs replaceTrack — no
    // renegotiation, and no dependency on the camera being on when you join.
    // `streams` ties the (initially empty) video sender to the same MediaStream
    // as the audio, so the remote groups the screen track into one stream once
    // it starts flowing — otherwise its ontrack sees no stream.
    if (this.localStream.getVideoTracks().length === 0) {
      pc.addTransceiver?.('video', { direction: 'sendrecv', streams: [this.localStream] });
    }
    // Re-balance outbound video across all peers (spec 0030/0031): the per-stream
    // cap is the upload budget ÷ peer count, so the total uplink stays ~constant as
    // the room fills; the browser's congestion control reduces further if needed.
    void this.applyBitrate();
    this.startStatsMonitor();

    pc.ontrack = (e) => {
      // Ignore receiver tracks that arrive without a stream (e.g. an inactive
      // video m-line before its msid is known) — they'd clobber the live stream.
      if (e.streams[0]) this.onRemoteStream(peerId, e.streams[0]);
    };
    pc.onicecandidate = (e) => {
      if (e.candidate) this.send({ type: 'ice', to: peerId, candidate: e.candidate.toJSON() });
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === 'failed' || pc.connectionState === 'closed') {
        this.removePeer(peerId);
      }
    };

    if (isInitiator) {
      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      this.send({ type: 'offer', to: peerId, sdp: offer.sdp! });
    }
  }

  async handleOffer(fromId: string, sdp: string): Promise<void> {
    if (!this.peers.has(fromId)) await this.addPeer(fromId, false);
    const pc = this.peers.get(fromId);
    if (!pc) return;
    await pc.setRemoteDescription({ type: 'offer', sdp });
    const answer = await pc.createAnswer();
    await pc.setLocalDescription(answer);
    this.send({ type: 'answer', to: fromId, sdp: answer.sdp! });
  }

  async handleAnswer(fromId: string, sdp: string): Promise<void> {
    const pc = this.peers.get(fromId);
    if (pc) await pc.setRemoteDescription({ type: 'answer', sdp });
  }

  async handleIce(fromId: string, candidate: RTCIceCandidateInit): Promise<void> {
    const pc = this.peers.get(fromId);
    if (!pc) return;
    try {
      await pc.addIceCandidate(candidate);
    } catch {
      /* ignore late/duplicate candidates */
    }
  }

  removePeer(peerId: string): void {
    const pc = this.peers.get(peerId);
    if (pc) {
      pc.close();
      this.peers.delete(peerId);
      // Fewer peers → more budget per remaining stream (spec 0031).
      void this.applyBitrate();
    }
    this.onPeerRemoved(peerId);
  }

  setAudioEnabled(enabled: boolean): void {
    this.localStream.getAudioTracks().forEach((t) => (t.enabled = enabled));
  }

  setVideoEnabled(enabled: boolean): void {
    this.localStream.getVideoTracks().forEach((t) => (t.enabled = enabled));
  }

  /** Per-stream target = the total upload budget split across the peers we send
   *  to, floored so video stays usable. As the room grows each stream gets less,
   *  so total uplink stays ~constant regardless of N (spec 0031). */
  private targetBitrate(): number {
    return Math.max(
      MIN_VIDEO_BITRATE,
      Math.floor(this.videoBudget / Math.max(1, this.peers.size)),
    );
  }

  /** Re-apply the current per-stream cap to every peer's video sender. Called when
   *  the peer count changes (join/leave) so the room re-balances (spec 0031). */
  private async applyBitrate(): Promise<void> {
    const target = this.targetBitrate();
    for (const pc of this.peers.values()) {
      try {
        const sender =
          pc.getSenders().find((s) => s.track?.kind === 'video') ?? this.videoSender(pc);
        if (!sender) continue;
        const params = sender.getParameters();
        if (!params.encodings || params.encodings.length === 0) params.encodings = [{}];
        params.encodings[0].maxBitrate = target;
        await sender.setParameters(params);
      } catch {
        /* unsupported / pre-negotiation / fake env — ignore */
      }
    }
  }

  /** Poll getStats across peers; fire `onNetworkWeak` once when the uplink is
   *  bandwidth-limited or lossy for two consecutive checks (spec 0030). */
  private startStatsMonitor(): void {
    if (this.statsTimer != null) return;
    let weakStreak = 0;
    this.statsTimer = setInterval(() => {
      void (async () => {
        let weak = false;
        for (const pc of this.peers.values()) {
          try {
            const stats = await pc.getStats();
            stats.forEach((r: unknown) => {
              const s = r as Record<string, unknown>;
              if (
                s.type === 'outbound-rtp' &&
                s.kind === 'video' &&
                s.qualityLimitationReason === 'bandwidth'
              )
                weak = true;
              if (s.type === 'remote-inbound-rtp' && ((s.fractionLost as number) ?? 0) > 0.08)
                weak = true;
            });
          } catch {
            /* ignore a transient getStats failure */
          }
        }
        weakStreak = weak ? weakStreak + 1 : 0;
        if (weakStreak >= 2) {
          weakStreak = 0;
          this.onNetworkWeak();
        }
      })();
    }, 5000);
  }

  destroy(): void {
    if (this.statsTimer != null) {
      clearInterval(this.statsTimer);
      this.statsTimer = null;
    }
    this.peers.forEach((pc) => pc.close());
    this.peers.clear();
  }
}
