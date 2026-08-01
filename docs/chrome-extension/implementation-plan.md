# Implementation plan — VoxTranslate for Chrome

Phase 2 output. Reads on top of [`discovery.md`](./discovery.md).

This plan states a recommendation for each of the three open decisions, and marks them **DECISION**.
They are not resolved unilaterally because each one changes the size of the project by an order of
magnitude.

---

## 0. The three decisions

### DECISION 1 — Authentication

**Recommendation: reuse the existing Google login through a short-lived handoff code. Do not build an OAuth authorization server.**

The brief asks for `/authorize` + PKCE + code exchange + refresh rotation + revocation against
VoxTranslate. Discovery §2: none of that exists. VoxTranslate is an OAuth *client*, not a server.
Building an AS means a token table, a migration, rotation logic, and revocation semantics on a
production billing service **that cannot be compiled locally** (§1.1).

The recommended flow keeps the brief's security properties — no tokens in a query string, one-time
code, PKCE-bound — while reusing the login that already works:

```
extension                          voxtranslate.app                    backend
  │ generate verifier + S256 challenge
  ├─ launchWebAuthFlow ───────────►│
  │   /extension/connect?challenge=…&state=…
  │                                │ existing Google sign-in (unchanged)
  │                                ├─ POST /api/extension/code ───────►│  (new, small)
  │                                │    { challenge }  Bearer <JWT>    │  stores one-time code,
  │                                │◄── { code } ──────────────────────┤  60 s TTL, single use
  │◄─ redirect https://<id>.chromiumapp.org/?code=…&state=…
  ├─ POST /api/extension/token { code, verifier } ────────────────────►│  verifies S256, burns code,
  │◄── { token, expires_at, user } ────────────────────────────────────┤  returns the SAME JWT type
```

Two new endpoints, one small table. Not five endpoints and an AS.

Honest limitations, to be documented, not hidden:
- The returned token is the existing long-lived JWT. "Short-lived access token + rotating refresh"
  is **not** achieved; achieving it means changing the session model for the web app too.
- Logout is client-side only (clear storage). There is no server-side revocation for *any*
  VoxTranslate client today, so the extension is no weaker than the web app — but it is not what
  the brief asked for.

*Alternative if you want the brief literally:* a real AS is roughly a `oauth_clients` +
`oauth_codes` + `refresh_tokens` schema, rotation, reuse-detection, revocation, and a migration of
the web app onto it. That is its own project and should be specced separately.

### DECISION 2 — What the UI shows for money

**Recommendation: show the real USD balance. Do not invent credits, EUR, plans, or tiers-per-plan.**

Discovery §3: consumer accounts hold a **USD `Decimal` balance**. There is no consumer plan, no
`allowed_tiers`, no currency field. Integer "credits" exist only in the **org** pool, which the
module comment forbids mixing with consumer money.

So the account payload becomes an adaptation of what exists, per the brief's own instruction to
*"adapt this shape to existing VoxTranslate conventions instead of duplicating incompatible domain models"*:

```jsonc
{
  "user":     { "id": "…", "name": "…", "email": "…" },
  "balance":  { "amount": 2.54, "currency": "USD" },   // users.balance — real
  "engines":  [ /* GET /api/engines verbatim: id, display_name, tier,
                   rate_per_minute, input_languages, output_languages, capabilities */ ],
  "preferences": { "target_language": "it", "default_engine": "standard", … },
  "session_usage": { "spent": 0.25, "seconds": 492 }   // derived from balance_update deltas
}
```

The UI then reads:

```
This session      $0.25        (12m 18s)
Since reset       $2.16
Remaining         $2.54
```

Truthful, uses the authoritative server numbers, and adds no second ledger. Every tier is offered to
every user because that is what the backend actually does — inventing plan gating in the extension
would be exactly the "hard-coded commercial rule" the brief forbids.

*Alternative if you want credits/EUR/plans:* that is a consumer billing redesign — schema, Stripe
products, pricing display, the web app, and the dashboard. Not extension scope.

### DECISION 3 — Session shape (the blocker)

**Recommendation: (b) a dedicated single-peer extension session on the existing `/ws`.**

