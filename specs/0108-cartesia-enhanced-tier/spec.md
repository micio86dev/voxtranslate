# 0108 — Enhanced tier (Cartesia: client-direct STT + TTS + voice cloning)

| | |
|---|---|
| **Status** | In progress |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-27 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | — |
| **Depends on** | [0093](../0093-premium-translation-engine/spec.md), [0099](../0099-premium-listener-pays/spec.md), [0101](../0101-soniox-enhanced-tier/spec.md) |
| **Replaces** | [0101](../0101-soniox-enhanced-tier/spec.md) (Soniox) |

## 1. Context & Problem

The **Enhanced** tier shipped on Soniox (spec 0101) as a *client-direct, listener-side*
engine: each listener mints a scoped temp key (`POST /api/soniox/session`) and connects
their browser straight to Soniox for real-time STT **+ translation**, rendering subtitles
locally and speaking them with the on-device voice. The server never proxies Enhanced
audio.

We replace Soniox with **Cartesia** to add real, low-latency streaming TTS (Sonic-3.5,
sub-100 ms TTFA) and **per-speaker voice cloning** — every participant is heard, translated,
in *their own* cloned voice. This is a swap of the Enhanced engine only; Standard / Pro /
Premium are untouched. Soniox is removed completely.

**Key architectural difference from 0101:** Cartesia does **STT and TTS but NOT
translation**. So the listener-side pipeline gains a Groq translation hop in the middle —
the same Groq the Standard tier already uses — while audio still never touches the backend:

```
peer WebRTC audio → Cartesia Ink-2 STT (browser, PCM16) → source transcript
  → backend Groq translate (text only, over the room WS) → translated text
  → Cartesia Sonic-3.5 TTS (browser) in the SPEAKER's cloned voice → play + subtitle
```

The tier stays listener-side and listener-pays so the meter, `cost_scales_per_language`,
and the graceful Standard fallback all carry over with no billing-logic change.

## 2. Goals / Non-Goals

**Goals**
- A client-direct Enhanced tier on Cartesia (STT Ink-2 + TTS Sonic-3.5), billed by the
  existing listener-pays meter from `CARTESIA_*` env vars — no billing-logic change.
- The raw `CARTESIA_API_KEY` never leaves the server; the browser gets only short-lived
  Cartesia **access tokens** (`grants {stt,tts}`, ≤1 h) passed as the WS `access_token`
  query param.
- Per-speaker **Instant Voice Cloning (IVC)**: each user clones their voice once at
  pre-join; the server stores `users.cartesia_voice_id` and propagates each peer's
  `voice_id` across the mesh so listeners render every speaker in that speaker's voice.
- Ship behind a flag (`CARTESIA_ENHANCED`, OFF by default); voice cloning behind a second
  master switch (`CARTESIA_VOICE_CLONING_ENABLED`).
- Preserve the graceful Standard fallback: on Cartesia 429 / WS rejection / exhausted
  retries the listener falls back via the existing `EnhancedFallback` → `EngineDowngraded
  { reason: "enhanced_unavailable" }` path; the meter respawns at Standard rate.

**Non-Goals**
- Server-side proxying of Enhanced audio (defeats the latency purpose).
- Translation by Cartesia (it does not translate; Groq stays the translator).
- Multi-region key routing (Soniox's US/EU/JP scaffolding is dropped — Cartesia is a single
  global endpoint).
- Professional Voice Cloning, `PREFER_BEST_ENGINE` auto-upgrade (does not exist today;
  selection stays explicit + localStorage + registry fallback).

## 3. Requirements

- **R1 — Enhanced in the picker.** A signed-in user in a listener-pays deployment with
  `CARTESIA_ENHANCED` on sees "Enhanced" between Standard and Pro, with the per-source rate.
- **R2 — Client-direct, key-safe.** Selecting Enhanced mints a short-lived Cartesia access
  token via `POST /api/sessions/enhanced/session`; the browser connects directly to
  Cartesia STT + TTS; the raw key never reaches the client.
- **R3 — Listener-side translated voice.** When a peer whose language differs speaks, I see
  real-time translated subtitles AND hear the translation, in that speaker's cloned voice,
  rendered locally.
- **R4 — No double translation.** An Enhanced listener is excluded from the server's
  Standard subtitle fan-out (`broadcast_excluding_client_direct`, unchanged from 0101).
- **R5 — Billing unchanged.** Metered by the existing listener meter at Cartesia's
  env-derived rate per active source — no new billing code.
