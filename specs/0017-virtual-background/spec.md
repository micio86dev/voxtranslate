# 0017 — Virtual Background (Camera Blur)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(see PR for #6)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md) |

## 1. Context & Problem

The camera feed shows the user's raw environment — a messy room, a shared
office, anything behind them. Users expect the privacy/polish of a blurred
background that modern meeting tools (Zoom, Google Meet) offer. Issue #6 asks
for real-time background segmentation with, as the MVP, a background **blur**
toggle.

## 2. Goals / Non-Goals

**Goals**
- Real-time background blur on the local camera, visible to peers, the self
  tile, and recordings.
- A single ON/OFF control in the in-call control bar.
- Zero cost when unused: the segmentation model is loaded lazily, only on first
  enable.
- Graceful degradation: if the model can't load or the browser lacks
  `canvas.captureStream`, the call continues with the unprocessed camera.

**Non-Goals (future)**
- Custom image / video backgrounds and uploads.
- Background selection UI beyond a single blur toggle.
- Pre-join blur (the toggle is in-call only for now).
- GPU/low-end tuning, edge smoothing, lighting correction.

## 3. Requirements

- **R1 — Toggle blur.** As a participant with the camera on, I want to toggle
  background blur, so that my surroundings are hidden.
  - *Given* my camera is on, *when* I click the background button, *then* peers,
    my self tile, and recordings show me sharp over a blurred background; *when*
    I click it again, *then* the raw camera is restored.
- **R2 — Persist across camera/share toggles.** *Given* blur is on, *when* I turn
  the camera off and on again, or start/stop screen sharing, *then* blur is still
  applied to the camera feed afterward.
- **R3 — LED honoured.** *Given* blur is on (the raw camera is owned by the
  effect, not `localStream`), *when* I turn the camera off, *then* the real
  device is released and the hardware LED turns off (consistent with #5).
- **R4 — Graceful fallback.** *Given* the segmentation model fails to load,
  *when* I enable blur, *then* the call keeps working with the raw camera and a
  toast explains the effect is unavailable.

## 4. Design & Architecture

- **Components / files:**
  - `client/src/scripts/virtual-background.ts` — `VirtualBackground` class +
    lazy MediaPipe loader. Wraps a raw camera track, runs MediaPipe Selfie
    Segmentation per frame, composites sharp-subject-over-blurred-frame on a
    canvas, and republishes via `canvas.captureStream()`. Exposes `active`
    (false ⇒ fell back to raw) and `source` (the raw track it reads).
  - `client/src/scripts/app.ts` — `bgMode` state, `toggleBgBlur`, and the
    `buildOutgoing`/`setOutgoingVideo`/`currentRawCameraTrack` helpers that route
    the camera through the effect and into `localStream` + peers + recorder.
  - `index.astro` `#btn-bg`, `icons.ts` `sparkles`, `i18n.ts`
    `bgBlurTip`/`bgBlurOn`/`bgUnavailable` (8 locales).
- **Segmentation:** MediaPipe Selfie Segmentation (`modelSelection: 1`), loaded
  from `cdn.jsdelivr.net/npm/@mediapipe/selfie_segmentation` via a one-time
  script injection (no npm dependency, no bundled model).
- **Compositing recipe (per frame):** draw mask → `source-in` draw frame (keep
  subject) → `destination-over` + `filter: blur(8px)` draw frame (blurred
  background behind). 8 px blur, 24 fps capture.
- **Track model:** `localStream`'s video track is always the *outgoing* track —
  the raw camera when blur is off, or the processed canvas track when on. The
  raw camera then lives as the VB's `source`. This keeps a single source of
  truth so existing screen-share / recorder / placeholder-on-disable logic
  (specs 0010, #5) works unchanged.
- **Key decisions:**
  - *CDN lazy-load over bundling* — keeps the build lean and avoids Vite/WASM
    asset wrangling; cost is a runtime CDN dependency, acceptable for an opt-in
    effect with a fallback.
  - *Processed track in `localStream`* — `localStream`'s video track is the
    outgoing track, pushed to peers with `MeshManager.replaceVideoTrack` (the
    always-negotiated video transceiver makes this reliable even for audio-only
    joins); new peers automatically receive the blurred track from `localStream`.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `VirtualBackground` (loader, segmentation, canvas composite, captureStream, fallback) | `virtual-background.ts` |
| S1 | Control-bar button, icon, i18n (8 locales) | `index.astro`, `icons.ts`, `i18n.ts` |
| S2 | App wiring: `bgMode`, toggle, outgoing-track routing, camera-off/leave teardown | `app.ts` |
| S3 | Unit tests (URL/loader/fallback) | `virtual-background.test.ts` |

## 6. Testing & Verification

- Unit (`virtual-background.test.ts`): CDN URL building, loader fast-path
  (global present), non-browser fallback (no `document`), script injection +
  promise caching, and `start()` returning the raw track / `active === false`
  when no model — pins **R4**.
- Typecheck (`astro check`) + production build green.
- Manual (real camera, Vercel preview): toggle blur on/off (**R1**), camera
  off→on and screen share start/stop keep blur (**R2**), camera LED off while
  blurred then disabled (**R3**). Canvas + segmentation need a real camera, so
  the compositor is verified manually (consistent with spec 0010's compositor).

## 7. Deployment & Operations

- No env vars, no migrations, no server changes — pure client.
- Runtime dependency on `cdn.jsdelivr.net` for the model; no CSP currently
  blocks it. If a CSP is added later, allow that origin in `script-src` /
  `connect-src`.

## 8. Risks / Open Items

- CPU/GPU cost: segmentation keeps running during screen share even though the
  camera isn't shown — future optimization to pause it.
- CDN availability/offline: blur silently degrades to the raw camera.
- Self-hosting the model (drop the CDN dependency) is a possible follow-up.

## 9. References

- Issue: #6
- Files: `client/src/scripts/virtual-background.ts`, `client/src/scripts/app.ts`
- External: https://google.github.io/mediapipe/solutions/selfie_segmentation
