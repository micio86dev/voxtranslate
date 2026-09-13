// WebRTC full-mesh manager: one RTCPeerConnection per remote peer.
//
// Negotiation uses the WebRTC "perfect negotiation" pattern (MDN): each pair has
// a deterministic *polite* / *impolite* role derived from the two peer ids, so
// simultaneous offers (glare) and stray answers can never wedge the connection.
// This matters most on reconnects, where both peers can renegotiate at once — the
// old code threw `setRemoteDescription ... wrong state: stable` and the video
// stayed black. We also ICE-restart a dropped peer instead of tearing it down, so
// a transient network blip recovers on its own once both sides are back online.

const ICE_SERVERS: RTCIceServer[] = [
  { urls: 'stun:stun.l.google.com:19302' },
  { urls: 'stun:stun1.l.google.com:19302' },
];

/** The `RTCPeerConnection` constructor, with the legacy `webkit`-prefixed fallback.
 *  Resolved lazily (not at module load) so a stripped environment — an in-app
 *  browser / restricted WebView, or a privacy setup that deletes the global — can't
 *  crash the whole bundle on import. `undefined` when the browser exposes neither;
 *  the join is gated on this up front in app.ts (`webrtcSupported`) so users see a
 *  clear "open in Chrome/Safari" message instead of an uncaught
 *  "RTCPeerConnection is not a constructor" crash here in createPeer. */
function rtcPeerConnectionCtor(): typeof RTCPeerConnection | undefined {
  const g = globalThis as unknown as {
    RTCPeerConnection?: typeof RTCPeerConnection;
    webkitRTCPeerConnection?: typeof RTCPeerConnection;
  };
  return g.RTCPeerConnection ?? g.webkitRTCPeerConnection;
}

/** Floor for the per-stream video cap (spec 0031): below this, video is too
 *  degraded to be worth more dividing — we just send the floor to each peer. */
const MIN_VIDEO_BITRATE = 200_000;

/** Grace period before we ICE-restart a peer that went `disconnected`. ICE often
 *  self-heals (a brief blip / NAT rebind) within a couple of seconds, so we only
 *  force a restart if it's still down after this. `failed` restarts immediately. */
const DISCONNECT_GRACE_MS = 2_000;

type Signal =
  | { type: 'offer'; to: string; sdp: string }
  | { type: 'answer'; to: string; sdp: string }
  | { type: 'ice'; to: string; candidate: RTCIceCandidateInit };

/** Per-peer connection + the small amount of perfect-negotiation bookkeeping. */
interface PeerState {
  id: string;
  pc: RTCPeerConnection;
  /** The polite peer yields on glare (rolls back its offer); the impolite one
   *  ignores the colliding offer. Derived from the id pair so the two sides
   *  always pick opposite roles. */
  polite: boolean;
  /** True while we're creating/applying a local offer — half of glare detection. */
  makingOffer: boolean;
  /** Set when we (impolite) drop a colliding offer, so its trailing ICE
   *  candidates failing to apply is expected, not an error. */
  ignoreOffer: boolean;
}

