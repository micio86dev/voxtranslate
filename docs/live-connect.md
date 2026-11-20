# Live Connect

A host and up to three guests on one link, each with a phone and a microphone, every phone an
independent translation stream at the same time (a conversation is a pair by default).
Decisions and requirements: [`specs/0121-live-connect/spec.md`](../specs/0121-live-connect/spec.md).

> **Status (2026-11-20): built, live behind `LIVE_CONNECT_ENABLED`, not yet verified on real devices.** Backend (sessions,
> `/ws/live`, billing, desks, finalisation/search/retention/erasure, dashboard data, admin routes),
> the client pages, the dashboard area and the Directus setup are implemented and covered by the
> suites listed in *Testing strategy*. **No real-device run has happened**: two real phones,
> speaker/Bluetooth combinations, distances and noisy rooms are untested. Beyond the browser's own
> AEC and the client guard, a server-side **echo filter** (see *Echo filter* below) stops a phone's
> own voice, picked up by the partner's microphone, from being translated back to it; it was built
> after a real two-phone test and is verified against the mock engine only. Hardware support must
> not be claimed until [`live-connect-device-matrix.md`](live-connect-device-matrix.md) has results.
> The feature stays behind `LIVE_CONNECT_ENABLED` (off by default). This document is updated in the
> same change as the code it describes.

## How it fits together

```
phone A ──PCM16 24 kHz──▶ ┐                              ┌─▶ translated audio ──▶ phone B
phone B ──PCM16 24 kHz──▶ ┤   /ws/live                   ├─▶ translated audio ──▶ phone C
phone C ──PCM16 24 kHz──▶ ┤   one private room           ├─▶ translated audio ──▶ phone D
phone D ──PCM16 24 kHz──▶ ┤   live-<session uuid>        │   (each phone hears every OTHER
                          │                              │    speaker, in its own language)
                          └─▶ engine: one upstream session per speaker per target language
                              (Standard/Qwen by default; capacity fallback as in meetings)
```

No WebRTC. Each phone sends only its own microphone and receives the translation of **every
other** speaker, in its own language. A conversation is the host plus 1 to 3 guests
(`max_guests`, default 1); the room holds four peers. Rooms, engines, the usage meter, speech events and the org ledger are the existing
meeting machinery; Live Connect adds sessions, credentials and a payer who is not on the call.

## Session lifecycle

```mermaid
stateDiagram-v2
    [*] --> waiting: create (B2C) / scan (B2B desk)
    waiting --> active: first guest joins (or staff answers a desk)
    waiting --> expired: join window / desk wait elapsed
    active --> active: more guests join the same link, up to max_guests
    active --> ended: host ends, payer runs out, max duration, everybody gone past grace
    ended --> [*]
    expired --> [*]
```

- B2C: the owner is the `host`; the invited people are the `guest`s.
- B2B desk: the customer who scanned is the `guest`; the member of staff who answers is `staff`.
  A desk conversation is always a pair.
- **One host (or staff) and up to `max_guests` guests** (1-3, chosen at create, default 1). The owner
  side is still a database fact (a partial unique index: exactly one `host`/`staff`). The number of
  guests is enforced **in code** (`sessions::join_guest`), because a unique index cannot say "at
  most N": the session row is locked `FOR UPDATE` while the guests are counted and the new one is
  inserted, so N simultaneous joins on one link admit exactly `max_guests`.
- **The link is reusable** while the session is `waiting` or `active`: each guest who opens it takes
  the next seat, with their own participant row, token and consent. `LIVE_JOIN_WINDOW_SECS` applies
  to a session nobody has joined yet; once it has a guest it is `active` and the link stays good
  until the session ends or fills (`session_full`). A seat is never handed on: a guest who dropped
  reconnects with the token they were given.
- A guest hanging up (`POST …/end` with a guest token) ends a **two-person** conversation as before,
  but in a conversation that admits several guests it only means "I am leaving": the answer is
  `{"ended": false, "left": true}` and the others carry on. The host's end is the end for everyone;
  the sweeper still ends a conversation once the room has been empty for the reconnect grace.

## Credentials *(implemented: `server/src/live/tokens.rs`)*

| Credential | What it is | Stored | Lifetime |
|---|---|---|---|
| Invite code | 192 random bits in a B2C link/QR | SHA-256 only (`live_sessions.invite_hash`) | join window |
| Guest token | signed claim: session, participant, role | hash only (`live_participants.token_hash`) | session life, so a phone can reconnect |
| Desk QR token | `HMAC(desk key, desk id ‖ version)`, 128 bits, printed in the QR | **nothing**; only `live_desks.token_version` | until revoked/regenerated |

- All three derive from one master secret `LIVE_TOKEN_KEY` through per-purpose labels. A signature
  for one purpose is worthless for another.
- The account JWT verifier cannot accept a guest token and the guest verifier cannot accept an
  account JWT, even if `LIVE_TOKEN_KEY` equals `JWT_SECRET` (covered by a test).
- A desk QR encodes `https://<app>/lc/d/<public_id>.<mac>`. No organisation, project or account
  identifier is derivable from it, and it never denotes an existing conversation: **every scan
  creates a new session**.
- Rotating `LIVE_TOKEN_KEY` invalidates every printed desk QR and every guest token in flight.

## Data model *(implemented: migrations `065_live_connect.sql`, `069_live_multi_guest.sql`)*

| Table | Purpose |
|---|---|
| `live_desks` | permanent QR entry points: org, optional project, name/location, default language, public handle, `token_version`, status |
| `live_sessions` | lifecycle, ownership (user **or** org), project and desk binding, expiry, `save_transcript`, `max_guests` *(069, 1-3, default 1)*, link to `call_sessions` |
| `live_participants` | who is on the call (one host/staff and up to three guests); `peer_id` is the room peer id, server-minted. `idx_live_participants_guest_side` is a plain index since 069 (see below) |
| `live_usage_ticks` | one row per (session, peer, tick): the idempotency ledger for billing |
| `live_session_events` | bounded diagnostics (join, leave, reconnect, error, capacity, echo, exhausted) |
| `live_session_summaries` *(067)* | one AI summary per (conversation, language); the row is also the idempotency and concurrency guard of the paid call. Content: erased with the conversation |
| `live_desk_usage` *(view, 068)* / `live_desk_usage_daily` *(table, 068)* | credits and conversations per desk per UTC day, metadata only. The table is what Directus charts (it only introspects base tables); `refresh_live_desk_usage()` fills it from the view |

The conversation itself uses the existing `call_sessions` (`kind = 'live_connect'`),
`session_participants`, `transcript_events`, `transcripts` and `transcript_embeddings`.
`live_sessions.call_session_id` is `ON DELETE SET NULL`, so purging a conversation never erases the
billing record. All tables are RLS default-deny; tenancy is enforced in Rust like everywhere else.

**Migration 069 (multi-guest).** Adds `live_sessions.max_guests SMALLINT NOT NULL DEFAULT 1` with a
guarded `CHECK (max_guests BETWEEN 1 AND 3) NOT VALID`; existing rows keep 1, which is what they
were. It turns the guest-side **unique** index into a plain one **under the same name**: migration 065
says `CREATE UNIQUE INDEX IF NOT EXISTS idx_live_participants_guest_side`, and `IF NOT EXISTS`
matches by name, so re-running 065 (the schema suite does; a drifted database may) finds the name
taken and leaves it alone instead of rebuilding a unique index over a three-guest conversation and
failing. The owner-side unique index stays. Additive and idempotent; a build that predates it
simply keeps treating every session as a pair.

## Configuration *(implemented: `server/src/live/config.rs`)*

See the Live Connect block in `server/.env.example`. The feature is off unless
`LIVE_CONNECT_ENABLED` is truthy **and** `LIVE_TOKEN_KEY` (≥ 32 chars) is valid; a bad key is an
error, never a fallback. Two limits against abuse of the public surface are tunable:
`LIVE_DESK_MAX_WAITING_PER_IP` (default 2: waiting sessions one client address may hold at one
desk) and `LIVE_AUDIO_MAX_BYTES_PER_SEC` (default 72 000: the microphone budget of one phone, see
*`/ws/live`*). `LIVE_MAX_GUESTS` (default and ceiling 3, clamped to 1-3, junk keeps the default) is the
most guests a host may ask one link to admit; a room holds four peers, so it can only be lowered. The echo filter has two knobs, `LIVE_ECHO_FILTER` (default on) and `LIVE_ECHO_SIM`
(default 0.6, clamped to 0.3-0.95; junk falls back to the default instead of switching the feature
off), see *Echo filter* below.

