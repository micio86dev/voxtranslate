# 0037 — Guest sign-in CTA on the home screen

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0005](../0005-accounts-credits-billing/spec.md), [0022](../0022-guest-public-room-gate/spec.md), [0036](../0036-reaction-anim-guest-auth-menu/spec.md) |

## 1. Context & Problem

A guest on the home screen had **no way back to the login screen**. The account bar
(with the logout button) only renders when signed in; "Continue as guest" is a
one-way door — once in guest mode you could create/join private rooms but never
reach sign-in again without clearing app data. After 0036 made stale tokens fall
back to guest correctly, this dead end became more visible: users who *were* logged
in (expired token) now land in guest mode with no obvious route back.

## 2. Goals / Non-Goals

**Goals**
- A clear, tasteful **sign-in CTA** visible to guests on the home screen that takes
  them to the existing login screen.
- Show it **only** when accounts exist (`billing` on) and the user is a guest; never
  in guest-only/self-hosted mode (nothing to sign into) or when signed in.

**Non-Goals**
- Changing the login screen or the Google flow (0005).
- A sign-in entry inside an active call (out of scope here).

## 3. Requirements

- **R1 — Guest-only.** `#guest-bar` shows iff `billing && !isLoggedIn()`. Signed-in →
  account bar instead; billing off → neither.
- **R2 — Routes to login.** Its button calls `showLogin()` (the same screen "Continue
  as guest" came from), where the value proposition already lives (`loginSub`).
- **R3 — Parity & restraint.** Same footprint/position as the account bar; one bold
  element (the accent-filled CTA), quiet copy, a faint accent wash to read as an
  invitation (not a warning).
- **R4 — i18n + a11y.** New strings in all 8 languages; visible focus ring; the
  CTA's micro-transition is dropped under `prefers-reduced-motion`. Compact on small
  phones (the sub line hides < 460 px).

## 4. Design & Architecture

Built with the `frontend-design` skill. Decisions:
- *Sits where the account bar sits.* A guest sees a sign-in affordance in the exact
  spot a signed-in user sees their account → one consistent mental model for "who am
  I / how do I switch".
- *One bold element.* The bar itself is quiet (muted copy, faint `--accent-dim`
  gradient); the only saturated thing is the `--accent-strong` **Accedi** button —
  spend boldness once.
- *Copy from the user's side.* Title states the state ("You're a guest"), sub states
  the gain ("save your credits, history & bookmarks"); the button is the plain verb
  `signIn` (already in the bundle, reused). The full pitch stays on the login screen,
  so the bar doesn't shout.

Touchpoints:
- `client/src/pages/index.astro` — `#guest-bar` markup in the home `<header>` after
  `#account-bar`; `.guest-bar` / `.guest-bar-copy` / `.guest-bar-title` /
  `.guest-bar-sub` / `.guest-signin` CSS (+ reduced-motion + < 460 px rules).
- `client/src/scripts/app.ts` — `guestBar` ref; toggle in `updatePublicGate()` (runs
  for everyone on home) and `renderAccount()` (covers the post-`refreshMe` flip);
  `#guest-signin-btn` → `showLogin()`.
- `client/src/scripts/i18n.ts` — `guestBarTitle` + `guestBarSub` ×8 languages.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Guest bar markup + CSS (accent-wash, one CTA) | `index.astro` |
| S1 | Show/hide logic + login wiring | `app.ts` |
| S2 | Strings ×8 | `i18n.ts` |

## 6. Testing & Verification

- `astro check` clean; **101/101** unit tests; production build OK.
- Logic: shown only for `billing && !isLoggedIn()`; hidden when signed in or billing
  off; button → login screen.
- Note: Vercel **preview** URLs aren't in `ALLOWED_ORIGINS`, so previews run in
  degraded guest-only mode (`billing=false`) and won't render the bar — verify on
  prod (`voxtranslate.app`), where billing is on and the origin is allowlisted.

## 7. Deployment & Operations

- **Client-only** — ships via the Vercel autodeploy on `main`. No server change.

## 8. Risks / Open Items

- Two-line copy could feel tight on very small phones — the sub line is hidden < 460 px.
- No in-call sign-in entry yet; can follow if wanted.

## 9. References

- Files: `client/src/pages/index.astro`, `client/src/scripts/app.ts`,
  `client/src/scripts/i18n.ts`. Built with the `frontend-design` skill.
