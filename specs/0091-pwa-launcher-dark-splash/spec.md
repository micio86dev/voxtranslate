# 0091 — PWA launcher dark splash + maskable icon

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Micio Dev |
| **Created** | 2026-06-17 |
| **Shipped** | 2026-06-17 |
| **Version** | — |
| **Commits** | `65e66ac` |
| **Depends on** | [0081](../0081-pwa-launcher-title/spec.md) (revises) |

## 1. Context & Problem

On mobile the installed PWA launches with a jarring experience: the splash flashes
white before the (dark) app shell paints, the logo is barely visible, and the app
name does not read. Root causes:

- **White splash on a dark app.** [0081](../0081-pwa-launcher-title/spec.md) set
  `background_color` to `#ffffff` to give the OS-derived splash title contrast. But
  `icon.png` is a fully-opaque, near-**white** square (RGB, no alpha; corners
  `#fefefe`). On a white splash the icon body vanishes into the background — only the
  blue logo elements float — and the app launches white→dark (`--bg: #0a0b10`),
  reading as a "strange animation". 0081 optimised the title at the cost of icon
  visibility; for this artwork that trade was wrong.
- **Mis-declared maskable icon.** The same `icon.png` was declared `purpose:
  "maskable"`, but its content spans ~84% of the tile — outside the 80% maskable
  safe zone — so the adaptive-icon mask clips the logo during the splash reveal.

## 2. Goals / Non-Goals

**Goals**
- The splash is seamless with the dark app shell (no white flash).
- The logo reads with strong contrast on the splash.
- The app name renders legibly on the splash.
- The maskable/adaptive icon is never clipped.

**Non-Goals**
- No redraw of the logo artwork (the icon stays white-background with the blue mark).
- iOS `apple-touch-startup-image` splash images (iOS ignores `background_color`) —
  tracked as a follow-up, not in scope here.

## 3. Requirements

- **R1 — Seamless dark splash.** *Given* the PWA is installed, *when* it launches,
  *then* the splash background matches the app shell (`#0a0b10`) — no white flash.
- **R2 — Visible logo.** *Given* the splash, *then* the white icon card contrasts
  strongly against the dark background.
- **R3 — Legible title.** *Given* the splash, *then* Chrome derives a light title
  from the dark `background_color`, so the app name reads clearly.
- **R4 — No clipping.** *Given* an adaptive-icon launcher, *then* the maskable icon
  keeps the full logo inside the safe zone with no clipping.

## 4. Design & Architecture

- **`client/public/manifest.webmanifest`:**
  - `background_color` `#ffffff` → `#0a0b10` (the app's `--bg`). Chrome derives the
    splash title colour from `background_color`, so a dark background yields a light,
    high-contrast title *and* a contrasting backdrop for the white icon. `theme_color`
    stays brand blue `#0871ab` for the browser/status-bar chrome.
  - The `maskable` entry now points at a dedicated `icon-maskable.png` instead of the
    full-bleed `icon.png` (which stays as the `any` icon).
- **`client/public/icon-maskable.png` (new):** `icon.png` scaled to 80% and padded
  back to 512×512 with white (`#ffffff`, matching the icon's own background so the
  bleed is invisible and is cropped away by the mask). Logo now sits inside the 80%
  safe zone. Generated with `sips`.
- **`client/public/sw.js`:** cache `voxtranslate-v1` → `v2` and `icon-maskable.png`
  added to the precached `SHELL`. The version bump changes the SW bytes, triggering
  the update cycle; `activate` purges the v1 cache that still held the old (white)
  manifest, so devices don't keep serving the stale splash.
- **Key decision:** dark splash over white. 0081 chose white to maximise title
  contrast; for a white-background icon that erased the icon. A dark splash satisfies
  both the title contrast *and* icon visibility, and matches the dark app for a
  seamless launch. This revises 0081 (now superseded).

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Dark splash background | `client/public/manifest.webmanifest` |
| S1 | Dedicated maskable icon | `client/public/icon-maskable.png`, `manifest.webmanifest` |
| S2 | SW cache bump + precache | `client/public/sw.js` |

## 6. Testing & Verification

- Manifest is valid JSON; `background_color` is `#0a0b10`; the maskable entry points
  at `/icon-maskable.png` (512×512).
- `icon-maskable.png` renders the full logo within the safe zone (verified visually).
- Manual: reinstall the PWA on Android; confirm the splash is dark, the icon and the
  title are legible, and the maskable home icon is not clipped.

## 7. Deployment & Operations

- Client-only static assets — ships with the Vercel client deploy on merge to `main`.
- Installed PWAs read `background_color`/icons from the manifest at install time;
  users must reinstall the PWA to see the new splash. The SW v2 bump refreshes the
  cached shell on next visit.

## 8. Risks / Open Items

- iOS still shows a plain `background_color`-less splash (no `apple-touch-startup-image`).
  Follow-up if iPhone splash polish is wanted.
- The maskable bleed is white; on launchers that don't mask (square tiles) a faint
  inner edge of the original card may be visible. Cropped away on circle/squircle masks.

## 9. References

- Revises: [0081](../0081-pwa-launcher-title/spec.md)
- Files: `client/public/manifest.webmanifest`, `client/public/icon-maskable.png`,
  `client/public/sw.js`
