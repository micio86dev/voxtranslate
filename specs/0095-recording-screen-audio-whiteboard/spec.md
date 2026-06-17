# 0095 — Recording: shared-screen audio + whiteboard capture

| | |
|---|---|
| **Status** | In progress |
| **Owner** | micio86dev |
| **Created** | 2026-06-17 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | _(pin on merge)_ |
| **Depends on** | 0010 (composite recording), 0053 (screen-share PiP), 0085 (screen-share audio), 0045 (whiteboard) |

## 1. Context & Problem

The composite recorder (spec 0010) tiles every participant's A/V onto a canvas and
captures it with mixed audio. Two sources were missing from the saved file (issue
#230):

1. **Shared-screen audio.** While screen-sharing, the self source handed to the
   recorder was the ScreenSharePip canvas stream — which is **video-only**. So the
   recorder lost *all* self audio during a share (the mic *and* the shared tab/
   system audio), even though peers heard it (after #229).
2. **Whiteboard.** Collaborative drawing on the whiteboard (spec 0045) never
   appeared in the recording at all.

## 2. Goals / Non-Goals

**Goals**
- A recording made while screen-sharing with "share audio" includes the shared
  audio (mixed with the mic), in sync with the shared-screen video.
- When the whiteboard is open, its live canvas is captured as a tile in the
  recording.

**Non-Goals**
- Per-source audio tracks / channel separation (single mixed track, as today).
- A dedicated whiteboard "scene" or layout — it is tiled like any participant.
- Streaming-to-disk; chunks still buffer in memory (the 0010 follow-up stands).

## 3. Requirements

- **R1 — Shared audio in the recording.** As a host, when I record while sharing a
  tab/screen with audio, *then* the saved WebM plays the shared audio (and my mic),
  not silence, for the duration of the share.
  - *Given* a share with audio is active, *when* the recorder captures the self
    source, *then* its audio track is the mic+screen mix (`shareMixTrack`).
  - *Given* the share stops, *then* the self audio reverts to the mic.
- **R2 — Mid-share record start.** *Given* a share is already active, *when* I press
  record, *then* the recording opens with the composite video + mixed audio.
- **R3 — Whiteboard tile.** *Given* the whiteboard is open, *when* I am (or start)
  recording, *then* the board appears as a tile; *when* I close it, the tile drops.

## 4. Design & Architecture

- **Components / files:**
  - `app.ts::selfRecordingStream()` — builds the self stream the recorder captures:
    current self **video** (screen composite while sharing, else camera/blur) +
    current self **audio** (`shareMixTrack` while sharing-with-audio, else mic).
    Used at record start (`recorderSources`) and on share start/stop.
  - `app.ts::whiteboardRecordingSource()` / `WB_RECORDING_ID` — wraps
    `whiteboard.captureStream()` as a synthetic participant; added/removed on
    `toggleWhiteboard` while `isRecording`, and included in `recorderSources()`
    when the board is open at record start.
  - `whiteboard.ts::captureStream(fps=10)` — `canvas.captureStream` of the board.
- **Key decisions:**
  - *Fold audio back into the self source* rather than teaching the recorder about
    screen audio separately — keeps the mixer's "one audio track per source" model
    and reuses the already-correct `shareMixTrack` from #229/0085.
  - *Whiteboard as a participant tile* (not an overlay) — minimal change, reuses the
    existing layout/compositor; matches "composited canvas capture" from the issue
    without a bespoke scene compositor.

## 5. Testing

- Client unit + type checks green (`vitest`, `tsc`). Recorder wiring is exercised
  via the existing `recording.test.ts`; the A/V plumbing (MediaStream/captureStream)
  is integration-only and verified manually in-browser.
