# 0101 — Enhanced tier (Soniox, client-direct)

| | |
|---|---|
| **Status** | In progress |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-18 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | — |
| **Depends on** | [0093](../0093-premium-translation-engine/spec.md), [0099](../0099-premium-listener-pays/spec.md), [0100](../0100-pro-gemini-live-translate/spec.md) |

## 1. Context & Problem

VoxTranslate has three translation tiers behind one engine registry (spec 0093):
**Standard** (Deepgram + Groq), **Pro** (OpenAI), **Premium** (Gemini). All three are
*server-mediated*: the browser streams captured audio to our backend, which opens the
upstream provider WebSocket and relays results back.

We add a fourth tier, **Enhanced** (Soniox), slotted **between Standard and Pro**
(Standard → Enhanced → Pro → Premium). Unlike every existing engine, Enhanced is
**client-direct**: the browser connects straight to Soniox using a short-lived temp key
minted by our backend; the backend never proxies Enhanced audio. This removes the server
relay hop (~200–400 ms) — the whole point of the tier ("Fastest"; sub-250 ms latency,
60+ languages, auto language detection, mid-conversation language switching).

The app already runs **listener-pays** in prod (`LISTENER_PAYS=1`, spec 0099): each
*listener* picks the quality THEY receive and is billed at their own engine's rate for
the cross-language sources they hear. Enhanced fits this model exactly.

## 2. Goals / Non-Goals

**Goals**
- A client-direct Enhanced tier, listener-side, billed by the existing listener-pays
  meter with **no billing-logic changes** (cost/markup read from `SONIOX_*` env vars).
- The raw `SONIOX_API_KEY` never leaves the server; the browser only gets scoped,
  single-use, expiring temp keys.
- Ship behind a flag (`SONIOX_ENHANCED`), OFF by default; per-tier flags for all four
  tiers for uniform operational control.

