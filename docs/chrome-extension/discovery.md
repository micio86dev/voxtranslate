# Discovery — VoxTranslate for Chrome

Phase 1 output. Everything below was read out of the workspace on 2026-08-01; nothing is assumed.
File references are `path:line` against the parent repo unless stated otherwise.

---

## 1. Workspace and repositories

| Repo | Path | Remote | Branch |
|---|---|---|---|
| Parent (server + client + infra) | `.` | `git@github.com:micio86dev/voxtranslate.git` | `develop` |
| Website (marketing) | `website/` | `git@github.com:micio86dev/voxtranslate-website.git` | `main` |
| Dashboard (B2B) | `dashboard/` | `git@github.com:micio86dev/voxtranslate-dashboard.git` | `main` |
| n8n workflows | `n8n/` | `git@github.com:micio86dev/voxtranslate-n8n.git` | `main` |

Submodules are declared in `.gitmodules` and pinned; `git submodule status` is clean.

**Naming convention is established**: every submodule is `voxtranslate-<thing>` under the `micio86dev`
personal account (not an org). So `voxtranslate-chrome-extension` is the correct name.

Verified with `gh`:
- Authenticated as `micio86dev`, token scopes `admin:public_key, gist, read:org, repo` → repo creation is possible.
- `micio86dev/voxtranslate-chrome-extension` **does not exist**. Nothing to overwrite.

Tooling present: `bun 1.3.0`, `node v24.18.0`, `rustc 1.89.0`.

### 1.1 Build verification constraint — CORRECTED 2026-08-02

> **This finding was wrong, and the error message is why.** The server DOES build and
> test locally. `/opt/homebrew/bin/rustc` (1.89) precedes `~/.cargo/bin` on PATH and
> shadows the installed rustup toolchains — which include 1.93, 1.97 and stable. Cargo
> then reports an MSRV failure that reads like a missing toolchain.
>
> What works (note that `rustup run stable cargo` is NOT enough — Homebrew's `cargo`
> still wins; the toolchain paths must be explicit):
>
> ```bash
> TC="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin"
> export PATH="$TC:$PATH" RUSTC="$TC/rustc" CARGO_INCREMENTAL=0
> "$TC/cargo" clippy --all-targets -- -D warnings
> "$TC/cargo" test --lib
> ```
>
> `CARGO_INCREMENTAL=0` avoids an rustc 1.97 ICE on `crossbeam_utils::CachePadded` while
> compiling the test profile. The Astro client builds too, under node v24.18.0.
>
> The original observation is kept below because the misleading error is worth
> recognising, not because the conclusion held.

`cargo check` in `server/` fails on a PATH where Homebrew's rustc wins:

```
error: rustc 1.89.0 is not supported by the following packages:
  typst@0.15.1 requires rustc 1.92   (+ krilla, hayro, 20 more — the PDF stack)
```

~~**Consequence: no backend Rust change can be compiled or tested locally.**~~ Superseded by the
correction above. The backend work for this feature *was* compiled, linted and tested locally
before being pushed — and doing so caught four defects CI would not have, three of them
billing-related.

The client is Astro 5 + vanilla TS under `client/src/{layouts,pages,scripts}`.

---

## 2. Authentication — what actually exists

`server/src/auth.rs`, routes in `server/src/lib.rs:566-592`.

- **Google-only.** `POST /api/auth/google` accepts either a legacy GSI `credential` (Google ID token)
  or an OAuth `code` + `redirect_uri` that the server exchanges with Google (`google_oauth.rs`).
- Verification: Google `tokeninfo`, then asserts `aud == our client id`, `iss` is Google, and
  `email_verified` (`auth.rs:91-106`).
- On success it upserts the user (granting `free_credits` on first login only) and returns:
  `{ token, user: UserProfile, calendar_connected, is_new }` (`auth.rs:341-354`).
- **Session token = one HS256 JWT** (`issue_jwt`, `auth.rs:147`) with claims `{sub, email, name, exp}`,
  lifetime `billing.jwt_expiry_hours`.
