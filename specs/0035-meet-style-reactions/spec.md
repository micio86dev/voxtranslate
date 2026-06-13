# 0035 — Google-Meet-style Emoji Reactions

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0020](../0020-session-sound-cues-sticky-reactions/spec.md) |

## 1. Context & Problem

Quick reactions (spec 0002) appended a small **2.2 rem** emoji **inside the sender's
video tile** and floated it ~70 px over 1.5 s. In a multi-tile grid it was easy to
miss — confined to one small cell, low and brief. The owner wants the
**Google-Meet** feel: a **big** emoji rising from the **centre of the whole stage**,
obvious, with a satisfying pop, and showing **who** reacted.

## 2. Goals / Non-Goals

**Goals**
- Reactions render large, from the **bottom-centre of the video stage** (not a tile).
- A pop entrance + upward float + fade, with a little horizontal **drift/jitter** so
  a burst scatters instead of stacking.
- Show the **sender's name**.
- Respect `prefers-reduced-motion`.

**Non-Goals**
- Changing the reaction protocol/relay or the picker (still the sticky panel, 0020).
- Avatars in the float (name label is enough).

## 3. Requirements

- **R1 — Stage-level.** `showEmojiReaction` appends a `.reaction-float` to
  `.video-stage` (not the peer cell), centred at the bottom.
- **R2 — Big + animated.** 3.4 rem emoji (2.8 rem mobile), rising ~62 vh with a
  pop (scale 0.4→1.18→1) and fade, over ~3.2 s; per-reaction random `--x` start
  (±110 px) and `--drift` (±40 px).
- **R3 — Who.** A small pill shows the sender's display name (or "You").
- **R4 — Reduced motion.** With `prefers-reduced-motion: reduce`, a static centred
  fade (no rise/drift).
- **R5 — Above video.** The float gets its own compositor layer (`will-change`) so
  it paints above the GPU-composited tiles.

## 4. Design & Architecture

- `client/src/pages/index.astro` — replace the per-tile `.emoji-float`/`emoji-pop`
  with stage-scoped `.reaction-float` / `.reaction-emoji` / `.reaction-name` +
  `reaction-rise` keyframes + a reduced-motion fallback.
- `client/src/scripts/app.ts` — `showEmojiReaction` builds the float (emoji + name),
  sets `--x`/`--drift` from `Math.random()`, appends to `.video-stage`, removes after
  3.4 s. Self reactions echo back (server broadcasts to everyone), so the sender sees
  their own with name = "You".
- `client/e2e/call.spec.ts` — assert `.video-stage .reaction-float` (was the cell
  `.emoji-float`).
- **Key decisions:**
  - *Stage container, not the tile* — a reaction is a room-level moment, not a
    per-person caption; centring it makes it the shared focal point Meet creates.
  - *Random drift* — turns a rapid burst (the sticky panel from 0020 makes bursts
    easy) into a lively scatter rather than one stacked column.
  - *Name pill, not avatar* — readable, light, and we already have the names; avatars
    would add fetch/complexity for little gain.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Stage-level float + name in JS | `app.ts` |
| S1 | Big rise/pop/drift CSS + reduced-motion | `index.astro` |
| S2 | E2E selector update | `e2e/call.spec.ts` |

## 6. Testing & Verification

- `astro check` clean; **101/101** client unit tests; build OK; E2E updated to the
  new selector.
- Manual: fire several reactions quickly → big emojis rise from centre with names,
  scattered; reduced-motion → they just fade in place.

## 7. Deployment & Operations

- Client-only — ships with the Vercel autodeploy on `main`. No server change.

## 8. Risks / Open Items

- Many simultaneous reactions could crowd the centre; the random drift + short life
  mitigate it. A hard cap per second already exists client-side (0020 RateLimiter).

## 9. References

- Files: `client/src/pages/index.astro`, `client/src/scripts/app.ts`,
  `client/e2e/call.spec.ts`. Built with the `frontend-design` skill.
