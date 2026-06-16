# 0083 — AI-quiz gating CTAs + Google sign-in button polish

| | |
|---|---|
| **Status** | In progress |
| **Owner** | Micio Dev |
| **Created** | 2026-06-16 |
| **Shipped** | — |
| **Version** | — |
| **Depends on** | [0022](../0022-guest-public-room-gate/spec.md) (sign-in gate), [0067](../0067-ai-quiz/spec.md) (AI quiz) |

## 1. Context & Problem

Two small in-call gating/polish gaps reported by the owner:

1. **AI quiz gives guests / out-of-credits users a dead end.** `quiz_generate`
   requires `AuthUser` (guests → 401) and charges credits (no balance → 402).
   The client mapped both to a flat status line: a guest saw a generic
   `quizAiError`, and an out-of-credits user saw `quizAiNoCredits` text with **no
   way to act on it**. The user should be told *how* to proceed: sign in, or buy
   credits.
2. **White frame around the Google sign-in button.** When Google recognises an
   existing session it renders a wider personalised button; with no explicit
   `width` the GSI iframe left a white gutter around the pill that clashes with
   the dark login card.

## 2. Goals / Non-Goals

**Goals**
- A guest who tries to generate an AI quiz gets the sign-in gate (the CTA), not a
  generic error.
- An out-of-credits user gets a **Buy credits** CTA that opens the purchase modal.
- The Google button reads cleanly on the dark card (no white gutter).

**Non-Goals**
- No change to the server quiz auth/credit model (the gates already exist server-side).
- No restyling of Google's iframe internals (cross-origin, not CSS-addressable) —
  only the documented `renderButton` options are used.

## 3. Requirements

- **R1 — Guest gate.** *Given* billing is on and the user is a guest, *when* they
  submit the AI-quiz prompt, *then* the sign-in gate opens and no request is made.
- **R2 — Buy CTA.** *Given* a signed-in user with insufficient credits, *when*
  quiz generation returns 402, *then* the `quizAiNoCredits` message shows **and** a
  Buy-credits button appears that opens the purchase modal.
- **R3 — Clean reset.** The Buy CTA is hidden on a fresh attempt and on success.
- **R4 — Button frame.** The Google button renders with an explicit width so no
  white gutter surrounds it on the dark card.

## 4. Design & Architecture

- `client/src/pages/index.astro`: add `#quiz-ai-buy` (hidden `btn-ghost`) after
  `#quiz-ai-msg`; small `.quiz-ai-buy` style.
- `client/src/scripts/app.ts`:
  - quiz submit: `if (billing && !auth.isLoggedIn()) { openSigninGate(); return; }`;
    on `insufficient_credits` reveal `#quiz-ai-buy`; reset it otherwise.
  - `setupGoogleSignIn`: pass `width: 300` to `renderButton`.
- Reuses existing copy (`buyCredits`) and flows (`openSigninGate`, `openBuyModal`).

## 5. Testing & Verification

- `vitest` + `astro check` + build green.
- Manual: guest → quiz prompt → sign-in gate. Out-of-credits → Buy CTA → modal.
- Google button frame: **owner to confirm visually** once deployed (the
  session-recognised state can't be reproduced headlessly).

## 6. Deployment & Operations

- Ships with the Vercel client deploy. No server change.

## 7. Risks / Open Items

- R4 depends on Google's GSI rendering; the `width` is the documented lever but the
  visual result needs owner confirmation in prod.

## 8. References

- Files: `client/src/pages/index.astro`, `client/src/scripts/app.ts`.