- `GET /api/auth/config` returns the public Google client id — also used by clients as the
  "is billing enabled?" probe.
- `GET /api/user/me` returns the profile + balance.

### What does NOT exist

This matters enormously for the task brief, which assumes an OAuth authorization server:

| Brief assumes | Reality |
|---|---|
| `GET /api/extension/authorize` | ✗ no authorization endpoint of our own |
| Authorization-code + PKCE issued by VoxTranslate | ✗ nothing issues our own auth codes |
| Refresh tokens, rotation | ✗ **no refresh token exists at all** |
| `POST /api/extension/logout` / revocation | ✗ JWTs are stateless; there is no revocation list or session table |
| Short-lived access tokens | ✗ one long-lived JWT is the whole model |

VoxTranslate is an **OAuth client** (to Google). It is **not an OAuth authorization server**.
Building `/authorize` + PKCE + code exchange + refresh rotation + revocation means writing an AS
from scratch, including a new token/session table and migration. (What shipped instead: a signed,
PKCE-bound 60-second code — no table, no migration. See ADR 0005 in the extension repo.)

`chrome.identity.launchWebAuthFlow` itself is fine and is the right API; the question is only what
it talks to on our side.

---

## 3. Money model — the brief and the codebase disagree

This is the most consequential mismatch found.

### Consumer (B2C) — what the extension would use

`server/src/billing.rs`, tables `users`, `credit_transactions`, `usage_sessions`.

- Balance is `users.balance`, a **`Decimal` in USD**. Not credits. There is no currency column.
- `credit_transactions` rows are USD amounts with `kind ∈ {purchase, usage, free_credit, ai_*, …}`.
- Metering deducts USD directly: `amount = usd(rate_per_second × interval × streams)` (`usage.rs:137`).
- `UserProfile.balance` is serialized as a plain `f64` (`auth.rs:284`).
- **There is no consumer subscription plan.** No plan table, no `allowed_tiers`, no `default_tier`.
  The consumer product is pure pay-as-you-go against a USD balance.

### Organisation (B2B) — separate, do not touch

`server/src/business/credits.rs` — `organizations.credits_balance` is an **`INTEGER` credit pool**
with its own `organization_credits_transactions` ledger and real plans/subscriptions.
The module header states the rule explicitly:

> "Org credits and consumer DECIMAL credits never mix."

### So, concretely

The brief's account payload is not implementable as written:

| Brief field | Reality |
|---|---|
| `credits.remaining: 1428.4` | consumer balance is USD dollars, e.g. `2.54` |
| `credits.currency: "EUR"` | no currency concept; everything is USD |
| `plan.code` / `plan.name` | no consumer plan exists |
| `plan.allowed_tiers` | no per-plan tier gating exists — all engines are available to everyone |
| `plan.default_tier` | not stored |
| `usage_counter.reset_at` | does not exist (see §7) |

Two honest options, to be decided in the plan: mirror the real USD model in the extension UI
(cheap, truthful, ships now), or invent a consumer credits/plans layer (large, touches billing,
needs migrations + Stripe rework). The brief's own rule — *"do not hard-code commercial rules
that already exist"* and *"never create a second independent credit ledger"* — points at the first.

---

## 4. Tiers are engines

`server/src/engine/mod.rs`, `engine/metadata.rs`, exposed by `GET /api/engines` (`api.rs:57`).

Four registered engines. **The persisted ids do not match the display names** — a frozen historical
quirk documented at `engine/mod.rs:40-57`:

| Display name | `tier` label | persisted `id` | translated audio | client-direct |
|---|---|---|---|---|
| Standard | `standard` | `standard` | no (client TTS) | no |
| Enhanced | `enhanced` | `cartesia` | yes (in browser) | **yes** |
| Pro | `pro` | **`premium`** | yes | no |
| Premium | `premium` | `gemini_live_translate` | yes | no |

