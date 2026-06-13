# 0043 — Lower translation delay: 100 ms audio capture chunks

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0001](../0001-voice-translation-rooms/spec.md), [0040](../0040-tts-no-cut/spec.md), [0042](../0042-tts-voice-selection/spec.md) |

## 1. Context & Problem

After making the TTS start instantly (local-first voice, 0042), the owner asked to cut
**end-to-end translation delay as much as possible without losing UX or translation
quality** — judged against a realistic conversation where people sometimes talk over
each other.

## 2. Conversation simulation → what's safe to change

Two speakers, occasional overlap. End-to-end delay = capture buffering → upload →
Deepgram `is_final` → Groq fan-out → download → TTS. Walking each lever:

- **Per-speaker STT.** Each mic has its own Deepgram WS, so overlap is transcribed
  cleanly per person — nothing to fix.
- **TTS queue (0040) / rate.** On overlap the listener *queues* both speakers
  (serialized, never cut off). Shrinking the queue or raising the rate would cut or
  rush speech → **UX loss → rejected.**
- **Deepgram endpointing / translate-on-interim.** Finalizing sooner fragments
  sentences mid-clause and translates partial context → **translation-quality loss →
  rejected.** We already translate on `is_final` (prompt) with parallel fan-out.
- **Region co-location.** Infra/secrets, not code → out of scope here.
- **Capture chunk (250 ms).** Pure buffering *before* audio reaches Deepgram. Sending
  the **same audio** in smaller pieces lowers delay with **zero** effect on STT
  accuracy, translation, or UX. ✅ **The one quality-neutral lever.**

## 3. Requirements

- **R1 — Smaller chunks.** MediaRecorder timeslice 250 ms → **100 ms** (`CHUNK_MS`),
  shaving ~150 ms off when the tail of each utterance reaches Deepgram.
- **R2 — No quality/UX regression.** Same codec (Opus/WebM 32 kbps), same STT path,
  same translation trigger, same TTS behaviour. Only the send cadence changes.

## 4. Design & Architecture

- `client/src/scripts/audio-capture.ts` — `CHUNK_MS = 100`; `recorder.start(CHUNK_MS)`.
- `CLAUDE.md` — audio note updated to 100 ms.
- **Key decision:** *only spend latency where it costs nothing.* Every other lever in
  the pipeline trades translation accuracy, UX, or needs infra; chunk size is the lone
  free win, so it's the only thing changed. The per-utterance overhead at 100 ms
  (~10 sends/s of ~400 B) is negligible.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | 250 ms → 100 ms capture chunk | `audio-capture.ts`, `CLAUDE.md` |

## 6. Testing & Verification

- `astro check` clean; **101/101** unit tests (audio-capture suite unaffected — the
  fake recorder ignores the timeslice); production build OK.
- Manual: a back-and-forth conversation (incl. talking over each other) feels snappier
  to first translated word; transcripts/translations read the same; no choppiness.

## 7. Deployment & Operations

- **Client-only** — ships via the Vercel autodeploy on `main`. No server change.

## 8. Risks / Open Items

- 100 ms is a safe floor on common browsers; going lower adds container overhead for
  diminishing return. The remaining big latency contributors (Deepgram finalization,
  region RTT) are deliberately untouched to protect translation quality / are infra.

## 9. References

- Files: `client/src/scripts/audio-capture.ts`, `CLAUDE.md`.
- Memory: `tts-minimal-delay-priority`.