**Non-Goals**
- Soniox TTS (`tts-rt`) spoken voice: v1's spoken leg reuses the app's on-device voice
  (lowest latency, within Soniox's small TTS cap). The backend can mint a TTS key
  (`spoken: true`) for a future higher-fidelity option, but the v1 client does not use it.
- Multi-region serving: EU/JP are scaffolded but resolve to US until those projects exist.
- Speaker-side translation / pushing translated audio over the mesh (the spec's original
  step-7 framing) — superseded by the listener-side decision below.

## 3. Requirements

- **R1 — Enhanced in the picker.** As a signed-in user in a listener-pays deployment with
  Enhanced enabled, *when* I open the engine picker, *then* I see "Enhanced" between
  Standard and Pro, with a "Fastest" badge and the per-source rate note.
- **R2 — Client-direct, key-safe.** *Given* I select Enhanced and join, *when* my browser
  needs to translate a remote speaker, *then* it mints a single-use temp key via
  `POST /api/soniox/session` and connects directly to Soniox; the raw key never reaches
  the client.
- **R3 — Listener-side translation.** *Given* a room with mixed languages, *when* a peer
  whose language differs from mine speaks, *then* I see real-time translated subtitles in
  my language, rendered locally; with the translated-voice toggle on, I also hear it via
  the on-device voice.
- **R4 — No double translation.** *Given* I'm an Enhanced listener, *then* the server does
  NOT also push me Standard subtitles for cross-language sources (I render my own).
- **R5 — Billing unchanged.** *Given* I'm a billed Enhanced listener, *then* I'm metered
  by the existing listener meter at Soniox's env-derived rate per active source — no new
  billing code.
- **R6 — Flag-gated rollout.** *Given* `SONIOX_ENHANCED` is unset, *then* Enhanced is not
  registered, not in `/api/engines`, and the client path is inert.

## 4. Design & Architecture

- **Components / files**
  - `server/src/config.rs` — `SonioxConfig` (cost/markup/model + region map US/EU/JP),
    `soniox_region_for_country`, the four per-tier flags (`DEEPGRAM_STANDARD`,
    `SONIOX_ENHANCED`, `OPENAI_PRO`, `GEMINI_PREMIUM`).
  - `server/src/engine/soniox.rs` — `SonioxEngine`: metadata-only, `client_direct: true`,
    `translated_audio: false`; `start_session` is a backstop (`Failed`) — never run on the
    server audio path.
  - `server/src/engine/metadata.rs` — new `client_direct` capability (mirrored client-side).
  - `server/src/rooms.rs` — `broadcast_excluding_client_direct` + `init_client_direct_engines`:
    the Standard "serve everyone" delivery skips client-direct listeners.
  - `server/src/api.rs` — `POST /api/soniox/session`: auth + credit gate, `CF-IPCountry` →
    region, mints `transcribe_websocket` (+ optional `tts_rt`) temp keys.
  - `client/src/scripts/soniox.ts` — `SonioxManager`: one Soniox pipeline per remote
    speaker, via `@soniox/speech-to-text-web` with a custom per-peer `MediaStream`.
  - `client/src/scripts/app.ts` — wiring: activate on join (listener-pays + client-direct),
    feed peer langs/streams, render via `showSubtitle`, speak via the on-device voice.

- **Protocol / API:** `POST /api/soniox/session { spoken } → { stt:{api_key,expires_at,
  endpoint}, tts:{…}|null, region, stt_model }`. Auth required (guests → 401); 503 when the
  tier is disabled; 402 on insufficient credit.

- **Sequence (listener-side):** join (engine=soniox) → server spawns the listener-pays
  meter at the Soniox rate → for each cross-language remote peer, the browser POSTs
  `/api/soniox/session`, opens a direct Soniox STT+translation WS over that peer's WebRTC
  audio → translated tokens render as subtitles + optional on-device voice. The server
  excludes this listener from its Standard subtitle fan-out.

- **Key decisions**
  - *Listener-side, not speaker-side* — matches listener-pays + local translated-audio
    playback; avoids per-language audio redistribution over the mesh.
  - *Metadata-only engine + `client_direct` capability* — keeps routing/billing/UI
    engine-agnostic; the one real change is excluding client-direct listeners from the
    Standard "serve everyone" broadcast.
  - *On-device voice for v1 spoken* — honors "prefer local voices, minimal delay" and
    Soniox's TTS-3-concurrent cap; Soniox TTS deferred.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `SonioxConfig` + region map + per-tier flags + fixtures | `config.rs`, `tests/*` |
| S1 | `client_direct` capability (server + client) | `engine/metadata.rs`, `engines.ts` |
| S2 | `SonioxEngine` + registry registration | `engine/soniox.rs`, `lib.rs` |
| S3 | Exclude client-direct listeners from server fan-out | `rooms.rs`, `deepgram.rs` |
| S4 | `POST /api/soniox/session` (temp keys, region, gates) | `api.rs`, `lib.rs` |
| S5 | Client metadata/i18n/picker (Enhanced desc + "Fastest" badge) | `engines.ts`, `i18n.ts`, `index.astro` |
| S6 | `soniox.ts` pipeline + `app.ts` wiring (per-peer audio, subtitles, voice) | `soniox.ts`, `app.ts`, `api.ts` |

## 6. Testing & Verification

- **Server:** `cargo fmt` + `cargo clippy --all-targets` + `cargo test` (green). New unit
  tests: region routing/fallback, Soniox metadata (client-direct), `broadcast_excluding_
  client_direct` (R4), temp-key request/response shaping; integration: `/api/soniox/session`
  401 guest / 503 when disabled (R2, R6).
- **Client:** `astro check` (0 errors), vitest for `engines.ts` (client-direct helper,
  `enhanced` desc, `commonLangs` unchanged) and `soniox.ts` (token extraction + manager
  lifecycle: start only cross-language, interim→final flush, restart on lang change,
  teardown). `npm run build` succeeds (SDK bundles).
- **Pending (needs live keys + auth + a multi-party call):** end-to-end Soniox handshake,
  real-latency check, and the spoken-translation path.

## 7. Deployment & Operations

- **Env:** `SONIOX_API_KEY`, `SONIOX_STT_MODEL` (`stt-rt-v5`), `SONIOX_COST_PER_MINUTE`
  (0.015), `SONIOX_COST_MARKUP_PERCENT` (85). Flags: `SONIOX_ENHANCED=1` to enable;
  per-tier `DEEPGRAM_STANDARD` (default ON), `OPENAI_PRO`, `GEMINI_PREMIUM`. Region
  scaffolding: `SONIOX_API_KEY_EU`, `SONIOX_API_KEY_JP` (empty → fall back to US).
- **Rollout:** set the four tier flags on Railway, then deploy. Note Pro/Premium now also
  require their flag in addition to the key — set `OPENAI_PRO=1` / `GEMINI_PREMIUM=1`
  before deploying so live tiers don't go dark.
- **Rollback:** unset `SONIOX_ENHANCED` → Enhanced disappears from the picker; everything
  else unchanged.

## 8. Risks / Open Items

- Soniox concurrency limits (STT 10 / TTS 3 account-wide): subtitles-only (STT) is the
  default and stays within budget; soft-fail logs a refused stream. Increases requested.
- Listener-pays delivery exclusion is the one change touching the live path — covered by
  `broadcast_excluding_client_direct` tests both ways.
- EU/JP regional endpoints + keys not yet provisioned (US-only today; code is ready).
- Soniox TTS (higher-fidelity spoken voice) deferred to a follow-up.

## 9. References

- Soniox: real-time STT+translation, temporary API keys, browser SDK
  (`@soniox/speech-to-text-web`), data residency.
- Internal: spec 0093 (engine registry), 0099 (listener-pays), 0100 (Gemini Pro/Premium).