Discovery §6: a one-peer room translates nothing and bills nothing, because targets are derived from
*other* peers. The extension has one audio source and one listener — asymmetric.

Option (a), the synthetic two-peer room, needs **zero** backend change, which is genuinely tempting
given §1.1. But it opens two WS connections per session, injects fake peers into
`active_peers` metrics and the public lobby, and routes the source==target bypass through a
confusing path. It is a hack that would be load-bearing in production.

Option (b) adds, on the existing `/ws` route:
- accept `client=chrome-extension` with `source` (default `auto`) and `target` params,
- a single-peer session that feeds Deepgram directly and translates `source → target`,
- a third `MeterScope::Solo` billing one stream while `detected_lang != target`, and zero otherwise
  (which reuses the existing bypass semantics exactly),
- reuse of `subtitle_interim` / `subtitle_final` / `translated_audio` / `balance_*` / `language_detected`
  unchanged.

New surface is contained and additive; no existing message changes shape.

**Both options carry the same unavoidable caveat: I cannot compile the server locally.** Option (b)
would be written carefully, reviewed, and verified in CI — not verified by me.

---

## 1. Architecture

```
┌──────────────── Chrome ─────────────────┐
│ Side panel (Vue 3)                      │  UI + state machine only, no business logic
│   ↕ chrome.runtime messaging (typed)    │
│ Service worker (MV3)                    │  auth, account sync, session orchestration
│   ↕                                     │
│ Offscreen document                      │  tabCapture consumer, AudioContext,
│   ├ MediaRecorder → WebM/Opus 32k/100ms │  MediaRecorder, WS, translated-audio queue
│   ├ WebSocket (binary + JSON)           │
│   └ playback: original (ducked) + TTS   │
│ Content script                          │  Shadow-DOM subtitle overlay only
└─────────────────────────────────────────┘
                    ↓ wss
        VoxTranslate backend  (Deepgram → Groq/OpenAI/Gemini)
```

Why this split: MV3 service workers are killed aggressively and cannot hold an `AudioContext` or a
long-lived `MediaStream`. The offscreen document is the only place a continuous capture pipeline can
live. The side panel is a view — it renders state and dispatches intents.

**Critical Chrome behaviour to honour:** `chrome.tabCapture` *takes over* the tab's audio — the user
hears silence unless the captured stream is explicitly re-routed back to the speakers
(`ctx.createMediaStreamSource(stream).connect(gain).connect(ctx.destination)`). That same `GainNode`
is exactly the 0–100 % original-audio control and the ducking mechanism. One node serves three
requirements.

Chrome APIs and minimum versions are to be **verified against current official docs at implementation
time** (the brief requires it, and it is cheap): `sidePanel` (~114+), `offscreen` (~109+),
`tabCapture.getMediaStreamId` in MV3, `identity.launchWebAuthFlow`. Minimum supported version gets
documented from whatever the verification finds, not from memory.

## 2. Permissions

```jsonc
{
  "permissions": ["activeTab", "storage", "tabCapture", "offscreen", "sidePanel", "identity", "scripting"],
  "host_permissions": ["https://api.voxtranslate.app/*", "https://voxtranslate.app/*"]
}
```

No `<all_urls>`. The overlay is injected with `scripting.executeScript` on the user's explicit
start gesture, which `activeTab` grants for that tab only. If validation shows this cannot survive
SPA navigation, the fallback is **optional** host permissions requested at runtime — not a blanket
grant, and documented in the store listing either way.

## 3. Extension structure

```
src/
  background/   service worker, message router, session orchestrator
  offscreen/    capture, encode, transport, audio playback + queue
  content/      shadow-DOM overlay
  sidepanel/    Vue 3 views + stores (view layer only)
  auth/         PKCE, code exchange, token storage, refresh-on-401
  api/          typed REST client
  websocket/    protocol client + runtime validation of every inbound frame
  audio/        gain/duck graph, translated-audio queue (seq-ordered, stale-drop)
  subtitles/    partial→final reconciliation, render model
  usage/        session meter, reset baseline, formatting
  state/        session state machine (pure, fully unit-tested)
  shared/       messaging contracts, config, errors
  types/        generated/derived from server DTOs + languages.json
```

