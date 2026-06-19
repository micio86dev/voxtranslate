# 0103 — Safari/WebKit voice-translation support (Soniox capture fix; PCM STT deferred)

| | |
|---|---|
| **Status** | Phase 1 ✅ Shipped · Phase 2 Deferred |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-19 |
| **Shipped** | 2026-06-19 (Phase 1) |
| **Version** | — |
| **Commits** | `5cc15b6` (#287, Phase 1) |
| **Depends on** | [0093](../0093-premium-translation-engine/spec.md) (PCM capture), [0099](../0099-premium-listener-pays/spec.md) (capture_format), [0101](../0101-soniox-enhanced-tier/spec.md) (Enhanced) |

## 0. Guiding constraints (this spec is scoped by them)

- **Minimal risk.** The app already works well on PC/Mac/Android (Chrome) and the
  owner reports Safari "seems to work". Any change must be **zero-impact on the
  working browsers** and trivially revertible.
- **Never add delay — audio above all** (project rule; see memory
  `tts-minimal-delay-priority`). A fix that increases audio latency is rejected,
  even if it improves Safari coverage.

→ Result: **only the small, latency-neutral, zero-Chrome-impact fix ships now
(Phase 1, "A").** The heavier PCM-routing change is documented but **deferred**
(Phase 2) until (a) a real Safari/iPhone confirms it's actually broken and (b) a
latency/bandwidth-safe design exists.

## 1. Context & Problem

The core is **video calls with real-time voice translation**. The STT **send**
paths are `AudioCapture` (WebM/Opus `MediaRecorder` — Standard/Enhanced) and
`PcmCapture` (PCM16@24k AudioWorklet — Pro/Premium, and any speaker once a Premium
listener is present). **Safari/WebKit (incl. every iOS browser — Chrome iOS is
WebKit) cannot record WebM with `MediaRecorder`**: `isTypeSupported('audio/webm…')`
is `false` and constructing with `{mimeType:'audio/webm'}` throws. WebKit records
`audio/mp4` (AAC) instead.

Two issues derived from the code:

1. **Enhanced RECEIVE regressed by #281 (the bug this spec fixes now).** Spec 0101's
   client-direct Soniox pipeline records each remote peer's audio via the Soniox SDK
   (`MediaRecorder` internally). #281 pinned `audio/webm;codecs=opus` (fallback
   `audio/webm`) to fix Android. On Safari that pin makes the SDK throw → an Enhanced
   **listener** on Safari gets nothing. Before #281 the SDK used its default (mp4 on
   Safari), which may have worked. (Soniox CSP was fixed separately in #285.)
2. **CORE GAP, pre-existing (documented, deferred).** A **Standard/Enhanced speaker**
   on Safari hits `AudioCapture.start()`, the constructor throws, `catch { return }`
   swallows it → no `start` frame, no audio → the server never transcribes that
   speaker → **nothing a Safari user says is translated** — UNLESS a Premium/PCM
   listener is in the room (which flips the speaker to PcmCapture, working on Safari).
   The server already supports PCM STT (`AudioFormat::Pcm16`,
   `encoding=linear16&sample_rate=24000`, `server/src/deepgram.rs:62-85`).

> ⚠️ Confidence: derived from code + the stable WebKit "`MediaRecorder` has no WebM,
> only mp4" limitation; **not** runtime-tested on real Safari here (dev env automates
> Chrome only). The owner reports Safari "seems to work" — so issue #2's real-world
> impact is **unconfirmed** and may be partial (e.g. their tests had a PCM listener,
> or Soniox's mp4 path already covered it). This is exactly why issue #2 is deferred
> behind a real-device check rather than fixed on assumption.

## 2. Goals / Non-Goals

**Goals (Phase 1 — ship now)**
- **Un-regress Enhanced receive on Safari** (issue #1) with a change that is
  **identical behaviour on Chrome/Android/Firefox** and adds **no audio delay**.

**Non-Goals (now)**
- Fixing the Standard/Enhanced **send** gap on Safari (issue #2) — **deferred to
  Phase 2**, see §8. It needs real-device confirmation and a latency/bandwidth-safe
  design before any code.
- Any change to engines, pricing, billing, the 24 kHz PCM pipeline, or to the
  working browsers' capture.

## 3. Requirements

- **R1 — Enhanced receive on Safari (Phase 1).** As a Safari listener on Enhanced,
  I want translated subtitles + on-device voice of the people I hear.
  - *Given* the CSP allows Soniox (#285), *when* a cross-language peer speaks,
    *then* the in-browser Soniox pipeline records in a WebKit-supported container
    (mp4) instead of throwing on a pinned WebM mimeType.
- **R2 — No regression on working browsers (Phase 1).** *Given* Chrome/Android/
  Firefox, *when* a peer is on Enhanced, *then* the Soniox capture still uses
  `audio/webm;codecs=opus` exactly as after #281 — byte-for-byte unchanged.
- **R3 — No added latency (Phase 1).** *Given* any browser, *when* Enhanced runs,
  *then* the change only swaps the recorder container/mimeType; no extra buffering,
  no new round-trips, no re-encode on the hot path.
- **R4 (Phase 2, deferred) — Safari speaker is translated.** Tracked in §8; not
  implemented by this spec.

## 4. Design & Architecture

### Phase 1 — browser-aware Soniox capture mimeType ("A") — ✅ SHIPPED (`5cc15b6`, #287)

- `client/src/scripts/soniox.ts` `sttMimeType()`: replace the WebM-or-WebM choice
  with an ordered candidate list (mirroring `recording/utils.ts pickMimeType`):
  1. `audio/webm;codecs=opus` (Chrome/Android/Firefox) — **unchanged from #281**,
  2. `audio/mp4` (Safari/WebKit),
  3. `undefined` → omit `mediaRecorderOptions`, let the SDK default.
  The SDK keeps `audioFormat:'auto'`, so Soniox sniffs whichever container we hand it.
- **Why latency-neutral:** this only changes the container the SDK's `MediaRecorder`
  produces; the timeslice, the WS, the translation hop, and the on-device TTS path
  are untouched. No change to the audio round-trip.
- **Why zero-risk for working browsers:** the first candidate is the post-#281
  value and is supported on Chrome/Android/Firefox, so they pick it exactly as today.

### Phase 2 — PCM STT fallback for no-WebM browsers (DEFERRED — see §8)

Sketch only (no code now): route no-WebM browsers to the existing `PcmCapture`
(AudioWorklet@24k → Deepgram `linear16@24k`), with the client declaring `capture=pcm`
on join so the server opens that speaker's Deepgram in PCM regardless of Premium
composition. Deferred because PCM16@24k is ~12× the upload of Opus and could add
audio/transport latency on weak mobile links — which violates §0.

**Key decisions:**
- *Ship only the latency-neutral, no-regression fix now.* Matches the minimal-risk +
  no-delay constraints.
- *Defer PCM routing* until issue #2 is confirmed real on a device and a design that
  doesn't add audio delay (e.g. 16 kHz PCM for the Deepgram-only path) is chosen.

## 5. Implementation

| Slice | Phase | What | Key files |
|-------|-------|------|-----------|
| S1 | 1 (now) | `sttMimeType()` → ordered candidates (webm/opus → mp4 → default); pass `mediaRecorderOptions` only when defined | `client/src/scripts/soniox.ts` |
| S2 | 1 (now) | Unit tests: Chrome stub → webm/opus; Safari stub (webm unsupported, mp4 supported) → mp4; neither → undefined; assert start() opts | `client/src/scripts/soniox.test.ts` |
| S3 | 2 (deferred) | `canCaptureWebm()` + PCM capture selection + `&capture=pcm` join signal | `client/src/scripts/app.ts`, `audio-capture.ts` |
| S4 | 2 (deferred) | Server: per-peer `pcm_input = room_wants_pcm \|\| client_declared_pcm`; never ask a pcm-only peer for WebM | `server/src/lib.rs` |

## 6. Testing & Verification

**Phase 1**
- **Unit (CI):** `sttMimeType()` returns `audio/webm;codecs=opus` (Chrome stub),
  `audio/mp4` (Safari stub: `isTypeSupported` true only for mp4), `undefined`
  (neither / no `MediaRecorder`). Existing soniox/audio-capture suites stay green.
- **Manual (real device — the actual validation):** Enhanced listener on Safari
  macOS + iPhone receives translated subtitles + on-device voice; Chrome/Android
  Enhanced unchanged. If Soniox `auto` cannot decode Safari's mp4, R1 is not met by
  Phase 1 — record the result and fall back to "no change vs today" (still no
  regression for Chrome/Android, since they keep webm/opus).

**Phase 2** — see §8; not in this PR.

## 7. Deployment & Operations

- Phase 1 is **client-only** (Vercel auto-deploy on main). No env vars, no server
  change, no migration.
- Rollback: revert the one-file change; Soniox capture returns to the #281 state.

## 8. Risks / Open Items

- **Soniox + Safari mp4 (Phase 1 effectiveness).** Soniox `audioFormat:'auto'`
  decoding AAC/mp4 is **unverified**. Phase 1 removes the hard `MediaRecorder` throw
  on Safari and gives Soniox a container it can produce; whether Soniox then
  transcribes it must be confirmed on a device. Worst case: Safari Enhanced stays as
  today (no regression), and we learn the SDK route can't serve Safari.
- **DEFERRED — Standard/Enhanced send on Safari (issue #2 / R4).** Real impact
  **unconfirmed** (owner reports Safari "seems to work"). Before any code: (1) confirm
  on a real iPhone/Safari that an all-Standard room actually drops the Safari
  speaker; (2) if real, design a **latency-safe** PCM path (PCM16@24k ≈ 384 kbps vs
  Opus ≈ 32 kbps — ~12× upload; consider 16 kHz for the Deepgram-only path) so §0's
  no-added-delay rule holds; (3) keep per-peer PCM as an OR on top of the room
  decision so the #282 swap-race fix and listener-pays logic don't desync.
- **Older WebKit (< 14.1)** lacks `MediaRecorder`/`AudioWorklet` — out of scope.

## 9. References

- Files: `client/src/scripts/soniox.ts` (`sttMimeType`), `recording/utils.ts`
  (`pickMimeType` pattern), `audio-capture.ts`, `pcm-capture.ts`,
  `server/src/deepgram.rs` (`AudioFormat`), `server/src/lib.rs` (`notify_capture_formats`).
- Related: #281 (Soniox Android mimeType), #282 (capture swap race), #285 (Soniox CSP).
- External: WebKit `MediaRecorder` supports `audio/mp4`, not WebM.
