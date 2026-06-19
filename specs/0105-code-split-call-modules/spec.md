# 0105 — Code-split in-call modules out of the landing entry chunk

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-19 |
| **Shipped** | 2026-06-19 |
| **Version** | 1.1.x |
| **Commits** | `c2ff348` (#290) |
| **Depends on** | [0104](../0104-mobile-performance-lazy-i18n/spec.md) |

## 1. Context & Problem

After spec 0104 the landing entry chunk was **205 KB raw / 64.8 KB gz**, of which Lighthouse
flagged **~47 KB as unused** on first load. The cause: `client/src/scripts/app.ts` (the
4.5k-line orchestrator) **statically imports every in-call module** — WebRTC mesh, chat, the
Soniox receive pipeline, audio/PCM capture, the collaborative whiteboard, the mini-games
(quiz alone is 40 KB raw), recording, screen-share PiP, virtual background, the post-call
session screen. A static import lands in the entry chunk **regardless of when it's
instantiated**, so the landing/lobby page downloaded + parsed all of it even though it never
makes a call.

## 2. Goals / Non-Goals

**Goals**
- Remove in-call code from the landing entry chunk; first-load JS toward < 50 KB gz.
- Mobile Performance ≥ 95 / LCP < 2.5 s where the network allows; no unused-JS on landing.
- The call flow works **identically** — just loaded later.

**Non-Goals**
- Touching i18n, billing, the translation engines, WebSocket message formats, or the backend.
- Splitting shared utilities (`i18n`, `auth`, `engines`, `langmap`, `icons`, `content`,
  `pcm-playback`, `peer-id`, `net-status`, `glossary`, `api`) — they're used on the landing path
  or are cross-cutting.

## 3. Requirements

- **R1 — Landing loads no call code.** *Given* a fresh landing/lobby visit, *when* the page
  loads, *then* none of the split call chunks appear in the network waterfall.
- **R2 — Zero perceived join latency.** *Given* a user opening pre-join, *when* they then click
  join, *then* the in-call modules are already loaded (warmed during camera/device setup).
- **R3 — Identical call behaviour.** *Given* a call, *when* WebRTC/chat/Soniox/whiteboard/games/
  recording/screen-share/blur/session-screen are used, *then* they behave exactly as before.
- **R4 — Collaborative features survive remote-first activation.** *Given* a peer starts the
  whiteboard or a game, *when* the op arrives before the local user opened it, *then* it renders
  (the feature is constructed at pre-join, not on local click).
- **R5 — Graceful load failure.** *Given* a chunk can't be fetched (offline), *when* the user
  tries to join/record, *then* an error is shown (`loadFailed`) and the app doesn't break.

## 4. Design & Architecture

Two split strategies by trigger:

- **Core + collaborative bundle — `call-modules.ts`** (`loadCallModules()` → `CallModules`
  namespaces): `webrtc`, `chat`, `soniox`, `audio-capture`, `pcm-capture`, `mic-meter`,
  `whiteboard`, `tictactoe`, `quiz`. A remote peer can trigger the collaborative ones, so they
  load as **one bundle at pre-join entry** (`goPrejoin` warms `ensureCallModules()`; `startCall`
  awaits it — the cached promise is usually already settled, so the download overlaps camera
  setup). In `app.ts` the module imports become `import type` and the construction sites read the
  runtime classes via `callMods!.<ns>.<Class>` (mesh/chat/capture/mic-meter/Soniox in
  `openSocket`/`startCall`; whiteboard/tictactoe/quiz constructed once in `initCallFeatures` via
  definite-assignment holders, so the ~60 call sites are untouched).
- **Local-only features — per-button dynamic `import()`**: `recording/composite-recorder`,
  `screenshare-pip`, `virtual-background`, `session-screen`. A remote peer can't trigger these,
  so each loads on its own activation (`startRecording`, `startScreenShare`, `buildOutgoing`,
  the post-call/ledger open). `recording/utils` stays static (`isRecordingSupported` is needed at
  `startCall` to show the button).

**Key decisions:**
- *Load the core bundle at pre-join, not at join* → overlaps with camera/device setup = no
  perceived latency; landing/lobby still never fetch it. *Rejected:* load at the join click
  (adds a visible stall) and a single `call.ts` extraction (3k-line move, far higher risk).
- *Definite-assignment holders (`let whiteboard!: Whiteboard`) constructed once at pre-join* →
  preserves "always present during a call" for collaborative features (R4) without changing the
  ~60 `whiteboard.`/`quiz.` call sites to null-safe.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `call-modules.ts` loader (cached, retry-on-fail) | `client/src/scripts/call-modules.ts` |
| S1 | `import type` for the split modules; `callMods` holder + `ensureCallModules`; warm at `goPrejoin`, await at `startCall`; route the ~8 core construction sites | `client/src/scripts/app.ts` |
| S2 | Collaborative features → `initCallFeatures(m)` with definite-assignment holders | `client/src/scripts/app.ts` |
| S3 | Per-button dynamic `import()` for recording / screenshare / virtual-bg / session-screen | `client/src/scripts/app.ts` |

## 6. Testing & Verification

- **Type:** `npm run check` (astro check) — 0 errors.
- **Unit:** `npm run test:unit` — 542 green.
- **Bundle:** post-build, the entry chunk is **205 → 112 KB raw / 64.8 → 34.8 KB gz** (−46%);
  13 call modules are separate on-demand chunks (`webrtc`, `chat`, `soniox`, `quiz`,
  `whiteboard`, `composite-recorder`, …); `index.html` statically loads only the entry; the
  shared `content` chunk is unchanged.
- **Lighthouse** (mobile, Slow-4G, CPU 4×): Performance ≈ **95** (92–97 across runs), LCP ≈ 2.7 s
  (2.4–3.0), TBT 100 → ~60 ms, TTI 2.8 → ~2.6 s, unused JS 47 → 26 KB. See
  `AUDIT_REPORT_mobile_perf.md` → Post-Split Results.
- **E2E (Playwright, guest backend on :3001):** the call suite — join/leave, in-call UI,
  whiteboard, screen-share, pre-join — exercises every lazy path; assert no console errors.

## 7. Deployment & Operations

No env vars, migrations, or flags. Static client; Vercel auto-deploys on merge. No backend change.

## 8. Risks / Open Items

- A first-ever call pays one extra chunk fetch, hidden behind pre-join camera setup; a true cold
  network failure surfaces `loadFailed` and keeps the user on pre-join (R5).
- Remaining ~26 KB "unused JS" is landing/lobby/pre-join/auth code in the entry; splitting
  further has diminishing returns and higher risk — left as-is.
- `api.ts` (29 KB) stays static — its functions are spread across features; splitting it cleanly
  is a separate effort.

## 9. References

- Files: `client/src/scripts/call-modules.ts`, `client/src/scripts/app.ts`
- Audit: `AUDIT_REPORT_mobile_perf.md`