Domain logic (state machine, language stability, queue ordering, backoff, formatting) is pure and
Chrome-free, so it is unit-testable without a browser. Chrome APIs sit behind thin adapters.

## 4. State machine

`logged_out → authenticating → ready → requesting_capture → connecting → streaming
 → (reconnecting) → stopping → stopped`, plus `error` and `credits_exhausted`.

Every session carries a client-side `sessionId`; every inbound frame is checked against it and stale
frames are dropped. Transitions are a pure function — the single highest-value unit test in the
project, since it is what prevents double-capture, double-WS, post-stop audio, and orphaned overlays.

## 5. Language bypass

The server already stops translating **and stops billing** when source == target (discovery §6).
The extension's job is only to (1) avoid flapping and (2) restore audible original audio.

Stability rule: switch modes only after **3 consecutive** `language_detected` events agreeing, each
with `confidence ≥ 0.85`, spanning **≥ 4 s**. Hysteresis is asymmetric — entering bypass is slower
than leaving it, because a wrong bypass leaves the user unable to understand the content, while a
wrong translate merely costs a few cents. On bypass, the gain node ramps original audio to audible
regardless of the user's 0 % setting, and the UI states plainly why.

## 6. Translated audio

Deferred to after subtitles are stable — as the brief instructs.

`translated_audio` already gives `{speaker_id, lang, seq, pcm16_b64}` — PCM16 mono 24 kHz. The queue
keys on `(sessionId, seq)`, plays in order, drops frames older than the head, bounds the buffer, and
cancels outright on stop/stale-session. On failure or starvation the gain node restores original
audio and a non-blocking warning appears; subtitles keep running. Silence is never an acceptable state.

**Enhanced tier is out of scope for v1** and will be documented as such: it is client-direct to
Cartesia (discovery §4) and needs a second in-browser STT+TTS pipeline plus the `translate_text` hop.
Standard/Pro/Premium cover the product goal.

## 7. Testing

- **Unit** (Vitest, no browser): state machine, locale normalisation against `languages.json`,
  language-detection stability, queue ordering + stale drop, reconnect backoff with jitter,
  usage formatting, counter baseline, inbound-frame validation.
- **Integration**: a fake WS server replaying real captured frame sequences — login, refresh-on-401,
  account sync, start/stop, low balance, exhausted, disconnect/reconnect, bypass enter/exit,
  preference sync. No provider is ever called; no real credits are spent.
- **E2E** (Playwright, unpacked extension): side panel states, overlay injection and cleanup,
  start/stop controls, error states. Tab capture and real audio are **not** automatable here and are
  covered by a written manual checklist instead.

Nothing will be reported as working that I have not run.

## 8. Milestones

| # | Milestone | Backend needed | Verifiable by me |
|---|---|---|---|
| 1 | Repo, submodule, toolchain, manifest, CI | no | yes |
| 2 | Side panel shell + state machine + messaging | no | yes (unit/e2e) |
| 3 | Auth + account sync | **yes** (2 endpoints, 1 table) | partly — client yes, server CI-only |
| 4 | Capture → offscreen → WS, original-audio gain | **yes** (DECISION 3) | client yes; end-to-end needs you |
| 5 | Subtitle overlay | no | partly (unit/e2e; real pages need you) |
| 6 | Language bypass | no | yes (unit) |
| 7 | Usage/billing UI + reset counter | maybe (baseline column) | yes (unit) |
| 8 | Translated audio | no | unit yes; latency needs you |
| 9 | Docs, ADRs, privacy, store package | no | yes |

Milestones 1–2 and 5–7 are unblocked regardless of how the decisions land. Milestones 3–4 are gated
on DECISION 1 and DECISION 3.

## 9. What I will not do without explicit approval

- Create the GitHub repository (I will create it locally first and show you the exact command).
- Write or apply any SQL migration.
- Push, deploy, publish, or submit anything to the Chrome Web Store.
- Claim any capture, latency, or on-page behaviour works without a human verifying it.
