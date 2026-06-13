# 0040 — Translated-voice TTS no longer cuts off rapid sentences

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0001](../0001-voice-translation-rooms/spec.md), [0002](../0002-video-calls-translated-chat/spec.md) |

## 1. Context & Problem

When a speaker says two sentences in quick succession, the listener's **translated
voice** (browser `SpeechSynthesis`) dropped the first one: the second sentence's
playback **cut off / skipped** the first. Root cause — `speak()` called
`speechSynthesis.cancel()` on **every** invocation, so each new `subtitle_final`
killed whatever utterance was still playing. Fast talking ⇒ only the latest sentence
survived; earlier translations were lost mid-word.

## 2. Goals / Non-Goals

**Goals**
- Every translated sentence is spoken in full, in order — none cut off by the next.
- Don't let a fast talker push playback unboundedly behind real time.

**Non-Goals**
- Changing STT/translation or the subtitle text (this is playback-only).
- Overlapping/parallel voices (sentences are serialized, not played simultaneously).

## 3. Requirements

- **R1 — Queue, don't cancel.** `speak()` enqueues an utterance and plays the queue
  **one at a time**, chained on `onend`/`onerror`; the in-progress utterance is never
  cancelled by a new sentence.
- **R2 — Bounded backlog.** The queue is capped (8); past the cap the **oldest
  waiting** lines are dropped so playback stays near live (a normal conversation,
  with pauses, never reaches it).
- **R3 — Clean stop.** Toggling TTS off or leaving the call clears the queue and
  cancels playback (`stopTts()`), leaving consistent state.

## 4. Design & Architecture

`client/src/scripts/app.ts`:
- Module state `ttsQueue: SpeechSynthesisUtterance[]`, `ttsSpeaking`, `TTS_MAX_QUEUE = 8`.
- `speak(text, lang)` — builds the utterance (voice/lang/rate as before), pushes it,
  trims to the cap, then `pumpTts()`.
- `pumpTts()` — if idle, shifts the next utterance, sets `ttsSpeaking`, and on
  `onend`/`onerror` resets and pumps again. Only **one** utterance is handed to the
  engine at a time (more reliable than the opaque native queue).
- `stopTts()` — clears `ttsQueue`, resets `ttsSpeaking`, `speechSynthesis.cancel()`.
  Replaces the bare `cancel()` at the TTS-off toggle and in `leaveCall()`.
- **Key decisions:**
  - *JS-managed single-flight queue.* Pumping one utterance at a time on `onend`
    gives explicit control over ordering and backlog (and dodges the flaky native
    multi-queue), so nothing is cancelled and nothing overlaps.
  - *Drop oldest past the cap, not newest.* In a runaway monologue, completeness and
    real-time can't both hold; keeping the most recent lines keeps the voice relevant
    to what's being said now. The cap is high enough that real conversations never
    hit it.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Queued, single-flight TTS player + cap | `app.ts` |
| S1 | Route TTS-off / leave through `stopTts()` | `app.ts` |

## 6. Testing & Verification

- `astro check` clean; **101/101** unit tests; production build OK.
- Manual: say two sentences back-to-back in a foreign language → the listener hears
  **both**, in order, neither truncated; toggling TTS off / leaving silences cleanly.

## 7. Deployment & Operations

- **Client-only** — ships via the Vercel autodeploy on `main`. No server change.

## 8. Risks / Open Items

- Serialized playback means TTS can trail a continuous fast talker by up to the cap
  before dropping; acceptable, and tunable via `TTS_MAX_QUEUE`.
- The known Chrome "SpeechSynthesis stalls after ~15 s" quirk isn't addressed here
  (short per-sentence utterances avoid it); revisit with a `resume()` keep-alive if it
  ever surfaces.

## 9. References

- Files: `client/src/scripts/app.ts` (`speak`/`pumpTts`/`stopTts`).
