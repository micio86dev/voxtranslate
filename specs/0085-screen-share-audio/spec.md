# 0085 — Share tab/window audio with the screen

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Micio Dev |
| **Created** | 2026-06-16 |
| **Shipped** | 2026-06-16 |
| **Version** | — |
| **Commits** | `1139404` |
| **Depends on** | [0033](../0033-screen-share/spec.md), [0053](../0053-screen-share-pip/spec.md) (screen share + PiP) |

## 1. Context & Problem

Screen sharing called `getDisplayMedia({ video: true, audio: false })`, so the
browser never offered the "share tab/system audio" checkbox — the owner couldn't
share a window's audio with the call. We want that checkbox, and when ticked, the
shared audio should reach the other participants.

## 2. Goals / Non-Goals

**Goals**
- The screen picker offers the share-audio checkbox.
- When the user shares audio, peers hear it mixed with the sharer's mic.
- Zero change to the voice path when audio is **not** shared.

**Non-Goals**
- No audio for late joiners who connect mid-share (they get mic-only until the
  next share); no system-audio on platforms the browser doesn't support it on
  (Firefox/Safari/mobile, and system — vs tab — audio off Windows/ChromeOS).
- Shared audio is **not** sent to speech-to-text (STT reads the mic directly).

## 3. Requirements

- **R1 — Checkbox.** `getDisplayMedia` requests `audio: true` so the browser shows
  the share-audio control.
- **R2 — Mix to peers.** If the captured stream has an audio track, mix it with the
  mic via WebAudio and `replaceTrack` it onto the outgoing audio sender.
- **R3 — Clean revert.** On stop (or picker cancel / failure), the audio sender
  reverts to the plain mic track and the WebAudio graph + mixed track are released.
- **R4 — No-audio safety.** No screen-audio track → the mic sender is never
  touched (the common share carries no voice-path risk).

## 4. Design & Architecture

- `client/src/scripts/webrtc.ts`: `replaceAudioTrack(track)` mirrors
  `replaceVideoTrack` — swaps the `kind === 'audio'` sender on every peer, no
  renegotiation.
- `client/src/scripts/app.ts`:
  - `startScreenShare`: `audio: true`; if the stream has audio, build an
    `AudioContext` mixing `localStream` mic + screen audio into a
    `MediaStreamDestination` track and `mesh.replaceAudioTrack(mix)`.
  - `stopScreenShare` + the start `catch`: revert to the mic track, `stop()` the
    mix, `close()` the context.

## 5. Testing & Verification

- `vitest`: `replaceAudioTrack swaps the audio sender and can revert it`.
- `astro check` + build green.
- **Manual (owner, real call required):** share a tab playing audio with the box
  ticked → a peer hears it; confirm normal voice still works during/after, that
  muting still works, and that a plain (no-audio) share is unaffected. Browser
  support: Chrome/Edge desktop.

## 6. Deployment & Operations

- Ships with the Vercel client deploy. No server change.

## 7. Risks / Open Items

- Touches the outgoing audio path while sharing-with-audio. Mitigated: the new
  path runs **only** when a screen-audio track exists; a suspended `AudioContext`
  is resumed; revert is covered on stop/cancel/failure. Needs the real-call check
  in §5 before it's trusted in prod.

## 8. References

- Files: `client/src/scripts/app.ts`, `client/src/scripts/webrtc.ts`.