- **R6 — Voice cloning.** With `CARTESIA_VOICE_CLONING_ENABLED` on, a new Enhanced-eligible
  user records ≥3 s of speech at pre-join (VAD/energy-gated, not raw time); the backend
  calls Cartesia IVC and stores `users.cartesia_voice_id`. A user who already has a
  `voice_id` skips the recording. Any failure/timeout (>5 s) silently falls back to a
  default Cartesia voice and never blocks the call.
- **R7 — Flag-gated rollout.** `CARTESIA_ENHANCED` unset ⇒ Enhanced is not registered, not
  in `/api/engines`, and the client path is inert. Rollback = unset it.

## 4. Design & Architecture

- **Components / files**
  - `server/src/config.rs` — `CartesiaConfig` (api_key, stt/tts model, cost/markup,
    `voice_cloning_enabled`, env-overridable base/version/endpoints); `CARTESIA_ENHANCED`
    flag. Removes `SonioxConfig` + region map.
  - `server/src/engine/cartesia.rs` — `CartesiaEngine`: metadata-only, `client_direct:
    true`, `translated_audio: false`, `cost_scales_per_language: true`; `start_session` is
    a backstop `Failed`. (`CARTESIA_ID = "cartesia"`.)
  - `server/src/api.rs` — `POST /api/sessions/enhanced/session` (mint access token; auth +
    credit + rate-limit gates from the Soniox handler) and `POST
    /api/sessions/enhanced/clone-voice` (multipart → Cartesia `/voices/clone` → store
    `voice_id`).
  - `server/src/protocol.rs` — `ClientMessage::TranslateText` / `ServerMessage::
    TranslatedText` (the translation hop); `cartesia_voice_id` added to `PeerInfo` +
    `PeerJoined` (voice-id propagation). `EnhancedFallback` kept (provider-neutral).
  - `server/src/lib.rs` — register `CartesiaEngine` behind the flag; handle `TranslateText`
    by reusing `groq.translate` (no cache — Standard-only, spec 0107 R; honored); thread
    `cartesia_voice_id` from the authed user row into presence; fallback `from` id uses
    `CARTESIA_ID`.
  - `server/src/billing.rs` — `get_cartesia_voice_id` / `set_cartesia_voice_id` (mirror
    `get_avatar`).
  - `server/src/rooms.rs` — `cartesia_voice_id` on `Peer` + in the `PeerInfo` snapshot.
  - `server/migrations/028_cartesia_voice_id.sql` — `ALTER TABLE users ADD COLUMN IF NOT
    EXISTS cartesia_voice_id TEXT;`.
  - `client/src/scripts/cartesia.ts` — `CartesiaManager`: one pipeline per remote speaker
    (STT via AudioWorklet PCM16 + WS → translate over the app WS → TTS in the speaker's
    voice). Replaces `soniox.ts`; the `@soniox/speech-to-text-web` dep is removed.
  - `client/src/scripts/{api.ts,app.ts}`, `client/src/pages/index.astro` (pre-join voice
    prep), account settings, CSP, `privacy.astro`, and all 84 i18n locales.

- **Protocol / API**
  - `POST /api/sessions/enhanced/session { } → { token, expires_at, cartesia_version,
    stt:{endpoint,model}, tts:{endpoint,model}, voice_cloning_enabled }`. Auth required
    (guests → 401); 503 when the tier is disabled; 402 on insufficient credit.
  - `POST /api/sessions/enhanced/clone-voice` (multipart `clip`, optional `language`) →
    `{ voice_id }` or `{ voice_id: null, fallback: true }`.
  - WS: `{type:"translate_text", request_id, text, source, target}` →
    `{type:"translated_text", request_id, text}`.
  - `PeerInfo` / `peer_joined` gain `cartesia_voice_id?: string`.

- **Cartesia contracts** (docs.cartesia.ai, `Cartesia-Version: 2026-03-01`)
  - Token: `POST https://api.cartesia.ai/access-token`, `Authorization: Bearer sk_car_…`,
    `{grants:{stt:true,tts:true},expires_in:3600}` → `{token}`.
  - STT WS: `wss://api.cartesia.ai/stt/websocket?model=ink-2&encoding=pcm_s16le&
    sample_rate=16000&cartesia_version=…&access_token=…`; binary PCM in →
    `{type:"transcript",is_final,text}`.
  - TTS WS: `wss://api.cartesia.ai/tts/websocket?cartesia_version=…&access_token=…`; send
    `{model_id:"sonic-3.5",transcript,voice:{mode:"id",id},output_format:{container:"raw",
    encoding:"pcm_s16le",sample_rate},language,context_id}` → `chunk`/`done`/`error`.
  - IVC: `POST https://api.cartesia.ai/voices/clone` multipart `{clip,name,language}` →
    `{id,…}`.

