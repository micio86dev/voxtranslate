# 0092 — iOS PWA splash screens

| | |
|---|---|
| **Status** | Draft |
| **Owner** | Micio Dev |
| **Created** | 2026-06-17 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | `<sha>` |
| **Depends on** | [0091](../0091-pwa-launcher-dark-splash/spec.md) |

## 1. Context & Problem

[0091](../0091-pwa-launcher-dark-splash/spec.md) fixed the Android PWA launch
(dark splash via the manifest `background_color`, plus a proper maskable icon).
iOS, however, **ignores** `background_color`: an installed PWA launched from the
Home Screen shows a blank screen unless the page declares per-device
`apple-touch-startup-image` links. So on iPhone/iPad the app still launched into a
bare screen with no branding — unprofessional next to the polished Android splash.

## 2. Goals / Non-Goals

**Goals**
- An installed iOS PWA shows a branded splash on launch matching Android: the dark
  app shell (`#0a0b10`) with the centred app icon.
- Cover the current iPhone and iPad line-up (recent + still-supported models), in
  both portrait and landscape.

**Non-Goals**
- No wordmark/text in the splash (native launch-screen convention is icon-only).
- No change to the Android splash or the manifest (0091 already covers those).

## 3. Requirements

- **R1 — Branded iOS splash.** *Given* the PWA is installed on iOS, *when* it
  launches from the Home Screen, *then* a dark splash with the centred icon shows
  (no blank/white screen).
- **R2 — Per-device sharpness.** *Given* any covered iPhone/iPad, *then* the
  matched image is the device's exact pixel resolution (no scaling blur), for the
  current orientation.
- **R3 — Visual parity.** The iOS splash uses the same `#0a0b10` background and the
  same icon as the Android splash for a consistent brand.

## 4. Design & Architecture

- **Assets — `client/public/splash/apple-splash-<W>x<H>.png`:** one image per
  covered device × orientation (42 total). Each is the app shell colour `#0a0b10`
  with a rounded white app-icon tile (the `icon.png` art) centred at ~27% of the
  short edge — the native launch-screen look, matching the Android splash icon.
- **Generator — `scripts/gen-ios-splash.py`:** reproducible, dependency-free.
  Decodes `icon.png`, builds an antialiased rounded master (icon corners filled
  with the background so the bleed is seamless) with a pure-Python PNG encoder,
  then uses `sips` (native macOS) to resize + centre-pad per device. Re-run to add
  new devices. The device list `[width, height, dpr]` (portrait points) lives here
  and is mirrored in `Base.astro`.
- **`client/src/layouts/Base.astro`:** an `appleSplash` array drives a `.map()` that
  emits two `<link rel="apple-touch-startup-image">` per device — a portrait and a
  landscape variant — each with a precise media query:
  `screen and (device-width: {dw}px) and (device-height: {dh}px) and
  (-webkit-device-pixel-ratio: {dpr}) and (orientation: …)`. iOS picks the first
  matching link; portrait/landscape hrefs swap the pixel dimensions.
- **Key decision:** images are not added to the service-worker precache `SHELL`
  (42 images would bloat install); they are served by the normal same-origin
  network-first handler. iOS caches the home-screen splash itself.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Splash asset generator | `scripts/gen-ios-splash.py` |
| S1 | 42 per-device splash images | `client/public/splash/*.png` |
| S2 | Startup-image link tags | `client/src/layouts/Base.astro` |

## 6. Testing & Verification

- `astro check` and the production build pass; the rendered HTML contains 42
  `apple-touch-startup-image` links and every referenced `/splash/*.png` exists.
- Visual: portrait (iPhone) and landscape (iPad) samples show the centred rounded
  icon on the dark background.
- Manual on-device: install the PWA on an iPhone/iPad and confirm the branded
  splash appears on launch in both orientations.

## 7. Deployment & Operations

- Client-only static assets + layout — ships with the Vercel client deploy on merge
  to `main`. Installed iOS PWAs pick up the new `<head>` on next launch (HTML is
  served network-first); a reinstall guarantees the refreshed splash.

## 8. Risks / Open Items

- Apple ships new screen sizes over time; re-run `scripts/gen-ios-splash.py` and add
  the `[w, h, dpr]` entry to both the script and `Base.astro` to cover them.
- Splash assets add ~8 MB to the repo (per-device PNGs); each client fetches only
  its one matching image from the CDN.

## 9. References

- Depends on: [0091](../0091-pwa-launcher-dark-splash/spec.md)
- Files: `scripts/gen-ios-splash.py`, `client/public/splash/`,
  `client/src/layouts/Base.astro`
