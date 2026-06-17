# 0093 — Premium translation engine (OpenAI GPT-Realtime-Translate) + engine registry

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Micio Dev |
| **Created** | 2026-06-17 |
| **Shipped** | 2026-06-17 |
| **Version** | — |
| **Commits** | `167820a` |
| **Depends on** | [0001](../0001-voice-translation-rooms/spec.md), [0005](../0005-accounts-credits-billing/spec.md), [0012](../0012-auto-language-detection/spec.md), [0043](../0043-low-latency-capture/spec.md), [0069](../0069-bounded-translate-fanout/spec.md) |

## 1. Context & Problem

Today VoxTranslate has exactly **one** translation path, hard-wired into the audio
pipeline: mic → MediaRecorder (WebM/Opus) → server → per-speaker **Deepgram** STT →
**Groq** `translate_fanout` → `subtitle_final` broadcast; translated *voice* is
synthesized **client-side** by the browser `SpeechSynthesis` API from the subtitle
text. This is cheap and fast but the synthesized voice is robotic and capped at the
8 UI languages.

We want a **premium, end-to-end speech-to-speech** option — OpenAI
**GPT-Realtime-Translate** — that returns natural translated audio plus transcript
deltas, while keeping the current pipeline byte-for-byte intact. More importantly,
we want the architecture to support **N engines**: adding a future engine must mean
"new module implementing a trait + register it", with **no** changes to routing,
billing, or UI (`if engine == X` is forbidden — polymorphism via the trait).

## 2. Goals / Non-Goals

**Goals**
- A **translation-engine registry**: each engine is a self-contained module behind a
  common trait, declaring its own metadata (id, languages, cost, capabilities).
- The current Deepgram+Groq pipeline becomes the **Standard** engine (the first
  implementation), not a special case.
- A **Premium** engine (OpenAI) offering translated audio + subtitles, billed at a
  higher, configurable rate.
- Per-participant engine selection on the pre-join screen, persisted in
  `localStorage`, with the language dropdown driven by the chosen engine.
- Zero regression to the Standard path; Premium is purely additive.

**Non-Goals**
- Changing chat translation (chat stays Groq fan-out — engines are about *speech*).
- Replacing browser TTS for the Standard engine.
- Per-listener engine choice: the engine is the **speaker's** choice (it governs how
  *their* outgoing speech is translated).

## 3. Requirements

- **R1 — Engine registry.** As a maintainer, I want engines behind one trait, so that
  adding an engine touches no routing/billing/UI code.
  - *Given* the registry, *when* a speaker starts, *then* the active engine is
    resolved by id (default fallback for unknown ids) and `start_session` is called
    polymorphically — no engine-specific branching in the handler.
- **R2 — Standard unchanged.** *Given* a Standard speaker, *when* they speak, *then*
  the Deepgram→Groq→`subtitle_final` flow and TTS are identical to pre-0093.
- **R3 — Engine selection.** As a participant, I want to pick Standard or Premium
  pre-join with the per-minute price shown, persisted across sessions.
  - *Given* a persisted engine id that no longer exists, *when* I join, *then* it
    falls back to the default gracefully.
- **R4 — Premium subtitles match audio.** *Given* a Premium speaker, *when* they
  speak, *then* subtitles come from OpenAI transcript deltas (NOT Deepgram+Groq), so
  the text always matches the heard translated voice.
- **R5 — Premium audio.** *Given* a Premium speaker, *when* they speak, *then* each
  listener hears OpenAI-translated audio in their own language via Web Audio /
  AudioWorklet (not browser TTS).
- **R6 — Billing.** *Given* a Premium session, *when* metered, *then* usage records
  carry `engine_id` and the speaker is charged the engine's own rate
  (`cost × (1 + markup)`); on exhaustion the speaker gracefully downgrades to
  Standard with a notice.
- **R7 — Group rooms.** *Given* a 3–4 person all-Premium room, *when* people speak,
  *then* sessions are deduped by distinct target language per speaker and capped by a
  process-wide concurrency limit.

## 4. Design & Architecture

- **Components / files**
  - `server/src/engine/mod.rs` — `TranslationEngine` trait, `EngineRegistry`,
    `SessionDeps`; `metadata.rs` — `EngineMetadata` (internal) + `EngineInfo` (public
    DTO); `standard.rs` — `StandardEngine`; `premium.rs` + `openai.rs` — Premium.
  - `server/src/lib.rs` — `AppState.engines`; `Start` handler resolves the engine and
    calls `start_session`.
  - `server/src/usage.rs`, `billing.rs` — engine-aware rate + `engine_id`.
  - `server/migrations/010_premium_engine.sql` — `usage_sessions.engine_id`.
  - `client/` — engine selector, `pcm-capture.ts`, `pcm-playback.ts`.

