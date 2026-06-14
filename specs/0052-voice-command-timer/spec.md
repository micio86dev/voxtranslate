# 0052 — Voice-command countdown timer

| | |
|---|---|
| **Status** | Draft |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-14 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | (this PR) |
| **Depends on** | [0001](../0001-voice-translation-rooms/spec.md) (streaming STT), [0042](../0042-tts-voice-selection/spec.md) (spoken confirmation), [0023](../0023-call-toolbar-overflow-menu/spec.md) (⋯ menu) |

## 1. Context & Problem

During a call there is no way to run a countdown ("5 more minutes", a break, a
timeboxed round). The only timers in the app are internal (the recording-elapsed
badge). Setting one would mean leaving the conversation to fiddle with UI — which
defeats the point of a *real-time* call and hurts accessibility.

GitHub issue [#74](https://github.com/micio86dev/voxtranslate/issues/74) asks for a
**hands-free, voice-driven** timer: say *"imposta timer di 10 minuti"* / *"set a 5
minute timer"* and a countdown starts, with visual (and optional spoken)
confirmation.

The key enabler already exists: every participant's speech is streamed to Deepgram
and the **speaker's own final transcript comes back to their client** as a
`subtitle_final` message (`speaker_id === myId`, raw text in `original`). So we do
**not** need a second STT/Whisper pipeline — we run a tiny intent parser on text we
already have. Zero added latency, zero added cost, no server change.

## 2. Goals / Non-Goals

**Goals**
- Set a countdown by speaking a natural command (Italian-first; English too).
- Parse the duration from digits **or** spoken number words, including idioms
  (*mezz'ora*, *un quarto d'ora*, *half an hour*).
- Immediate visual confirmation (a live MM:SS badge on the stage), a sound cue,
  and an **optional** spoken confirmation (gated on the existing "translated
  voice" toggle).
- A manual fallback (override): a popover with quick-pick durations + a custom
  minutes field, and a one-tap cancel.
- No false positives: ordinary talk that merely mentions "30 minutes" must not arm
  a timer.

**Non-Goals**
- **Shared / synced** timers across the room. Detection is per-speaker and local
  to the device that set it (see Risks). A room-wide break timer is a follow-up.
- Pause/resume, multiple concurrent timers, recurring timers.
- A new STT model or any backend/Rust change.
- Wake-word / continuous assistant mode.

## 3. Requirements

- **R1 — Set a timer by voice.** As a speaker, I want to say *"imposta timer di 10
  minuti"* and have a 10-minute countdown start.
  - *Given* I'm in a call with my mic on, *when* my final transcript matches a
    timer command, *then* a countdown badge appears and begins counting down.
- **R2 — Robust duration parsing.** As a speaker, I want digits, number words and
  common idioms understood.
  - *Given* I say *"timer di mezz'ora"* / *"set a five minute timer"* / *"un quarto
    d'ora"*, *when* it's parsed, *then* the durations are 30 / 5 / 15 minutes.
- **R3 — No false positives.** As a participant, I don't want a stray phrase to
  start a timer.
  - *Given* I say *"the meeting runs for 30 minutes"* (no trigger word) or *"set a
    timer"* (no duration), *when* it's parsed, *then* nothing starts.
- **R4 — Confirmation feedback.** As a speaker, I want to know it worked.
  - *Given* a timer starts, *then* a toast ("Timer set · 10 minutes"), a sound cue,
    and — when translated-voice output is on — a spoken confirmation all fire.
- **R5 — Done feedback.** *Given* the countdown reaches zero, *then* the badge
  flashes, an alarm cue plays, a "Time's up!" toast shows, and the badge
  auto-hides after a short hold.
- **R6 — Manual override / cancel.** As a user, I want a non-voice path.
  - *Given* I open the ⋯ → Timer popover, *when* I tap a quick-pick or enter custom
    minutes, *then* the timer starts; *and* the badge's × cancels a running timer.
- **R7 — Localised.** All UI strings + unit words exist in the 8 supported
  languages; the spoken confirmation and quick-pick labels follow the UI language.
- **R8 — Local & private.** *Given* a peer speaks a timer command, *then* it never
  controls my clock — only my own transcript arms my timer.

## 4. Design & Architecture

- **Components / files**
  - `client/src/scripts/timer-intent.ts` — **pure**, browser-free: `parseTimerCommand`,
    `formatClock`, `spokenDuration` (+ number-word map, idiom table, multi-language
    unit vocabulary). Unit-tested; added to the coverage `include` list.
  - `client/src/scripts/timer.ts` — `CallTimer` class: owns the badge DOM and the
    countdown tick; re-exports the pure helpers for a single import surface.
  - `client/src/scripts/app.ts` — instantiates `CallTimer`, feeds it the local
    final transcript, wires the ⋯ button + popover + badge cancel, supplies
    confirmation feedback (toast / sfx / `speak`), resets on leave.
  - `client/src/scripts/sfx.ts` — `playTimerSetSound`, `playTimerDoneSound`.
  - `client/src/scripts/icons.ts` — `timer` (clock) glyph.
  - `client/src/scripts/i18n.ts` — `timer*` keys + `unitH1/HN, unitM1/MN, unitS1/SN`.
  - `client/src/pages/index.astro` — badge overlay, manual popover, ⋯ menu entry,
    CSS.

- **Intent parser (`parseTimerCommand`)**
  1. Normalise: lower-case, unify apostrophes, strip punctuation, collapse spaces.
  2. **Gate:** require a trigger keyword — `timer`, or `break`/`pausa`/`pause`
     (which also flags `isBreak`). No trigger → not a command.
  3. Expand idioms (`mezz'ora`→30m, `un quarto d'ora`→15m, `half an hour`→30m,
     `un'ora`/`a minute`→…) into canonical `<n> unit` tokens.
  4. Convert spoken number words to digits (IT+EN, incl. "twenty five" compounds).
  5. Scan `<number><unit>` matches (multi-language unit vocab, longest-first
     alternation) and **sum** components (`1 hour 30 minutes` → 5400 s).
  6. Reject out-of-range totals (`< 1 s` or `> 6 h` — a mis-parse guard).

- **Countdown (`CallTimer`)** — a 250 ms tick recomputes remaining time from a
  stored deadline (`Date.now()`-based, robust to throttling) and renders MM:SS /
  H:MM:SS. At zero it flashes the badge, fires `onDone`, and auto-hides after 6 s.

- **Sequence (voice happy path)**
  1. Speaker says *"imposta timer di 10 minuti"*.
  2. Deepgram → server → speaker's client receives `subtitle_final`
     (`speaker_id === myId`, `original` = the raw Italian text).
  3. `callTimer.handleTranscript(original)` → `parseTimerCommand` → `{seconds:600}`.
  4. Badge shows `10:00` and ticks down; `onSet` fires toast + cue + (optional)
     spoken "Timer impostato per 10 minuti".
  5. At `00:00`: alarm cue + "Tempo scaduto!" toast + badge flash.

- **Key decisions**
  - *Reuse the existing STT transcript instead of a new Whisper pipeline* →
    no latency/cost/backend cost; the issue's "Whisper or equivalent" is satisfied
    by the Deepgram stream already in place.
  - *Local-only (not synced)* → keeps the change client-only and side-steps
    authority/conflict questions; matches the MVP scope. Synced timers are a noted
    follow-up.
  - *Trigger-word gate + range cap* → the two guards that keep normal conversation
    from arming timers.
  - *Pure parser split into `timer-intent.ts`* → mirrors the repo's testing
    convention (pure helpers isolated from DOM siblings, coverage-enforced).
  - *Spoken confirmation gated on the TTS toggle* → opt-in audio; visual + cue
    always fire.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Pure intent parser + formatters + unit tests | `timer-intent.ts`, `timer.test.ts` |
| S1 | `CallTimer` countdown + badge | `timer.ts` |
| S2 | Icon + sound cues | `icons.ts`, `sfx.ts` |
| S3 | i18n keys (8 languages) | `i18n.ts` |
| S4 | Badge + manual popover + ⋯ entry + CSS | `index.astro` |
| S5 | Wire-up: voice hook, manual controls, confirmation, leave-reset | `app.ts` |
| S6 | Coverage include + spec | `vitest.config.ts`, this file |

## 6. Testing & Verification

- **Unit (`timer-intent.test.ts`, 32 cases):** IT + EN commands; digits, number
  words and "twenty five" compounds; minutes/seconds/hours; idioms; the gating /
  no-false-positive cases (R3); zero and >6 h range rejection; `formatClock`
  MM:SS↔H:MM:SS and clamping; `spokenDuration` singular/plural + multi-component.
  `timer-intent.ts` coverage: 100 % lines / 100 % functions.
- **Runtime (Playwright smoke):** revealed the call screen on the built bundle and
  drove the manual flow — popover opens, chip reads "5 min", badge counts
  `05:00 → 04:58`, × cancels (badge hidden), custom "2" + Enter starts `02:00`,
  confirmation toasts render. No console errors from app code.
- **Gates:** `astro check` (0 errors), `astro build` (clean), full unit suite
  (135 passed).

## 7. Deployment & Operations

- Client-only; ships with the Vercel autodeploy on merge to `main`. No env vars,
  no migrations, no server/Railway change.
- No feature flag — the ⋯ → Timer entry and voice detection are always available
  in-call. Voice detection is inert unless a transcript matches.

## 8. Risks / Open Items

- **Local-only (not synced).** A timer set by one participant is visible only to
  them. A room-wide / shared break timer (relayed over the WS like the whiteboard /
  mini-games `game` channel) is the natural follow-up.
- **STT dependence.** Voice setting needs an active Deepgram stream (mic on, not
  out of credits) and depends on transcription quality; `smart_format=true` usually
  digitises numbers, and the number-word fallback covers the rest. The manual
  popover is always available as the override.
- **Parser breadth.** IT + EN command phrasings are covered; other UI languages can
  still set a timer manually and can be added to the trigger/idiom tables later.
- **Edge false positive.** A break-phrasing that also names a tiny duration (e.g.
  "take a break, back in a second") can arm a very short timer; harmless and
  cancellable. Tightening is a follow-up if it proves annoying.

## 9. References

- Issue: <https://github.com/micio86dev/voxtranslate/issues/74>
- Files: `client/src/scripts/timer-intent.ts`, `client/src/scripts/timer.ts`,
  `client/src/scripts/app.ts` (`subtitle_final` hook, ⋯/popover wiring),
  `client/src/pages/index.astro`, `client/src/scripts/{i18n,sfx,icons}.ts`.
- Related: [0042](../0042-tts-voice-selection/spec.md) (`speak` / `pickVoice`),
  [0023](../0023-call-toolbar-overflow-menu/spec.md) (⋯ menu),
  [0043](../0043-low-latency-capture/spec.md) (audio capture path).
