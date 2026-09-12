# 0113 — Business Phone on the public website

| | |
|---|---|
| **Status** | 🚧 In progress |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-09-11 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | — |
| **Depends on** | [0111](../0111-translated-voip/spec.md), [0112](../0112-business-phone-dashboard/spec.md), [0106](../0106-voxtranslate-for-business/spec.md) |

## 1. Context & Problem

`website/src/pages/phone-call-translation.astro` is a good page. It has a hero, a
four-step explanation, eight capabilities, **five honest "cannot do yet"s**, a consent
section, six use cases, a twelve-question FAQ and `FAQPage` + `BreadcrumbList` JSON-LD.
Its claims are generated from `src/lib/phone-capabilities.ts`, so marketing copy cannot
get ahead of the build — which is the right design and rare.

**Nobody can reach it.** Verified 2026-09-11:

| # | Defect | Evidence |
|---|---|---|
| D1 | **Zero inbound internal links.** `rg 'phone-call-translation'` over `src/` returns the page itself and `sitemap-pages.xml.ts`. Not in the header, the footer, the home page, `/business/` or `/pricing/`. | `rg` over `website/src` |
| D2 | **Absent from `llms.txt`.** The curated map lists Product, extension, platforms, guides, personas and blog. No phone. | `src/pages/llms.txt.ts` |
| D3 | **No CTA.** The page ends in a "Related" list. Every other commercial page ends in a `Button` to the app. | `phone-call-translation.astro` |
| D4 | **No price, anywhere.** "What it costs" is prose. Telephony has no equivalent of `src/data/engines.json` or `org-plans.json`, and the site's own history says why that matters: the four plan figures were duplicated across five locale files and the site advertised USD against EUR Stripe prices for two months. | `src/lib/org-plans.ts` header |
| D5 | **No localised mention at all.** `rg -ic 'telephone\|phone call'` over the five `src/i18n/*.json` → **0**. A German B2B visitor has no way to learn the product does telephony. | `src/i18n/*.json` |

The project's own comment states the rule D1 breaks, about the guides hub: *"a sitemap
entry is a crawl hint, not a vote."*

## 2. Goals / Non-Goals

**Goals**
- The phone page is reachable from the site, by a human and by a crawler.
- A B2B visitor in any of the five site locales learns the product does telephony.
- The page ends in a way to start.
- Any price on the site comes from configuration, never from copy.
- `llms.txt` can answer "can VoxTranslate translate phone calls?"

**Non-Goals**
- **Localising `/phone-call-translation/` itself.** `seo-routes.ts` says it is "English-only
  until localisation of the SEO routes is real", and the whole non-prefixed SEO tree —
  `/live-translation/`, `/alternatives/`, `/compare/`, `/guides/`, `/for/` — shares that
  decision. Translating one page of it would break the tree's consistency for one feature.
  Localised visibility is delivered instead where the localised site already lives (R3).
- A per-destination rate table or estimator. Carrier rates are a CSV rate deck imported
  into `voip_rates` (`docs/voip-telnyx-setup.md` §6); publishing them needs a sync the
  site does not have, and inventing "from" numbers is what D4 exists to prevent.
- Any claim about inbound calls, PSTN bridging, video, China or EU-only processing —
  `phone-capabilities.ts` marks all five `false`, and they must keep rendering as nos.

## 3. Requirements

- **R1 — The page is reachable.** As a visitor, I want to find the phone product without
  being told its URL.
  - *Given* any page, *then* the footer's Product column links to it — the same place
    `/guides/` is linked from, for the same reason.
  - *Given* `/business/`, *then* a section describes telephony and links to it.

- **R2 — A crawler and an agent can find it.** 
  - *Given* `llms.txt`, *then* the phone page appears under Product with a description
    that answers what it is and which plans include it.

- **R3 — A B2B visitor reads about it in their own language.** As a non-English visitor,
  I want to learn the product does telephony without being handed an English page first.
  - *Given* `/business/` in any of the five locales, *then* the telephony section's copy
    is in that locale, and only the deep-dive link leads to English.