## API

| Route | Who | Status |
|---|---|---|
| `POST /api/live/sessions` body `{lang, save_transcript?, org_id?, project_id?, max_guests?}` | signed-in owner (B2C), or org member with `org_id` + `project_id` (B2B) | implemented |
| `POST /api/live/join` body `{code, name, lang, consent}` | anyone with an invite code (no account); the same code admits guests one after the other | implemented |
| `GET /api/live/sessions/{id}`, `POST …/end` | a participant, with its Live Connect token (`Authorization: Bearer`) | implemented |
| `GET /ws/live?token=…` | a participant | implemented |
| `DELETE /api/live/sessions/{id}` | the signed-in **owner** of a consumer conversation (account JWT); 404 for anyone else, 409 while open | implemented |
| `DELETE /api/business/organizations/{org}/live/sessions/{id}` | org **admin/owner**; 404 non-member or another org's session, 403 plain member, 409 while open; audited as `live_session.delete` | implemented |
| `GET /api/business/organizations/{org}/search?q=…` | org member, scoped as for meetings; Live Connect hits carry `session_kind` and `live_session_id` | implemented (extended, see below) |
| `POST /api/live/desks/scan` | anyone with a desk QR | implemented |
| `GET /api/live/desks/{id}/waiting`, `POST /api/live/sessions/{id}/answer`, `GET /api/live/my-desks` | org members | implemented |
| `POST/GET …/organizations/{org}/live/desks`, `PATCH …/{desk}`, `GET …/{desk}/qr`, `POST …/{desk}/revoke|regenerate` | org admin (list: member) | implemented |
| `GET …/organizations/{org}/live/sessions`, `GET …/live/sessions/{id}`, `GET …/live/usage` | admin sees all, member only their own (usage: admin) | implemented |
| `DELETE …/organizations/{org}/live/sessions/{id}`, `DELETE /api/live/sessions/{id}` | org admin / consumer owner | implemented |
| `POST …/organizations/{org}/live/sessions/{id}/summary` body `{lang?}`, `GET …/summary[?lang=]` | whoever may read the conversation (admin: all; member: only one they sat in); anything else 404 like a missing conversation | implemented, see *AI summary* |
| `POST /api/admin/live/{desk-revoke,desk-regenerate,session-end,session-reconcile,session-transcript}` | platform operators, through Directus Flows (shared `ADMIN_API_SECRET`, like every `/api/admin/*` route) | implemented, see *Administration (Directus)* |

Errors are `{"code","message"}` with a stable code: `invite_not_found`, `invite_expired`,
`session_full`, `session_ended`, `consent_required`, `bad_lang`, `bad_max_guests`, `too_many_sessions`,
`insufficient_balance`, `insufficient_credits`, `subscription_required`, `project_required`,
`project_not_found`, `live_busy`, `rate_limited`; summaries add `session_open`, `nothing_to_summarize`
(both 409), `summary_unavailable` (502), `summary_in_progress` (409), `summary_not_found` (404).

### Rules the code enforces

- Creating needs an account that can pay. A business session also needs a project of **that** org
  (a foreign or archived project answers like a missing one), an active subscription and credit.
  `max_guests` is optional (omitted or `null` = 1, a pair) and must be a whole number from 1 to
  `LIVE_MAX_GUESTS`; anything else is `400 bad_max_guests` and creates nothing. It is stored, and
  returned by create (`max_guests`), by join and by `GET …/sessions/{id}` (`session.max_guests`).
- Joining needs consent, a real language (never `auto`) and a well-formed code; a malformed and an
  unknown code answer identically. Each guest takes a seat of their own; once `max_guests` are taken
  the next one is refused (`409 session_full`, and it takes nothing) and a guest who dropped
  reconnects with their token.
- An owner holds at most `LIVE_MAX_CONCURRENT_PER_OWNER` open sessions; creates are serialised with
  an advisory lock so a burst cannot beat the cap.
- `live_busy` (503) is returned at create when the default engine has fewer than two free upstream
  sessions, and at join when it cannot open what the new guest's arrival needs: Standard never
  reports `AtCapacity`, it silently skips a direction. Every participant opens one upstream session
  per distinct OTHER language in the room, so a guest adds their own streams plus one for their
  language to every speaker already there (host `it`, guest `es`, new guest `en`: 6 sessions where
  there were 2, i.e. 4 more; a guest in a language already present adds only their own). A join
  needs at least the two a pair has always needed and the engine's free sessions to cover that;
  otherwise it is refused `live_busy` **inside the same transaction** that would have seated the
  guest, so nobody is seated into a direction that would stay silent. A snapshot, not a
  reservation.

### `/ws/live`

- The token is checked **before** the upgrade (bad, expired, ended-session → 401/410, no socket).
- Identity comes from the database (peer id, name, language), never from the client. The room is
  `live-<session uuid>`, private, and unguessable.
- Client → server: `start`, `stop`, `mute_audio{muted}` and binary PCM16 mono 24 kHz frames.
  Mute is **enforced server-side**: a muted phone sends nothing to the engine.
- A room of up to four phones: each phone is told who is already there (`room_joined.peers`), who
  arrives (`peer_joined`), who leaves (`peer_left`) and who mutes (`peer_muted`, per peer). A
  reconnect with the same token replaces the stale socket (no `peer_left`). Captions and
  `translated_audio` carry the `speaker_id` of whoever spoke and arrive in the listener's language;
  two phones that share a language hear each other natively (nothing is translated, nothing billed).
- Server → client: the existing meeting frames (`room_joined`, `peer_joined`, `peer_left`,
  `subtitle_*`, `translated_audio`, `peer_muted`, `balance_*`, `capture_format`) plus
  `session_state{status, reason}` before the socket closes, and `error{code}`:
  `insufficient_balance`, `insufficient_credits`, `live_busy`, `engine_unavailable`,
  `audio_session_closed`, `billing_unavailable`.
- Hardening copied from `/ws`: connection cap, per-IP connect limit, frame and message-rate caps,
  ping every 15 s, close after 45 s of silence, slow-consumer drop. A momentarily slow engine drops
  a 100 ms chunk instead of ending the session.
- **The engine is fed at most what a microphone produces.** Billing is by the wall clock, so audio
  pushed faster than it plays would be translated (and billed upstream) for less than it costs the
  payer. A binary frame over 16 KiB (`MAX_AUDIO_FRAME_BYTES`; an honest one is 4 800 bytes, 100 ms of
  PCM16 mono 24 kHz) is never forwarded and the sender gets `error{code:"frame_too_large"}`. Each
  phone also has a byte budget of `LIVE_AUDIO_MAX_BYTES_PER_SEC` (72 000 B/s, 1.5x real time, with
  one second of burst): audio beyond it is **dropped, not queued**. A real client (one frame per
  100 ms, even after a network stall delivers a second at once) never touches it; the tests send
  one. Drops are reported at most once a second as a `diagnostics` event
  (`{audio_dropped:{frames,bytes,oversized}}`, counts only) and at most 10 times per socket. The
  budget assumes the 24 kHz capture the client uses; a client capturing at 48 kHz would be throttled.
- Each phone re-checks the session every 3 s, so an end by the host, the sweeper, an admin or
  the hard ceiling reaches it with the reason.
- Persistence follows policy: a business session always has its `call_sessions` row
  (`kind='live_connect'`, org and project set); a consumer one only when the owner opted in to saving.
  A room's session id is minted when the room is created, so a conversation that survives both phones
  dropping spans more than one; each is recorded as a `room_session` event.

### Several guests: the client *(implemented: `client/src/scripts/live/*`)*

- **Host setup** has one extra control, "Guests" (1, 2 or 3; default 1), sent as `max_guests` and
  stored with the session (`StoredSession.maxGuests`; a reload takes the size and the seats already
  taken from `GET /api/live/sessions/{id}`). A guest stores the `max_guests` the join returned.
- **Session machine**: `LiveContext.partners` replaces the single partner. `waiting_peer` until the
  first partner, `live` while at least one is connected, back to `waiting_peer` (not `ended`) when
  the last one leaves. `room_joined.peers` replaces the list (and keeps the mute flags already
  known), so a reconnect needs no `peer_left`. A pair still ends because the server sends
  `session_state`, not because the client guesses.
- **Captions** are one block per other speaker (`speaker_id`), the most recently active first, at
  most four; a speaker's block goes when they leave and their last sentence moves to the history.
  The playback sequence guard stays per `speaker_id`. Names show once more than one guest is possible.
