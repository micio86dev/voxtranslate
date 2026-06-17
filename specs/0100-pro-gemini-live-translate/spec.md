# 0100 — Pro translation engine (Gemini 3.5 Live Translate)

| | |
|---|---|
| **Status** | Draft |
| **Owner** | Micio Dev |
| **Created** | 2026-06-17 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | — |
| **Depends on** | [0093](../0093-premium-translation-engine/spec.md), [0094](../0094-premium-capacity-fallback/spec.md) |

## 1. Context & Problem

Spec 0093 turned the single hard-wired translation path into an **N-engine
registry**: each engine is a module implementing the `TranslationEngine` trait and
declaring its `EngineMetadata`; routing, billing, and the UI are engine-agnostic
(`if engine == X` is forbidden — polymorphism via the trait). Two engines ship
today: **Standard** (Deepgram + Groq, TTS voice) and **Premium** (OpenAI
GPT-Realtime-Translate, natural speech-to-speech).

On **2026-06-09** Google shipped **Gemini 3.5 Live Translate**
(`gemini-3.5-live-translate-preview`) on the Gemini Live API: streaming
speech-to-speech across 70+ input languages with automatic source detection,
natural translated audio, and a transcript sidecar. It sits between Standard and
Premium on the price/quality curve — a natural **"Pro"** tier.

This spec adds Gemini as a third engine. By design (0093) this is a **new module +
one `register` call**, not a refactor. The trait does **not** change: Gemini's
source auto-detection is internal to the model, and like OpenAI we open **one
session per target language**, so the engine still only needs the *target*
language the registry already computes.

## 2. Goals / Non-Goals

**Goals**
- Register a third engine, `gemini_live_translate`, display name **"Pro"**,
  positioned between Standard (order 0) and Premium (order 2) in the selector.
- Speech-to-speech: translated audio (PCM16) + synchronized subtitles, reusing the
  existing `TranslatedAudio` / `SubtitleInterim` / `SubtitleFinal` plumbing.
- Feature-gated on `GOOGLE_AI_API_KEY` (ships dark until configured), exactly like
  Premium on `OPENAI_API_KEY`.
- Per-`(target language)` billing and capacity fallback identical to Premium
  (spec 0094): at capacity → caller falls back to the default engine.
- No changes to the engine trait, routing, billing math, or the dynamic selector.

**Non-Goals**
- Listener-pays billing (spec 0099) — orthogonal and unmerged; Gemini ships into the
  **current speaker-pays** model and inherits listener-pays automatically when 0099
  lands (same engine-agnostic routing as OpenAI).
- Exposing Gemini's full 70+ output-language set — we surface the app's shipped UI
  language set for now (mixed-room safety, spec 0094 `commonLangs`).
- Vertex AI / OAuth auth — we use a simple Google API key (server-to-server).

## 3. Requirements

- **R1 — Pro appears when provisioned.** As an operator, when I set
  `GOOGLE_AI_API_KEY`, the Pro engine is registered and listed by `GET /api/engines`
  between Standard and Premium; without the key it is absent.
  - *Given* no `GOOGLE_AI_API_KEY`, *when* the server starts, *then* `/api/engines`
    contains only the engines whose keys are set (Pro omitted).
- **R2 — Pro translates speech to speech.** As a listener who chose Pro, *given* a
  speaker on Pro talking in language A and me listening in B≠A, *when* they speak,
  *then* I receive `TranslatedAudio` (PCM16 @ 24 kHz) for B plus `SubtitleInterim`/
  `SubtitleFinal` captions, and the speaker sees their own original-language interim.
