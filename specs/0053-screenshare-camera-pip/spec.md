# 0053 — Keep camera visible (PiP) during screen-share

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-14 |
| **Shipped** | 2026-06-14 |
| **Version** | — |
| **Commits** | `867c03b` |
| **Depends on** | [0033](../0033-screenshare-pan-zoom/spec.md), [0002](../0002-video-calls-translated-chat/spec.md) |

## 1. Context & Problem

On desktop, starting a screen-share **replaces** the outgoing camera track with the
screen track (`mesh.replaceVideoTrack(screen)`). The sharer's tile shows the screen and
their camera feed disappears for everyone — presenters become invisible while they talk
over a slide. Google Meet keeps the presenter visible as a small Picture-in-Picture (PiP)
overlay on top of the shared screen. Closes issue #59 (split out from #50 item 4).

The mesh (specs 0002/0030/0031) sends **exactly one** outgoing video m-line per peer and
was deliberately built so every video change is a `replaceTrack` on that single
pre-negotiated sender — **never a renegotiation** (avoids offer-glare across the
full-mesh). Sending screen *and* camera as two simultaneous tracks would require
re-negotiating every peer connection when a share starts/stops, which contradicts that
design and carries real regression risk.

## 2. Goals / Non-Goals

**Goals**
- While sharing, peers see the **screen** as the main view with the sharer's **camera as a
  small PiP overlay** in the bottom-right corner.
- Stopping the share returns cleanly to the normal camera tile (no orphan tracks, no
  leaked canvas/RAF loop).
- Zero changes to the signaling protocol, the server, or the mesh's
  one-video-track-per-peer model — **no renegotiation**.
- Graceful fallback: camera off / audio-only join → the composite is just the screen
  (no PiP), exactly today's behaviour.

**Non-Goals**
- Letting viewers reposition or resize the PiP (it is baked into the shared frame). The
  remote, independently-movable PiP window is the separate Document-PiP feature (issue #73).
- A second simultaneous WebRTC video track / SVC / simulcast.
- Mobile screen-share (still desktop-only — `getDisplayMedia` is gated off on mobile).

## 3. Requirements

- **R1 — Presenter stays visible.** As a viewer, when a peer screen-shares with their
  camera on, I want to see their camera as a PiP overlay on the screen.
  - *Given* a peer sharing with camera on, *when* the share is active, *then* every remote
    tile shows the screen with the sharer's camera composited in the bottom-right corner.
- **R2 — Clean stop.** As a sharer, when I stop sharing, the view returns to my normal
  camera tile.
  - *Given* an active share, *when* I stop it, *then* peers see my camera tile (or
    camera-off avatar) again, the canvas/RAF loop is torn down, and no screen/canvas track
    is left running.
- **R3 — Camera-less fallback.** As a sharer who joined audio-only or with the camera off,
  *when* I share, *then* peers receive the screen with **no** PiP (no regression to spec
  0033 / issue #4's camera-less share path).
- **R4 — Toggle camera mid-share.** As a sharer, *when* I turn my camera on/off while
  sharing, *then* the PiP appears/disappears inside the composite **without** hiding the
  shared screen on peers and without a renegotiation.
- **R5 — Self-view & recording match.** *Given* an active share, *then* the sharer's own
  tile and any composite recording show exactly what peers see (screen + PiP).

## 4. Design & Architecture

- **Components / files:**
  - `client/src/scripts/screenshare-pip.ts` — new. `ScreenSharePip` draws the screen
    full-frame onto a hidden canvas and the camera as a rounded PiP overlay (bottom-right),
    then `canvas.captureStream()`s a single composited video track. Pure geometry helpers
    `fitCanvas` / `pipRect` / `coverCrop` are exported and unit-tested (the canvas/RAF parts
    are covered by e2e + manual, mirroring `recording/canvas-compositor.ts`).
  - `client/src/scripts/app.ts` — `startScreenShare`/`stopScreenShare` build/tear down the
    compositor; the **composite** track is what reaches peers, the self-tile, and the
    recorder. `setOutgoingVideo` / `disableCamera` / `toggleCamera` feed the compositor the
    current camera track (R4) instead of pushing to peers during a share.
- **Data model:** module-level `screenPip: ScreenSharePip | null` alongside `screenStream`.
- **Protocol / API:** unchanged. Still one video sender per peer; the `screen_share` and
  `mute_video` WS messages are unchanged.
- **Sequence (start):** getDisplayMedia → `new ScreenSharePip(screen)` →
  `setCamera(currentCameraTrack)` → `start()` returns the composite stream →
  `mesh.replaceVideoTrack(compositeTrack)` (same sender, no renegotiation) → self-view +
  recorder point at the composite.
- **Key decisions:**
  - **Canvas compositor, not a second track** → keeps the mesh's no-renegotiation
    invariant and needs no server/protocol change. Cost: the screen is re-encoded through a
    canvas (slightly softer text, modest extra CPU on the sharer) and the PiP is baked-in
    (not viewer-movable). Accepted per the issue's "compositor" option and issue #59
    discussion.
  - **Canvas capped to 1920×1080** preserving the screen's aspect → bounds CPU when sharing
    a 4K display while keeping common 1080p shares pixel-exact.
  - **1s safety tick** alongside RAF → background tabs throttle RAF; the tick keeps frames
    flowing to peers (same trick as the recording compositor).
  - **During a share, camera toggles never send `mute_video` / never hide the self tile** →
    the composite is always present, so hiding it would blank the shared screen (also fixes
    a latent bug where toggling the camera mid-share hid the screen on peers).

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `ScreenSharePip` compositor + pure geometry helpers | `client/src/scripts/screenshare-pip.ts` |
| S1 | Wire composite into start/stop + camera-track feed | `client/src/scripts/app.ts` |
| S2 | Unit tests for geometry helpers | `client/src/scripts/screenshare-pip.test.ts` |

## 6. Testing & Verification

- **Unit** (`screenshare-pip.test.ts`): `fitCanvas` caps & preserves aspect; `pipRect`
  sizes/positions the overlay bottom-right; `coverCrop` centre-crops the camera to the box.
- **e2e** (`screenshare.spec.ts`):
  - *existing test* — the camera-less sharer path still delivers flowing video to a viewer
    and cleans up on stop (R3, R2): proves the compositor is a drop-in for the raw screen
    track.
  - *new test* — a sharer **with** a camera composites it (viewer sees flowing video + 🖥
    badge), then toggling the camera off mid-share keeps the composite flowing on the
    viewer with no camera-off avatar (R1, R4) — guards the latent "toggle blanks the
    screen" bug the fix closes.
- **Manual:** two cameras → start share → both peers see screen + PiP; toggle camera
  on/off mid-share (PiP appears/disappears, screen never blanks); blur toggle mid-share;
  stop → camera tile returns.

## 7. Deployment & Operations

- Client-only. No env vars, migrations, or server changes. Vercel auto-deploys on `main`.

## 8. Risks / Open Items

- Canvas re-encode softens screen text vs. the native track; if users complain, revisit the
  second-track approach behind a capability check.
- PiP position is fixed (bottom-right); a future spec could let the sharer pick a corner.

## 9. References

- Issue: #59 (refs #50, specs 0033, 0002)
- Files: `client/src/scripts/screenshare-pip.ts`, `client/src/scripts/app.ts`
