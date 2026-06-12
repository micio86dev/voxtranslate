# 0022 — Guest Sign-in Gate for Public Rooms

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0005](../0005-accounts-credits-billing/spec.md), [0008](../0008-managed-content-i18n/spec.md) |

## 1. Context & Problem

Public rooms require an account: the server rejects a guest with
`login_required`, and the home screen already disables the **public** visibility
toggle for guests (forcing private) with the `publicNeedsLogin` hint. **But the
live public-room list was still fully clickable.** A guest who clicked an online
public room was sent all the way into pre-join (camera warm-up, device pickers)
only to be bounced back on connect with a terse error toast — a dead-end that
wastes the click and never explains *why* or what signing in would unlock.

We want to intercept that click up front and turn the dead-end into a friendly
**conversion moment**: explain that public rooms need an account and list what
the user gains by registering, with a one-tap path to sign in.

## 2. Goals / Non-Goals

**Goals**
- A guest clicking an online public room sees a **sign-in gate**, not pre-join.
- The gate **explains the benefits** of an account and offers a **Sign in** CTA.
- Fully localized in all 8 UI languages.

**Non-Goals**
- Changing who *can* join public rooms (server policy unchanged — accounts only).
- A new auth method or sign-up flow (the gate routes into the existing
  `showLogin()` screen).
- Touching guest-only mode (billing off): with no accounts there's nothing to
  gate, so the list stays directly clickable there.
- Regenerating `directus/seed-i18n.sql` (it is already stale vs. the bundled
  strings from earlier specs; the client uses bundled defaults, so the gate works
  without it — a full re-seed is tracked separately, see §8).

## 3. Requirements

- **R1 — Intercept guest click.** As a guest, *when* I click an online public
  room in the lobby list, *then* a sign-in gate modal opens and I am **not** taken
  to pre-join.
- **R2 — Explain + route.** *Given* the gate is open, *then* it shows a title, a
  short reason, and a bulleted list of account benefits; *when* I press **Sign
  in**, *then* the gate closes and the login screen opens (`showLogin()`); *when*
  I press **Dismiss** or Escape, *then* the gate closes and I stay in the lobby.
- **R3 — Logged-in / guest-only unaffected.** *Given* I am signed in (or the
  backend runs guest-only with billing off), *when* I click a public room, *then*
  I go straight to pre-join as before.
- **R4 — Localized.** The gate's strings are present in all 8 shipped languages
  (`it, en, es, fr, de, pt, ja, zh`).

## 4. Design & Architecture

- **Components / files:**
  - `client/src/pages/index.astro` — a new `#signin-gate-modal` reusing the
    existing `.modal-overlay`/`.modal.card.modal-narrow` pattern (so it inherits
    the focus-trap, Escape-to-close, and focus-restore behaviour of `show()`).
    Benefits render as a left-aligned `.gate-benefits` list inside the centered
    modal. All copy via `data-i18n` (auto-applied + re-applied on language change).
  - `client/src/scripts/app.ts` — the lobby list click handler (`renderRooms`)
    now guards on `billing && !auth.isLoggedIn()` → `openSigninGate()` and returns
    before `goPrejoin()`. `openSigninGate()` = `show(modal, true)`; the **Sign
    in** button closes the modal then calls `showLogin()`; **Dismiss** closes it.
  - `client/src/scripts/i18n.ts` — five new keys (`guestGateTitle`,
    `guestGateText`, `guestGateB1..B3`) in all 8 languages; the CTA/secondary
    buttons reuse the existing `signIn` / `dismiss` keys.
- **Key decisions:**
  - *Client-side gate, server policy unchanged* — the server is still the
    authority (`login_required`); this is purely a better front door, so a
    bypassed client just hits the same server rejection it always did.
  - *Reuse the guest predicate* `billing && !auth.isLoggedIn()` — identical to the
    visibility-toggle gate, so guest detection stays consistent in one place's
    worth of logic.
  - *Reuse the modal infra* — no new focus-trap/close code; the gate is just
    another `.modal-overlay`, so a11y (WCAG 2.1.2 / 2.4.3) comes for free.
  - *Benefits are real auth-only perks* — saved credits/history (spec 0005),
    transcripts (0009) / bookmarks (0013) / AI report (0014), room glossary
    (0011) — not vague marketing.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Sign-in gate modal markup + benefits list CSS | `index.astro` |
| S1 | Guard the lobby click; open/dismiss/sign-in wiring | `app.ts` |
| S2 | i18n keys in all 8 languages | `i18n.ts` |

## 6. Testing & Verification

- `astro check` clean; 98/98 client unit tests pass; production build OK.
- Manual: as a guest, click a public room → gate opens (no pre-join); **Sign in**
  → login screen; **Dismiss**/Escape → back to lobby. Signed in → joins directly.
- Languages: switch UI language → gate copy follows (English fallback for any
  un-seeded language is moot since all 8 are bundled).

## 7. Deployment & Operations

- Client-only change → ships with the Vercel autodeploy on `main`. No server
  change, no env vars, no migration.

## 8. Risks / Open Items

- **Stale `seed-i18n.sql`.** The committed Directus i18n seed predates several
  specs' strings; regenerating it here would have added ~4k lines of unrelated
  drift, so it was deliberately left out. The new gate strings work via the
  client's bundled defaults. Follow-up: regenerate `seed-i18n.sql` (and
  `seed-content.sql`) in a dedicated chore so every bundled string is editable in
  Directus again.

## 9. References

- Files: `client/src/pages/index.astro`, `client/src/scripts/app.ts`,
  `client/src/scripts/i18n.ts`
- Related: `publicNeedsLogin` gate on the create/visibility toggle (spec 0005),
  managed i18n (spec 0008).
