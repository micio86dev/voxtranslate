# 0049 — Quiz pack expanded to 40 questions

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0047](../0047-minigame-quiz/spec.md), [0048](../0048-quiz-localized/spec.md) |

## 1. Context & Problem

The quiz shipped with 6 questions — too few for replay variety. The owner asked for
**40 total** (34 more), each localized like the rest.

## 2. Goals / Non-Goals

**Goals**
- 40 questions, every one localized in all 8 UI languages (0048 model).
- More questions per round now that the pool is larger.

**Non-Goals**
- New game mechanics (still 0047/0048 logic).
- AI-generated questions (still the bundled pack).

## 3. Requirements

- **R1 — 40 localized questions.** Add 34 items. Each has `q` in all 8 languages;
  options follow the 0048 rule — **number/symbol/universal** options give only `en`
  (language-neutral), **word** options carry all 8 languages, with the correct answer
  at the same index across languages.
- **R2 — Round length.** `ROUND_QS` 4 → **8** (a fuller game from the larger pool).

## 4. Design & Architecture

- `client/src/scripts/quiz.ts` — 34 new `PACK` entries appended; `ROUND_QS = 8`.
  No other change (rendering/relay/scoring from 0047/0048 unchanged). Mix: ~26
  neutral-option (counts, years, math, chemical symbols) + 8 word-option (geography,
  animals, colours, science) for variety while keeping translation reliable.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | +34 localized questions; round 4→8 | `quiz.ts` |

## 6. Testing & Verification

- `astro check` clean; **101/101** unit tests; production build OK. Pack size = 40.
- Spot-check: answer index lines up with the correct option in every language.

## 7. Deployment & Operations

- **Client-only** — Vercel autodeploy on `main`. No server change.

## 8. Risks / Open Items

- Hand-authored translations; minor wording nits possible — easy to amend per string.
- Still general-knowledge trivia in a fixed pack; AI/dynamic questions remain a future
  swap of the source (would cost credits).

## 9. References

- Issue: #21
- Files: `client/src/scripts/quiz.ts`.
