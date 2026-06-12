# 0036 — Reaction animation polish + guest-auth & overflow-menu fixes

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0035](../0035-meet-style-reactions/spec.md), [0023](../0023-call-toolbar-overflow-menu/spec.md), [0022](../0022-guest-public-room-gate/spec.md), [0005](../0005-accounts-credits-billing/spec.md) |

## 1. Context & Problem

Four issues surfaced while testing the live app:

1. **Reaction animation janky & faint.** The Meet-style reaction (0035) keyed the
   emoji's *transform* at 12 % / 26 % / 100 % (−8 vh, −16 vh, −62 vh) under a single
   heavily front-loaded quint curve, so it shot up then crawled — not fluid. It was
   also smaller than it could be.
2. **Bookmark "Salva questo momento" errors "as a guest".** A signed-in user with a
   **stale/expired token** saw the authed-only 🔖 button (and could pick public
   rooms), because `isLoggedIn()` only checks the token *exists*, not that it's
   *valid*. The server rightly rejects the expired token, so the bookmark POST 401s
   — the user is effectively treated as a guest.
3. **Overflow ⋯ menu closes on every pick.** Toggling tts / hand / screen-share from
   the menu (0023) closed it each time, so you had to reopen ⋯ before the next pick.
4. **Guests could not reach the public-room gate.** The "public" visibility segment
   was natively `disabled` for guests, so tapping it did nothing — no explanation,
   no path to sign in.

## 2. Goals / Non-Goals

**Goals**
- A genuinely fluid reaction rise (continuous velocity) that's bigger and glows.
- The client's auth state matches the server's: a stale token is cleared on boot, so
  authed-only UI (🔖, public rooms) never shows to an effectively-guest session.
- The ⋯ menu stays open while you act on its controls; only the ⋯ button, an outside
  click, or Escape close it.
- A guest tapping "public" gets the sign-in benefits modal (0022), not dead silence.

**Non-Goals**
- Changing the reaction protocol/relay or the picker (still 0020/0035).
- Server-side auth changes — the server already rejects stale tokens correctly; the
  fix is client-side.
- Full JWT refresh-token rotation (tracked separately).

## 3. Requirements

- **R1 — Fluid rise.** `.reaction-float` animates *transform* only at 0 %/100 % (one
  easing governs the whole travel → no velocity hitch); the pop lives on
  `.reaction-emoji` as a separate spring, so both layers stay GPU-composited.
- **R2 — More prominent.** Emoji 4.2 rem (3.3 rem mobile, was 3.4/2.8), dual
  drop-shadow incl. an `--accent` glow; bolder name pill.
- **R3 — Token validated on boot.** `boot()` `await`s `refreshMe()` when a token is
  present *before* deciding login vs home, so an expired token (→ 401) is cleared and
  the session falls back to guest. Mere network errors keep the session.
- **R4 — Sticky overflow menu.** Acting on a `.control-btn` inside `#more-menu` no
  longer closes it; close is via the ⋯ toggle, an outside click, or Escape.
- **R5 — Guest public-room gate.** The public segment stays clickable for guests
  (not natively disabled) with a 🔒 affordance; tapping it (or pressing Enter with
  public selected) opens the sign-in benefits modal. Private stays the forced
  default for guests.

## 4. Design & Architecture

- `client/src/pages/index.astro`
  - Reaction CSS: split `reaction-rise` (container translate+fade, single
    `cubic-bezier(0.39,0.575,0.565,1)` ease-out, transform only at 0 %/100 %, opacity
    held 9 %→78 %) from `reaction-pop` (emoji scale spring
    `cubic-bezier(0.34,1.56,0.64,1)`). Bigger emoji + accent glow + bolder pill.
    Reduced-motion: static fade, pop disabled.
  - `.seg-btn.locked::after { content: '🔒' }` — the guest-gated public affordance.
- `client/src/scripts/app.ts`
  - `boot()`: `if (billing && isLoggedIn()) await refreshMe()` before the login/home
    branch.
  - `visGroup` click + `enterBtn` click: a guest choosing public → `openSigninGate()`.
  - `updatePublicGate()`: keep the public button enabled (`disabled = false`) + add
    `.locked`, instead of a native `disabled` that swallows the click.
  - Removed the `#more-menu` "close on control-btn click" listener.
  - `showEmojiReaction` removal timeout 3400 → 3700 ms (matches the 3.6 s rise).
- `client/e2e/{call,screenshare}.spec.ts`: the menu now stays open across picks, so
  the inter-pick `#btn-more` reopen clicks were removed.
- **Key decisions:**
  - *Validate the token on boot, not lazily.* The previous fire-and-forget
    `refreshMe()` raced the first authed action; awaiting it once up front makes the
    whole UI consistent with the server for every authed feature, not just bookmarks.
  - *Decouple pop from rise.* One easing over one transform segment is the only
    reliable way to get continuous velocity; the pop is a second composited layer.
  - *Keep the gated button clickable.* A native `disabled` gives a guest no feedback;
    a clickable lock that opens the benefits modal turns a dead end into a sign-up.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Fluid + bigger reaction (decoupled pop/rise, glow, name pill) | `index.astro`, `app.ts` |
| S1 | Validate stored token on boot (clears stale → guest) | `app.ts` |
| S2 | Sticky overflow menu (no close-on-pick) | `app.ts`, `e2e/*` |
| S3 | Guest → public-room sign-in gate (clickable lock) | `app.ts`, `index.astro` |

## 6. Testing & Verification

- `astro check` clean; **101/101** client unit tests; production build OK.
- E2E updated: `call.spec` / `screenshare.spec` open ⋯ once and act multiple times.
- Manual: fire several reactions → big emojis rise smoothly from centre with names;
  with an expired token the app lands on the login screen (no phantom 🔖 / public);
  in-call ⋯ menu stays open as you toggle; a guest tapping "public" sees the modal.

## 7. Deployment & Operations

- **Client-only** — ships with the Vercel autodeploy on `main`. No server change.

## 8. Risks / Open Items

- Awaiting `refreshMe()` adds one `/api/user/me` round-trip to boot for signed-in
  users (they need the balance anyway); guests pay nothing. Network failure keeps the
  session rather than logging them out.
- A long-lived open ⋯ menu can overlap a side panel opened from within it; acceptable
  and matches the "toggle is the ⋯ button" model the owner asked for.

## 9. References

- Files: `client/src/pages/index.astro`, `client/src/scripts/app.ts`,
  `client/e2e/call.spec.ts`, `client/e2e/screenshare.spec.ts`. Reaction work used the
  `frontend-design` skill.
