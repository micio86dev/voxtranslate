# 0070 — UX fixes batch: chat input/emoji, call-header overlaps, mini-game lifecycle, session CTAs

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-15 |
| **Shipped** | 2026-06-15 |
| **Version** | — |
| **Commits** | S1 `36d9b17` (#143), S2 `1bcb702` (#144), S3 `c6d85df` (#145), S4 `bd10c02` (#149), S5 `b58d8de` (#150); follow-up fixes `2988bc8` (#146 popover z-index), `9dc9126` (#147 hand icon), `5c3bae6` (#148 TTT rejoin) |
| **Depends on** | [0018](../0018-chat-file-upload/spec.md), [0034](../0034-ui-cta-zfix/spec.md), [0038](../0038-session-glossary-ux/spec.md), [0046](../0046-minigame-tictactoe/spec.md), [0047](../0047-minigame-quiz/spec.md), [0057](../0057-pip-controls/spec.md), [0061](../0061-immersive-call-overlays/spec.md), [0067](../0067-ai-quiz-on-demand/spec.md) |

## 1. Context & Problem

A round of in-call usability testing surfaced five clusters of defects/rough edges. None
change core capability; all are correctness/UX fixes on features that already shipped.
They are independent and ship as **five separate PRs** (slices S1–S5), all tracked under
this one spec so the intent is recorded once.

Relevant recent history this spec builds on (do **not** redo it):

- **#139 (`26192ff`)** folded the self name into the top-left meta and made it a single
  wrapping line (`clock | ROOM · you | visibility · timer · balance`), bumped the emoji
  glyph to `1.3rem`, and centered the whiteboard ✕. It did **not** give the emoji button
  real height parity with the attach button, and it did **not** address REC-badge / right-
  badge collisions, avatar-vs-initials, report-button coverage, or PiP overlap.
- **0034 / 0038** established the tonal secondary button (`.btn-ghost`): an accent-tonal
  fill (`color-mix(accent 14%, surface)`) with full text colour, meant to read as
  on-brand and tappable on dark surfaces.

The five problem clusters:

1. **Chat input & emoji UX.** The emoji button doesn't match the attachment button's
   height; the message field is a single-line `<input maxlength=500>` (poor for longer
   text); the emoji-picker action buttons are small, low-contrast, and not touch-friendly.
2. **Call-header layout / overlays.** On narrow widths the `REC : MM:SS` badge (center,
   `z-index:5`), the wrapping top-left meta (`z-index:8`), and the top-right participant
   badge (`z-index:8`) can collide; per-cell report/block buttons (`.cell-actions`, no
   explicit z-index, hover-only) get covered; the participant badge shows an initial
   ("M") even when an avatar URL is available; PiP needs an overlap audit.
3. **Tic-Tac-Toe.** After "New Game" the board is unresponsive (the initiator is X but the
   game sits in `status:'waiting'`, which disables every cell until a second player clicks
   Join); state-sync robustness across late joiners is unproven; behaviour with >2
   participants is undefined (3rd+ are silent permanent spectators, no rotation).
4. **Quiz sync & lifecycle.** Creating a quiz only opens the modal for the creator (peers
   receive the state but the modal container is never un-hidden); a new quiz can clobber an
   in-progress one; there is no cancel; per-participant completion isn't tracked.
5. **Session Details CTAs.** The download / AI-action CTAs use `.btn-ghost`, whose 14%
   accent wash over the dark surface reads as flat grey and visually disconnected from the
   primary actions.

## 2. Goals / Non-Goals

**Goals**

- Chat input that comfortably handles long messages, with a clear character budget, and
  an emoji affordance that is visually consistent and touch-friendly.
- A call header in which the recording indicator, balance, name, CTAs, participant area
  and moderation controls never overlap — desktop, mobile, and PiP.
- Participant identity shown as an avatar image whenever one exists, initials otherwise.
- A Tic-Tac-Toe that is immediately playable after "New Game", stays in sync across all
  clients, and has defined ≤2-active-players + spectator-rotation semantics.
- A quiz whose modal opens for everyone on start, that forbids a second concurrent quiz,
  is cancellable, and tracks per-participant completion.
- Session Details CTAs that are unmistakably part of the VoxTranslate design system.

**Non-Goals**

- No server-side anti-cheat / authoritative game-rule enforcement (the relay stays
  game-agnostic; see 0046). Tic-Tac-Toe & quiz remain client/host-authoritative.
- No new emoji-picker engine or third-party picker library; restyle the existing one.
- No redesign of the whole call layout — only the floating header/overlay coordination.
- No avatar upload feature; reuse the existing `avatar_url` already on peers/self.
- No persistence of quiz results to the DB (completion tracking is in-room/in-state only).

## 3. Requirements

> Grouped by area. `R{area}.{n}`.

### Area 1 — Chat input & emoji UX

- **R1.1 — Auto-resizing textarea.** As a user typing a longer message, I want the field
  to grow with my text instead of scrolling one line.
  - *Given* the chat composer, *when* I type past one line, *then* the field grows up to a
    capped max height (then scrolls internally), and shrinks back when text is removed.
  - *Given* focus in the textarea, *when* I press **Enter**, *then* the message sends;
    *when* I press **Shift+Enter**, *then* a newline is inserted (no send).
  - *Given* I sent or cleared the message, *then* the field resets to its single-row height.
- **R1.2 — Char limit + counter.** As a user, I want to see how much room I have left.
  - *Given* a configurable max (default 500, single source of truth in TS + the textarea
    `maxlength`), *when* my text length approaches the cap (e.g. ≥ ~80%), *then* a counter
    `{used}/{max}` is shown bottom-right of the composer and switches to a warning style
    near/at the cap; input is hard-capped at the max (incl. emoji insertion).
- **R1.3 — Emoji button parity.** As a user, I want the emoji button to look like a peer of
  the attachment button.
  - *Given* the chat input row, *when* both the emoji and attach buttons are visible,
    *then* they share the same height, width and visual treatment (same `icon-btn` sizing),
    and align with the textarea's first row and the send button.
- **R1.4 — Emoji picker redesign.** As a mobile user, I want emoji/reaction buttons that
  are easy to tap and clearly grouped.
  - *Given* the open picker, *then* the action buttons have larger touch targets
    (≥ ~40px tap area), comfortable spacing, and a clear visual separation between the
    "react" row (broadcast to room) and the "insert" grid (into the message), with hover/
    active/focus states that read as interactive (not the current near-invisible grey).
- **R1.5 — Cross-device consistency.** *Given* desktop and mobile (the bottom-sheet chat),
  *then* the composer, counter and picker stay usable and visually consistent.

### Area 2 — Call header layout & overlays

- **R2.1 — No header overlaps.** As a participant, I want the floating header elements to
  never overlap. *Given* any viewport width (incl. ~320px) and the REC badge visible,
  *when* the meta line wraps and the participant badge is shown, *then* the REC indicator,
  balance, name and top controls remain fully visible and non-overlapping (coordinated
  positions / reserved lanes / responsive repositioning of the REC badge).
- **R2.2 — Moderation controls reachable.** *Given* a video cell, *when* I hover/focus it,
  *then* the report/block buttons render above any overlapping floating element (explicit
  stacking) and are never covered by the participant badge or other overlays.
- **R2.3 — Avatars over initials.** *Given* a participant (self or peer) that has an
  `avatar_url`, *then* the on-video participant badge (and participants-list rows) show the
  avatar image; *when* none exists or it fails to load, *then* fall back to the initial(s)
  + gradient currently used.
- **R2.4 — PiP without overlap.** *Given* PiP mode, *then* any header/overlay elements that
  appear there do not overlap the PiP controls; audit and fix any collisions.

### Area 3 — Tic-Tac-Toe

- **R3.1 — Playable after New Game.** As the player who starts a game, I want the board
  usable right away when an opponent is present. *Given* exactly two participants in the
  call, *when* I click "New Game", *then* the second participant is seated as O and the
  board is immediately playable (X to move) for both — no manual Join step. *Given* I am
  alone (or no opponent has joined), *then* the board shows a clear "waiting for opponent"
  state, and becomes playable the moment a second player is seated.
- **R3.2 — Synchronised state.** *Given* a move by any player, *then* every other client
  (including a participant who joined mid-game via snapshot) converges to the same board;
  stale/out-of-order or illegal incoming states are rejected so clients don't diverge.
- **R3.3 — Two active players, rest spectate.** *Given* >2 participants, *then* exactly two
  hold seats (X/O); everyone else is a read-only spectator who sees the live board.
- **R3.4 — Spectator rotation.** *Given* a finished match with ≥1 spectator waiting, *when*
  the next game starts, *then* seats rotate so a waiting spectator gets a seat (deterministic
  order, e.g. join order / loser-rotates-out), and the rotation is visible to all.

### Area 4 — Quiz sync & lifecycle

- **R4.1 — Opens for everyone.** *Given* a participant starts (or AI-generates) a quiz,
  *then* the quiz modal opens automatically for every participant in the room, not only the
  creator.
- **R4.2 — Single active quiz.** *Given* a quiz is in progress (not `done`/cancelled),
  *when* anyone attempts to start/generate another, *then* it is blocked with a clear
  message; starting is only allowed when there is no active quiz.
- **R4.3 — Cancellable.** *Given* an active quiz, *when* the creator (or an authorized
  user) clicks "Cancel Quiz", *then* the quiz ends for everyone, the modal closes (or shows
  a cancelled state), and all quiz state is reset so a new quiz can start.
- **R4.4 — Per-participant completion.** *Given* an active/finished quiz, *then* the host
  tracks, per participant, how many questions they answered (and whether they finished),
  surfaced in the quiz UI (e.g. on the leaderboard / status).

### Area 5 — Session Details CTAs

- **R5.1 — On-brand CTAs.** As a user on the Session Details page, I want the CTAs to match
  the app's design language. *Given* the download and AI-action buttons, *then* their
  colours, typography, radius, hover/focus/active and mobile sizing align with the
  established button system (primary `--accent-strong`; secondary clearly accent-tonal,
  not flat grey), with a coherent primary-vs-secondary hierarchy on the page.

## 4. Design & Architecture

### Area 1 — Chat input & emoji

- **Components / files:** `client/src/pages/index.astro` (chat composer markup ~L452–467 +
  CSS `.chat-input-row` ~L1297, `.emoji-*` ~L1471–1497, mobile ~L1873); `client/src/scripts/app.ts`
  (`sendChat` ~L2060, `insertEmoji` ~L3208, emoji wiring ~L3146); `client/src/scripts/chat.ts`
  (`sendMessage` trims/sends).
- **Markup:** replace `<input id="chat-input" type="text" maxlength=500>` with
  `<textarea id="chat-input" rows="1" maxlength=…>`; add a `<span class="chat-counter">`
  inside/under the row, bottom-right aligned. Keep ids stable where other code references
  them (`chat-input`); update the cast in `app.ts` to `HTMLTextAreaElement`.
- **Auto-resize:** on `input`, reset `height:auto` then set to `scrollHeight` capped at a
  max (e.g. ~5 rows / `max-height`), toggling `overflow-y` when capped. Reset height after
  send/clear. Define `CHAT_MAX_LEN` once and apply to both the element `maxlength` and the
  counter; `insertEmoji` already guards `maxLength` — keep it.
- **Enter vs Shift+Enter:** the existing `keydown` Enter→send becomes `if (e.key==='Enter'
  && !e.shiftKey){ e.preventDefault(); sendChat(); }`.
- **Button parity:** give `.emoji-btn` and `.attach-btn` (and `.send-btn`) a shared
  `min-height`/`width` via the `icon-btn` sizing in `.chat-input-row` so all controls match
  the textarea's first-row height; drop the `!important` font hack where possible.
- **Picker redesign:** enlarge `.emoji-grid button`/`.emoji-react-row button` tap targets
  (min ~40px, larger radius, real hover/active/`:focus-visible` using accent-tonal, not
  `var(--border)`), increase grid gap, and strengthen the react/insert section divider.
- **Key decision:** textarea over content-editable — simpler, preserves maxlength + plain
  text (chat is plain-text + emoji), avoids paste-sanitisation risk.

### Area 2 — Call header & overlays

- **Components / files:** `client/src/pages/index.astro` (`.rec-badge` ~L1001 z5,
  `.stage-meta-wrap` ~L925 z8, `.part-count-badge`/`.pc-avatar` ~L970 z8, `.cell-actions`
  ~L1414 no-z, `.timer-badge` z6, `.pip-controls` ~L1213); `client/src/scripts/app.ts`
  (`updateParticipantsList` ~L1368 sets `partAvatarEl.textContent`; per-cell `cell-actions`
  ~L972; avatar helpers `auth.avatarUrl`, `avatarGradient`; video-cell avatar render ~L912
  is the existing image-with-initials-fallback pattern to mirror).
- **Overlap strategy:** define explicit, non-colliding "lanes": top-left meta cluster,
  top-right participant/timer stack, and the REC badge. Make the REC badge not fight the
  center by (a) raising its stacking above the wrapping meta and (b) on narrow widths
  repositioning it (e.g. into a reserved row / inline with the meta) so it can't sit under
  other overlays. Keep a single documented z-index scale (meta=8, part=8, timer=6, rec
  promoted, cell-actions given an explicit z above transient overlays).
- **Avatars (R2.3):** in `updateParticipantsList`, when `auth.avatarUrl(self avatar)` exists
  render an `<img>` into `#part-avatar` (and into participants-list `.part-avatar`),
  `onerror` → fall back to initial + `avatarGradient` (reuse the L912 pattern). Self avatar
  from `auth`; peer avatars from `peerNames.get(id).avatar`.
- **Moderation (R2.2):** give `.cell-actions` an explicit z-index above floating transient
  overlays (reactions/timer-pop) and ensure the top-right participant badge can't sit over
  a cell's actions (they live in different stacking contexts — verify with the cell on
  hover/focus at small sizes).