- **R4 — The page ends in a way to start.**
  - *Given* the end of the page, *then* a primary CTA leads to the app, matching the
    tracking attributes the other commercial pages use.

- **R5 — Prices come from configuration.** As a maintainer, I want no figure on this site
  that a person typed into copy.
  - *Given* any price the phone content shows, *then* it is read from a committed data
    snapshot carrying `fetchedAt` and a `source` — which for the translation component
    means `src/data/engines.json` via `engines.ts`, already in exactly that pattern.
  - *Given* no verified per-destination rate exists, *then* the page says what varies and
    why, and shows no number rather than an invented one.
  - *Note:* **no new snapshot file.** The only publishable figure is the translation
    tier's per-minute rate, and `engines.ts` already serves it from a dated snapshot.
    Carrier rates live in a CSV deck imported into `voip_rates` and are not publishable;
    number fees do not exist until 0115; and the margin policy is internal and must not
    be. A `phone-pricing.json` holding nothing real would be the ceremony this requirement
    exists to prevent.

- **R6 — Nothing claimed that is not built.**
  - *Given* `phone-capabilities.ts` marks a capability `false`, *then* no copy added by
    this change contradicts it, on any page or in `llms.txt`.

## 4. Design & Architecture

| Path | Responsibility |
|---|---|
| `src/components/layout/Footer.astro` | Product column gains the phone link (R1) |
| `src/pages/[lang]/business.astro` | New localised telephony section (R1, R3) |
| `src/pages/phone-call-translation.astro` | CTA, and the pricing section fed from data (R4, R5) |
| `src/pages/llms.txt.ts` | Product entry (R2) |
| `src/i18n/*.json` | The localised business-page copy, five locales (R3) |

**Why the footer and not the header.** The header is five links and is localised; the
target is English. The footer's Product column already carries `/guides/`, another
English-only SEO route, for exactly this reason — it is the established place for a page
that belongs to the product but not to the localised tree.

**What the pricing section can honestly say.** Three components, only two of which have a
publishable number: the Vox translation tier (in `engines.json` already), the telephone
network (varies by destination, imported from a carrier rate deck, not published), and
optional services. The section names the three, shows the tier rate from configuration,
and says the network rate is quoted before each call in the dashboard — which is true, and
is what the dialer does.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Spec | this file |
| S1 | R1, R4, R5 — footer link, CTA, pricing section fed from `engines.ts` | `Footer.astro`, `phone-call-translation.astro` |
| S2 | R1, R3 — localised section on `/business/`, five locales | `business.astro`, `src/i18n/*.json` |
| S3 | R2 — `llms.txt` | `src/pages/llms.txt.ts` |

## 6. Testing & Verification

The website has no test runner. Verification is therefore the build plus explicit checks:

- `npm run build` and `npm run verify:schemas` both clean.
- A link check: `rg 'phone-call-translation' src/` returns the page, the sitemap, the
  footer, `/business/` and `llms.txt` — the whole point of R1 is that this list grows.
- Locale key parity across the five `src/i18n/*.json` for every key added by S3.
- No number in the phone copy that is not read from `engines.json`.
- Every `false` capability in `phone-capabilities.ts` still renders as a "cannot do yet".

## 7. Deployment & Operations

- Static content only. No env var, no migration, no server change.
- The translation rate shown comes from `src/data/engines.json`, which already carries
  `fetchedAt` and `source` and is refreshed by the same kind of script as its sibling.

## 8. Risks / Open Items

1. The website has **no automated tests at all** — no vitest, no Playwright. Everything
   above is verified by the build and by reading. That is a pre-existing gap, named here
   rather than quietly accepted.
2. `/phone-call-translation/` stays English. R3 mitigates it for B2B visitors; the real
   fix is localising the SEO tree, which is its own piece of work.
3. The website is on Astro 5 while the dashboard and client are on Astro 7. Not touched
   here, but it is drift.

## 9. References

- Specs: [0111](../0111-translated-voip/spec.md), [0112](../0112-business-phone-dashboard/spec.md)
- Files: `website/src/lib/phone-capabilities.ts` (the source of truth for claims),
  `website/src/lib/org-plans.ts` (the snapshot pattern, and why it exists)
