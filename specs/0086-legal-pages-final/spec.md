# 0086 — Production legal pages (controller identity + governing law)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Micio Dev |
| **Created** | 2026-06-16 |
| **Shipped** | 2026-06-16 |
| **Version** | — |
| **Commits** | `f9b32dc` |
| **Depends on** | [0035](../0035-legal-pages/spec.md) (legal pages scaffold) |

> **Not legal advice.** These pages are good-faith, production-presentable policy
> text tailored to the app's real data flows. They are NOT a substitute for review
> by a qualified lawyer; a one-time professional review is still recommended.

## 1. Context & Problem

`/terms`, `/privacy`, `/acceptable-use` carried a visible ⚠️ "Draft template — not
legal advice" banner and placeholder notes (`(Replace with your legal entity…)`,
`(Confirm each provider…)`). The owner wants production text that doesn't read as a
draft, for a worldwide audience, run by an individual (no company).

## 2. Goals / Non-Goals

**Goals**
- Remove the draft banner/notes; fill the GDPR Art.13 controller identity.
- Complete the sub-processor list with what the app actually uses.
- State governing law + an English-prevails clause for translations.

**Non-Goals**
- No publication of the owner's national ID (NIE) or home address — name + a
  contact email + "address on request" only (privacy/identity-theft safety).
- No 8 binding legal translations: English is authoritative, translations (via the
  backend content overlay) are for convenience.

## 3. Decisions

- **Controller:** Alessandro Micelli, an individual sole trader (autónomo)
  established in the Canary Islands, Spain. Lead supervisory authority: AEPD (Spain).
- **Public contact:** `privacy@voxtranslate.app` (privacy) and
  `support@voxtranslate.app` (terms) — to be delivered via Cloudflare Email Routing
  to the owner's mailbox (owner action; destination must be verified).
- **Sub-processors added:** Resend (email), Cloudflare (edge/DDoS/TURN), Better
  Stack (monitoring/logs), alongside the existing Google, Deepgram, Groq, Stripe,
  Supabase, Vercel, Railway.
- **Governing law:** Spain, preserving consumers' mandatory local rights; English
  version prevails.

## 4. Files

- `client/src/layouts/Legal.astro`: drop the draft banner + note + its CSS; bump
  "Last updated" to 16 June 2026.
- `client/src/pages/privacy.astro`: controller identity (§1); sub-processors (§4).
- `client/src/pages/terms.astro`: operator in the intro; new "Governing law and
  language" section.

## 5. Testing & Verification

- `astro check` + build green. Owner to read the published pages.

## 6. Risks / Open Items

- Not legal advice (see banner above). High-risk areas to flag for any future
  review: voice/biometric-adjacent data, minors, CCPA/CPRA (US users).
- `privacy@` / `support@` only deliver once Cloudflare Email Routing is configured.

## 7. References

- Files above; `client/src/pages/acceptable-use.astro` (unchanged, banner was shared).