`EngineInfo` (the client-safe DTO) carries `rate_per_minute = cost × (1 + markup)`.
Raw `cost_per_minute` and `markup` are **never serialized** — there is a test asserting it
(`metadata.rs:134`). The extension must display `rate_per_minute` and nothing else.

**Enhanced is architecturally special**: `capabilities.client_direct = true`. The browser connects
straight to Cartesia with a server-minted short-lived token (`POST /api/sessions/enhanced/session`);
the server never sees that audio and only bills the meter. Porting Enhanced to the extension means
porting a whole second client-side pipeline (Cartesia STT + TTS + a `translate_text` round-trip
through our WS for the translation hop, since Cartesia does not translate — `protocol.rs:106-115`).

### Languages

`server/src/engine/languages.json` — **84 languages**, the single source of truth, shared by the
Rust backend (`include_str!`) and the TS frontend (`client/src/scripts/langmap.ts`).
Keyed per tier: `standard`, `enhanced`, `pro`, `premium`. The extension should consume this same
file rather than duplicating a language list.

---

## 5. WebSocket protocol

`server/src/protocol.rs`, handler `lib.rs:992`.

**Connect:** `GET /ws?room=&lang=&name=&id=&public=&token=&engine=`
- `token` is the session JWT **in the query string** — the existing, established convention.
- `lang=auto` is explicitly allowed at join time (`lib.rs:1001-1014`); `set_lang` later rejects `auto`.
- No token → guest (unbilled). Invalid token → connection rejected.

**Client → server** (JSON text frames): `start`, `stop`, `offer`, `answer`, `ice`, `chat`,
`mute_audio`, `mute_video`, `emoji`, `hand_raise`, `screen_share`, `set_lang`, `whiteboard`, `game`,
`enhanced_fallback`, `translate_text`.

**Server → client**: `room_joined`, `peer_joined`, `peer_left`, `room_full`, signaling,
`chat_message`, `subtitle_interim`, `subtitle_final`, `translated_audio`, `language_detected`,
`balance_update`, `low_balance`, `balance_exhausted`, `engine_downgraded`, `capture_format`,
`translated_text`, `moderation_warning`, `error{message, code}`.

Notable, and directly reusable by the extension:

- `subtitle_interim { speaker_id, speaker_name, text, lang }` — partials.
- `subtitle_final { speaker_id, speaker_name, original, lang, translations: {lang → text} }` — finals
  fan out to **every** language in the room; the client picks `translations[my_lang]`.
- `translated_audio { speaker_id, lang, seq, pcm16_b64 }` — **PCM16 mono @ 24 kHz, base64, sequenced**.
  A translated-audio queue with ordering already has a wire format. No need to invent one.
- `language_detected { peer_id, lang, confidence? }`.
- `balance_update` / `low_balance` (once) / `balance_exhausted` — the usage/credit events already exist.
- `error.code` (e.g. `insufficient_balance`, `invalid_token`, `banned`) — structured errors exist.

**Audio is sent as binary frames.**

### Audio format (settled — do not choose a new codec)

- Default: **WebM/Opus, 32 kbps, mono, 100 ms `MediaRecorder` timeslice**
  (`client/src/scripts/audio-capture.ts:37-39`; spec 0043).
- Deepgram is opened with `container=webm` and lets the header carry rate/encoding (`deepgram.rs:80-84`),
  model `nova-2`.
- Alternative: **PCM16 / 24 kHz / mono**, used only when the server pushes `capture_format {pcm:true}`
  because a Pro/Premium listener needs the same stream to feed both OpenAI and Deepgram
  (`deepgram.rs:87`, `protocol.rs:348-356`).

The extension should emit WebM/Opus 32 kbps @ 100 ms and honour `capture_format`. That matches the
brief's "100–250 ms" target and requires zero backend codec work.

---

## 6. The core architectural blocker: rooms are symmetric, the extension is not

This is the finding that most shapes the plan.

