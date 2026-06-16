# 0080 — WebRTC reconnect recovery (no more permanent black screen)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Micio Dev |
| **Created** | 2026-06-16 |
| **Shipped** | 2026-06-16 |
| **Version** | 1.1.x |
| **Commits** | `0e36c82` |
| **Depends on** | [0026](../0026-turn-relay/spec.md), [0002](../0002-video-calls-translated-chat/spec.md) |

## 1. Context & Problem

When a participant hit a network blip — the connection-status toast (spec
[net-status]) appears — their video tile went **black and stayed black**. With two
people in a room, after the blip they could no longer see each other (only their
own self-view), **even after both browsers were back online**. The Chrome console
showed:

```
Uncaught (in promise) InvalidStateError: Failed to execute 'setRemoteDescription'
on 'RTCPeerConnection': Failed to set remote answer sdp: Called in wrong state: stable.
```

Root cause was two compounding bugs across the stack:

1. **Server — same-id reconnect created a duplicate / nuked the live peer.** There
   is no WS heartbeat, so on an abrupt network loss the server keeps the stale
   peer entry for a long time (until TCP times out). The client reconnects reusing
   the **same peer id**, but `RoomManager::join` did a bare `peers.push` with no
   same-id dedup. Result: two entries share one id, so `relay_to_peer` (a
   first-match `find`) relayed offers/answers/ICE to the **dead socket** → the live
   client never finished negotiating → black screen. Worse, when the stale socket
   finally timed out, its teardown ran `remove(room, id)` = `retain(|p| p.id != id)`,
   which deleted **both** entries (including the live reconnect) and broadcast
   `PeerLeft` → the other peer dropped the tile → "you only see yourself".

2. **Client — the mesh couldn't survive glare or an ICE drop.** `MeshManager`
   tore a peer down on `connectionState === 'failed'` with no recovery path, and
   it had no glare handling, so a simultaneous renegotiation (common on reconnect)
   threw `setRemoteDescription … wrong state: stable` and wedged the connection.
   A re-`addPeer` of a peer it still held was ignored, so the side that *stayed*
   online never rebuilt its dead connection to the side that reconnected.

## 2. Goals / Non-Goals

**Goals**
- A transient network drop (one side or both) recovers automatically once
  connectivity returns: peers see each other again with no manual rejoin.
- Eliminate the `setRemoteDescription … wrong state: stable` error.
- A same-id reconnect supersedes its own stale server entry; signaling always
  reaches the live socket and the stale teardown never drops the live peer.

**Non-Goals**
- A full WS heartbeat/ping-pong keepalive (the eviction-on-rejoin makes it
  unnecessary for this bug; tracked as a follow-up).
- Media continuity *during* the outage (the tile may freeze while offline; it must
  recover, not stay black).
- Changing the mesh topology or the TURN/STUN configuration.

## 3. Requirements

- **R1 — Both peers drop and recover.** *Given* two peers in a room, *when* both
  lose the network and both come back online, *then* they see each other's video
  again automatically.
