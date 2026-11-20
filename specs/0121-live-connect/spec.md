# 0121 — Live Connect

| | |
|---|---|
| **Status** | In progress: built, pending real-device verification and release decisions |
| **Owner** | micio86dev |
| **Created** | 2026-11-19 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | — |
| **Depends on** | [0110](../0110-talk-to-anyone/spec.md), [0106](../0106-voxtranslate-for-business/spec.md), [0093](../0093-premium-translation-engine/spec.md), [0094](../0094-premium-capacity-fallback/spec.md), [0080](../0080-webrtc-reconnect-recovery/spec.md), [0096](../0096-supabase-rls-lockdown/spec.md) |

Tracking: `odd/tasks/live-connect.md` (the original request is its `L1`, quoted verbatim).
Architecture, lifecycle and operations live in [`docs/live-connect.md`](../../docs/live-connect.md).

## 1. Context & Problem

"Talk to anyone" (0110) puts two people around **one** phone: one microphone, an automatic
direction gate, and a self-echo guard that makes it half-duplex. It cannot serve two people who
are not standing next to each other, or a business that wants a customer to speak with a member
of staff through the customer's own phone.

Live Connect is the two-device counterpart: two phones, two microphones, two independent
translation streams running **at the same time**, no turn-taking. It must reuse the platform
rather than fork it: the same rooms, engines, wallet, projects and conversation search.

What the audit (2026-11-19, branch `develop`) established and this spec relies on:

- Mic audio already reaches the server as PCM16 mono 24 kHz binary WS frames, and translated audio
  returns as JSON + base64 PCM16 24 kHz. The Standard engine opens one upstream session per
  speaker **per target language** and reconciles every second, so two peers in one room already
  translate in both directions concurrently (`engine/standard.rs`, `tests/room_session_standard.rs`).
- The direction gate of Talk (`talk/direction.rs`, `talk/utterance.rs`) exists only because one
  microphone carries two languages. It is not needed when each phone declares its own.
- No QR/desk entity, no server-minted session, no guest credential bound to a session, no
  "owner pays for the guest" payer, and no embedding of realtime transcript events exist today.
- `/ws` is hardened (ping 15 s, idle close 45 s); `/ws/talk` is not.

## 2. Goals / Non-Goals

**Goals**
- Two phones converse in two languages simultaneously, with no accounts required for the guest.
- B2C: the signed-in owner creates the session and pays for both directions.
- B2B: permanent, revocable, regenerable desk QR codes; every scan creates a new session bound to
  an existing project and billed to the org wallet; conversations searchable like meetings.
- No new plan or tier, no parallel wallet, project model or search index.

**Non-Goals**
- WebRTC audio between the phones (the server must receive each microphone regardless).
- The Enhanced (Cartesia, client-direct) engine: it cannot serve guests.
- More than two participants per session; video; permanent raw-audio storage.
- Any claim of hardware support before real-device verification (M8).

## 3. Requirements

- **R1 — Create a session (B2C).** *Given* a signed-in user with balance, *when* they create a
  session, *then* they receive a session id, an invitation URL and a QR for it, valid for the join
  window, and it counts against their concurrent-session cap.
- **R2 — Guest joins without an account.** *Given* a valid invitation, *when* a guest opens it,
  chooses a language, accepts the notice and grants the microphone, *then* they join as the only
  guest, with no registration and no payment.
- **R3 — Two directions, concurrently.** *Given* two joined peers with different languages,
  *when* both speak at once, *then* each hears only the other's translation and neither direction
  waits for the other.
- **R4 — One invitation, one guest.** *Given* a guest already joined, *when* a second device uses
  the same invitation, *then* it is refused (`session_full`); the first device can reconnect with
  its own token.
- **R5 — Permanent desk QR.** *Given* an active desk, *when* anyone scans its QR, *then* a **new**
  session is created every time; the QR never resolves to an existing conversation.
- **R6 — Revoke and regenerate.** *Given* a printed QR, *when* the desk is revoked, *then* scans are
  refused; *when* it is regenerated, *then* the old print stops working and the new one works.