- **PiP (R2.4):** audit `buildPipControls`/PiP document; controls already at max z in a
  separate document — confirm no in-PiP header element overlaps and fix if found.
- **Key decision:** reserve lanes rather than rely on incidental wrapping; the bug class is
  "independent absolutely-positioned elements with no shared layout".

### Area 3 — Tic-Tac-Toe

- **Components / files:** `client/src/scripts/tictactoe.ts` (state machine), markup/CSS
  `index.astro` ~L344/L1647, relay routing `app.ts` ~L781 + `toggleMinigame` ~L3029; server
  relay `server/src/lib.rs` ~L1099 + snapshot ~L813, `server/src/rooms.rs` `game_set`/
  `game_snapshot`. Server stays a dumb game-agnostic relay (no Rust changes expected).
- **R3.1 fix:** in `onAction`/`startNew`, when starting and exactly one other participant is
  present, seat them immediately as O and set `status:'playing'` so the board is live at
  once; otherwise keep `status:'waiting'` but render an unambiguous "waiting for opponent"
  status and auto-promote the next joiner to O. (Client needs the current participant set;
  pass a `peers()` accessor into `TicTacToe`, sourced from `peerNames`.)
- **R3.2 sync:** extend `GameState` with a monotonically increasing `seq` (and optionally
  `lastMoveBy`); `applyRemote` ignores states with `seq` ≤ current to drop stale/out-of-
  order frames, and validates basic legality (cell was empty, correct turn) before applying.
  Snapshot path already replays latest state to late joiners.