- **Invitation (host)**: "N of M guests joined" always; the QR and link stay open while nobody is
  connected, fold behind a toggle while a guest is, and disappear when every seat is taken. A seat
  stays taken when its guest drops, so the count comes from every peer ever seen, not who is online.
- **Leaving (guest)**: in a conversation that admits several guests, hanging up shows "Leave" and the
  "you left" screen (`left_by_me`) and the others carry on; the server's `{ended:false,left:true}` is
  honoured even for a session stored before this existed.
- **Join errors**: `session_full` says the conversation is full; `live_busy` is a calm "try again"
  that keeps the form and the typed name.

### Sweeper

Every 15 s: expire invitations nobody used, close sessions past the ceiling, and end an **active**
session whose room has been empty for the whole reconnect grace (`abandoned`). It reads presence from
this process's room registry, which is correct because the server is a single replica. Each close
hands the conversation to finalisation. Once an hour it also applies retention and finalises
stragglers (see above).

## Conversation intelligence: finalise, then search *(implemented: `server/src/live/finalize.rs`)*

While a conversation runs, speech is stored by the meeting machinery as `transcript_events`
(original and translations in separate columns). Search, insights and the dashboards work off a
`transcripts` row plus `transcript_embeddings`, so when a **business** conversation ends it is
*finalised* into the same record a recorded meeting or a voice message has:

- **One `transcripts` row** on the conversation's latest room session: segments (speaker id and
  name exactly as the room used them, the **original** text, start relative to the first
  utterance, end estimated at ~400 ms per word, because events record when something was captured
  and not for how long), source language (most spoken), session duration (start to end of the
  conversation), word count. `translations` stays `{}`: per-utterance translations remain in
  `transcript_events`, where the live reader resolves them per viewer.
- **All room sessions** of the conversation are gathered (`live_sessions.call_session_id` plus the
  `room_session` events), so a conversation that survived both phones dropping is one transcript.
- **Embeddings** through the existing embedding provider, one speaker at a time: the meeting
  chunker labels a chunk with whoever spoke first, which for short alternating turns would put both
  people's words under one name. If the provider is down or unset the transcript is still saved and
  `/internal/embeddings/backfill` indexes it later.
- A **consumer** conversation gets no `transcripts` row (the table is organisation-owned). An
  opted-in transcript stays in `transcript_events`, readable by its owner.
- Idempotent and safe concurrently: the latest room row is locked and `transcript_status` is the
  guard (`none`/`failed` → `ready`; `expired`, i.e. removed by retention, is never resurrected).

**Triggers.** Every way a conversation ends calls one helper, `finalize::on_session_ended`, which
finalises after `LIVE_FINALIZE_GRACE_SECS` (5 s, so the engines' last finals are in): REST end, the
socket's balance-exhausted and hard-ceiling paths, the sweeper's abandoned/expired/max-duration
closes. The timer is in memory, so an hourly pass (`finalize_pending`) also finalises ended
conversations that have speech but no transcript, which is what a restart between "ended" and
"finalised" leaves behind.

**Search filters** (`GET …/search`, all optional, validated before anything is embedded; a search
without them is unchanged): `kind` (`call|voice_message|phone|live_connect`), `lang`, `from` / `to`
(RFC 3339, inclusive, on when the session started), `session_id` (a call-session id or a Live
Connect session id), `speaker` (case-insensitive substring of the name, `%`/`_` are literal), plus
the existing `project_id`. `lang` matches the transcript's source language, and for a Live Connect
conversation either participant's language (a conversation has two). Scope is as for meetings:
admins see the org, members only projects they created or took part in. A Live Connect conversation
is narrower, as in the dashboard: a member finds it only if they **sat in it** (host, or staff who
answered it), not merely because it was held under a project of theirs. `from` and `to` are
timestamps like `2026-11-19T10:00:00+01:00`: a client must percent-encode the `+` of an offset
(`%2B`), or the server reads it as a space and refuses the value. A Live Connect hit keeps its existing fields and adds `session_kind:"live_connect"`
and `live_session_id`; hits of every other kind are byte-for-byte what they were.

**Insights.** `POST …/insights` retrieves from `transcript_embeddings` without a kind filter, so a
finalised Live Connect conversation already grounds project and question answers. Its *activity
metrics* (calls, minutes, collaborators) are deliberately `kind = 'call'` only, like analytics, and
Live Connect conversations are not counted there; adding them is a product decision, not a bug.

## Billing *(implemented: `server/src/live/billing.rs`)*

The owner pays for **every seat**: a user's personal balance (B2C) or the org wallet (B2B). Guests are
never charged, need no credit, and never see the owner's balance (balance frames go to the owner's
socket only). There is no new plan, tier or wallet: the meter reuses `billing::deduct_usage_tx`
and `business::credits::deduct_org_credits_tx`. The backend is the only authority; nothing the client
reports is trusted.

- **The host watches the spend live.** After every charged tick the owner's socket (and only it) gets
  `balance_update {balance}` followed by `session_spent {spent}`, the sum of that conversation's
  user-paid ticks (`live::billing::session_spent`). The host screen shows
  `Credits left: $X.XXXX · −$Y.YYYY` to **four decimals**, because a tick costs a fraction of a cent
  and two decimals would look frozen. The cached balance is painted at once, before the first tick.
  Organisation-paid sessions show no wallet figure (whole credits are not a wallet amount).

- **One meter per phone**, started with that phone's `start` and cancelled by `stop`, mute, or the
  socket closing. Each tick covers `LiveMeter.interval_secs` (`USAGE_UPDATE_INTERVAL`, default 5 s).
- **Per DISTINCT target language, as for webinars.** A phone's tick bills one stream for every
  distinct language present in the room other than its own speaker's, at the session tier's rate,
  and nothing when everyone shares the speaker's language. The rule is Live Connect's own and does
  **not** read the engine's `cost_scales_per_language` (the upstream cost of a seat is per speaker
  per target language whatever the tier's metadata says). Host `it` with guests `es`, `es`, `en`:
  the host's tick is 2 streams (es, en), a Spanish guest's 2 (it, en), the English guest's 2
  (it, es). Two guests in the same language never double a cost (host `it`, guests `es`, `es`,
  `es`: every seat 1 stream); four languages cost 3 streams per seat. The idempotency key
  `(session, peer, tick_seq)`, the opening charge and the per-seat org carry are unchanged: they
  were already per peer.
- **A charge and its `live_usage_ticks` row commit together**, keyed `(session, peer, tick_seq)` with
  `ON CONFLICT DO NOTHING`. The tick number comes from the wall clock, so two meters covering the same
  interval (a phone that reconnects before its old socket is torn down) agree and only one charge
  lands. The failure mode is under-charging, never double-charging.
- **Opening charge.** A `start` is charged immediately, with a key of its own, because the engine
  session is already open; otherwise a start/stop shorter than one interval would be free.
- **What is billed is audio that reached the engine**, counted per interval, not the mute flag: a
  phone cannot mute around each tick and translate for free. An interval with no forwarded audio
  after the opening charge costs nothing; nothing is charged while everyone shares the speaker's
  language.
- **Units.** A consumer pays dollars (`credit_transactions` plus the owner's `usage_sessions` row). An
  org pays whole credits (1 credit = 1 cent) into `organization_credits_transactions` of type
  `live_connect`, with sub-cent amounts carried to the next tick (`CreditAccumulator`). The carry is
  kept **per seat for the lifetime of the session** (`LiveState::take_owed`, keyed by session and
  peer), not per Start: ten four-second Start/Stop cycles that are each worth 0.4 credit cost the
  four credits they add up to, not nothing. A running total under one credit when the session ends
  (less than one cent per seat) is not charged, exactly as for one uninterrupted run. Two meters of
  one seat covering the same interval add its cost once. The carry is in memory (the server is
  single-replica, see *Operations*); a restart forgets less than a credit per open seat.
- **Insufficient credit.** When a tick cannot be covered nothing is debited, every phone gets
  `balance_exhausted` and then `session_state{ended, balance_exhausted}`, and the session ends
  cleanly. A start is refused up front (`insufficient_balance` / `insufficient_credits`) when the
  payer cannot cover it. Six consecutive failed charges end the conversation (`billing_error`)
  rather than translating for free.
- **Concurrency.** Debits take the same row locks as the rest of the wallet code; the org gate is
  checked at create and again at every start (`business::ws_gate`). `tests/live_billing.rs` covers
  the idempotent tick, the exhausted payer and concurrent sessions on one wallet.
- **Reconciliation** of metering against the ledger is a read-only operator tool, see
  *Administration (Directus)*.

## AI summary *(implemented: `server/src/live/summary.rs`, migration `067`)*

S12 asks for "AI-generated summaries" and "relevant conversation insights". Cross-conversation
insights already exist (the org assistant, `business/insights.rs`, which also finds Live Connect
transcripts). This adds the missing per-conversation piece, for business conversations.

- `POST …/organizations/{org}/live/sessions/{id}/summary` with `{"lang"?}` writes it, `GET …/summary[?lang=]`
  reads what was stored (404 `summary_not_found`, 202 while one is being written; without `lang`, the
  most recent). Visibility is exactly that of the conversation detail, so a member who did not sit in
  the conversation, another organisation's admin and a stranger all get the same 404 as for a
  conversation that does not exist. Without `lang` the summary is written in the caller's language in
  that conversation, else the staff side's.
- **Input.** The original utterances in order, with speaker name and language, never the translations.
  At most 30 000 characters go to the model: a longer conversation loses its **middle** (a quarter of
  the budget keeps the start, the rest the end, one line says how many utterances were left out), never
  its end, where what was agreed usually is. The transcript is data: the prompt tells the model to
  ignore any instruction inside it. The model is the insights assistant's (`GROQ_REPORT_MODEL`, then
  `GROQ_FALLBACK_MODEL`); there is no second AI client.
