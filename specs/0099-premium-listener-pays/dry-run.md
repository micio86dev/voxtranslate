# 0099 Listener-pays — Billing Dry-Run Plan

**Goal:** prove, with real services, that listener-pays (a) routes each listener
**their chosen engine's** output and (b) bills the **listener, not the speaker**,
at the right rate — *before* flipping `LISTENER_PAYS` on in production.

The feature is fully built and gated behind `LISTENER_PAYS` (OFF in prod → the live
speaker-pays path is byte-identical). This plan is the only gate left.

---

## 0. Safety first

- **Never** set `LISTENER_PAYS` in prod until sign-off. It's a per-deployment env
  var; set it only on a **staging/canary** deployment (or a local stack).
- Use a **test account with a small balance** (e.g. top up €1) so a billing bug
  can't drain real credit.
- **Rollback = unset `LISTENER_PAYS`** and redeploy/restart → instant revert to
  speaker-pays. No data migration, no schema change.
- Prefer a **staging DB**. If you must use prod, use a throwaway test user and
  delete its rows after (`usage_sessions`, `usage_events` for that user).

## 1. Environment

A non-prod server with:

| Var | Value |
|---|---|
| `LISTENER_PAYS` | `1` |
| `OPENAI_API_KEY` | real (Premium engine must be live) |
| `OPENAI_REALTIME_MAX_SESSIONS` | `16` (set `1` only for the capacity test, §C1) |
| `DEEPGRAM_API_KEY`, `GROQ_API_KEY` | real |
| `DATABASE_URL`, `GOOGLE_CLIENT_ID`, `JWT_SECRET` | set → billing ON (needed to bill listeners) |

Local quick-start (routing checks only; billing needs the DB vars above):

```bash
# from server/ — billing OFF (guest), good for the routing/A checks
LISTENER_PAYS=1 OPENAI_API_KEY=… DEEPGRAM_API_KEY=… GROQ_API_KEY=… cargo run
# point the client at it: PUBLIC_WS_HOST=localhost:3001 npm run build && npx astro preview
```

You need **2 browsers/devices** and, for billing, **2 signed-in test accounts** (or
1 account + 1 guest — guests are always Standard).

## 2. How to choose the receive-engine

Each participant picks their engine in the **pre-join engine selector** (it currently
reads "Translation engine"; the copy reframe to "quality you RECEIVE" is launch-polish,
step 7 — functionally the selector already sets the receive engine).

---

## A. Routing correctness (each listener gets THEIR engine)

1:1 call, **speaker IT, listener ES** — vary the LISTENER's engine:

| # | Listener ES engine | Expect for ES | Expect for IT (speaker) | OpenAI session? |
|---|---|---|---|---|
| A1 | Standard | Groq subtitle in ES; browser TTS voice | sees own IT caption | no |
| A2 | Premium | OpenAI **voice** + OpenAI subtitle in ES | sees own IT caption | yes (it→es) |

Run both directions (each side speaks). Then the **headline mixed case** — 3 peers:
**IT speaker, ES-Standard listener, ES-Premium listener**. When IT speaks:
- ES-Standard listener → **Groq** subtitle, **no** premium audio.
- ES-Premium listener → **OpenAI** voice + subtitle.
- Server log shows **both** a Deepgram and an OpenAI session for the IT speaker
  ("ognuno il suo engine"). Neither listener sees the other engine's output.

**Capture format:** with a Premium listener present, the speaker's browser must send
PCM16 — confirm a `{"type":"capture_format","pcm":true}` WS frame arrived (devtools)
and the server has **no** Deepgram demux errors. Remove the Premium listener → a
`pcm:false` frame should follow.

---

## B. Billing attribution + amounts (the critical part)

Record each participant's balance **before/after** (UI or `GET /api/billing/...`).

- **B1 — it↔es, both Premium.** While IT speaks, the **ES listener's** balance drops
  at the **Premium** rate; **IT's balance does not drop** for IT's own speech.
  Symmetric when ES speaks. (Both end up billed for what they *heard*.)
- **B2 — it↔es, IT Premium / ES Standard.** When ES speaks, **IT is billed Premium**;
  when IT speaks, **ES is billed Standard**. The speaker is **never** billed for their
  own speech.
- **B3 — scaling.** Per-tick charge ≈ `rate × interval × (active cross-language
  speakers)`. In a group where 2 other languages speak at once, the listener is
  billed ×2.
- **B4 — no charge when idle.** Nobody else speaking → no deduction. Everyone shares
  the listener's language → no deduction.
- **B5 — pre-join gate.** A listener with 0 balance **cannot join** (the `can_join`
  check now gates the *listener*, which is correct — they pay to receive).

---

## C. Edge cases (incl. the 2 KNOWN items to decide on)

- **C1 — Capacity fallback (KNOWN item #1).** Set `OPENAI_REALTIME_MAX_SESSIONS=1`,
  create 2 distinct Premium target langs → the 2nd is `AtCapacity`. Those listeners
  get `EngineDowngraded(premium→standard, premium_at_capacity)` and the **Standard**
  stream. **KNOWN:** they're still metered at the **Premium** rate for that fallback
  window (the meter rate is fixed per connection). **Decision:** accept (capacity is
  rarely hit) or bill Standard during fallback before launch.
- **C2 — Standard listener exhausts (KNOWN item #2).** A Standard listener drains to
  0 → the meter stops; **KNOWN:** they keep receiving **free** Standard. **Decision:**
  acceptable (Standard is near-free) or add a hard cap (stop translation / nudge to
  top up).
- **C3 — Premium listener exhausts.** Drains to 0 → `EngineDowngraded(insufficient_
  balance)`, dropped to **free Standard**, `capture_format` re-pushed, billing stops.
  Verify they keep receiving Standard and aren't charged further.
- **C4 — auto-detect listener.** A listener on `lang=auto` isn't billed until their
  language resolves (the meter reads the live lang each tick).

---

## D. Sign-off → launch

1. A + B all pass; C1/C2 decisions made (and fixed if required).
2. **Flip `LISTENER_PAYS=1` in prod** (one env var; redeploy).
3. Launch-polish (step 7): expose the flag to the client and reword the engine /
   pricing copy to "quality you **receive**" / "per source you listen to" (8 langs).
4. **Monitor for the first days:** live OpenAI session count vs
   `OPENAI_REALTIME_MAX_SESSIONS`; per-listener rows in `usage_sessions`; OpenAI spend
   (mixed rooms run more sessions than speaker-pays — expected per the approved model).

---

## What's already proven (no manual check needed)

Unit + DB-gated tests cover the pure logic: the `(tgt_lang, engine)` resolver
(`routes_for_speaker`), engine-filtered targets/delivery, `active_source_count`
scaling, and the listener meter deducting from the listener at their engine rate
(`usage::listener_scope_bills_per_active_source`, DB-gated → runs in CI coverage).
The dry-run is specifically the **end-to-end real-services + real-money** validation
those can't perform.