export class MeshManager {
  private peers = new Map<string, PeerState>();
  private localStream: MediaStream;
  private send: (s: Signal) => void;
  private iceServers: RTCIceServer[];
  /** When set (to `'relay'`), every peer connection is forced through TURN,
   *  skipping host/srflx candidates. Used for Great-Firewall-restricted clients,
   *  whose direct UDP candidates the GFW resets anyway — going straight to the
   *  `turns://…:443` relay avoids stalling on doomed candidate pairs. Left
   *  `undefined` for everyone else so behavior is unchanged (browser default `all`). */
  private iceTransportPolicy?: RTCIceTransportPolicy;
  private videoBudget: number;
  /** This client's own peer id — compared against each remote id to pick the
   *  polite/impolite role for perfect negotiation. */
  private localId: string;
  /** Live budget (spec 0032): backs off under a struggling uplink, recovers toward
   *  `videoBudget` when healthy. `targetBitrate()` divides THIS across the peers. */
  private currentBudget: number;
  private statsTimer: ReturnType<typeof setInterval> | null = null;
  /** The stream handed to the UI for each peer. Kept because a remote track can
   *  arrive with NO stream attached and has to be added to something — see the
   *  `ontrack` handler for why that case is not rare. */
  private remoteStreams = new Map<string, MediaStream>();

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
    localId = '',
    iceTransportPolicy?: RTCIceTransportPolicy,
  ) {
    this.localStream = localStream;
    this.send = send;
    this.iceServers = iceServers;
    this.iceTransportPolicy = iceTransportPolicy;
    this.videoBudget = videoBudget;
    this.currentBudget = videoBudget;
    this.localId = localId;
  }

  /** Replace the local stream's tracks on all peers (e.g. after a device change). */
  setLocalStream(stream: MediaStream): void {
    this.localStream = stream;
    for (const { pc } of this.peers.values()) {
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
    for (const { pc } of this.peers.values()) {
      const sender = this.videoSender(pc);
      if (sender) void sender.replaceTrack(track);
    }
  }

  /**
   * Swap the outgoing audio track on every peer. Used to send a mic+screen-audio
   * mix while sharing a tab/window with audio, then revert to the plain mic track
   * on stop (spec 0085). replaceTrack reuses the existing audio sender — no
   * renegotiation.
   */
  replaceAudioTrack(track: MediaStreamTrack | null): void {
    for (const { pc } of this.peers.values()) {
      const sender = pc.getSenders().find((s) => s.track?.kind === 'audio') ?? null;
      if (sender) void sender.replaceTrack(track);
    }
  }

  /** The RTCRtpSender for our outgoing video, even if it has no track yet.
   *
   *  Associated transceivers first — one with a `mid` is attached to a real
   *  m-line, one without is attached to nothing. Putting the screen track on an
   *  orphan is silent: `replaceTrack` resolves happily and the media goes nowhere.
   *  See `ensureSendable` for how an orphan comes about. */
  private videoSender(pc: RTCPeerConnection): RTCRtpSender | null {
    const isVideo = (t: RTCRtpTransceiver) =>
      (t.sender.track?.kind ?? t.receiver?.track?.kind) === 'video';
    const all = pc.getTransceivers?.() ?? [];
    const tx = all.find((t) => isVideo(t) && t.mid != null) ?? all.find(isVideo);
    if (tx) return tx.sender;
    // Fallback for environments without getTransceivers: a sender that
    // currently carries a video track.
    return pc.getSenders().find((s) => s.track?.kind === 'video') ?? null;
  }

  /**
   * Make every m-line we might later send on `sendrecv`, before answering.
   *
   * `addPeer` pre-creates an empty audio/video transceiver so a screen share (or
   * turning the camera on) needs only `replaceTrack` — no renegotiation. That works
   * when WE offer. When the REMOTE offers, Chrome does not reuse our pre-created
   * transceiver: it makes a fresh `recvonly` one for the remote m-line and leaves
   * ours unassociated (`mid: null`). The answer then says `a=recvonly`, so nothing
   * we ever put on that connection can be sent, and `replaceTrack` lands on an
   * orphan attached to no m-line. Both failures are completely silent.
   *
   * Which side offers is decided by the polite/impolite role, i.e. by comparing two
   * random UUIDs — so this broke a camera-less peer's screen share almost exactly
   * half the time. That is the `screenshare.spec` flake, and it is the same defect
   * issue #4 was about, reappearing through its own fix.
   *
   * Upgrading before `createAnswer` costs nothing when there is nothing to send: an
   * m-line with no track sends no media. It only buys back the ability to start.
   */
  private ensureSendable(pc: RTCPeerConnection): void {
    for (const t of pc.getTransceivers?.() ?? []) {
      if (t.direction === 'recvonly' || t.direction === 'inactive') {
        try {
          t.direction = 'sendrecv';
        } catch {
          /* a stopped transceiver refuses — nothing to send on it anyway */
        }
      }
    }
  }

  /**
   * Add a peer to the mesh. The second arg is legacy (the old explicit-initiator
   * flag) and is ignored: who sends the first offer is now decided by the polite/
   * impolite role, so a server message-ordering race can't leave both sides
   * waiting (deadlock → black screen) or both offering (glare → wrong-state error).
   */
  async addPeer(peerId: string, _isInitiator?: boolean): Promise<void> {
    const existing = this.peers.get(peerId);
    if (existing) {
      // Re-add of a peer we still hold = their socket reconnected with a fresh
      // PeerConnection on their side, so ours is now dead. Replace it (keeping
      // their tile) instead of ignoring the event — ignoring left their video
      // permanently black when only one side dropped (this is the bug).
      existing.pc.close();
      this.peers.delete(peerId);
    }
    const peer = this.createPeer(peerId);
    // The impolite peer kicks off negotiation; the polite peer waits for the
    // offer. Exactly one side offers in the common case (no glare), yet either
    // side can recover from a race because handleOffer/handleAnswer are
    // collision-safe.
    if (!peer.polite) await this.negotiate(peer);
  }

  /** Build the RTCPeerConnection + state and wire its event handlers, WITHOUT
   *  sending an offer. Used by addPeer and by handleOffer when an offer arrives
   *  for a peer we haven't set up yet. */
  private createPeer(peerId: string): PeerState {
    const Ctor = rtcPeerConnectionCtor();
    if (!Ctor) {
      // Should be unreachable: callers gate the join on `webrtcSupported()`. Throw
      // a clear error rather than the opaque "RTCPeerConnection is not a constructor".
      throw new Error('WebRTC is not supported in this browser');
    }
    // Only set iceTransportPolicy when forcing relay — omitting the key keeps the
    // config byte-for-byte identical to the default (unrestricted) path.
    const pc = new Ctor({
      iceServers: this.iceServers,
      ...(this.iceTransportPolicy ? { iceTransportPolicy: this.iceTransportPolicy } : {}),
      // Pre-gather up to 2 ICE candidates before the offer is created so the
      // browser has STUN/TURN reflexive candidates ready immediately — reduces
      // the ICE establishment time (no cold-start 5-second STUN timeout before
      // TURN fallback) which is the primary cause of multi-second call latency.
      iceCandidatePoolSize: 2,
    });
    const peer: PeerState = {
      id: peerId,
      pc,
      // Deterministic, opposite on the two ends: the lexicographically-greater
      // id is polite. (Ids are random UUIDs, so they're never equal.)
      polite: this.localId > peerId,
      makingOffer: false,
      ignoreOffer: false,
    };
    this.peers.set(peerId, peer);

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
    // Same guarantee for AUDIO: a peer who joined without a mic (denied or
    // audio-only-off) otherwise has no audio sender, so a later tab/screen audio
    // share couldn't reach anyone (replaceTrack has nothing to swap). Ensure an
    // audio m-line exists up front so share audio flows with no renegotiation (#229).
    if (this.localStream.getAudioTracks().length === 0) {
      pc.addTransceiver?.('audio', { direction: 'sendrecv', streams: [this.localStream] });
    }
    // Re-balance outbound video across all peers (spec 0030/0031): the per-stream
    // cap is the upload budget ÷ peer count, so the total uplink stays ~constant as
    // the room fills; the browser's congestion control reduces further if needed.
    void this.applyBitrate();
    this.startStatsMonitor();

    pc.ontrack = (e) => {
      // One stream per peer, owned by us, and tracks are only ever ADDED to it.
      //
      // Two things made the previous shape lose media. A remote track can arrive
      // with NO stream — the video m-line we add up front for a camera-less join
      // (see above) carries no media yet, so the browser may report no msid and
      // hand us the track alone — and dropping it was silent AND permanent, because
      // when that peer later starts a screen share `replaceTrack` puts real video on
      // the SAME receiver and fires NO second `ontrack`. The screen reached the
      // transport and never reached the UI. And taking whichever stream arrived last
      // was just as lossy in the other order: an announced audio-only stream landing
      // after a stream-less video track threw the video away again.
      //
      // Adding to one stable stream is order-independent, which is what makes it
      // right — the previous versions were both races, and that is exactly how
      // `screenshare.spec` came to fail about half the time.
      let stream = this.remoteStreams.get(peerId);
      if (!stream) {
        stream = new MediaStream();
        this.remoteStreams.set(peerId, stream);
      }
      const incoming = e.streams[0] ? e.streams[0].getTracks() : e.track ? [e.track] : [];
      for (const t of incoming) {
        if (!stream.getTracks().includes(t)) stream.addTrack(t);
      }
      // A track that is present but MUTED is an m-line with nothing flowing yet.
      // The moment it unmutes is the moment there is a picture to show, and it is
      // the only event the browser gives us for a `replaceTrack` on the far side —
      // so re-emit then, and let the UI decide what to render from the track state.
      if (e.track) {
        const reemit = () => {
          const current = this.remoteStreams.get(peerId);
          if (current) this.onRemoteStream(peerId, current);
        };
        e.track.addEventListener('unmute', reemit);
        e.track.addEventListener('mute', reemit);
      }
      this.onRemoteStream(peerId, stream);
    };
    pc.onicecandidate = (e) => {
      if (e.candidate) this.send({ type: 'ice', to: peerId, candidate: e.candidate.toJSON() });
    };
    // Recover instead of tearing down: a network blip drives ICE to
    // `disconnected`/`failed`, but the peer is still in the room, so we
    // renegotiate with an ICE restart rather than dropping the cell (which left
    // the video permanently black). Removal happens only on an explicit
    // `peer_left` from the server (→ removePeer).
    pc.oniceconnectionstatechange = () => {
      const st = pc.iceConnectionState;
      if (st === 'connected' || st === 'completed') {
        // Minimize receiver jitter buffer to reduce A/V playback latency. The
        // browser adaptive jitter buffer can grow to several seconds under packet
        // loss; setting jitterBufferTarget = 0 tells it to use the minimum
        // possible buffering. Feature-detected — not yet in all browsers.
        for (const receiver of pc.getReceivers()) {
          if ('jitterBufferTarget' in receiver) {
            receiver.jitterBufferTarget = 0;
          }
        }
      } else if (st === 'failed') {
        void this.negotiate(peer, true);
      } else if (st === 'disconnected') {
        setTimeout(() => {
          const now = pc.iceConnectionState;
          if (now === 'disconnected' || now === 'failed') void this.negotiate(peer, true);
        }, DISCONNECT_GRACE_MS);
      }
    };

    return peer;
  }

  /** Create + send an offer for `peer`, guarded so it's glare-safe. `iceRestart`
   *  re-gathers ICE candidates to recover a dropped connection. */
  private async negotiate(peer: PeerState, iceRestart = false): Promise<void> {
    const { pc, id } = peer;
    try {
      peer.makingOffer = true;
      const offer = await pc.createOffer(iceRestart ? { iceRestart: true } : undefined);
      // A remote offer may have raced in while we were creating ours; let the
      // collision handling in handleOffer settle it rather than forcing a bad
      // local description.
      if (pc.signalingState !== 'stable') return;
      await pc.setLocalDescription(offer);
      this.send({ type: 'offer', to: id, sdp: pc.localDescription?.sdp ?? offer.sdp! });
    } catch {
      /* offer failed (pre-negotiation / fake env / transient) — ignore */
    } finally {
      peer.makingOffer = false;
    }
  }

  async handleOffer(fromId: string, sdp: string): Promise<void> {
    const peer = this.peers.get(fromId) ?? this.createPeer(fromId);
    const pc = peer.pc;
    // Glare: an offer arrived while we have one in flight (or aren't stable).
    const collision = peer.makingOffer || pc.signalingState !== 'stable';
    peer.ignoreOffer = !peer.polite && collision;
    if (peer.ignoreOffer) return; // impolite peer keeps its own offer
    if (collision) {
      // Polite peer yields: drop our pending offer, then take theirs.
      await pc.setLocalDescription({ type: 'rollback' } as RTCLocalSessionDescriptionInit).catch(
        () => {},
      );
    }
    await pc.setRemoteDescription({ type: 'offer', sdp });
    this.ensureSendable(pc);
    const answer = await pc.createAnswer();
    await pc.setLocalDescription(answer);
    this.send({ type: 'answer', to: fromId, sdp: pc.localDescription?.sdp ?? answer.sdp! });
  }

  async handleAnswer(fromId: string, sdp: string): Promise<void> {
    const peer = this.peers.get(fromId);
    if (!peer) return;
    // Only apply an answer to an offer we're actually waiting on. A stray /
    // duplicate / post-rollback answer would otherwise throw
    // `setRemoteDescription ... wrong state: stable` and wedge the connection.
    if (peer.pc.signalingState !== 'have-local-offer') return;
    await peer.pc.setRemoteDescription({ type: 'answer', sdp });
  }

  async handleIce(fromId: string, candidate: RTCIceCandidateInit): Promise<void> {
    const peer = this.peers.get(fromId);
    if (!peer) return;
    try {
      await peer.pc.addIceCandidate(candidate);
    } catch (err) {
      // Candidates trailing an offer we deliberately ignored (glare) can't apply
      // — that's expected. Anything else is a genuinely late/duplicate candidate.
      if (!peer.ignoreOffer) {
        /* ignore late/duplicate candidates */
      }
    }
  }

  removePeer(peerId: string): void {
    const peer = this.peers.get(peerId);
    // A peer who leaves and rejoins gets a fresh connection; keeping their old
    // stream would have the new one's stream-less tracks land in a dead object.
    this.remoteStreams.delete(peerId);
    if (peer) {
      peer.pc.close();
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

  /** Replace the upload budget at runtime and re-apply the per-stream cap now.
   *  Used to raise the cap while screen sharing — shared text/UI stays sharp
   *  instead of grainy — and to restore the camera budget on stop (spec 0088).
   *  Network adaptation (spec 0032) continues from the new value. */
  setVideoBudget(budget: number): void {
    this.videoBudget = budget;
    this.currentBudget = budget;
    void this.applyBitrate();
  }

  /** Per-stream target = the total upload budget split across the peers we send
   *  to, floored so video stays usable. As the room grows each stream gets less,
   *  so total uplink stays ~constant regardless of N (spec 0031). */
  private targetBitrate(): number {
    return Math.max(
      MIN_VIDEO_BITRATE,
      Math.floor(this.currentBudget / Math.max(1, this.peers.size)),
    );
  }

  /** Re-apply the current per-stream cap to every peer's video sender. Called when
   *  the peer count changes (join/leave) so the room re-balances (spec 0031). */
  private async applyBitrate(): Promise<void> {
    const target = this.targetBitrate();
    for (const { pc } of this.peers.values()) {
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
        for (const { pc } of this.peers.values()) {
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
        // Adapt the budget (spec 0032): multiplicative decrease when the uplink
        // struggles, gentle increase back toward the max when it's healthy. The
        // per-stream floor + the browser's own congestion control still apply.
        const before = this.currentBudget;
        this.currentBudget = weak
          ? Math.max(MIN_VIDEO_BITRATE, Math.floor(this.currentBudget * 0.75))
          : Math.min(this.videoBudget, Math.floor(this.currentBudget * 1.2));
        if (this.currentBudget !== before) void this.applyBitrate();

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
    this.peers.forEach(({ pc }) => pc.close());
    this.peers.clear();
  }
}