- **R7 — Staff answers a desk scan.** *Given* a waiting desk session, *when* an org member answers
  from the dashboard Desk console or from the main app, *then* they take the owner side; unanswered
  sessions expire after the desk wait.
- **R8 — Billing.** *Given* a session with two different languages, *then* usage for **both**
  directions is billed to the owner (user wallet or org wallet), once per tick even if a meter
  restarts, and never to the guest.
- **R9 — Insufficient credit.** *Given* the payer runs out mid-call, *then* both phones are told
  (`balance_exhausted`), audio stops and the session ends cleanly.
- **R10 — Concurrent sessions cannot overspend.** *Given* N sessions on one org wallet, *then* the
  balance never goes negative and no tick is charged twice.
- **R11 — Project binding.** *Given* a B2B session, *then* its project is validated against the
  org at creation, persisted, and survives the session.
- **R12 — Transcripts.** *Given* a B2B session (or an opted-in B2C one), *then* original and
  translated text are stored separately, attributed to the two participants, and searchable with
  the existing `/search` and `/insights`, scoped by org and project.
- **R13 — Retention and erasure.** *Given* org retention, *then* live conversations are purged by
  the same sweep; an org admin can delete one session including guest speech.
- **R14 — Reconnection.** *Given* a dropped phone, *when* it returns within the grace with its
  token, *then* it resumes in the same session without a second meter.
- **R15 — Existing features unchanged.** "Talk to anyone", rooms, webinars and calls behave as before.

## 4. Design & Architecture

- **Components / files:**
  - `server/src/live/` — `config` (flags, limits), `tokens` (invite, guest token, desk MAC), then
    `sessions`, `desks`, `ws`, `presence`, `echo`, `billing`, `finalize` as milestones land.
  - `server/migrations/065_live_connect.sql` — five tables, see below.
  - Client: `pages/live.astro`, `pages/lc/[code].astro`, `pages/lc/d/[token].astro`, `scripts/live/*`.
  - Dashboard (submodule): `pages/[lang]/live/*`, `scripts/live-*.ts`, `api.ts` additions.
  - Directus: `directus/setup-live-connect.mjs`; admin routes `/api/admin/live/*`.
- **Data model** (all new tables RLS default-deny, like every other):
  `live_desks`, `live_sessions` (+ `call_sessions.kind='live_connect'` row), `live_participants`,
  `live_usage_ticks` (idempotency ledger), `live_session_events` (diagnostics).
- **Protocol / API:** REST under `/api/live` and `/api/business/organizations/{org}/live/*`; one
  WebSocket `/ws/live?token=…` reusing the existing frame vocabulary plus `session_state`,
  `language_mismatch`, `echo_flush`, `error{code}`. Full table in `docs/live-connect.md`.
- **Sequence (B2C):** owner `POST /api/live/sessions` → link/QR → guest `POST /api/live/join` →
  both open `/ws/live` → each `start`s its mic → the room opens one engine session per direction →
  translated audio and subtitles fan out → meters tick per peer against the owner → `end`/expiry
  finalizes, persists (if policy says so) and releases the seats.
- **Sequence (B2B desk):** customer scans → `POST /api/live/desks/scan` creates a `waiting`
  session and returns a guest token → staff console polls `waiting`, answers → both on `/ws/live`.

**Key decisions** (decision → rationale → alternative rejected)

1. **WebSocket only, no WebRTC.** The server must receive each microphone to translate it, and the
   engine returns audio over WS; WebRTC would add a raw-voice leg nobody wants and TURN to run.
   → new `/ws/live` handler reusing `RoomManager`, engines and the meter, with `/ws` hardening
   copied in (idle, frame and rate caps). Rejected: extending `/ws/talk` (no idle timeout, signed-in
   only, single-mic assumptions) and mutating `/ws` (too large and too shared to risk).
2. **A session is a `live_sessions` row linked to a `call_sessions` row.** Rooms are in memory; the
   durable record of a meeting is `call_sessions`. Linking keeps transcripts, search, history, the
   meter and the org ledger (whose `session_id` must reference `call_sessions`) working unchanged;
   `ON DELETE SET NULL` lets retention purge the conversation without erasing usage.