- **Output.** Markdown with four sections: key points, questions asked by the customer, requests and
  promises, follow-ups still open, written in the requested language.
- **Billing.** The organisation wallet, at the flat price of an insight (`credits::insight_credits`, 3
  credits), as ledger type `live_summary` attributed to the conversation's `call_sessions` row while it
  exists. Charged **before** the model is called and refunded (a second ledger row) when the provider
  fails: `502 summary_unavailable`. An organisation that cannot pay gets `402 insufficient_credits`
  (`balance`, `required`) and nothing is stored. A conversation with nothing said is
  `409 nothing_to_summarize`, one still open `409 session_open`; neither charges.
- **Once per (conversation, language).** A request claims the pair by inserting a `pending` row in
  `live_session_summaries` and charging in the same transaction; the primary key, not a lock in the
  process, picks the one writer, so concurrent requests and replicas charge once. Every other request
  waits for it and returns the stored summary with `charged: 0`; a repeat after that is also free (even
  for an organisation that has since run out of credit). The model call runs in its own task, so a
  client that hangs up cannot strand a charge, and a claim abandoned by a crashed process is taken over
  after ten minutes without charging again.
- **Privacy.** The summary is content: `erase::erase_content` deletes it with the conversation (an
  admin deleting it, retention, account deletion), pending claims included, while the ledger rows stay.
  Retention also finds a conversation whose only remaining content is a summary. The transcript is sent
  to the model and nowhere else: never logged (a provider error is logged by kind only, because a 4xx
  body can quote the request) and never in the audit entry (`live.summary`: language, credits, number of
  utterances, number omitted).
- A plain member who sat in a conversation can ask for its summary, and the organisation pays: that is
  the same rule as reading the conversation, not a new permission.

## Privacy, retention and erasure *(implemented: `server/src/live/erase.rs`)*

- Raw audio is never stored.
- B2C conversations store nothing unless the owner opts in.
- B2B conversations are stored under the org's retention and the guest is told before the
  microphone opens.

| | B2C (owner pays) | B2B (desk, org pays) |
|---|---|---|
| Audio | never stored; streamed to the translation provider and discarded | same |
| Transcript text (original and translation, kept apart) | only if the owner ticked "save transcript" (`save_transcript`), kept `LIVE_B2C_RETENTION_DAYS` (30) after the end, `0` = until the owner deletes it | always, under the org's `retention_days` (else `LIVE_RETENTION_DEFAULT_DAYS`, 90) |
| Guest's display name | in `live_participants` while the conversation exists; scrubbed to `Erased` on erasure | same |
| Billing facts (`live_sessions`, `live_usage_ticks`, ledger) | kept; hold no speech | kept; hold no speech |
| Diagnostics (`live_session_events`) | bounded, no speech, no token material | same |
| Who can read the content | the owner (opted in) | org admins; a member only within the existing meeting scope (projects they created or took part in, and desks they answered); platform operators only through the audited, reasoned Directus transcript read |
| Guest notice before the microphone opens | `liveJoinNotice`: "Your voice is sent to be translated in real time. Audio is never recorded. The person who invited you can choose to keep a text transcript." plus a required consent box | `liveDeskNotice`: "...Audio is not recorded, but the business keeps a text transcript of this conversation." plus a required consent box |
| Provider | the Standard engine sends the voice to Alibaba Model Studio (Singapore region by default); this is an international transfer the privacy notice must name | same |

The notice wording is product/legal copy: it needs sign-off before launch, and the translations
flagged low-confidence in `odd/tasks/live-connect.md` (L8) need a native read.