- **Key decisions**
  - *Listener-side, metadata-only engine* — keeps routing/billing/UI engine-agnostic, same
    as 0101; the only live-path change remains the client-direct fan-out exclusion.
  - *Translation hop over the existing room WS, reusing `groq.translate` directly (no
    DragonflyDB cache — Standard-only)* — avoids a new HTTP path and the cache (rule #5).
  - *Speaker-voice cloning via presence propagation* — each user clones their own voice;
    `cartesia_voice_id` rides `PeerInfo`/`peer_joined` exactly like `avatar_url`.
  - *Access tokens, not single-use keys* — Cartesia issues one short-lived token (both
    grants) per session; the browser reconnects with it until near expiry.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `CartesiaConfig` + `CARTESIA_ENHANCED`; remove `SonioxConfig` | `config.rs`, `tests/config_env.rs` |
| S1 | `CartesiaEngine` + registry swap; delete `soniox.rs` | `engine/cartesia.rs`, `engine/mod.rs`, `lib.rs` |
| S2 | `cartesia_voice_id` migration + billing get/set + presence | `migrations/028*`, `billing.rs`, `rooms.rs`, `protocol.rs`, `lib.rs` |
| S3 | Translate hop (`TranslateText`/`TranslatedText`) | `protocol.rs`, `lib.rs` |
| S4 | `/session` (token) + `/clone-voice` endpoints | `api.rs`, `lib.rs` |
| S5 | Client pipeline + worklet + wiring; remove Soniox SDK | `cartesia.ts`, `app.ts`, `api.ts`, `engines.ts` |
| S6 | Pre-join voice prep + account status + CSP + privacy | `index.astro`, settings, CSP, `privacy.astro` |
| S7 | i18n: rewrite Enhanced + voice strings, all 84 locales | `i18n/*.json` |

## 6. Testing & Verification

- **Server:** `cargo fmt` + `cargo clippy --all-targets` + `cargo test`. New: `CartesiaConfig`
  env validation + missing-key, Cartesia metadata (client-direct), `/session` 401/503/402 +
  token shape, `/clone-voice` success stores `voice_id` / failure → fallback / skip when
  present, billing rate×markup, and the adapted graceful-degradation integration test
  (`enhanced_fallback_downgrades_listener_to_standard`).
- **Client:** `astro check`, vitest (`cartesia.ts` transcript extraction + manager
  lifecycle + translate round-trip + TTS orchestration; `engines.ts` enhanced desc), e2e
  pre-join voice prep. `npm run build` succeeds.
- **Acceptance gate:** repo-wide `soniox` grep is empty (this spec and 0101 excepted).

## 7. Deployment & Operations

- **Env:** `CARTESIA_API_KEY`, `CARTESIA_STT_MODEL` (`ink-2`), `CARTESIA_TTS_MODEL`
  (`sonic-3.5`), `CARTESIA_COST_PER_MINUTE` (0.036), `CARTESIA_COST_MARKUP_PERCENT` (85),
  `CARTESIA_VOICE_CLONING_ENABLED` (true). Flag: `CARTESIA_ENHANCED=1`.
- **Rollout:** set the vars + flag on Railway, `railway up`. Remove the `SONIOX_*` vars.
- **Rollback:** unset `CARTESIA_ENHANCED` → Enhanced leaves the picker; everything else
  unchanged.

## 8. Risks / Open Items

- Cartesia STT wants raw PCM16 (not WebM/Opus): the client decodes peer audio to PCM16 @
  16 kHz via an AudioWorklet (generalize the spec-0099 PCM-capture worklet).
- Concurrency caps → 429: covered by the existing graceful-degradation path (R5/R7).
- `cartesia_voice_id` propagation is the one schema addition beyond 0101 — additive,
  `skip_serializing_if = None`, documented here per the no-schema-change-without-doc rule.
- Pre-existing `usage_sessions.engine_id = "soniox"` rows stay valid historically;
  `EngineRegistry::resolve` already falls unknown ids back to the default.

## 9. References

- Cartesia: access tokens, STT (Ink-2) / TTS (Sonic-3.5) WebSockets, Instant Voice Cloning
  — docs.cartesia.ai (`2026-03-01`).
- Internal: spec 0093 (engine registry), 0099 (listener-pays), 0101 (Soniox Enhanced, replaced).