`server/src/rooms.rs`, `usage.rs`.

VoxTranslate's model is a **room of symmetric peers**. Every peer has exactly **one `lang`** —
the language they speak *and* receive in. There is no per-peer source/target split.

Translation fan-out is derived from *other* peers:
- `get_room_languages(room, exclude_id)` returns distinct languages of **other** peers, skipping `"auto"`
  (`rooms.rs:610-620`).
- `billable_streams(targets, speaker_lang, …)` returns **`None`** when no target language differs from
  the speaker's (`usage.rs:69-82`) — the meter then *skips the tick entirely*.
- A listener whose language equals the speaker's is skipped from delivery (`rooms.rs:145-155`).

**Therefore a single-peer room transcribes nothing useful, translates nothing, and bills nothing.**

The extension's shape is fundamentally different:

```
room model:      peer(lang=it) ⇄ peer(lang=en)      symmetric, both speak
extension model: tab audio (lang=?, never listens) → user (lang=it, never speaks)
```

One foreign audio **source**, one **listener**, asymmetric, and the source's language is unknown
and may change mid-stream. This does not map onto a one-peer room.

**Two ways out** (decision belongs in the plan):

- **(a) Synthetic two-peer room.** The extension opens two WS connections into a private room:
  one "tab" peer with `lang=auto` that only sends audio, one "listener" peer with `lang=<target>`
  that only receives. Zero protocol change; but it doubles connections, abuses room semantics,
  pollutes room/peer metrics and the lobby, and makes the source==target bypass behave oddly
  (the tab peer resolving to the listener's language collapses the room to one language, which
  is *correct* for bypass but arrives via a confusing path).
- **(b) A dedicated single-peer extension session.** A `client=chrome-extension` mode on the WS
  (or a sibling route) with an explicit `source`/`target` split and a new `MeterScope`. Cleaner
  semantics, isolated blast radius, but it is genuinely new backend code — in a repo I cannot
  compile locally.

### What already exists and should NOT be rebuilt

- **Source == target bypass**: already the server's behaviour. When the speaker's language matches
  the only listener language, delivery is skipped *and* the meter skips the tick — i.e. **no charge**.
  The brief's "don't translate, don't bill when languages match" is existing behaviour, room-shaped.
- **Language detection**: Deepgram `detect_language=true`, surfaced as `language_detected` with a
  confidence. Stability/hysteresis on top of it is client work and does not exist yet.
- **Low/exhausted credit**: `low_balance` (fires once) and `balance_exhausted` + the meter stopping
  the audio session while leaving the connection up.

---

## 7. Preferences and the usage counter

Existing user-scoped preferences (columns on `users`):

| Preference | Endpoint | Column |
|---|---|---|
| Preferred language | `POST /api/user/language` | `users.language` (migration 045) |
| TTS engine + voice | `POST /api/user/tts-prefs` | `users.tts_engine_pref`, `users.tts_voice_id` |
| Locale | set at login | `users.locale` (migration 026) |
| Cartesia cloned voice | `POST /api/sessions/enhanced/clone-voice` | `users.cartesia_voice_id` |

**Not present**: subtitle preferences, translated-audio toggle, original-audio volume, subtitle font
size/position, dual-language mode, default tier/engine, and — importantly — **any usage-counter
reset baseline**. `POST /api/extension/usage-counter/reset` has no existing counterpart; it needs a
column + migration, or it lives in `chrome.storage` and is device-local.

Migrations: 51 files, `server/migrations/001..051`. Project rule (CLAUDE.md) is that they must be
idempotent and that an already-applied migration must **never** be edited (`sqlx::migrate!` checksums them).

---

## 8. Reusable HTTP surface

Directly usable by the extension with no backend change:

| Endpoint | Gives |
|---|---|
| `GET /api/auth/config` | Google client id; billing-enabled probe |
| `POST /api/auth/google` | login → JWT + profile |
| `GET /api/user/me` | id, email, name, avatar, **balance**, consent, tts prefs, language |
| `GET /api/engines` | all tiers with `rate_per_minute`, per-tier language lists, capabilities, flags |
| `GET /api/billing/packages` | purchasable credit packages |
| `POST /api/billing/checkout` | Stripe checkout session |
| `GET /api/billing/history` | ledger |
| `GET /api/usage/sessions` | past usage sessions |
| `POST /api/user/language` | persist target language |

Auth middleware is `middleware.rs` `AuthUser` (Bearer JWT). CORS is an explicit allow-list of app
origins with `allow_credentials(true)` (`lib.rs:540-565`) — **a `chrome-extension://` origin is not
currently allowed**, which is a real, small backend change needed for any XHR from the extension.

Base URLs come from config, not hard-coding: `APP_BASE_URL` (default `https://voxtranslate.app`),
`DASHBOARD_BASE_URL` (`config.rs:1091-1100`).

---

## 9. Required backend changes (minimum viable, ranked by risk)

| # | Change | Risk | Why needed |
|---|---|---|---|
| 1 | Allow `chrome-extension://<id>` in CORS allow-list | low | any `fetch` from the side panel |
| 2 | Extension session mode on `/ws` (source/target split, single peer) + a `MeterScope` for it | **high** | §6 blocker; nothing else makes the product work |
| 3 | Extension preferences (subtitles, volume, font, position, default engine) | medium | needs columns + migration, or keep device-local |
| 4 | Usage-counter reset baseline | medium | needs a column + migration |
| 5 | OAuth AS: `/authorize`, code+PKCE exchange, refresh rotation, revocation | **very high** | only if we refuse to reuse the existing JWT handoff |

Every one of these is unverifiable locally (§1.1).

---

## 10. Risks and unknowns

1. ~~**Backend cannot be compiled or tested on this machine.**~~ **Wrong — see §1.1.** A
   shadowed toolchain produced a convincing error message and I accepted it. The lesson:
   "verified" means the conclusion was tested, not that an error message was read
   carefully.
2. **The room model does not fit the product.** §6. No amount of extension-side cleverness avoids
   a backend decision here.
3. **The brief's commercial model does not exist.** Credits, EUR, plans, allowed tiers, default tier
   (§3). Implementing it as specified means building a consumer plan/credits system — far beyond a
   Chrome extension, and in direct tension with the brief's own "never create a second ledger" rule.
4. **No OAuth authorization server** (§2). PKCE-with-VoxTranslate is a from-scratch AS, not a wiring job.
5. **Enhanced tier is client-direct.** Supporting it in the extension means a second in-browser
   pipeline (Cartesia STT+TTS) plus the translate-hop, inside an MV3 offscreen document.
6. **`<all_urls>`-shaped tension.** Subtitles over "any website" needs broad content-script injection.
   The brief asks to avoid `<all_urls>`. `activeTab` + `scripting.executeScript` on user gesture is
   the narrow path, but it must be validated against fullscreen and SPA navigation.
7. **Chrome API behaviour must be checked against current docs** before committing — in particular
   `chrome.tabCapture` in MV3 (the `getMediaStreamId` + offscreen consumer pattern), and whether the
   captured stream still plays through to the user by default (it does not; audio must be re-routed
   or the tab goes silent).
8. **Not verifiable by me**: tab capture, real subtitle rendering on YouTube, translated-audio
   latency, repeated start/stop cleanup. These need a human with Chrome, a logged-in account and a
   funded balance.

---

## 11. Summary judgement

The extension's **client half** is well-supported: the audio format, subtitle events, translated-audio
frames, language-detection events, balance events, engine catalogue, and language catalogue all exist
and are reusable almost verbatim.

The **backend half** is where the brief and reality diverge, on three independent axes — session
shape (§6), commercial model (§3), and auth (§2). None of those is a coding detail; each is a product
decision with a materially different cost. They are put to the user in the implementation plan rather
than resolved silently.
