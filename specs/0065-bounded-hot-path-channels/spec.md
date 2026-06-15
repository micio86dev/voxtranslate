# 0065 — Bounded hot-path channels (out_tx / audio_tx + backpressure)

| | |
|---|---|
| **Status** | In progress |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-15 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | `<sha>` |
| **Depends on** | [0064](../0064-high-traffic-abuse-hardening/spec.md), [0012](../0012-auto-language-detection/spec.md), [0050](../0050-observability/spec.md) |

## 1. Context & Problem

The high-traffic audit (issue #114) flagged the two **hot-path channels** in
`server/src/lib.rs` as the last deferred item of the abuse-hardening work
(#117), tracked as **#123**. Both were `mpsc::unbounded_channel`:

- **`out_tx`** (`String`) — the per-peer server→WS sender. Carries signalling,
  chat, subtitles, whiteboard/game snapshots, translations, balance/state. A
  peer whose socket stops draining (dead/slow reader) makes `pump_to_ws` stall,
  so frames accumulate **without bound** → memory growth on the single Railway
  instance. **Dropping a text frame breaks correctness** (signalling/chat desync).
- **`audio_tx`** (`Vec<u8>`) — the per-speaking-session audio→Deepgram sender
  (plus the auto-detect bridge `dg_tx`). If Deepgram's sink backpressures, chunks
  accumulate without bound. The bytes are a **continuous WebM/Opus container**, so
  dropping a mid-stream chunk would corrupt the stream for the rest of the session.

This was deferred as risky (the senders are used in many places) and low-payoff
(spec 0064 already caps global connections + per-connection message rate). It is
done now with **per-channel** drop/close policies, not a uniform one.

## 2. Goals / Non-Goals

**Goals**
- Both hot-path channels **bounded** → memory is bounded under a slow/stalled
  consumer, with a documented **per-channel** policy.
- `out_tx`: **never drop a control frame**; on persistent saturation, close the
  connection cleanly (full teardown), bounding memory to a fixed backlog.
- `audio_tx`: never corrupt the WebM stream; on saturation, **end the STT session
  cleanly** (resumes on the next `Start`) rather than drop mid-stream chunks.
- **Zero regression** to signalling, chat, snapshots, subtitles, auto-detect, and
  metering. All caps sit far above any legitimate burst.

**Non-Goals**
- The transcript DB-writer channel (`run_recorder`, `transcripts.rs`) — a
  different class (its consumer is Postgres, not a socket); a separate follow-up.
- An admission **semaphore** limiting concurrent Deepgram/Groq fan-out — desirable
  but a distinct change; tracked under #114/#123 as a follow-up. The bounded
  channels already cap per-session memory.
- Horizontal scale / shared room registry (#114, separate).

## 3. Requirements

- **R1 — Bounded outbound, no control-frame loss.** *Given* a peer's reader has
  stalled, *when* its outbound backlog reaches `OUT_CHANNEL_CAP`, *then* the next
  send signals overflow and the receive loop closes the connection via the normal
  teardown (PeerLeft broadcast, session finalize) — **no frame is silently dropped
  on a peer that stays connected**, and memory is bounded to the cap.
- **R2 — Prune still works.** *Given* a peer's receiver is gone, *when* a broadcast
  or relay targets it, *then* `send` reports closed and the peer is pruned (existing
  behaviour preserved).
- **R3 — Bounded audio, no stream corruption.** *Given* Deepgram backpressures past
  `AUDIO_CHANNEL_CAP` chunks, *when* the next audio frame arrives, *then* the STT
  session ends cleanly (sender dropped → `forward_audio` flushes `CloseStream`);
  no mid-stream chunk is dropped, and STT resumes on the next `Start`.
- **R4 — Auto-detect backpressure.** *Given* the auto-detect bridge forwards
  buffered + live chunks, *when* the Deepgram forwarder is slow, *then* the
  dedicated detector task **awaits** the bounded send (backpressures) rather than
  dropping; if the forwarder is gone the bridge ends. The REST probe window
  (<2 s) never overflows the cap.
- **R5 — No regression.** *Then* the full `cargo test` suite (incl. WS integration:
  signalling/mute/lobby, chat fan-out, `audio_produces_subtitles`, auto `set_lang`)
  stays green; `clippy -D warnings` and `fmt` clean.

## 4. Design & Architecture

- **Files:** `server/src/rooms.rs` (new `PeerTx`), `server/src/lib.rs` (channel
  creation, receive loop, audio sessions), `server/src/deepgram.rs`
  (`forward_audio` receiver type), `server/src/usage.rs` (meters take `PeerTx`).

- **`PeerTx` (rooms.rs):** a small newtype wrapping `mpsc::Sender<String>`
  (bounded) + an `Arc<Notify>` *overflow* signal. `Peer.tx` becomes `PeerTx`.
  - `send(msg) -> bool`: `try_send`; `Ok` → `true`; `Full` → drop the frame, but
    **`overflow.notify_one()`** and return `true` (keep the peer — it is torn down
    cleanly, never silently pruned mid-call); `Closed` → `false` (prune).
  - `is_closed()` delegates to the sender, so `prune` is unchanged.
  - `channel(cap) -> (PeerTx, Receiver<String>, Arc<Notify>)`. `handle_peer` keeps
    the `Arc<Notify>` and adds a `select!` arm `_ = out_overflow.notified() =>
    break` → the existing teardown runs.
  - **Why close, not drop:** a control frame (offer/answer/ICE/chat) is not
    droppable without desync; a reader that is `OUT_CHANNEL_CAP` frames behind is
    already broken for a real-time call, so a clean close (client reconnects) is
    the correct degradation.

- **Audio (lib.rs + deepgram.rs):** `start_speaking_session` /
  `start_detecting_session` create `mpsc::channel(AUDIO_CHANNEL_CAP)`;
  `forward_audio` takes `Receiver<Vec<u8>>`.
  - **Receive loop** (the `Message::Binary` arm) uses `try_send`; on `Full` *or*
    `Closed` it clears `audio_tx` → the sender drops → `forward_audio` flushes
    `CloseStream` and closes. It runs in the `select!` loop, so it must not block.
  - **Detector task** (`start_detecting_session`) is a *dedicated* task, so it
    `await`s the bounded `dg_tx.send` — natural backpressure: if Deepgram stalls,
    `audio_rx` fills to the cap and the receive loop ends the session. No drops.

- **Constants:** `OUT_CHANNEL_CAP = 512` (rooms.rs) — frames are tiny and
  `pump_to_ws` drains to the socket immediately, so a healthy backlog is ~0; 512
  is far above a 4-way join's signalling+snapshot burst, yet caps a dead socket to
  a few hundred KiB. `AUDIO_CHANNEL_CAP = 256` (lib.rs) — ~25 s of 100 ms chunks,
  far above the <2 s auto-detect probe pause, capping a stalled session to ~100 KiB.

- **Key decisions:**
  - *Per-channel policy, not uniform:* text = never-drop/close; audio =
    never-corrupt/end-session. Mirrors the data's correctness contract.
  - *`Notify` over a flag:* `notify_one` stores a permit, so an overflow that
    happens between `select!` polls still wakes the loop on the next iteration.
  - *Detector awaits, receive loop `try_send`s:* the only place blocking is safe is
    the dedicated task; the `select!` loop must stay responsive to control frames.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `PeerTx` newtype + `OUT_CHANNEL_CAP`; `Peer.tx: PeerTx`; broadcast/relay/prune use `bool` send | `server/src/rooms.rs` |
| S1 | `out_tx` = `PeerTx::channel`; `select!` overflow arm; `pump_to_ws` `Receiver`; meters take `PeerTx` | `server/src/lib.rs`, `server/src/usage.rs` |
| S2 | `AUDIO_CHANNEL_CAP`; bounded audio sessions; `Binary` arm close-on-saturation; detector `await` sends | `server/src/lib.rs` |
| S3 | `forward_audio(Receiver<Vec<u8>>)` | `server/src/deepgram.rs` |
| S4 | Unit tests for `PeerTx` (overflow keeps peer + signals; closed → prune) | `server/src/rooms.rs` |

## 6. Testing & Verification

- **Unit:** `peertx_overflow_keeps_peer_and_signals_close` — a stalled reader fills
  the buffer; the overflowing `send` returns `true` (peer kept) and the overflow
  `Notify` fires (handler would close). `peertx_send_reports_closed_receiver` — a
  dropped receiver makes `send` return `false` (prune) and `is_closed()` true.
- **Regression (existing, must stay green):** `broadcast_relay_and_except`,
  `prune_reports_dropped_sessions`, the usage-meter tests, and the WS integration
  tests `lifecycle_signaling_mute_and_lobby`, `chat_is_translated_and_broadcast`,
  `room_full_rejects_fifth`, `audio_produces_subtitles`,
  `set_lang_resolves_auto_and_updates_participant_row`.
- **Suite:** full `cargo test` green + `clippy -D warnings` + `fmt --check`.
- **Load (issue #114):** a k6 scenario with a deliberately slow WS reader confirms
  memory stays flat (bounded) and legitimate peers are unaffected — run against the
  bounded build (`load-test/` k6 script).

## 7. Deployment & Operations

- Server-only; **Railway deploy is manual** (`railway up` from `server/`).
- No new env vars; the caps are compile-time constants sized with generous headroom.
- Observability: overflow/saturation emits a `warn` log (`#123`) on the canonical
  target, visible via Better Stack log shipping (spec 0063) once enabled (#109).

## 8. Risks / Open Items

- Caps are constants, not env-tunable. They sit far above real usage; revisit only
  if a load test shows legitimate bursts approaching them.
- Follow-ups (tracked under #114/#123): admission **semaphore** on Deepgram/Groq
  fan-out; bounding the transcript DB-writer channel (`run_recorder`); the real **k6
  load test** for measured capacity numbers.

## 9. References

- Issues: #123 (this), #114 (audit), #117 (parent hardening). Builds on spec 0064.
- Files: `server/src/rooms.rs`, `server/src/lib.rs`, `server/src/deepgram.rs`,
  `server/src/usage.rs`.