- **R2 — One peer drops and recovers.** *Given* peer A drops while peer B stays
  online, *when* A reconnects, *then* B's tile for A and A's tile for B both go
  live again (B rebuilds its stale connection on A's re-join).
- **R3 — No wedging error.** *Given* simultaneous (glare) renegotiation, *when*
  offers collide or a stray answer arrives, *then* the connection still
  establishes and no `wrong state: stable` error is thrown.
- **R4 — Reconnect is not self-blocked.** *Given* a peer's stale entry still
  occupies the room (even at capacity), *when* it reconnects with the same id,
  *then* the join succeeds (its own ghost is evicted first) and `existing` lists
  only real peers.
- **R5 — No spurious leave.** *Given* a stale connection superseded by a
  reconnect, *when* its delayed teardown runs, *then* no `PeerLeft` is broadcast
  and the live peer is untouched.

## 4. Design & Architecture

- **Components / files:**
  - `client/src/scripts/webrtc.ts` — `MeshManager` rewritten to the WebRTC
    **perfect-negotiation** pattern + **ICE restart** recovery.
  - `client/src/scripts/app.ts` — pass `myId` to the mesh; `mesh.destroy()` the
    old mesh on WS reconnect before building a fresh one.
  - `server/src/rooms.rs` — `Peer.conn` (per-connection UUID); `join` evicts a
    stale same-id entry; `remove(id, conn)` returns `LeaveOutcome`.
  - `server/src/lib.rs` — generate `conn`; teardown branches on `LeaveOutcome`.

- **Key decisions:**
  - **Perfect negotiation** (MDN). Each pair derives a deterministic *polite* /
    *impolite* role from the two peer ids (`polite = localId > peerId`), so the
    roles are always opposite. The impolite peer sends the first offer; on glare
    the polite peer rolls back and the impolite peer ignores the colliding offer.
    `handleAnswer` applies an answer only in `have-local-offer`, which kills the
    `wrong state: stable` error. *Alternative rejected:* keeping the explicit
    server-driven initiator flag — it can't resolve glare and deadlocks when the
    server's message ordering races on reconnect.
  - **ICE restart over teardown.** On `iceConnectionState` `failed` (or a
    still-`disconnected` peer after a 2 s grace) the mesh renegotiates with
    `createOffer({ iceRestart: true })` instead of removing the tile. Removal now
    happens only on an explicit `peer_left`.
  - **Replace on re-`addPeer`.** Re-adding a peer the mesh still holds means their
    socket reconnected; close the dead pc and build a fresh one (R2).
  - **Per-connection id on the server.** `remove` matches `(id, conn)`, so a stale
    connection's late teardown is a no-op (`Superseded`) and can't evict the live
    reconnect (R5). `join` evicts any same-id ghost before the capacity check and
    push (R4) and signals its handler to close promptly (no heartbeat needed).

- **Sequence (R1, both reconnect):** WS reopens → `mesh.destroy()` → new
  `MeshManager(localId)` → server `join` evicts ghost, returns real `existing` →
  `room_joined`/`peer_joined` → `addPeer` per peer → impolite side offers →
  perfect negotiation → ICE connects → `ontrack` → tiles live.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Perfect-negotiation `MeshManager` (polite/impolite, glare rollback, answer-state guard) | `client/src/scripts/webrtc.ts` |
| S1 | ICE restart on `failed` / grace-`disconnected`; replace on re-add; stop removing on `failed` | `client/src/scripts/webrtc.ts` |
| S2 | Pass `myId`; destroy stale mesh on reconnect | `client/src/scripts/app.ts` |
| S3 | `Peer.conn`, evict-on-join, `remove(id, conn) -> LeaveOutcome`, `signal_close` | `server/src/rooms.rs` |
| S4 | Generate `conn`; teardown honours `LeaveOutcome` (suppress spurious `PeerLeft`) | `server/src/lib.rs` |

## 6. Testing & Verification

- **Client (`webrtc.test.ts`, 12 tests):** impolite-offers / polite-waits;
  answerer flow; **answer applied only when an offer is pending** (R3); glare from
  both the impolite and polite side (R3); **ICE restart on `failed`** and after a
  grace-period `disconnected`, without removing the peer (R1/R2); restart skipped
  when the peer self-heals; **replace on re-add** (R2). `FakePC` now tracks
  `signalingState`/`iceConnectionState` so the logic is exercised for real.
- **Server (`rooms.rs`):** `reconnect_supersedes_stale_entry_without_dropping_the_live_one`
  — fills the room to capacity, reconnects a same-id peer, asserts the join is
  accepted with real `existing` (R4), relays reach the live socket, the stale
  teardown returns `Superseded` and keeps the live peer (R5), and the live
  teardown removes it for real.
- Full suites green: client 210/210 + `astro check`; server `cargo test` +
  `cargo clippy --all-targets` + `cargo fmt --check`.

## 7. Deployment & Operations

- No env vars, migrations, or feature flags. Pure client + server logic.
- Client auto-deploys to Vercel on `main`; server auto-deploys to Railway on
  `main` merge. No coordinated rollout needed — the server change is
  backward-compatible with older clients (they simply don't benefit from the
  client-side recovery).

## 8. Risks / Open Items

- No WS heartbeat still means a *silently* dead socket lingers until TCP timeout
  if the peer never reconnects; eviction-on-rejoin covers the reconnect path, but
  a dedicated ping/idle-timeout is a worthwhile follow-up.
- Reconnect creates a fresh transcript participant row + usage session per
  reconnection (pre-existing behaviour; acceptable, billed to actual usage).

## 9. References

- MDN — *Establishing a connection: the WebRTC perfect negotiation pattern.*
- Symptom: `setRemoteDescription … Called in wrong state: stable`.