3. **Server-minted identity.** Room code `live-<uuid>`, peer id = participant uuid, max two
   participants enforced by two partial unique indexes. This removes the client-chosen peer id that
   lets any joiner evict another today.
4. **Guest credential = signed token with its own key and audience.** Invite code (192 bits, only
   its SHA-256 stored) is exchanged for a token naming session, participant and role. Keys are
   derived per purpose from `LIVE_TOKEN_KEY`; the account JWT verifier cannot accept it and vice
   versa (tested, including the reused-secret misconfiguration).
5. **Desk QR token is derived, not stored.** `HMAC(desk key, desk id ‖ version)` truncated to 128
   bits; the database keeps only `token_version`. Revoke is a status flag, regenerate bumps the
   version. Chosen over a stored hash so printed assets can be re-downloaded at any time. The QR
   carries a public handle and the MAC only — never an org id or account identifier.
6. **The owner pays for both directions; ticks are idempotent.** Guest peers get a meter whose
   payer is the owner. A debit and its `live_usage_ticks` row commit in one transaction with
   `ON CONFLICT DO NOTHING`, so a duplicated meter charges nothing twice. One meter per peer.
7. **Language is declared, not guessed.** Each phone sets its language; `auto` is not allowed.
   A wrong-language check is advisory only and never delays audio. JEV is not on the realtime path:
   it is not an audio detector and spikes were rejected for latency (see Risks).
