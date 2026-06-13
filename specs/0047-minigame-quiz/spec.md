# 0047 — Mini-game: trivia quiz (built-in pack)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0046](../0046-minigame-tictactoe/spec.md) |

## 1. Context & Problem

Second mini-game from issue #21, on the **game-agnostic relay** built in 0046. A
multiplayer **trivia quiz** with a **built-in question pack** (owner's choice — free,
no LLM/credits). Unlike turn-based Tic-Tac-Toe, several players answer at once, so it's
**host-authoritative**.

## 2. Goals / Non-Goals

**Goals**
- Multiplayer trivia: everyone answers each question; correct = +1; final leaderboard.
- Reuse the 0046 `game` relay + snapshot — **no server change**.
- Questions from a bundled pack (no cost). Coexist with Tic-Tac-Toe.

**Non-Goals**
- AI-generated questions (deferred — would cost credits; same UI could swap the source).
- Per-language questions, timers, speed bonus, persistent leaderboard.

## 3. Requirements

- **R1 — Host-authoritative.** The player who starts owns the canonical state and
  drives phases (`question → reveal → … → done`). Others send their answer; the host
  tallies. State + answers travel over the existing `game` channel, tagged
  `game:'quiz'` (Tic-Tac-Toe states have none → the router default), so both coexist.
- **R2 — Answers stay secret.** During `question` the broadcast carries only *who*
  answered (count), not their choice; the correct index and all choices are revealed
  only at `reveal`. The host holds the correct answers until then.
- **R3 — Flow.** Start → 5 random questions from the pack. Each: 4 options, tap to lock
  your pick; host **Reveal**s, scores update, host **Next** → … → **Finish** →
  leaderboard. **New quiz** (open to anyone) restarts and recovers a left host.
- **R4 — Snapshot.** A late-joiner gets the current quiz (0046 snapshot) and can answer
  the live question.

## 4. Design & Architecture

- `client/src/scripts/quiz.ts` — `Quiz` class: built-in `PACK`, host picks 5
  (shuffled), broadcasts each question's text/options; `pending` answers (host-only)
  → scores at reveal. `applyRemote` routes `t:'answer'` to the host and `t:'state'` to
  render. Players keep a local `myChoice`.
- `client/src/scripts/app.ts` — one `game` channel routed by `state.game === 'quiz'`
  → `quiz`, else → `tictactoe`. Shared `gameName`/`sendGame` helpers; ⋯ toggle; leave
  reset.
- `index.astro` + CSS (reuses the `.minigame` card), `icons.ts` (`quiz`), i18n ×8.
- **Key decisions:**
  - *Host-authoritative, one writer per transition.* Concurrent answers can't be
    serialized by turns (unlike TTT), so a single host computes the canonical state;
    players only emit answers.
  - *Secrecy by omission.* Not broadcasting choices until reveal prevents copying,
    without server logic (it stays a dumb relay).
  - *Discriminated channel.* Tagging quiz states lets two games share the 0046 relay +
    single room slot (one active game at a time) with zero server change.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Quiz state machine + pack + host logic | `quiz.ts` |
| S1 | Panel UI + options + leaderboard | `index.astro` |
| S2 | Channel routing + ⋯ toggle + leave reset | `app.ts` |
| S3 | Icon + i18n ×8 | `icons.ts`, `i18n.ts` |

## 6. Testing & Verification

- `astro check` clean; **101/101** unit tests; production build OK. No server change.
- Manual: 2–4 players answer; host reveals + advances; scores/leaderboard correct; a
  late-joiner sees the live question; coexists with Tic-Tac-Toe; leaving resets.

## 7. Deployment & Operations

- **Client-only** — ships via the Vercel autodeploy on `main`. The relay already
  exists (0046), so no `railway up`.

## 8. Risks / Open Items

- Client-authoritative: the correct answer is in the host's memory but the *reveal*
  state exposes it; a tampered client could peek pre-reveal. Acceptable for a casual
  in-call quiz.
- One active game per room (TTT and quiz share the slot); starting one supersedes the
  other.
- Built-in English pack; AI-generated / localized questions are a future swap of the
  question source.

## 9. References

- Issue: #21
- Files: `client/src/scripts/quiz.ts`, `client/src/scripts/app.ts`,
  `client/src/pages/index.astro`, `client/src/scripts/{icons,i18n}.ts`.
