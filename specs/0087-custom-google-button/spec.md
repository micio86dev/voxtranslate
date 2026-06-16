# 0087 — Custom Google sign-in button (no white personalized card)

| | |
|---|---|
| **Status** | In progress |
| **Owner** | Micio Dev |
| **Created** | 2026-06-16 |
| **Shipped** | — |
| **Version** | — |
| **Depends on** | [0083](../0083-quiz-gating-gsi-polish/spec.md) (GSI button) |

## 1. Context & Problem

Google's official `renderButton`, in its **personalized** state ("Continua come
\<email\>" + a separate account chooser), draws a **white card** around the pill.
That white is rendered inside Google's cross-origin iframe and is not controllable
via `theme`, `width`, or CSS (confirmed: `filled_black` only darkened the pill and
left the white). On the dark login card it looks broken.

## 2. Goals / Non-Goals

**Goals**
- A sign-in button that visually matches Google's official filled-blue "Sign in
  with Google" button (recognizable, trusted), with **no white card**.
- Sign-in must never break: a fallback path always works.
- No server change, no new secrets.

**Non-Goals**
- No change to the credential flow (still the ID-token via `id.initialize`).
- Not pixel-perfect to Google's spec; close enough to read as the Google button.

## 3. Design

- `index.astro`: a custom `<button id="gsi-signin">` styled like the official
  filled-blue button (white G tile + 4-colour Google `G` SVG + blue pill +
  localized label `continueGoogle`), plus an empty `#gsi-official` fallback slot.
- `app.ts` `setupGoogleSignIn`: `id.initialize({…, use_fedcm_for_prompt: true})`,
  reveal the custom button; on click call `id.prompt()` → the browser's **native
  account chooser** (FedCM), no white card. The credential returns via the
  unchanged `onGoogleCredential`.
- **Fallback:** if `prompt()` reports not-displayed/skipped, or no credential
  lands within 7s (FedCM hides the moment API), render Google's **official**
  button into `#gsi-official` so sign-in still works. The timer is cleared the
  moment a credential arrives.
- `i18n.ts`: `continueGoogle` added for all 8 languages.

## 4. Testing & Verification

- `astro check` 0 errors; `vitest` 211; build green.
- **Manual (owner, on a real device — CANNOT be tested headlessly):** open the
  login screen → the button is the clean blue Google button with no white card;
  tapping it opens the native chooser and signs in; if the chooser is suppressed,
  the official button appears and still signs in.

## 5. Risks / Open Items

- Touches the sign-in entry point and can't be verified here (owner's browser
  blocks the app). FedCM `prompt()` reliability varies by browser; the
  official-button fallback is the safety net. Instant revert available if needed.

## 6. References

- Files: `client/src/pages/index.astro`, `client/src/scripts/app.ts`,
  `client/src/scripts/i18n.ts`.
