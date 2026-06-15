# 0067 — AI quiz on demand (Groq-generated, credit-charged)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-15 |
| **Shipped** | 2026-06-15 |
| **Version** | — |
| **Commits** | `85959de` |
| **Depends on** | [0047](../0047-minigame-quiz/spec.md), [0046](../0046-minigame-tictactoe/spec.md) |

## 1. Context & Problem

The in-call quiz (spec 0047/0048/0049) plays a **built-in, pre-translated** 40-question
pack: the host broadcasts only the question *index* and each client renders it in its
own language — instant, no LLM, no credits. Issue **#124** asks for a quiz **generated
on demand from a user prompt** (e.g. "JavaScript basics", "1990s movies"), via Groq,
**charged to credits** — custom questions/answers, not the fixed pack.

## 2. Goals / Non-Goals

**Goals**
- A signed-in user can generate a custom quiz from a free-text prompt; it plays
  through the **existing** host-authoritative quiz engine (spec 0047).
- **Credit-charged** with the same safe billing shape as the other AI features
  (sentiment/report): pre-check → generate → atomic deduct; **never charged on a
  generation failure**.
- Strict server-side **validation + moderation** of the model output and the prompt
  before anything is charged or shown.
- **Zero regression** to the built-in quiz, Tic-Tac-Toe, and the game relay.

**Non-Goals**
- Per-client translation of the generated quiz (v1 generates in **one** language
  chosen by the host; stored in the `en` fallback slot so every client renders it).
  Multilingual generation is a follow-up.
- Persisting generated quizzes (ephemeral, like a live game).
- A question bank / difficulty tuning beyond a count + free-text prompt.

## 3. Requirements

- **R1 — Generate.** *Given* a signed-in user with enough credits, *when* they POST a
  prompt to `/api/quiz/generate`, *then* the server returns `count` validated
  questions (each: text, 4 options, answer index) and charges `quiz_base +
  quiz_per_question·count`.
- **R2 — Never charge on failure.** *Given* Groq errors or returns unusable JSON,
  *then* the response is `502` "you were not charged" and **no** deduction happens.
- **R3 — Gate on credits.** *Given* the balance can't cover the cost, *then* `402`
  with `{required, available}` and no generation/charge (advisory pre-check + the
  atomic `deduct_feature` is the real gate, mirroring sentiment).
- **R4 — Safe content.** *Then* the prompt is length-capped and moderated (severe →
  `422`); each returned question has a non-empty stem, exactly 4 non-empty options,
  and an `answer` in `0..=3` — anything else is rejected as a generation failure (R2).
- **R5 — Plays via the existing engine.** *Then* the host calls `quiz.startAiQuiz(qs)`;
  the generated pack rides **inline** in the broadcast `QuizState.pack`, so peers and
  late-joiners (game snapshot, spec 0046) render it. The built-in pack path is
  untouched when `pack` is absent.

## 4. Design & Architecture

- **Server**
  - `ai/quiz.rs`: `quiz_cost(ai, count)`, `clamp_count`, `QuizQuestion`,
    `validate_questions(json, want)` (pure, unit-tested), and
    `generate(groq, ai, prompt, count, lang)` → builds a JSON-mode `ChatRequest`
    (model `ai.report_model`, fallback `ai.fallback_model`), calls `Groq::chat_json`,
    validates, truncates to `count`.
  - `api::quiz_generate` (`POST /api/quiz/generate`, `AuthUser`): rate-limit
    `ai:{user}` (reuse), 503 if billing off, cap+moderate prompt, cost, balance
    pre-check (`insufficient_credits`), `generate`, then `deduct_feature("ai_quiz")`
    with the same InsufficientFunds-withholds / other-error-delivers-free handling as
    `sentiment_generate`. Returns `{questions, cost, balance}`.
  - `AiConfig`: `quiz_base` (`CREDITS_QUIZ_BASE`, def 0.03), `quiz_per_question`
    (`CREDITS_QUIZ_PER_QUESTION`, def 0.01); surfaced in `/api/billing/ai-pricing`.
  - Route added in `lib.rs::app`.
- **Client**
  - `api.ts`: `generateAiQuiz(prompt, count, lang)` (authed POST; maps 402/502).
  - `quiz.ts`: `QuizState` gains optional `pack?: PackItem[]`; a `currentItem(s)`
    helper returns `s.pack[qIndex]` when present else `PACK[packIndex]`; `render`,
    `reveal`, `next`, `reset` honor it; new `startAiQuiz(qs)` seeds a `pack` round.
    Generated questions are stored as `{ q:{en}, options:{en}, answer }` so
    `pick()`’s `en` fallback renders them for every viewer.
  - `index.astro`: an AI-prompt row in the quiz panel (input + Generate button + cost
    hint). `app.ts`: wire the button → `generateAiQuiz` → `quiz.startAiQuiz`.
  - i18n: a few `quizAi*` keys (en; other langs fall back via `t()`).

- **Key decisions**
  - *Inline pack in the relayed state* (not a new message/DB): the relay is opaque
    JSON (spec 0046), so peers + late-joiners get the pack for free; the built-in
    index path stays the default.
  - *Single-language v1 via the `en` slot*: reuses `pick()`’s fallback with no schema
    change to `PackItem`; multilingual is a clean follow-up.
  - *Same billing order as sentiment*: proven safe (pre-check → generate → atomic
    deduct), so a failed generation is free and a 402 withholds the result.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `ai/quiz.rs`: cost + clamp + validation + generate | `server/src/ai/quiz.rs`, `ai/mod.rs` |
| S1 | `AiConfig` quiz fields + `/ai-pricing` entry | `server/src/config.rs`, `server/src/api.rs` |
| S2 | `quiz_generate` handler + route | `server/src/api.rs`, `server/src/lib.rs` |
| S3 | Client `pack` support + `startAiQuiz` | `client/src/scripts/quiz.ts` |
| S4 | `generateAiQuiz` fetch + prompt UI + wiring + i18n | `client/src/scripts/{api,app}.ts`, `index.astro`, `i18n.ts` |

## 6. Testing & Verification

- **Server unit (pure):** `quiz_cost` math; `clamp_count` bounds; `validate_questions`
  accepts a well-formed pack and rejects each defect (wrong option count, empty stem,
  out-of-range `answer`, non-array, too few). No network.
- **Suite:** full `cargo test` + `clippy -D warnings` + `fmt`.
- **Client:** `astro check` + `astro build` + `vitest` green; manual smoke that a
  generated quiz plays and the built-in quiz is unchanged.

## 7. Deployment & Operations

- Server **manual** `railway up`. New optional env `CREDITS_QUIZ_BASE`,
  `CREDITS_QUIZ_PER_QUESTION` (defaults set). No migration (ephemeral).

## 8. Risks / Open Items

- LLM output quality/safety — bounded by strict validation + moderation + the "not
  charged on failure" contract.
- v1 is single-language; multilingual generation/translation is a follow-up.
- No rate of generated-quiz abuse beyond the per-user `ai:{user}` limit + credit cost.

## 9. References

- Issue #124. Builds on specs 0046/0047/0048/0049 and the AI-feature billing pattern
  (`sentiment_generate`). Files: `server/src/ai/quiz.rs`, `server/src/api.rs`,
  `client/src/scripts/quiz.ts`.
