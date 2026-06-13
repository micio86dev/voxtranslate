# 0042 — TTS voice selection: prefer local + premium (delay-first)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0040](../0040-tts-no-cut/spec.md) |

## 1. Context & Problem

The translated voice sounded robotic. The browser exposes several voices per
language: **local/offline** voices start instantly but are often the robotic
"compact" ones; **network** voices (e.g. Chrome's "Google …") sound natural but
fetch audio over the wire, adding **start-up latency**. `speak()` just took the
*first* voice matching the language (`getVoices().find(...)`) — no preference — so
quality was luck, and it could even pick a laggy network voice.

The owner's standing constraint: **minimal delay is the absolute priority** (see
memory `tts-minimal-delay-priority`). So the goal is "less robotic" *without* ever
trading latency.

## 2. Goals / Non-Goals

**Goals**
- Prefer **local** voices (instant playback) — delay stays minimal.
- Among local voices, prefer **premium/enhanced** ones (less robotic) at no latency cost.
- Use a network voice only as a last resort when no local voice matches the language.

**Non-Goals**
- Bundling/downloading voices (premium *local* voices are an OS-level install the user
  makes; we just select them when present).
- A user-facing voice picker.
- Any change to the queue/latency behaviour from 0040.

## 3. Requirements

- **R1 — Local-first.** `pickVoice(lang)` scores `localService` voices far above
  network ones, so a local voice always wins when one matches the language.
- **R2 — Premium among local.** Within the same locality tier, voices whose
  name/`voiceURI` match `premium|enhanced|neural|natural|siri` rank higher (the
  enhanced Apple/OS voices) — better quality, identical (instant) latency.
- **R3 — Safe fallback.** If no local voice matches, fall back to the best matching
  (possibly network) voice so the language is still correct; if none match at all,
  leave the voice unset and let the browser pick from `lang`.
- **R4 — No perf cost.** Selection is a filter + reduce over the (small) voice list
  per utterance; nothing is fetched or pre-warmed beyond the existing `getVoices()`.

## 4. Design & Architecture

`client/src/scripts/app.ts`:
- New `pickVoice(lang)` — filters voices by `lang` prefix, scores
  `localService*100 + premiumName*10 + default*1`, returns the max.
- `speak()` uses `pickVoice(lang)` instead of the bare `find()`. Everything else
  (queue, rate, pump) is unchanged.
- **Key decision:** *locality dominates the score (×100).* Naturalness is a tie-break
  among equally-instant local voices, never a reason to accept network latency — this
  encodes the delay-first priority directly in the ranking.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `pickVoice()` (local-first, premium tie-break) + wire into `speak()` | `app.ts` |

## 6. Testing & Verification

- `astro check` clean; **101/101** unit tests; production build OK.
- Manual: on a device with an Apple **Enhanced** (or OS premium) local voice for the
  target language installed, the translated voice is noticeably less robotic and still
  starts instantly; with only compact local voices it stays instant (just plainer);
  network "Google" voices are not selected when a local match exists.

## 7. Deployment & Operations

- **Client-only** — ships via the Vercel autodeploy on `main`. No server change.
- To get the *best* quality with zero added delay, install the **Enhanced/Premium**
  local voice for the language at the OS level (macOS/iOS: Accessibility → Spoken
  Content → Voice → manage; Android: Google TTS voice data) — the app will prefer it.

## 8. Risks / Open Items

- The premium-name regex is a heuristic; vendors name voices inconsistently. Worst
  case it just doesn't *up*-rank a good local voice — it never forces a network one,
  so the delay guarantee holds regardless.

## 9. References

- Files: `client/src/scripts/app.ts` (`pickVoice`, `speak`).
- Memory: `tts-minimal-delay-priority`.