- **R3.3 / R3.4 seats & rotation:** extend `GameState` to carry the seat assignment and a
  spectator queue (ordered by join). On `won`/`draw`, the next `startNew` (open to anyone)
  computes new X/O from the rotation (e.g. winner stays, loser → back of queue, next
  spectator in) and broadcasts it; spectators render read-only (existing `mark()===0` path).
  Broadcast the seat/queue so all clients show the same rotation.
- **Auto-open (R-shared):** when a TTT state arrives for a non-initiator and a game is
  active, call `toggleMinigame(true)` so spectators/opponents see the board (mirrors the
  quiz R4.1 fix). Close/return to launcher when state is null.
- **Key decision:** keep client-authoritative; `seq`-guarded last-writer-wins is enough for
  a turn-based 2-player game and avoids server game logic (preserves 0046's non-goal).

### Area 4 — Quiz

- **Components / files:** `client/src/scripts/quiz.ts` (`QuizState`, `onAction`, `startNew`/
  `startAiQuiz`, `applyRemote`, `render`), markup/CSS `index.astro` ~L353/L1670, wiring
  `app.ts` `toggleQuiz` ~L3038 + game routing ~L781 + AI form ~L3057. Server relay only
  (no quiz logic on server); reuses the single per-room `game` slot.
- **R4.1 (root cause):** `applyRemote` updates state + re-renders but never un-hides the
  modal; the routing at `app.ts:785` only calls `quiz.applyRemote`. Fix: when a quiz state
  arrives and is active, the app opens the modal — either `applyRemote` returns/exposes a
  "should be open" signal that the router uses to call `toggleQuiz(true)`, or the router
  inspects `msg.state.game==='quiz'` + phase and toggles. Close on cancel/null.
- **R4.2 single active:** in `onAction`/`startNew`/`startAiQuiz`, guard: if `this.state` is
  non-null and `phase !== 'done'` (and not cancelled), refuse to start and show a message.
  The current unconditional `else → startNew()` (quiz.ts ~L262) is the offending path.
- **R4.3 cancel:** add a `cancel()` that broadcasts a terminal state (a `phase:'cancelled'`
  or `null` via the relay) and resets local state; render a "Cancel Quiz" button for the
  host (and define "authorized" = host; non-hosts don't see it). All clients receiving the
  terminal state reset and close/return to launcher. Add i18n strings.
- **R4.4 completion:** extend the host-tracked `players[id]` (or a parallel map) with an
  `answered` count (increment in `recordAnswer`), and show "answered N/total" /
  finished-state per player in the leaderboard/status render.
- **Host authority:** `hostId` already exists in `QuizState`; cancel + single-active are
  gated on `isHost()`; the start guard applies to everyone (you can't start over an active
  remote quiz because your local state mirrors it).
- **Key decision:** reuse the existing single `game` slot and host-authoritative engine
  (0047) — no new message types beyond what the state discriminator already supports.

### Area 5 — Session Details CTAs

- **Components / files:** `.btn-ghost` + overrides in `index.astro` ~L674–685 and the
  `.session-downloads`/`#session-ai` scoping; tokens in `client/src/layouts/Base.astro`
  ~L34–58; dynamic buttons in `report.ts`/`sentiment.ts`/`email.ts`. Session markup
  `index.astro` ~L109–175.
- **Approach:** keep the 0034/0038 tonal-secondary direction but make it unmistakably
  on-brand on the Session Details surface: strengthen the accent presence (e.g. raise the
  tonal mix / add a clearer accent border, or promote the page's main action to
  `.btn-primary`) so secondary CTAs no longer read as flat grey, while preserving a clear
  primary-vs-secondary hierarchy. Use existing tokens (`--accent`, `--accent-strong`,
  `--radius-sm`) and the global `:focus-visible`. Change is CSS-only where possible; verify
  every Session Details CTA (back, PDF/JSON/SRT/VTT, AI generate/copy/regenerate/run/send).
- **Key decision:** amend, don't fork, the button system — one secondary style across the
  app stays consistent (0034/0038 remain the source of truth; this tightens them).

### Sequence (representative — quiz R4.1)

1. Host clicks Start → `quiz.startNew()` → `setState` → relay `{type:'game', state}`.
2. Server stores + `broadcast_except` to peers; snapshots to late joiners.
3. Each peer's router sees `game==='quiz'` + active phase → `quiz.applyRemote(state)` **and**
   `toggleQuiz(true)` → modal opens for everyone.
4. Host clicks Cancel → terminal state broadcast → all peers reset + close.

## 5. Implementation

| Slice | What | Key files | PR |
|-------|------|-----------|----|
| S1 | Chat textarea + counter + emoji button parity + picker redesign (R1.1–R1.5) | `index.astro` (composer markup/CSS, emoji CSS), `app.ts` (sendChat/keydown/auto-resize/counter), `chat.ts` | PR-1 |
| S2 | Header overlap lanes + REC reposition + avatars + report-button stacking + PiP audit (R2.1–R2.4) | `index.astro` (overlay CSS/z-index), `app.ts` (`updateParticipantsList` avatar render, cell-actions) | PR-2 |
| S3 | Tic-Tac-Toe: instant-play New Game, `seq` sync guard, 2-seat + spectator rotation, auto-open (R3.1–R3.4) | `tictactoe.ts`, `app.ts` (peers accessor, auto-open), `index.astro` (status/CSS) | PR-3 |
| S4 | Quiz: open-for-all, single-active guard, cancel, per-participant completion (R4.1–R4.4) | `quiz.ts`, `app.ts` (routing/auto-open/cancel wiring), `index.astro` (cancel btn/CSS), i18n | PR-4 |
| S5 | Session Details CTA restyle (R5.1) | `index.astro` (`.btn-ghost` + session scopes), `Base.astro` tokens if needed | PR-5 |

Each slice lands the relevant part of this spec's status/commit row at ship time (hash-pin
flow): squash-merge, then a separate docs PR pins the short-SHA and flips status.

## 6. Testing & Verification

- **Unit/logic (vitest):**
  - S1: counter math + hard-cap (incl. emoji insertion at the boundary); Enter sends /
    Shift+Enter newlines; auto-resize reset after send.
  - S3: `tictactoe` pure logic — instant-seat when 2 present; `seq` guard drops stale
    states; rotation picks the expected next seats for 2/3/4 participants; spectator board
    is read-only.
  - S4: `quiz` — single-active guard refuses start while active; `cancel()` resets; remote
    active state signals "open"; per-participant `answered` increments correctly.
- **E2E (Playwright, `client/e2e`):** extend `call.spec.ts` / `meet-ui.spec.ts` —
  - Chat: type multi-line, counter visible near cap, send via Enter, emoji button aligned.
  - Header: at a narrow viewport with REC active, assert no bounding-box overlap between
    rec badge / meta / participant badge; report buttons clickable on hover.
  - Quiz: two browser contexts — creator starts, assert the modal opens in the second
    context; second start is blocked; cancel closes both.
  - TTT: two contexts — New Game → board immediately playable both sides; third context
    joins as read-only spectator.
- **Manual / `/run`:** drive the app for each slice; verify desktop + mobile widths + PiP.
- **Gates:** `cargo fmt --check` is irrelevant unless server changes (none expected);
  client `vitest` + `tsc` + Playwright + a11y (`a11y.spec.ts`) must pass. Keep ≥85% bar.

## 7. Deployment & Operations

- Frontend-only across all five slices (no env vars, no migrations). Client auto-deploys on
  merge to `main` via Vercel. No server deploy expected (relay unchanged). If S3/S4 end up
  needing a server tweak, that becomes a Railway `railway up` step — flagged in that PR.
- No feature flags; each slice is independently revertible.

## 8. Risks / Open Items

- **R3 rotation policy** (winner-stays vs strict join-order) is a product choice — pick one
  in S3 and document it; getting it "fair" with churn (leavers mid-queue) needs care.
- **R3.2** stays client-authoritative; a hostile client can still forge a state. `seq` +
  legality checks reduce accidental divergence but are not anti-cheat (0046 non-goal holds).
- **R4 "authorized users"** is defined as the host only; if a host leaves mid-quiz, cancel
  authority is lost — consider allowing any participant to cancel a host-orphaned quiz
  (follow-up).
- **R2.1** narrow-width REC reposition must not regress the #139 single-line meta; verify
  at 320–768px.
- **R5** must not regress 0034/0038 across the rest of the app — it's one shared secondary
  style; screenshot-diff the home/billing/glossary CTAs.

## 9. References

- Specs: [0018](../0018-chat-file-upload/spec.md), [0034](../0034-ui-cta-zfix/spec.md),
  [0038](../0038-session-glossary-ux/spec.md), [0046](../0046-minigame-tictactoe/spec.md),
  [0047](../0047-minigame-quiz/spec.md)/[0048](../0048-quiz-localized/spec.md)/[0049](../0049-quiz-pack-40/spec.md)/[0067](../0067-ai-quiz-on-demand/spec.md),
  [0055](../0055-meet-like-session-ui/spec.md)/[0060](../0060-meet-ui-refinements/spec.md)/[0061](../0061-immersive-call-overlays/spec.md),
  [0057](../0057-pip-controls/spec.md).
- Files: `client/src/pages/index.astro`, `client/src/scripts/{app,chat,quiz,tictactoe}.ts`,
  `client/src/layouts/Base.astro`, `server/src/{lib,rooms,protocol}.rs`.
- Prior PR built on: #139 (`26192ff`).

## 10. Amendments

- **2026-06-15 — post-merge fixes (from in-app QA).** Three follow-ups landed after the
  slices: (a) **#146** — S2 lifted a hovered video cell to `z-index:13`, which covered the
  bookmark (z10) and voice-timer (z11) popovers on desktop hover; lowered to `z-index:9`
  (still above the participant badge z8, below the popovers). (b) **#147** — replaced the
  raise-hand glyph (read as a keypad) with a recognizable open palm. (c) **#148** — `nextSeats`
  could seat a departed player's stale id after a leave/rejoin ("ghost seat"); rotation now
  validates seats against the live participant list and falls back to a fresh seating.
- **Still open (tracked separately):** mobile top-meta overlap (needs a repro screenshot);
  public-rooms avatar stack (server `avatar_url` + client); user bug-reporting → spec
  [0071](../0071-user-bug-report/spec.md).
