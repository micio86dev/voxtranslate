# 0001 — Real-time multilingual voice-translation rooms

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | micio86dev |
| **Created** | 2026-06-09 |
| **Shipped** | 2026-06-09 |
| **Version** | initial |
| **Commits** | `7cea003` |
| **Depends on** | — (foundation) |

## 1. Context & Problem

People who speak different languages cannot hold a live conversation without a
human interpreter. The opportunity: chain a streaming speech-to-text engine, a fast
LLM translator, and the browser's built-in speech synthesis into a sub-second loop
so that each participant **speaks and hears in their own language**. This spec is
the foundation everything else is built on — the room model, the WebSocket
protocol, and the STT→translate fan-out pipeline.

## 2. Goals / Non-Goals

**Goals**
- A participant joins a named room with a single chosen language and is heard by
  everyone else **in their language**.
- Streaming, low-latency loop: partial transcripts appear live; finalized
  utterances are translated to every other language present and spoken aloud.
- The server orchestrates STT + translation; it never persists conversation audio.
- Symmetric peers — everyone both speaks and listens (no "host" role).

**Non-Goals**
- Video, P2P media, or text chat (added in [0002](../0002-video-calls-translated-chat/spec.md)).
- Accounts, billing, or usage limits (added in [0005](../0005-accounts-credits-billing/spec.md)).
- Persisting transcripts or translations.

## 3. Requirements

- **R1 — Join a room in my language.** As a participant, I open `/ws?room=&lang=&name=`
  and become a member of that room.
  - *Given* a room name and language, *when* I connect, *then* the server replies
    `room_joined` with my `peer_id` and the existing peers, and others receive `peer_joined`.
- **R2 — Be transcribed as I speak.** As a speaker, my audio is streamed to STT.
  - *Given* an open speaking session, *when* I send audio binary frames, *then* the
    server forwards them to a **dedicated Deepgram Nova-2** streaming connection and
    emits `subtitle_interim` (live partials) to the room.
- **R3 — Be translated when I finish a phrase.** 
  - *Given* a finalized Deepgram result, *when* it is non-empty, *then* the server
    translates it **in parallel** into every other distinct language in the room and
    broadcasts `subtitle_final` carrying `original` + `translations` per language.
- **R4 — Hear others in my language.** As a listener, finalized utterances are spoken.
  - *Given* a `subtitle_final`, *when* my language has a translation, *then* the client
    renders `translations[my_lang]` (falling back to `original`) and speaks it via TTS.
- **R5 — Start/stop speaking explicitly.** `start` opens a fresh Deepgram connection;
  `stop` flushes and closes it, so STT cost maps to actual speech.
- **R6 — Discover activity.** `GET /rooms` lists public rooms with online members.

## 4. Design & Architecture

**Components (server, `server/src/`)**
- `lib.rs` — WS upgrade (`/ws`), per-connection task, room wiring.
- `rooms.rs` — in-memory room registry; peer join/leave; broadcast channels.
- `deepgram.rs` — per-peer Deepgram streaming client (open/feed/finalize/close).
- `translator.rs` + `groq.rs` — translation fan-out to unique target languages via
  Groq `openai/gpt-oss-20b`.
- `protocol.rs` — the wire contract (see below).
- `config.rs` — env + API keys (`DEEPGRAM_API_KEY`, `GROQ_API_KEY`).

**Protocol (`protocol.rs`)** — JSON over WS **text** frames, audio over WS **binary** frames.
- Client→Server: `start`, `stop` (+ later: chat, signaling, mute).
- Server→Client: `room_joined`, `peer_joined`, `peer_left`, `room_full`,
  `subtitle_interim { speaker_id, speaker_name, text, lang }`,
  `subtitle_final { speaker_id, speaker_name, original, lang, translations }`,
  `error { message, code? }`.
- Connect params: `WsParams { room, lang, name?, id?, public? }`.
- Deepgram parsing: `DeepgramResponse::best_alternative()` returns the first
  non-empty alternative of a `Results` message; everything else is ignored.

**Translation fan-out.** Compute the set of distinct languages in the room minus the
speaker's; translate the finalized text into each **concurrently**; assemble a
`HashMap<lang, text>`; broadcast once. One utterance → N translations → one frame.

**Sequence (happy path)**
1. Client connects `/ws?room=demo&lang=it&name=Alice` → `room_joined`.
2. Client sends `start`; server opens a Deepgram WS for this peer.
3. Client streams Opus/WebM audio as binary frames → forwarded to Deepgram.
4. Deepgram partials → `subtitle_interim` to the room (speaker's cell).
5. Deepgram `is_final` → translate into every other language → `subtitle_final`.
6. Listeners render their language and speak it via SpeechSynthesis.
7. Client sends `stop`; Deepgram is flushed + closed.

**Key decisions**
- **One Deepgram WS per peer**, opened only between `start`/`stop` → cost tracks speech, streams are isolated.
- **Server orchestrates, never stores** audio → privacy + statelessness.
- **Single language per peer** (`lang`) keeps the fan-out set small and the UX simple.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Axum WS server + room registry + lobby | `lib.rs`, `rooms.rs`, `main.rs` |
| S1 | Wire protocol + Deepgram response parsing | `protocol.rs` |
| S2 | Deepgram streaming client (per peer) | `deepgram.rs` |
| S3 | Groq translation fan-out | `translator.rs`, `groq.rs` |
| S4 | Browser capture + TTS + room UI | `client/src/scripts/` |

## 6. Testing & Verification

- `protocol.rs` unit tests pin every message tag and `DeepgramResponse::best_alternative`
  behavior (empty transcript / non-`Results` / no-alternative → `None`).
- End-to-end pipeline smoke via `scripts/pipeline-test.mjs`.
- Full automated coverage was formalized later in [0004](../0004-quality-testing-ci/spec.md).

## 7. Deployment & Operations

- Requires `DEEPGRAM_API_KEY` and `GROQ_API_KEY` (via dotenvy; real values in
  `server/.env`, gitignored). `.env.example` ships empty placeholders.
- Stateless beyond in-memory rooms — safe to restart; clients reconnect.

## 8. Risks / Open Items

- In-memory rooms don't survive a restart or scale horizontally (acceptable at current scale).
- Translation quality is bounded by the 8B model; latency bounded by Groq + Deepgram.

## 9. References

- Commit: `7cea003` "VoxTranslate: real-time multilingual voice translation rooms"
- Files: `server/src/{lib,rooms,deepgram,translator,groq,protocol,config}.rs`, `scripts/pipeline-test.mjs`
- External: Deepgram Nova-2 streaming, Groq `openai/gpt-oss-20b`, Web SpeechSynthesis API