A conversation is **content** (what was said, who said it, the guest's name) and **billing facts**
(`live_sessions`, `live_usage_ticks`, ledger entries). Erasing removes the first and keeps the
second. `erase::erase_content(pool, live_session_id)` is the one function behind every reason to
erase, and runs in one transaction:

1. deletes every `call_sessions` row of the conversation (its latest room and the earlier ones named
   by `room_session` events; only rows of `kind='live_connect'` whose room is `live-<this id>`, so
   a corrupted event can never reach another conversation), which cascades to
   `session_participants`, `transcript_events`, `transcripts`, `transcript_embeddings` and the other
   per-session tables;
2. scrubs what the cascade cannot reach: `live_participants.display_name` becomes `Erased` and
   `token_hash` is cleared (the guest's name is personal data).

Kept: `live_sessions` (status, timing, ownership, project), `live_usage_ticks`, the org ledger
(`organization_credits_transactions.session_id` becomes NULL through its `ON DELETE SET NULL`),
diagnostics, and the participants' user ids.

| Reason | Who / when |
|---|---|
| Delete request | org admin/owner (`DELETE …/organizations/{org}/live/sessions/{id}`, audited) or the consumer owner (`DELETE /api/live/sessions/{id}`). An open conversation answers 409: end it first. |
| Retention (business) | the org's `settings.retention_days` after the conversation **ended**; `0` = the org keeps everything. An org with no (or a non-numeric) setting gets `LIVE_RETENTION_DEFAULT_DAYS` (90); `0` there means no default. |
| Retention (consumer) | opted-in transcripts after `LIVE_B2C_RETENTION_DAYS` (30; `0` keeps them until the owner deletes them). |
| Account deletion | `SafetyService::delete_user` erases every conversation the user **owns** before the user row goes. Without this the guest's name and words survived the owner, because the foreign key points from `live_sessions` to `call_sessions` and not back (defect D4). |

Retention runs from the sweeper, at most once an hour (tracked in `LiveState`), 100 conversations a
pass, oldest first; a conversation already erased is not selected again. It runs only while Live
Connect is enabled. The enterprise recording retention (`business/retention.rs`) is unchanged; for a
Live Connect conversation of an enterprise org it removes the `transcripts` row at the same window
and leaves the events, which this pass then erases.

## Operations

- Rooms are in memory and the server runs as a single replica: a deploy drops live sessions;
  phones reconnect within `LIVE_RECONNECT_GRACE_SECS`. The sweeper reads presence from this
  process, so a second replica would end conversations whose phones are connected to the other one.
- Capacity: the Standard engine is capped at 32 upstream sessions per process
  (`QWEN_REALTIME_MAX_SESSIONS`), and a pair opens two (one per speaking direction), so about 16
  simultaneous pairs; a bigger conversation needs one per speaker per distinct other language (host
  `it` with guests `es`, `es`, `en` is up to 8 while all four talk, four languages 12). Standard
  never reports `AtCapacity`, it silently skips a direction, so the gate is `engine_has_room` at
  create (two free) and the same check inside the join transaction against what the new guest adds
  (`live_busy`, 503). It cannot see sessions that
  have been created but are not speaking yet, so a burst past ~16 pairs shows up as phones that
  never hear a direction, not as `live_busy`. `loadtest/live-connect.js` counts both.
- Metrics (`/metrics`, unauthenticated aggregate, no ids): `voxtranslate_live_translation_ms`,
  `voxtranslate_live_echo_suppressed_total`, `voxtranslate_live_echo_failopen_total`.

### Latency measurement *(implemented: `live/ws.rs`, `live/state.rs`, `metrics.rs`)*

Spec S18 asks for speech-to-translated-audio latency. The server records one histogram,
`voxtranslate_live_translation_ms` (buckets 5 ms to 10 s, no labels, no session/peer/user ids): from
**the first voiced frame of an utterance forwarded to the engine** to **the first `translated_audio`
frame of that utterance handed to the other phone's socket**. One observation per utterance, however
many audio chunks answer it.

- *Utterance onset.* The client streams its microphone continuously while unmuted, so there are no
  gaps in the frames to detect. A frame is "voiced" when its mean absolute sample is at least 80 of
  32 768 (about -44 dBFS); an utterance starts at the first voiced frame after a `start`/unmute or
  after one second without one. One pass over a 4.8 kB frame, no allocation.
- *Probe.* A per-session set of atomics (`SessionLatency`), one slot per **seat**, four at most: each
  socket claims a slot for its peer id when it connects (the same one again on a reconnect). The
  speaker's socket stamps its slot at onset; whichever listener's writer first sends a
  `translated_audio` of that speaker (the frame names its `speaker_id`) takes the stamp and records
  the difference, so one utterance is one observation however many phones hear it. No locks on the
  hot path. A stamp older than 30 s is dropped, and a newer
  onset overwrites an older one whose audio never came (same-language speech is not translated).
- *What it excludes.* Everything on the phones: capture buffering, the network to and from the
  server, playback start. It also includes the provider's own latency, so with the mock upstream it
  is near zero. The phone-side legs, audio interruptions, duplicate translations, echo handling and
  reconnection behaviour are only measurable on devices: `live-connect-device-matrix.md`.
- *Reading it.* Prometheus: `histogram_quantile(0.95, sum(rate(voxtranslate_live_translation_ms_bucket[5m])) by (le))`.
  Compare against `voxtranslate_qwen_ttfa_ms` (the provider's own time to first audio): the gap is
  room fan-out and the socket writer.

### Echo filter *(implemented: `live/echo.rs`, used by `pump_live` in `live/ws.rs`)*

**Symptom.** Two phones side by side, owner Italian and guest Spanish. The owner's voice also reaches
the guest's microphone, so the guest's seat opens a session (es to it) that "translates" Italian
speech it takes for Spanish, and delivers captions and `translated_audio` of it to the owner, who
hears their own sentence back.

**What the server does.** Every frame destined to a phone passes that seat's `EchoFilter` before the
socket. It is a pure state machine (no I/O, no clock of its own), written for N seats (a host and up
to three guests) although a conversation has two today:

1. *Ledger of everyone's words.* The recent ORIGINAL words of every speaker this seat has seen,
   each entry tagged with its speaker: my own captions (`speaker_id` = this seat), and another
   speaker's words once their utterance was judged real. The last 8 per speaker within 15 s (an
   interim updates the open entry, a final closes it).
2. *Classify each other speaker's utterance*, state kept per speaker (a map bounded to 8; an
   utterance starts at the first frame after a `subtitle_final`, or after 1.5 s of silence from
   that speaker). It is an echo when its **`original`** (that speaker's ASR words, never `text`,
   which is a translation into this phone's language and can share words with a genuine reply) is
   similar to a recent entry whose speaker is **not** that speaker: word containment with
   edit-distance tolerance ("ciao come stai" ~ "chao como estai"), or character-trigram Dice,
   whichever is higher, against each entry and the last two of one speaker joined; at least
   `LIVE_ECHO_SIM`; and at least two words, or one token of 12+ characters for scripts written without spaces (a lone word such as "si"/"ok"/"gracias" is never
   suppressed). That covers my own words coming back and, with three or more seats, guest B's
   microphone hearing guest A and B's session delivering A's words to the host as if B had said them.
   With no `original` on a cross-language frame the utterance is not judged; when the speaker's
   language is mine, `text` is the original and is used.
3. *No added latency normally.* If nobody else spoke for 12 s (no entry of another speaker, no
   voiced microphone audio of mine), nothing is held. Otherwise a new utterance is held, audio and
   captions, for at most 700 ms while its first `original` is awaited (each speaker has its own
   hold and timer); real is released in order, echo is dropped whole (audio, interims, final and the
   audio that trails the final), and a timeout **fails open**. The buffer is capped at 64 frames per
   speaker. The pump has a timer branch for the timeout.

**The symmetry trap, and the loudness rule.** Two phones hear the same sentence, so each seat sees
a similar pair: the owner's seat sees (own caption, the guest's original) and the guest's seat sees
the mirror image, because the guest's "own caption" is the engine's reading of the owner's voice.
Judged on text alone both filters would drop the other's frames and the conversation would lose the
real translation as well. So an utterance is only an echo of Y's words when **Y's microphone is
clearly louder than the speaker's own** (smoothed mean absolute sample of the audio each socket
feeds the engine, ratio above 5/4; equal or unknown levels never suppress). Of any two seats at most
one can be the louder. It also means a seat that is talking holds the other's new utterance until
its own caption has arrived (still at most 700 ms), because the echo can be heard before this
seat's caption. The pump answers the filter's level question **per peer**: every speaker id on a frame
is a peer id, each peer's socket holds a seat in the conversation's `SessionLatency` (up to four,
each with its own smoothed microphone level and last-voiced time), and a speaker whose phone has not
connected reads as 0, which is "unknown" and never suppresses. So with a host and two guests side
by side, guest 2 is not handed guest 1's reading of the host's voice either (the host's microphone is
the louder one), which a two-microphone lookup could not tell.

**What is verified, and what is not.** Verified: the similarity rules and the state machine (unit
tests in `echo.rs`), the config knobs, the metric, the diagnostics event, and the whole path through
the real socket and the mock engine (`tests/live_ws.rs`: the echo never reaches the speaker, the
other phone still gets the translation, a real reply still gets through, kill switch; and with a
host and two guests the echo reaches neither the host nor the other guest); the other 3-seat
cases (two simultaneous real speakers, a reply sharing a few words with someone else) are
unit-level. **Not
verified: anything acoustic or about the real provider.** In particular (a) what Qwen's ASR returns
for Italian forced as Spanish, how close it is to the real words, and whether it returns
`original` early enough, (b) the relative loudness of two real phones (browser auto-gain and noise
suppression can flatten it, in which case the filter fails safe and suppresses nothing), (c) real
latency of the hold on genuine replies, which grows when speakers alternate quickly (a new
speaker after somebody else spoke is held until their first `original`, at most 700 ms), and (d) a
third speaker's echo whose real source's words have not reached the ledger yet leaks (only my own
caption is awaited). Test with two real devices.

**Tuning and first signals.**

- `LIVE_ECHO_FILTER=0` switches it off with no deploy of code (a restart of the service).
- `LIVE_ECHO_SIM` (0.3-0.95, default 0.6). Echo still audible: lower it (0.5). Real replies being
  swallowed: raise it (0.7).
- `voxtranslate_live_echo_suppressed_total`: utterances dropped. Should rise when the scene above
  happens, and stay near zero for two phones in different rooms.
- `voxtranslate_live_echo_failopen_total`: utterances the filter could not judge in 700 ms. A high
  ratio to suppressed means it is mostly blind (no `original` in time, or never louder): echo will
  persist and this is the first thing to look at.
- One `diagnostics` event per conversation per 10 s at most, `{"echo_filter": {"suppressed": n,
  "failed_open": n}}`. Counts only; no transcript text is ever logged.

**What it does not change.** Persistence and billing are upstream of the pump and untouched. The
echo utterance is still produced by the guest's engine session, so it is still metered and still
recorded in `transcript_events` as speech of the guest (the filter only stops its *delivery*).

### Troubleshooting

