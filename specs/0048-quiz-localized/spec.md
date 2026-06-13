# 0048 — Quiz questions localized per player

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0047](../0047-minigame-quiz/spec.md), [0008](../0008-managed-content-i18n/spec.md) |

## 1. Context & Problem

The quiz (0047) showed every player the same English question/options. The owner
wants each person to see the quiz **in their own language** — fitting a translation
app. Since the owner chose the free built-in pack (no LLM/credits), the answer is to
**pre-translate the pack** rather than translate at runtime.

## 2. Goals / Non-Goals

**Goals**
- Each client renders the current question + options in **its own UI language**
  (it/en/es/fr/de/pt/ja/zh), with English fallback.
- **No** runtime translation / Groq / credits — translations are bundled, instant.

**Non-Goals**
- AI-generated or user-authored questions (the source could be swapped later).
- Translating player-typed content (there is none — questions are from the pack).

## 3. Requirements

- **R1 — Localized pack.** Each `PACK` item carries `q` and `options` keyed by
  language (English required as the fallback). The correct option sits at the **same
  index** in every language. Number/symbol options (e.g. "7", "Au") are
  language-neutral — only `en` is stored and every language falls back to it.
- **R2 — Reference, not text, on the wire.** The host broadcasts the question's
  **`packIndex`** (not its text); each client resolves `PACK[packIndex]` in its own
  language via `myLang()`. Keeps the relay payload tiny and the rendering per-client.
- **R3 — Correctness unchanged.** Scoring/turn/reveal logic is identical; `correct`
  is the pack's answer index (consistent across languages).

## 4. Design & Architecture

- `client/src/scripts/quiz.ts` — `PACK` becomes `{ answer, q: Record<lang,string>,
  options: Record<lang,string[]> }[]`; the broadcast `QuizState` replaces
  `question/options` strings with `packIndex`; `Quiz` gains a `myLang()` ctor arg and
  `render()` resolves text/options with `pick(dict, lang) = dict[lang] ?? dict.en`.
- `client/src/scripts/app.ts` — passes `() => session?.lang || 'en'` as `myLang`.
- **Key decisions:**
  - *Pre-translate, don't call the API.* The pack is fixed and small, so bundling 8
    languages is free, instant, offline-safe — and consistent with the owner's
    no-credits choice. Runtime Groq translation would bill per quiz.
  - *Broadcast the index.* All clients share the same bundle/pack, so sending
    `packIndex` lets each render locally; nothing language-specific travels the wire.
  - *Same answer index across languages.* Options are authored aligned, so `correct`
    is language-independent.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Localized PACK (6 Qs ×8 langs) + `pick()` | `quiz.ts` |
| S1 | Broadcast `packIndex`; render in `myLang()` | `quiz.ts` |
| S2 | Pass `session.lang` as `myLang` | `app.ts` |

## 6. Testing & Verification

- `astro check` clean; **101/101** unit tests; production build OK. No server change.
- Manual: two players on different UI languages see the same question each in their own
  language; the correct answer highlights consistently; scores match.

## 7. Deployment & Operations

- **Client-only** — ships via the Vercel autodeploy on `main`. No relay/server change
  (still the 0046 `game` channel carrying `packIndex`).

## 8. Risks / Open Items

- A language not in the pack falls back to English (defensive). All 8 UI languages are
  covered.
- Changing your UI language mid-quiz re-renders only on the next state update
  (acceptable; switching language mid-game is rare).
- Pack is 6 questions; AI-generated / localized-on-the-fly questions remain a future
  swap of the source (would cost credits).

## 9. References

- Files: `client/src/scripts/quiz.ts`, `client/src/scripts/app.ts`.