8. **Echo defence in layers, measured on devices.** Browser AEC/NS/AGC → guarded mic gate by
   default → open mode only for declared headphones → server `EchoLedger` (drop echo from the
   transcript and flush the partner's queue) → loop breaker. *Status: the first three layers are
   built; the `EchoLedger` and loop breaker were deferred until real-device measurements show whether
   they are needed.*
9. **Persistence by policy.** B2C stores nothing unless the owner opts in; B2B follows org
   retention with a notice to the guest. Raw audio is never stored.
10. **Rollout behind `LIVE_CONNECT_ENABLED`**, off by default; single replica documented.

## 5. Implementation

Status as of 2026-11-19. "Built" means implemented and covered by automated tests; nothing here has
been run on real devices (S7/M8 below).

| Slice | What | Key files | State |
|-------|------|-----------|-------|
| S0 (M1) | Spec, docs, migration 065, tokens, config | `specs/0121-*`, `docs/live-connect.md`, `065_live_connect.sql`, `live/{config,tokens}.rs` | built |
| S1 (M2) | Sessions REST, `/ws/live`, sweeper, engine wiring, capacity gate | `live/{sessions,routes,ws,sweeper,state}.rs` | built. **Not built**: `presence.rs` (presence is read from the room registry), `echo.rs` / the server `EchoLedger`, the `language_mismatch` and `echo_flush` frames (deferred until device measurements, see the tracking file, L5) |
| S2 (M5) | Per-phone owner-paid meters, idempotent ticks, caps | `live/billing.rs`, migration 066 | built, independently verified (L7) |
| S3 (M3) | Home entry, host/guest/desk-scan pages, audio, 84 locales | `client/src/{pages,scripts/live,scripts/i18n}` | built; one client defect pinned by e2e (partner id read as `peer_id`, the server sends `id`); legal copy and low-confidence locales awaiting review |
| S4 (M4) | Finalize, transcripts, embeddings, filters, retention, erasure | `live/{finalize,erase}.rs`, `business/search.rs` | built, independently verified (L9) |
| S5 (M6) | Dashboard area, desks, QR assets, console, usage | `dashboard/src/…` (submodule), `live/{desks,dashboard}.rs` | built |
| S6 (M7) | Directus collections, roles, flows, admin routes, AI summary | `directus/setup-live-connect.mjs`, `live/{admin,summary}.rs`, migrations 067-068 | built; the admin routes' independent verification was not completed |
| S7 (M8) | Latency, load, browser e2e, device matrix, runbook | `docs/live-connect*.md`, `loadtest/live-connect.js`, `client/e2e/live.spec.ts` | latency histogram, k6 script (6-socket smoke only), real-server e2e, docs and the **empty** device matrix are done; **real-device verification and a penetration test are not** |

## 6. Testing & Verification

- **Schema (S0):** `tests/live_schema.rs` — RLS, owner check, vocabularies, two-seat cap, tick
  idempotency, purge-keeps-usage, desk uniqueness, migration re-run.
- **Tokens/config (S0):** unit tests for forgery, expiry, audience, JWT confusion, desk MAC
  version binding, shape checks, flag/key/limit validation.
- **Server integration:** `live_rest`, `live_ws` (two peers on the mock engine, join/full/expiry,
  mute, reconnect, latency histogram), `live_billing` (double tick, exhaustion, concurrency),
  `live_desks`, `live_intelligence`, `live_dashboard`, `live_admin`. DB-gated tests must run against
  pgvector Postgres (`docker compose up -d --wait postgres`, db `voxtranslate_voip_test`) — a run of
  a few milliseconds skipped everything.
- **Browser e2e against a real local server** (`npm run test:e2e:live`, opt-in): host/guest, used and
  expired links, desk scan/queue/answer/revoke, the `?answer=` deep link, and that Talk is untouched,
  with the provider mocked. One test pins a known client defect.
- **Load:** `loadtest/live-connect.js`, smoke-run only (6 sockets against the mock).
- Simulated vs real-device results are separated in `docs/live-connect-device-matrix.md`, which has
  **no results**: the real-device run is the product owner's.

## 7. Deployment & Operations

- Env: `LIVE_CONNECT_ENABLED`, `LIVE_TOKEN_KEY`, `LIVE_JOIN_WINDOW_SECS`, `LIVE_DESK_WAIT_SECS`,
  `LIVE_MAX_SESSION_MIN`, `LIVE_MAX_CONCURRENT_PER_OWNER`, `LIVE_RECONNECT_GRACE_SECS`
  `LIVE_DESK_MAX_WAITING`, `LIVE_FINALIZE_GRACE_SECS`, `LIVE_RETENTION_DEFAULT_DAYS`,
  `LIVE_B2C_RETENTION_DAYS` (`server/.env.example`).
- Migrations 065-068 are additive and idempotent. Flag off → no route is reachable (but the two
  erase endpoints stay mounted). Enable and rollback steps: `docs/live-connect.md`, "Deployment and
  migration".
- Rooms are in memory and the server is single-replica: a deploy drops live sessions; phones
  reconnect within the grace. Capacity: Qwen's 32-session cap ≈ 16 simultaneous pairs.
- Rotating `LIVE_TOKEN_KEY` invalidates every printed desk QR and every guest token in flight.

## 8. Risks / Open Items

- Echo loop on speaker-only phones may make full duplex unusable; quality is unknown until M8.
- Standard silently skips a direction at capacity; a capacity gate is part of S1.
- GDPR: guests bypass the 18+/ToS gate today, Standard exports data to Singapore, no guest erasure
  exists yet. Needs consent copy, notice and a DPIA review (legal pages, 0086). Not decided here.
- `LISTENER_PAYS` production value is unknown; Live Connect bills the owner regardless.
- Specs 0093/0100/0107 are stale (describe Deepgram+Groq Standard).
- JEV/TypeSafe: no code exists; rejected twice for latency on realtime paths.
- Open defaults to confirm: B2C transcripts opt-in; 90-day retention default for Business-plan live
  sessions; Premium selectable for signed-in owners; 120-minute cap; 3 concurrent sessions per owner.

## 9. References

- [0110 Talk to anyone](../0110-talk-to-anyone/spec.md) · [0106 Business](../0106-voxtranslate-for-business/spec.md)
- `server/src/engine/standard.rs`, `server/src/rooms.rs`, `server/src/usage.rs`,
  `server/src/business/credits.rs`, `server/src/voip/{video,token}.rs` (signed-ticket patterns)
- `odd/tasks/live-connect.md`