Start from the conversation's diagnostics: `SELECT ts, kind, detail FROM live_session_events WHERE
session_id = '<id>' ORDER BY ts` (kinds: `created`, `guest_joined`, `desk_scan`, `answered`,
`connected`, `disconnected`, `room_session`, `idle_timeout`, `slow_consumer`, `ended`,
`ended_by_billing`, `abandoned`, `admin_ended`, `content_erased`, `diagnostics`). Directus shows the same rows;
the dashboard session detail shows the timeline to org admins. For billing, the Directus
**Reconcile** button (`POST /api/admin/live/session-reconcile`) compares ticks with the ledger.

| Symptom | Check |
|---|---|
| Feature unreachable / `/live` says unavailable | `LIVE_CONNECT_ENABLED` truthy and `LIVE_TOKEN_KEY` ≥ 32 chars; the server logs why it stayed off (`live connect misconfigured`). Billing must also be configured (`DATABASE_URL`, `GOOGLE_CLIENT_ID`, `JWT_SECRET`) |
| Every printed desk QR stopped working | was `LIVE_TOKEN_KEY` rotated? was the desk regenerated or revoked? (`live_desks.status`, `token_version`) |
| Guest gets `invite_expired` | the invitation outlived `LIVE_JOIN_WINDOW_SECS` before anyone joined; create a new one |
| Guest gets `session_full` | the link's `max_guests` seats are all taken (`SELECT count(*) FROM live_participants WHERE session_id = '<id>' AND role = 'guest'` against `live_sessions.max_guests`); a guest who dropped reconnects with their token, anyone else needs the host to create a new conversation with more seats (1-3, `LIVE_MAX_GUESTS`) |
| Host's `max_guests` is refused `bad_max_guests` | not a whole number from 1 to `LIVE_MAX_GUESTS` (default 3) |
| Create/join answers `live_busy` (503) | create: fewer than two free upstream sessions; join: fewer than the new guest's arrival needs (each guest adds their own streams plus their language to every speaker). Check `QWEN_REALTIME_MAX_SESSIONS`, the Qwen connect metrics and how many conversations are active |
| One direction is silent | the engine skipped it at capacity, or its upstream session dropped (`audio_session_closed` / `engine_unavailable` frames, retried by the client a few times); check `voxtranslate_qwen_connect_ms` and the server log for `qwen connect failed` |
| `insufficient_balance` / `insufficient_credits` | the **owner's** balance or the org wallet, not the guest's: top up, or check the org's subscription |
| Conversation ended on its own | `live_sessions.end_reason`: `balance_exhausted`, `max_duration`, `abandoned` (both phones gone past the grace), `expired`, `desk_revoked`/`desk_regenerated`, `ended_by_admin`, or an `ended_by_*` |
| The owner hears their own sentence translated back | echo filter: `LIVE_ECHO_FILTER` on? `voxtranslate_live_echo_suppressed_total` vs `..._failopen_total`, then `LIVE_ECHO_SIM` (see *Echo filter*) |
| One phone's audio is being dropped | `live_session_events` kind `diagnostics` with `audio_dropped{frames,bytes,oversized}`: the client sent audio faster than real time or frames over 16 KiB (a client capturing at 48 kHz instead of 24 kHz would do this; check `LIVE_AUDIO_MAX_BYTES_PER_SEC`) |
| A phone keeps reconnecting | `slow_consumer` (its outbound queue overflowed: a bad network) or `idle_timeout` (no frame for 45 s); it resumes with the same token inside the grace |
| A finished conversation is not in search | `transcripts` row present? (`transcript_status = 'ready'` on its latest `call_sessions` row.) No embedding provider or a provider error leaves a transcript without chunks: run `/internal/embeddings/backfill`. A conversation with no speech is never finalised |
| Guest words survive an erase | only rows named by the conversation's `room_session` events and pointer are deleted, and only of `kind='live_connect'` in room `live-<id>`; check `live_session_events` for `content_erased` |
| Retention stopped | it runs from the sweeper, which runs only while `LIVE_CONNECT_ENABLED` is truthy |
| A seat is charged more (or less) than expected | each seat bills one stream per distinct language in the room other than its speaker's: `SELECT peer_id, amount FROM live_usage_ticks WHERE session_id = '<id>'`; two guests in one language are one stream for everybody else |
| Billing mismatch | Reconcile: `missing_ticks` (a tick without a debit), `duplicate_ticks` (a debit no tick accounts for), `unlinked` (the meeting rows were erased, nothing can be claimed) |
| DB-gated tests "pass" in milliseconds | they skipped: the database is missing or lacks pgvector — use `docker compose up -d --wait postgres` and `voxtranslate_voip_test` |

## Administration (Directus) *(implemented: `server/src/live/admin.rs`, `directus/setup-live-connect.mjs`)*

Directus is for platform superadmins and a few authorised operators (S15). It is a thin UI over the data: it **never writes** the live tables, every action is a Flow button that calls the server, and the server audits it. Customer-facing workflows (desks, QR download, session history, usage) stay in the B2B dashboard. Directus and the dashboard do **not** share an authorization model: Directus authenticates operators with its own roles, the server trusts it only through the shared secret.

### Server routes

All five are `POST`, sit beside the other `/api/admin/*` routes, and use the same `AdminAuth` extractor: `X-Admin-Secret` or `Authorization: Bearer`, constant-time, checked **before** the body is parsed (a wrong secret is `403`, as everywhere under `/api/admin`). Each action that changes something (`desk-revoke`, `desk-regenerate`, `session-end`) writes its `admin_audit` row **in the same transaction as the change**: if the row cannot be written the change does not happen and the answer is `500 internal_error`. The transcript read writes its row first and reads nothing if that fails. `session-reconcile` is a read and writes its row after. A row holds `actor` (the operator's email as the Flow reports it), `actor_ip`, the reason, ids and counts; never tokens, QR material, names or transcript text. Errors are `{"code","message"}`.

| Route | Body | Effect | Notable errors |
|---|---|---|---|
| `desk-revoke` | `desk_id, actor, reason` | Same function the org admin's revoke calls: status `revoked`, waiting sessions closed. Idempotent. | `desk_not_found` 404 |
| `desk-regenerate` | `desk_id, actor, reason` | Same function the org admin's regenerate calls: version bumped, desk active, waiting sessions closed. Answers **only** `status`, `token_version`, `waiting_closed`: no QR URL, no MAC, no public handle, so a platform operator never holds the credential. | `desk_not_found` 404 |
| `session-end` | `session_id, actor, reason` | Ends a `waiting`/`active` session with `end_reason = ended_by_admin`, records a `live_session_events` row (`admin_ended`, empty detail: the timeline is visible to the org's admins) and starts finalisation (`finalize::on_session_ended`). | `session_not_found` 404, `session_over` 409 (also when two operators race) |
| `session-reconcile` | `session_id`, optional `actor` | **Read-only.** Compares the metering (`live_usage_ticks`) with the ledger, see below. | `session_not_found` 404 |
| `session-transcript` | `session_id, actor, reason` | The only way for the platform to read what was said. Same shape as the dashboard's session detail (participants, utterances with the original apart from its translations). The reason is mandatory (≥ 10 characters and at least 4 different non-space characters); the audit row is written **first** and the read is refused (`500 internal_error`, no content) if it cannot be written. The answer carries `truncated: true` when the conversation has more than 5 000 utterances (the earliest 5 000 are returned). | `reason_required`, `reason_too_short`, `reason_invalid`, `actor_required`, `actor_invalid` 400, `session_not_found` 404 |

`actor` is required on every mutating route and on the transcript read (an action nobody can be asked about is not performed); `reason` must be at least 3 characters on the others. Wherever a reason is taken it also needs at least 4 different non-space characters (`aaaaaaaa` is refused as `reason_too_short`), and neither `reason` nor `actor` may contain control characters (NUL, line breaks, tabs: `reason_invalid` / `actor_invalid`; `session-reconcile` refuses such an `actor` too). There is no rate limit: like the other admin routes it sits behind a secret that only Directus holds.

### What reconciliation can and cannot prove

`live_usage_ticks` is written in the same transaction as the debit, so for an org-paid conversation every debit is `organization_credits_transactions` (`type = 'live_connect'`, `session_id` = one of the conversation's `call_sessions`) and every debit has one tick: the counts and the credits must match. The answer is `{ticks, ledger, status: ok | mismatch | unlinked, missing_ticks, duplicate_ticks, notes}`.

* **Amounts are compared one by one, not only in total**: the org debits (as positive numbers) and the ticks' amounts, sorted, must be the same list, so a debit too small beside another too large (equal sums) is a `mismatch`. A consumer conversation is still compared in total only, for the reason below. Reconcile is a read: a test compares an md5 of every row of every table it reads, before and after.
* `missing_ticks`: ticks with no debit (the ledger lost a row). `duplicate_ticks`: debits no tick accounts for (a charge the idempotency key never saw).
* **`unlinked`** when the meeting-side rows were erased (retention, owner or admin erase): the ledger link is `ON DELETE SET NULL`, the debits survive but cannot be told apart from the org's others. The report says so and claims nothing (`missing_ticks`/`duplicate_ticks` are `null`) rather than guess.
* A **consumer**-paid conversation can only be compared in total: `credit_transactions` has no session column, so the report compares the ticks with the owner's `usage_sessions.cost` for the room `live-<id>` (updated in the debit's transaction). Single debits cannot be counted (`null`); no `usage_sessions` row means `unlinked`.
* It never repairs anything. A correction is a human decision using the existing credit endpoint, with the report in hand.

### Roles, Flows and Insights

`node directus/setup-live-connect.mjs` (idempotent; `--dry-run` prints every call and changes nothing) provisions, on Directus 11: the *Live Connect* folder with `live_desks`, `live_sessions`, `live_participants`, `live_session_events` and the `live_desk_usage_daily` rollup (every field read-only in the app; `invite_hash`, `token_hash`, `call_session_id`, the desk's `public_id` hidden); the roles *Live Connect Support* (metadata only) and *Live Connect Privacy* (Support plus the transcript button); five Flow buttons; and the *Live Connect - Operations* dashboard. `live_usage_ticks` is **not** a Directus collection: its primary key is composite and Directus ignores such tables, so per-session billing is checked with the Reconcile button instead, and credits per desk come from `live_desk_usage_daily`: a table (one row per desk and UTC day, ids and two counts, no personal data) refreshed from the `live_desk_usage` view by `refresh_live_desk_usage()`, because Directus 11 only introspects base tables and cannot see a view. The two dashboard panels *Credits by desk (30d)* and *Top 10 desk-days by credits (30d)* read it. See `directus/README.md` ("Live Connect administration") for what each role sees and the privacy rules.

Existing Directus roles are **not modified**. `transcript_events`, `session_participants`, `transcripts` and `call_sessions` already hold Live Connect rows (a conversation reuses the meeting tables), so any role that must not read live conversations needs the permission filter printed by `node directus/setup-live-connect.mjs --print-transcript-filter`.

## Desks (permanent QR codes)

- A desk belongs to an org and **must** have one of its projects. Its QR carries `/lc/d/<public_id>.<mac>`; the MAC is derived from `LIVE_TOKEN_KEY` and the desk's `token_version`, never stored.
- **Every scan creates a new session** (the QR is never a conversation). The scan answers a forged, unknown, revoked or replaced QR with the same response, and never reveals an org's wallet state to the public (`desk_unavailable`).
- Revoking closes the sessions still waiting at that desk; regenerating bumps the version (the old print stops working), re-activates the desk and closes waiting sessions too.
- The customer waits up to `LIVE_DESK_WAIT_SECS`; at most `LIVE_DESK_MAX_WAITING` sessions wait at one desk, and at most `LIVE_DESK_MAX_WAITING_PER_IP` (2) from one client address (`429 desk_busy` for the excess). The address is the one every other limiter uses (`CLIENT_IP_HEADER`, else the last `x-forwarded-for` hop); one the proxy could not name is not capped, since that would cap every anonymous customer at once. The per-address count lives in memory with the wait window as its time to live, and asks the database whether those sessions are still waiting, so a customer who was answered or gave up frees the place. A printed QR is public: without this one phone could take all twenty places. Several customers behind one NAT (a hotel's wifi) share their two places per desk.
- **Revoke and regenerate take the same advisory lock as a scan**, and a scan re-checks, after taking it, that the desk is still active on the QR's version: a scan that read the desk just before a revoke cannot leave a waiting session behind it. A member answers from the dashboard Desk console or from the main app (`/live/desks`); exactly one colleague wins (the staff seat is a unique index).

## Dashboard data

- `GET …/live/sessions` filters by `status`, `project_id`, `desk_id`, `from`, `to` and pages (`limit` ≤ 100). `GET …/live/sessions/{id}?lang=xx` returns the participants and each utterance with its **original** and, beside it, the translation into `lang` (never instead of it); admins also get the diagnostic timeline.
- A conversation the caller may not see answers exactly like one that does not exist.
- `GET …/live/usage?group=desk|project|day&from&to` counts only the org's own metered ticks, plus the wallet balance and the conversations still open with what they have cost so far.

## Real-time design: why WebSocket only *(S9)*

The decision record is in the spec (*Key decisions* 1-3). In short:

- The server has to receive **each microphone** to translate it, and the engine returns translated
  audio over a WebSocket anyway. A WebRTC leg between the phones would carry a second, untranslated
  copy of the voice nobody wants, and would need TURN to work across networks. So there is no
  phone-to-phone media at all: each phone sends its own PCM16 24 kHz to `/ws/live` and receives only
  the **other** side's translation.
- The existing machinery is reused rather than forked: `RoomManager` (one private room
  `live-<session uuid>`), the Standard engine (one upstream session per speaker per target language,
  so two phones with different languages are two concurrent, independent translation streams), the
  meter, speech events and the org ledger. "No unnecessary infrastructure" means no media server,
  no queue and no second database.
- A new handler instead of extending `/ws/talk` (no idle timeout, signed-in only, single-microphone
  assumptions) or `/ws` (too shared to risk): the hardening of `/ws` is copied in, and identity is
  server-minted (the client chooses no peer id, so nobody can evict another seat).
- Trade-offs accepted: rooms are in memory, so one replica and a deploy drops live conversations
  (phones reconnect within the grace); the realtime path adds a server hop that WebRTC would not
  have, which is the price of translating at all; capacity is bounded by the provider's session cap.

## Security model *(S6, S9, S15, S20)*

| Credential | Held by | Can do | Cannot do |
|---|---|---|---|
| Account JWT | an owner, an org member | create a B2C session; with a role, manage desks, read dashboard data, answer a desk | be used as a guest token: the guest verifier rejects it, even if `LIVE_TOKEN_KEY` equals `JWT_SECRET` (tested) |
| Invite code (192 bits) | whoever holds the link | exchange once for a guest seat (`POST /api/live/join`) | read anything; only its SHA-256 is stored; a malformed and an unknown code answer identically |
| Guest / staff / host live token | one participant | open `/ws/live` for **its** session and seat, read/end that session | act on another session or seat, or call any account route; an ended session invalidates it |
| Desk QR token | anyone holding the printed code | create a **new** waiting session at that desk | resolve to a conversation, reveal the org/project/wallet, or work after revoke/regenerate |
| `ADMIN_API_SECRET` (Directus Flows only) | platform operators through Directus | desk revoke/regenerate, end a session, reconcile, audited transcript read | read content without a reason (written to `admin_audit` first; the read fails closed) |

- **Isolation.** The room name is `live-<uuid>` and the public `/ws` and the business room binding
  refuse that shape (`live::is_live_room`), so a stranger who learned a session id cannot join its
  room. The glossary routes (`/api/rooms/{room}/glossary…`) refuse it too (`403 reserved room`).
  Room peers carry no account id: the other phone never learns who pays. Tenancy is enforced
  in Rust like the rest of the server; a conversation a caller may not see answers exactly like one
  that does not exist (404). Tables are RLS default-deny.
- **Rate limits.** Create: 20/min per account. Desk administration: 30/min per account. Join: 30/min per client IP.
  Desk scan: 20/min per client IP and 60/min per desk, with at most `LIVE_DESK_MAX_WAITING` (20) waiting at one desk
  and at most 2 of them from one client IP.
  `/ws/live`: the same per-IP connect limit and connection cap as `/ws`, a message-rate cap
  (500 messages per 5 s), a 16 KiB cap on an audio frame (64 KiB on any other frame) and a
  per-phone audio budget of 1.5x real time (excess dropped), a ping every 15 s and a close after 45 s of silence;
  an outbound queue that overflows drops the phone (it reconnects) instead of growing.
- **Server-authoritative.** Mute is enforced on the server (a muted phone sends nothing to the
  engine); billing follows audio the engine received; the owner's balance is pushed to the owner
  only; identity, language and name come from the database, never from the client.
- **Concurrency caps.** `LIVE_MAX_CONCURRENT_PER_OWNER` (3) per owner, advisory-locked so a burst
  cannot beat it; `live_busy` when the engine cannot take two more upstream sessions.
- **What this document does not claim.** No penetration test of Live Connect has been run as part of
  this unit. The independent verifications recorded in `odd/tasks/live-connect.md` (L7, L9) are the
  adversarial reviews to date; the T13 admin routes were still awaiting theirs when this was written.

## Testing strategy *(S18)*

| Suite | Where | What it pins | Run it |
|---|---|---|---|
| Schema | `server/tests/live_schema.rs` | RLS, owner check, vocabularies, one owner-side seat and no database cap on guests, `max_guests` check and default, tick idempotency, purge keeps usage, migration re-run (069 included, and 065 after it) | `cargo test --test live_schema` |
| Tokens / config | unit tests in `live/{tokens,config}.rs` | forgery, expiry, audience, JWT confusion, desk MAC version binding, flag/key/limit validation | `cargo test --lib live::` |
| REST | `tests/live_rest.rs` | create/join/get/end, caps, consent, project validation, error codes; several guests per link (create validation, `session_full` at the limit, simultaneous joins admit exactly `max_guests`, reusable link and join window, guest leaving, `live_busy` on a join the engine cannot serve) | `cargo test --test live_rest` |
| `/ws/live` | `tests/live_ws.rs` | both phones translated at once, four phones in three languages hearing every other speaker in their own, peer joined/left/muted per peer, server-side mute, reconnect takeover, sweeper, billing ticks and exhaustion, no account id leak, latency metric (one observation per utterance with three listeners), billing per distinct language across seats, the 3-seat echo filter, audio frame cap and 1.5x real-time budget (honest sender untouched) | `cargo test --test live_ws` |
| Billing | `tests/live_billing.rs` | idempotent ticks, insufficient funds, concurrent sessions on one wallet, the org carry across Start/Stop cycles, one stream per distinct language for every seat (user and org, to the last digit) | `cargo test --test live_billing` |
| Desks | `tests/live_desks.rs` | scan creates a new session each time, revoke/regenerate, exactly one answerer, queue and per-address limits, scan-vs-revoke ordering, no existence oracle, a real WS conversation | `cargo test --test live_desks` |
| Intelligence | `tests/live_intelligence.rs` | finalisation, search filters and scope, retention, erasure (also for a conversation with three guests) | `cargo test --test live_intelligence` |
| Dashboard / admin | `tests/live_dashboard.rs`, `tests/live_admin.rs` | list/detail/usage scope (first guest and `guest_count` for a conversation with several), admin routes and audit | `cargo test --test live_dashboard --test live_admin` |
| Client unit | `client/src/scripts/live/*.test.ts` | state machine, protocol, view, pages (fake socket and fake media) | `cd client && npx vitest run --coverage.enabled=false` |
| **Browser e2e, real server** | `client/e2e/live.spec.ts` | the user journeys below, in real browsers against the real server and database | `cd client && npm run test:e2e:live` |
| Load | `loadtest/live-connect.js` | connect time, errors, `live_busy`, silent directions at N conversations | `loadtest/README.md`, "Live Connect" |

The server uses **the real Standard engine pointed at `RealtimeMock`** (`engine/realtime_mock.rs`);
the e2e uses a Node twin of it (`client/e2e/support/mock-qwen.mjs`) because the server there is a
separate process. Only the paid provider is a stand-in.

**The DB-gating trap.** The integration suites skip silently without `DATABASE_URL` or without
pgvector, and a skipped run prints `ok` in a few milliseconds. Run them against the repo's own
Postgres (`docker compose up -d --wait postgres`) with
`DATABASE_URL=postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_voip_test` and read the
duration: `live_ws` takes about 11 s, `live_intelligence` about 6 s. Local only: a test database is
written to and swept; the server refuses remote databases from a laptop.

**Browser e2e (`playwright.live.config.ts`).** Opt-in; the default `npm run test:e2e` and CI never
start it (the spec also skips itself without the stack). It starts, on loopback only: the server's
already-built debug binary on :3101 (`LIVE_E2E_BUILD=1` runs `cargo build` first; it needs the
rustup 1.97 toolchain first on PATH and `MACOSX_DEPLOYMENT_TARGET` on macOS), the mock upstream on
:3102, and the client under `astro dev` on :4331. The server gets a scrubbed environment and an empty
working directory, so no `server/.env`, real key or deployed database can leak in. Users and
organisations are seeded by SQL into the local test database (the only way to obtain an account is
Google sign-in) and signed with the stack's own `JWT_SECRET`; everything after login goes through
the real HTTP API. Needs `psql` on PATH and `npx playwright install chromium`. Journeys covered:

1. B2C end to end: host creates, link and QR appear, guest joins (language, consent), both connect,
   each sees the other's translated caption and original, translated audio is delivered and handed
   to the playback worklet on both phones, mute is reflected, the owner's balance falls, the server
   latency histogram moves, the host ends it and each side gets its own wording.
2. A used link says `session_full`, an expired one `invite_expired`, a junk one `invite_not_found`.
3. B2B: customer scans a desk QR, staff answer from the queue, both talk, the org wallet is
   debited and the session is tied to the project; the same QR starts a **new** session; revoking
   while a customer waits shows the invalid-QR screen and a later scan is refused.
4. `?answer=<session>` opens the right customer's language choice and is consumed; a malformed value
   is ignored.
5. `/talk` still loads and `#hero-talk` still points at `/talk`.

One test is a pinned defect (`test.fail`): the phone that joins second never sees the first phone
mute, because the client reads `peers[].peer_id` where the server sends `peers[].id`.

**Simulated vs verified.** Verified by automation: protocol, concurrency of the two directions,
billing, isolation, lifecycle, UI flows in Chromium against the real server. Simulated: the provider
(mock replies), the microphone (Chromium's synthetic tone, not speech), the loudspeaker (headless
output). **Not verified by anything**: real speech through the real provider, two real phones,
speaker/Bluetooth/mixed combinations, distance, noisy rooms, echo and self-translation loops,
Safari/iOS audio unlock, mobile networks and reconnection on them. That is the product owner's run:
[`live-connect-device-matrix.md`](live-connect-device-matrix.md).

## Deployment and migration

**Order.** Migrations run automatically at server start (`sqlx::migrate!`), in order, and every one
is idempotent (`IF NOT EXISTS`, guarded constraints, safe to re-run):

| # | What | Notes |
|---|---|---|
| 065 | the five Live Connect tables, indexes, RLS default-deny | additive; flag off means no route reaches it |
| 066 | `live_participants.user_id` becomes `ON DELETE SET NULL` | only acts while the foreign key still cascades; keeps the tick ledger when an account is deleted |
| 067 | `live_session_summaries` (one AI summary per conversation and language) | content: removed with the conversation |
| 068 | `live_desk_usage` view and `live_desk_usage_daily` table | metadata only; exists so Directus (which ignores composite-key tables) can chart credits per desk |
| 069 | `live_sessions.max_guests` (1-3, default 1) and the guest-side index made non-unique | additive column, guarded `NOT VALID` check. The one thing it removes is the database's old cap of one guest per session (the application enforces `max_guests` now), so **a server binary that predates it, run against a migrated database, would let a second guest in**: roll the schema forward with the binary, do not run an old build on a 069 database while sessions admit several guests. Existing sessions keep `max_guests = 1` |

All additive but for that last point: an older server binary ignores the new tables, so a rollback of
the application does not require a rollback of the schema, and there is no destructive migration to
undo (the guest-side index it relaxes can be made unique again only while no session has several
guests).

**Constraints.** A single replica (rooms and presence are in memory; see *Operations*). Rotating
`LIVE_TOKEN_KEY` invalidates printed desk QR codes and guest tokens in flight, so treat it as a
secret that is generated once per environment and kept.

**Enable per environment** (staging first, then production, each from its own CI deploy; wait for
green CI before promoting, as for any release):

1. Set `LIVE_TOKEN_KEY` (`openssl rand -base64 48`, never `JWT_SECRET`) and `LIVE_CONNECT_ENABLED=1`
   in the environment's Railway variables. Optional limits are in `server/.env.example`.
2. Billing must already be on (`DATABASE_URL`, `GOOGLE_CLIENT_ID`, `JWT_SECRET`) and the Standard
   engine configured (`QWEN_API_KEY`). Staging cannot place phone calls, but Live Connect does not
   use the telephone provider, so it can be exercised there.
3. Deploy, then check the log line `live connect enabled`, `POST /api/live/sessions` as a signed-in
   user, and `/metrics` for `voxtranslate_live_translation_ms`.
4. Run the Directus script once per Directus instance (`node directus/setup-live-connect.mjs`, see
   `directus/README.md`) and deploy the dashboard.
5. Run the real-device matrix before telling customers it is supported.

The home screen always shows the Live Connect entry (the client is not flag-gated): with the flag
off `/live` shows the "unavailable" screen and nothing else of the feature is reachable (the two
erase endpoints stay mounted so content can always be deleted).

**Rollback.** Set `LIVE_CONNECT_ENABLED=0` (or unset the key) and redeploy/restart: the routes
disappear, the sweeper stops (so retention stops until it is re-enabled), conversations in flight
end when their sockets drop, and nothing is lost: billing records, transcripts and desks stay in the
database. To remove the home entry as well, revert the client hero change.