- **The trait (altitude = whole per-speaker session).** OpenAI is speech-to-speech,
  one WS session per output language — a different shape from Groq text fan-out — so
  the trait wraps the entire session (audio-in → subtitles + audio-out), not "translate
  text". `start_session(ctx, deps) -> Option<mpsc::Sender<Vec<u8>>>` returns the same
  audio sender the handler already holds, so the binary hot-path and its bounded-channel
  backpressure (#123 / spec 0065) are unchanged. Engine-specific audio format
  (WebM/Opus for Standard, PCM16 for Premium) is interpreted inside the engine.

- **Data model.** `usage_sessions.engine_id TEXT NOT NULL DEFAULT 'standard'`
  (extensible string, not enum).

- **Protocol / API.**
  - `GET /api/engines` → `[EngineInfo]` (id, display_name, tier, description,
    `rate_per_minute` = cost×(1+markup), input/output languages, capabilities). Raw
    cost/markup are NEVER serialized (mirrors `PricingConfig`).
  - `WsParams.engine` join query param (default → default engine).
  - Server→client: `translated_audio { speaker_id, lang, seq, pcm16_b64 }`,
    `engine_downgraded { from, to, reason }`.

- **OpenAI Realtime Translation.** `wss://api.openai.com/v1/realtime/translations?model=gpt-realtime-translate`,
  auth `Authorization: Bearer $OPENAI_API_KEY`; input PCM16/24kHz base64 in
  `session.input_audio_buffer.append`; `session.update` sets
  `session.audio.output.language`; server events `session.output_audio.delta`,
  `session.output_transcript.delta`, `session.input_transcript.delta`,
  `session.closed`. One session per output language. 70+ input / 13 output languages.

- **Audio capture (Premium).** The client captures a parallel PCM16/24kHz stream via
  AudioWorklet when Premium is selected and sends raw PCM frames; the server base64-
  relays to OpenAI (server never decodes audio — CLAUDE.md invariant).

- **Group rooms.** Sessions = speakers × distinct target languages (deduped), bounded
  by an `OPENAI_REALTIME_MAX_SESSIONS` admission semaphore (cf. spec 0069). 4-person
  worst case ≈ 12 concurrent sessions.

- **Per-stream billing + transparency (amendment 2026-06-17).** Because Premium opens
  one paid OpenAI session per distinct target language, the meter bills **per
  translation stream**: the per-minute rate is multiplied by the number of distinct
  other-languages in the room (the `EngineCapabilities.cost_scales_per_language` flag
  drives this; Standard stays flat — Groq fan-out is covered by the flat rate). So a
  group call genuinely costs the speaker more, tracking real OpenAI cost. The pre-join
  selector states this up front ("rate is per translation language — a group call
  costs more") so pricing is transparent. The displayed `rate_per_minute` is the
  per-stream rate. Calibration: at OpenAI $0.034/min, Stripe ~2.9%+$0.30, and the
  most-bonused package, `OPENAI_COST_PER_MINUTE=0.04` × 50% markup nets ≥25% per
  stream.

- **Key decisions**
  - Engine choice is the **speaker's** and bills the speaker (consistent with the
    current per-speaking-second meter). → Mixed rooms work independently.
  - Translated audio rides the existing text-frame outbound channel as base64 JSON,
    leaving the bounded `PeerTx`/`pump_to_ws` (spec 0065) untouched. (Binary framing
    is a possible later latency optimization.)
  - Per-session services (rooms, moderator, transcripts) are passed via `SessionDeps`
    at speak time — not captured in the engine — so the registry builds before the DB
    connects and the DB-augmented moderator is always used.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Engine registry + trait; Standard wraps the existing pipeline (no behavior change) | `engine/{mod,metadata,standard}.rs`, `lib.rs` |
| S1 | `GET /api/engines`; `OpenAiConfig`; dynamic selector + `localStorage` + join payload | `config.rs`, `api.rs`, `lib.rs`, `client/` |
| S2 | Premium subtitles from OpenAI transcript deltas | `engine/{openai,premium}.rs` |
| S3 | Premium translated audio (AudioWorklet capture + playback) | `protocol.rs`, `rooms.rs`, `client/pcm-*.ts` |
| S4 | Billing: `engine_id` migration + per-engine rate | `migrations/010_*.sql`, `usage.rs`, `billing.rs` |
| S5 | Graceful downgrade on exhaustion + reconnect/backoff + edge cases | `usage.rs`, `engine/premium.rs`, `protocol.rs` |

## 6. Testing & Verification

- **S0** (done): registry register/get/default/unknown-fallback (`engine::tests`);
  rate math + `EngineInfo` no-leak (`engine::metadata::tests`); existing
  `integration.rs` (`audio_produces_subtitles`, `deepgram_unavailable_sends_error`,
  `set_lang_resolves_auto_…`) stay green → proves zero regression.
- **S1**: `EngineInfo` serialization leak test (Rust); client vitest for
  engine-preference load/validate/fallback + selector render.
- **S2**: pure `parse_openai_event` unit tests; live integration gated behind
  `OPENAI_API_KEY` (keyless CI skips, mirroring DB-gating).
- **S3**: client vitest for PCM conversion/resample + playback ordering; Rust
  `broadcast_to_lang` targeting.
- **S4**: DB-gated meter test asserting `engine_id` + premium rate.
- **S5**: downgrade decision + reconnect backoff unit tests.

## 7. Deployment & Operations

- New env (all optional; Premium appears only when `OPENAI_API_KEY` is set):
  `OPENAI_API_KEY`, `OPENAI_REALTIME_MODEL=gpt-realtime-translate`,
  `OPENAI_COST_PER_MINUTE`, `OPENAI_COST_MARKUP_PERCENT=50`,
  `ENGINE_DEFAULT_MARKUP_PERCENT=50`, `OPENAI_REALTIME_MAX_SESSIONS=16`.
- Migration `010_premium_engine.sql` runs at startup (backward-compatible default).
- Rollout: Premium is invisible until the key is configured, so the feature ships
  dark and is enabled by setting the env on Railway.

## 8. Risks / Open Items

- OpenAI realtime-translate is a new product: re-confirm exact event `type` strings
  and the 13 output languages against live docs during S1/S2.
- AudioWorklet playback needs an iOS/Safari gesture-unlock (parallels `unlockTts`).
- Premium is real money per session-minute × sessions; the concurrency cap + the
  existing same-language-skip meter bound spend.

## 9. References

- Commits: `<sha>`
- Files: `server/src/engine/`, `server/src/lib.rs`, `client/src/scripts/`
- External: OpenAI Realtime Translation — https://developers.openai.com/api/docs/guides/realtime-translation
