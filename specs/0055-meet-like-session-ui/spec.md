# 0055 — Meet-like session UI: duration, participant count, quick reactions

| | |
|---|---|
| **Status** | In progress |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-14 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | `<pending>` |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0023](../0023-call-toolbar-overflow-menu/spec.md), [0052](../0052-voice-command-timer/spec.md) |

## 1. Context & Problem

The call surface (issue #72) lacks the at-a-glance affordances every meeting tool has:

- **No session duration.** Nothing shows how long the call has been running.
- **No participant count.** You can't tell how many people are in the room without opening
  the participants panel from the ⋯ menu.
- **Reactions are buried.** Emoji reactions exist (they float over the stage, rate-limited to
  5/s) but are reachable only through the chat panel's emoji picker — too many taps for a
  spur-of-the-moment 👍 mid-conversation.

All three are pure presentation of state the client **already has**: `callStartedAt` is stamped
on `room_joined`, the `peerNames` map is kept in sync by the join/leave handlers, and
`sendEmoji()` already broadcasts reactions. So this is a client-only surface change — no
server, protocol, or data-model work.

## 2. Goals / Non-Goals

**Goals**
- A live **session-duration** clock, top-left, ticking every second from join.
- A live **participant count** (self + peers), top-right, updating on every join/leave.
- **Quick-reaction** buttons in the control bar that fire a reaction in one tap, stay put
  (no menu), and are spammable within the existing 5/s cap.
- Always visible but non-intrusive; zero changes to signaling, the server, or the mesh.

**Non-Goals**
- A new reaction transport or expanded emoji set (reuse `sendEmoji` + the existing 5/s limiter).
- Floating-emoji animation changes, speaking indicators, or a globally-synced clock (the timer
  is each client's own elapsed-since-join — listed as a future improvement in the issue).
- Moving/duplicating raise-hand (it stays a toggle in the ⋯ menu; quick reactions are emoji-only).

## 3. Requirements

- **R1 — Session duration.** As a participant, *when* I'm in a call, *then* a top-left chip shows
  elapsed time (`MM:SS`, rolling to `H:MM:SS` past an hour), updating every second from the moment
  I joined, and disappearing/ resetting when I leave.
- **R2 — Participant count.** As a participant, *when* someone joins or leaves, *then* a top-right
  chip shows the current count (self + remote peers) and updates in real time.
- **R3 — Quick reactions.** As a participant, *when* I tap a reaction button in the control bar,
  *then* the emoji floats over the stage for everyone immediately, the button does **not** open or
  close any menu, and I can tap it repeatedly (bounded by the existing 5/s limiter).
  - *Given* I tap faster than 5/s, *then* excess taps are silently dropped (no flood) — unchanged
    `reactionLimiter` behaviour.
- **R4 — Non-intrusive & accessible.** The chips sit in the call header (not over faces), the
  reaction group carries an aria-label, and the timer chip a title; nothing blocks existing
  controls or the ⋯ menu.

## 4. Design & Architecture

- **Components / files:**
  - `client/src/pages/index.astro` — DOM + CSS:
    - `#session-timer` chip (clock icon + `00:00`) in `.call-header`, left cluster after the
      room/visibility badges.
    - `#part-count` chip (users icon + number) in `.call-header`, `margin-left:auto` (right edge,
      coexisting with the rarely-shown `.call-balance`).
    - A `.ctl-group` of quick-reaction buttons (`#quick-reactions`) in `.control-bar`, between the
      subtitle/chat group and the ⋯ group; compact `.react-btn` styling.
  - `client/src/scripts/app.ts`:
    - `startSessionTimer()` / `stopSessionTimer()` — a 1 s `setInterval` (id in module state) that
      writes `formatClock((Date.now() - callStartedAt)/1000)` into `#session-timer`. Started in the
      `room_joined` handler (right after `callStartedAt = Date.now()`), cleared in the leave reset.
    - In `updateParticipantsList()` (already called on every join/leave), set `#part-count` to
      `items.length`.
    - Quick-reaction buttons created from a 4-emoji subset of `REACTION_LIST`, each wired to the
      existing `sendEmoji()` — same rate-limited broadcast the picker uses.
  - `client/src/scripts/i18n.ts` — two new keys (`sessionDurationTip`, `quickReactTip`) in all 8
    languages; the count chip reuses the existing `participantsTip`.
- **Reused helpers:** `formatClock` (timer-intent.ts), `sendEmoji` + `reactionLimiter`, `icon('timer')`,
  `icon('users')`.
- **Protocol / data model:** unchanged.
- **Key decisions:**
  - **Header chips, not stage-corner overlays** → the stage top corners already hold the REC badge
    (top-left, z5) and the voice-command timer badge (top-right, z6); the header is collision-free,
    genuinely top-left/right, and "non-intrusive" per the issue.
  - **Reuse `sendEmoji` / `reactionLimiter`** → identical broadcast + flood-protection as the picker;
    no new transport, and "spammable within reason" falls out of the existing 5/s cap for free.
  - **Count computed in `updateParticipantsList`** → single source of truth (`1 + peerNames.size`),
    already invoked on every join/leave, so no new sync path.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Header chips DOM + CSS (`#session-timer`, `#part-count`) | `client/src/pages/index.astro` |
| S1 | Session-timer interval + participant-count update | `client/src/scripts/app.ts` |
| S2 | Quick-reaction `.ctl-group` + buttons wired to `sendEmoji` | `index.astro`, `app.ts` |
| S3 | i18n keys (`sessionDurationTip`, `quickReactTip`) ×8 langs | `client/src/scripts/i18n.ts` |

## 6. Testing & Verification

- **Automated:** `astro check` (0 errors), `astro build`, and the 146-test unit suite stay green.
- **e2e (`e2e/meet-ui.spec.ts`, new — run green locally against the :3001 backend):** a solo join
  asserts the session chip ticks (MM:SS, advancing) (R1); a `NodePeer` joining/leaving drives the
  count `1 → 2 → 1` (R2); the control bar exposes exactly four `.react-btn`s that fire without
  opening/closing a menu (R3). Note: the e2e job is skipped on PR CI (needs the keyed backend).
- **Manual (desktop + mobile):** confirm chips don't overlap the REC / voice-timer badges or block
  the ⋯ menu, and reactions float for all peers (R4).
- **No new unit test:** presentation of existing synced state + reused send path; the e2e above
  pins R1–R3, per the spec 0052/0053 convention for in-call DOM behaviour.

## 7. Deployment & Operations

- Client-only. No env vars, migrations, or server changes. Vercel auto-deploys on `main`.

## 8. Risks / Open Items

- Adding a 4-button reaction group widens the control bar; on very narrow phones it wraps to an
  extra row (the bar is already `flex-wrap`). Acceptable; revisit if it crowds.
- The duration is per-client (starts at *my* join), not a room-global clock — matches the issue's
  MVP; a synced clock is listed there as a future improvement.

## 9. References

- Issue: #72 (refs specs 0002 video+chat, 0023 control bar, 0052 timer badge)
- Files: `client/src/pages/index.astro`, `client/src/scripts/app.ts`, `client/src/scripts/i18n.ts`
