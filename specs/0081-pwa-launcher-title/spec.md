# 0081 — PWA launcher title legibility

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Micio Dev |
| **Created** | 2026-06-16 |
| **Shipped** | 2026-06-16 |
| **Version** | — |
| **Commits** | `bfb4db2` |
| **Depends on** | — |

## 1. Context & Problem

Installed as a PWA, the app name shown on the launch/splash experience needs to
read clearly. The manifest `background_color` was the brand blue `#0871ab`, a
mid-tone: it both clashes with the icon's light background (a visible card edge
on the Android splash) and gives the OS-chosen splash title only borderline
contrast. The owner asked for the VoxTranslate title to read well against the
background, keeping the icon itself unchanged.

## 2. Goals / Non-Goals

**Goals**
- The launch title reads with strong contrast and the splash looks seamless.

**Non-Goals**
- No change to the icon art (decision: "Solo etichetta OS" — OS label only).
- No baking a wordmark into `icon.png`.

## 3. Requirements

- **R1 — Legible title.** *Given* the PWA is installed, *when* it launches, *then*
  the app name renders in high contrast against the splash background.
- **R2 — Seamless splash.** The light icon background blends into the splash rather
  than sitting on a contrasting card.

## 4. Design & Architecture

- `client/public/manifest.webmanifest`: `background_color` `#0871ab` → `#ffffff`.
  Chrome derives the splash title colour from `background_color` for max contrast,
  so a white background yields a dark, high-contrast title and a seamless backdrop
  for the light icon. `theme_color` stays brand blue (`#0871ab`) for the browser UI.
- Apple PWA metas (`apple-mobile-web-app-title` = `VoxTranslate`, capable) already
  present in `Base.astro` — no change needed.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | White splash background | `client/public/manifest.webmanifest` |

## 6. Testing & Verification

- Manual: reinstall the PWA; confirm the splash title is dark on white and legible.
  (The OS home-screen label colour is OS-controlled and unaffected.)

## 7. Deployment & Operations

- Ships with the Vercel client deploy. Installed PWAs refresh the manifest on next
  launch; no action required.

## 8. Risks / Open Items

- A white splash is a deliberate brand choice over the previous blue. `theme_color`
  keeps the brand blue in the browser chrome.

## 9. References

- Files: `client/public/manifest.webmanifest`, `client/src/layouts/Base.astro`