- **R3 — Auto source detection, target-driven sessions.** *Given* the speaker's
  resolved source language, *when* Pro starts, *then* it opens one Gemini session per
  **distinct other** target language in the room (never the speaker's own), exactly
  as Premium does — no trait change for auto-detect.
- **R4 — Capacity fallback.** *Given* the Gemini session pool
  (`GEMINI_LIVE_MAX_SESSIONS`) is exhausted, *when* a speaker starts on Pro, *then*
  `start_session` returns `AtCapacity` (all-or-nothing reservation) and the caller
  falls back to the default engine — translation never stalls (spec 0094).
- **R5 — Pricing never leaks.** *Given* the configured raw cost + markup, *when*
  `/api/engines` is serialized, *then* only `rate_per_minute` (= cost × (1+markup))
  is exposed; raw cost and markup are server-only.
- **R6 — No regression.** Standard and Premium behave byte-for-byte as before; the
  selector remains fully dynamic from the registry.

## 4. Design & Architecture

- **Components / files:**
  - `server/src/engine/gemini.rs` — low-level Gemini Live API client (mirrors
    `openai.rs`): build the WS URL with the API key in the query string (kept out of
    logs/`Debug`), the `setup` frame, the `realtimeInput` audio frame, parse
    `serverContent`, and a **24 kHz → 16 kHz** PCM16 resampler. Pure build/parse
    functions are unit-tested; socket I/O needs a live key.
  - `server/src/engine/pro.rs` — `ProEngine: TranslationEngine` (mirrors
    `premium.rs`): metadata, capacity semaphore, per-target-language reconnect loop,
    resample the speaker feed to 16 kHz, emit subtitles + translated audio. Caption
    segmentation uses Gemini's explicit `turnComplete`/`generationComplete` signals,
    falling back to an idle debounce.
  - `server/src/engine/mod.rs` — `pub const GEMINI_ID = "gemini_live_translate"`,
    `pub mod gemini; pub mod pro;`, re-export `ProEngine`.
  - `server/src/config.rs` — `GeminiConfig`, `Config.google: Option<GeminiConfig>`,
    present-gated on `GOOGLE_AI_API_KEY`.
  - `server/src/lib.rs` — register Pro **between** Standard and Premium (registration
    order = display order → Pro lands at tier order 1).
  - `client/src/scripts/engines.ts` + `i18n.ts` — add the `engineDescPro` i18n key
    and map `tier == "pro"` to it; otherwise zero functional client change (the
    selector is already dynamic).

- **Protocol / API (Gemini Live API):**
  - Endpoint: `wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key={API_KEY}`
    (API key in **query string**, not a header).
  - Setup (one per target language):
    ```json
    {"setup":{"model":"models/gemini-3.5-live-translate-preview",
      "generationConfig":{"responseModalities":["AUDIO"],
        "inputAudioTranscription":{},"outputAudioTranscription":{},
        "translationConfig":{"targetLanguageCode":"<lang>","echoTargetLanguage":true}}}}
    ```
  - Input audio: `{"realtimeInput":{"audio":{"data":"<b64 PCM16>","mimeType":"audio/pcm;rate=16000"}}}`
    — **16 kHz** mono LE (vs OpenAI's 24 kHz).
  - Output: `serverContent.modelTurn.parts[].inlineData{data,mimeType:"audio/pcm;rate=24000"}`
    — **24 kHz** (matches our playback + `TranslatedAudio`).
  - Transcripts: `serverContent.inputTranscription.text` (speaker) /
    `outputTranscription.text` (translation), incremental plain-text fragments.
  - Segmentation: `serverContent.turnComplete` / `generationComplete`. Session
    lifecycle: `setupComplete` (ack), `goAway{timeLeft}` (disconnect warning),
    `sessionResumption` (resume handle — honored opportunistically).

- **Key decisions:**
  - **Branch off `main`, speaker-pays.** Prereq is only 0093 (on main). Listener-pays
    (0099) is unmerged WIP; coupling would block merge. Engine-agnostic routing means
    0099 will extend Pro the same way it extends Premium. *Rejected:* building on the
    listener-pays branch.
  - **Server-side 24 k → 16 k resample.** The client captures one 24 kHz PCM stream
    (shared across listeners/engines); Gemini needs 16 kHz in. Resampling lives in the
    Gemini feed, not the capture path, so a mixed room (OpenAI@24 k + Gemini@16 k)
    works from the same stream. Linear interpolation for the preview; upgradeable to a
    filtered decimator if STT quality demands. *Rejected:* renegotiating capture rate.
  - **No trait change for auto-detect.** Gemini detects the source internally; the
    target-per-session model already fits. *Rejected:* adding a `source: auto` concept.
  - **Key in URL must never be logged.** Build the request URL locally; trace only
    `model` + `lang`, never the URL/key.

## 5. Configuration

```
GOOGLE_AI_API_KEY=            # gates the Pro engine (ships dark when empty)
GEMINI_LIVE_TRANSLATE_MODEL=gemini-3.5-live-translate-preview
GEMINI_COST_PER_MINUTE=0.023  # raw cost (USD); real preview pricing TBC before launch
GEMINI_COST_MARKUP_PERCENT=50 # PERCENT; falls back to ENGINE_DEFAULT_MARKUP_PERCENT, then 50
GEMINI_LIVE_MAX_SESSIONS=16   # process-wide concurrent-session cap (preview limits!)
```

## 6. Testing

- **Unit (`gemini.rs`):** setup-frame nesting + model + `translationConfig`;
  `serverContent` parsing → audio `inlineData`, input/output transcription,
  `turnComplete`; garbage/unknown frames degrade to a no-op (never panic); URL
  builder keeps the key out of `Debug`/logs; resampler output length ≈ ⅔ input and
  preserves a constant signal.
- **Unit (`pro.rs`):** `at_capacity_returns_atcapacity` (exhausted semaphore),
  `no_targets_returns_started` (speaker alone — no permit, no cost), target-language
  set = distinct others.
- **Integration:** Pro present in `/api/engines` when keyed; mixed room (Pro + Standard
  or Premium) routes correctly; live-API path gated on `GOOGLE_AI_API_KEY`.
- **Regression:** Standard + Premium unit/integration tests unchanged and green.

## 7. Risks & Flags

- **Preview pricing unknown.** `0.023` is the planning figure; confirm real Gemini
  Live preview pricing before launch (env-tunable, so no code change). *(See the prior
  Premium pricing-tuning gotcha.)*
- **Preview concurrency limits.** Set `GEMINI_LIVE_MAX_SESSIONS` conservatively; the
  preview tier caps concurrent sessions, and we hold one per target language.
- **Model id changes at GA.** Read from `GEMINI_LIVE_TRANSLATE_MODEL`; update the env
  default when GA renames it.
