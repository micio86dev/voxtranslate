# VoIP operations — translated telephone calls (spec 0111)

What to do when phone calls misbehave, and how to turn them off in a hurry.

Related: [`docs/voip-telnyx-setup.md`](../voip-telnyx-setup.md) (account setup and env),
[`docs/voip-billing.md`](../voip-billing.md) (the pricing derivation),
[`docs/voip-data-flow.md`](../voip-data-flow.md) (where each step runs),
[`docs/voip-china-validation.md`](../voip-china-validation.md) (the China gate).

---

## 1. Turning it off

Three levels, cheapest first. **None of them touches data.**

| What | How | Effect |
|---|---|---|
| Stop new calls, let current ones finish | `VOIP_ROLLOUT_STAGE=disabled` | The policy gate refuses every dial. Calls in progress run to a clean end and settle normally. |
| Remove the feature entirely | `VOIP_ENABLED=false` | The routes are **not registered** — every VoIP request 404s. |
| Stop it at the source | Disable the Outbound Voice Profile in the Telnyx portal | The carrier refuses the dial even if something in our stack tries. |

For one organisation rather than the whole deployment, set `enabled = false` in its
`voip_org_settings` row (or through the dashboard's phone settings, which writes an audit
entry — preferable).

Nothing here needs a migration reverted. `056_voip.sql` only adds tables and one partial
index; leaving it applied with the feature off is inert.

## 2. The dashboard to watch

Everything is on `/metrics`, prefixed `voxtranslate_voip_`.

| Metric | What a move means |
|---|---|
| `calls_refused_total` rising | Usually a **customer** hitting a limit they set. Check their `voip_org_settings` before assuming a fault. |
| `calls_failed_total` rising | Usually **us or the carrier**. Cross-check `provider_errors_total`. |
| `provider_errors_total` rising | Telnyx side. Check credentials, account balance and their status page — in that order, because the first two are far more common. |
| `credit_reservation_failures_total` rising | Organisations running out of credits. Not an incident unless it is sudden and broad, which would suggest a pricing bug rather than empty pools. |
| `webhooks_rejected_total` non-zero and sustained | Someone who cannot sign is posting to the endpoint. Worth an alert on its own. A brief burst after a key rotation is expected. |
| `webhooks_duplicate_total` spiking | The provider believes we are not acknowledging. Look at webhook latency and 5xx rates before anything else. |
| `voip_setup_ms` p95 climbing | Post-dial delay. Usually a route problem for one destination — segment by country using `voip_calls`, not by metric label (there isn't one, on purpose). |
| `voip_translation_ms` p95 > 2500 | The headline. This is what a customer describes as "it feels laggy". |
| `codec_renegotiations_total` rising | A route is downgrading L16 to µ-law. The call still works; it works on narrower audio, so STT quality drops before anything reports an error. Segment by destination in `voip_call_quality.codec`. |
| `disclosure_failures_total` non-zero | **Alert on this.** Each one is a customer who asked for a recording and did not get one, because we could not tell the recipient. Capture is switched off deliberately — see §3. |
| `unerasable_recordings_total` non-zero | **Alert on this too, and treat it as a compliance defect.** The carrier saved a recording and gave us no id for it, so nothing can delete those bytes. Chase the provider's `call.recording.saved` payload shape before anything else. |

There are deliberately **no destination, organisation or phone-number labels** on any of
these. A metric label is exported to whoever scrapes us, and a country on
`calls_failed_total` would put a customer's calling pattern into a third-party system. The
per-call detail lives in `voip_calls`, behind authorisation.

### The background sweep

`webhook::run_sweep` runs every 60 seconds whenever a provider is configured and the
database is reachable. It does three things a request path structurally cannot, because
all three are about calls nobody is watching any more:

1. **Ends calls past their maximum duration** — the cap cannot be a per-call timer, because
   that timer lives in the process that placed the call and dies with it, which is exactly
   the case the cap exists for.
2. **Fails calls stuck before answer** for ten minutes — a provider that accepted a dial and
   then said nothing leaves a row consuming a concurrency slot forever.
3. **Closes credit holds** left open by a crash or a hangup webhook that never arrived.
4. **Closes consent gates nobody answered**, twenty seconds after the announcement. The
   carrier's own gather timeout fires on some routes and not others, and either way the
   task that opened the gate may be gone — so the deadline is enforced from the row. A
   timeout is **not** consent: it resolves to `timeout` and then follows the org's
   `consent_refused_action`.

## 3. Common situations

### Every call is refused with `rate_unavailable`

The rate deck is empty or stale. This is the **designed** failure: a destination with no
fresh price is refused rather than dialed at a guess.

```sql
SELECT provider, count(*), max(fetched_at) FROM voip_rates GROUP BY provider;
```

If `max(fetched_at)` is older than `VOIP_RATE_MAX_AGE_SECS` (default 24 h), the sync has
stopped. Re-run it; the calls resume immediately. Do **not** widen the staleness window to
make the symptom go away — that trades a loud failure for a silent mispricing.

### Every call is refused with `eu_processing_unavailable`

`VOIP_REQUIRE_EU_PROCESSING` is on. **No translation tier can satisfy it today** — see
`docs/voip-data-flow.md`. Turning the flag off is the correct action if the organisation
did not intend to require EU-only processing; if they did, the honest answer is that the
capability is not there yet.

### A call finished but the credits are still held

The webhook settles atomically with the state change, so this means the process died
mid-transaction or the hangup webhook never arrived. The recovery sweep closes it:

```sql
SELECT c.id, c.status, r.held_credits
FROM voip_calls c
JOIN voip_credit_reservations r ON r.call_id = c.id AND r.state = 'held'
WHERE c.status IN ('completed', 'failed');
```

`webhook::settle_finished_calls` runs over exactly this set every 60 seconds
(`webhook::run_sweep`, spawned in `lib.rs` when a provider is configured) and is
idempotent. If rows persist, the calls are not terminal — look for a missing hangup
webhook, which the stall reaper below handles.

### Dialling stops with `concurrency_limit` and nobody is on a call

Almost certainly stuck rows, not real traffic. A non-terminal call counts towards **every**
cap, so a provider that accepted a dial and then went silent leaves a row in `dialing`
consuming a slot indefinitely.

```sql
SELECT status, count(*) FROM voip_calls
WHERE status NOT IN ('completed','failed') GROUP BY 1;
```

`webhook::fail_stalled_calls` clears these automatically ten minutes after the dial, and
releases their credit holds on the next settlement pass. Seeing a backlog means the sweep
is not running — check that `VOIP_PROVIDER` resolves and the database is reachable, because
`run_sweep` is only spawned when both are true.

### `actual_provider_cost_usd` is NULL on every call

Expected today. Telnyx rates asynchronously through batched usage reports and the adapter
reports per-leg CDR as unsupported rather than returning a fabricated zero — a zero would
show every call at 100% margin, which is worse than admitting the number is not in yet.
Tracked in `docs/voip-telnyx-setup.md` §6. Margin reporting reads as **unreconciled**, not
as profitable.

### A customer says the other person heard nothing / heard themselves

Check, in order:

1. `voip_call_quality.codec` — a route that forced PCMU where L16 was requested changes the
   audio path.
2. `media_disconnects_total` and `websocket_reconnects_total` around the call's window.
3. Whether the legs were ever bridged. **They must not be.** The design keeps the two legs
   independent and cross-wires them through the translation; a bridged pair is how each
   party ends up hearing the other's untranslated voice.

### The customer asked for recordings and none are being made

Look at `consent_status` first, then at `disclosure_failures_total`.

```sql
SELECT consent_status, recording_status, disclosure_language, count(*)
FROM voip_calls
WHERE org_id = '…' AND started_at > now() - interval '24 hours'
GROUP BY 1, 2, 3 ORDER BY 4 DESC;
```

| What you see | What it means |
|---|---|
| `consent_status = 'denied'` / `'timeout'` | The recipients said no, or said nothing. Working as designed. If it is *every* call, check `disclosure_language` — an announcement nobody understood is an announcement nobody answers. |
| `consent_status = 'pending'` and rows are old | The sweep is not running. Check that `VOIP_PROVIDER` resolves and the database is reachable; `run_sweep` is only spawned when both are true. |
| `recording_status = 'none'` with `disclosure_played_at IS NULL` | We could not speak to the recipient at all. `disclosure_failures_total` will be non-zero and the log carries the reason: `provider_cannot_speak`, `announcement_failed` or `gather_failed`. Capture was disabled **on purpose** — recording someone who was never told is the incident this prevents. |

### A call connects but nobody hears anything

The audio path is separate from the call path, so a call can be perfectly healthy and
silent. In order:

1. `codec_renegotiations_total` and `voip_call_quality.codec` — a forced downgrade changes
   the decode path on both directions.
2. `media_disconnects_total` around the call's window — the socket closed and the room lost
   its phone peer.
3. The logs for `call answered with no phone leg parked` — the leg was never parked or was
   already claimed, which means no ticket could be issued and the carrier was never asked
   to stream.
4. The logs for `could not start the media stream` — the carrier refused. The call is up
   and both parties hear silence; this is the one failure mode that is invisible from
   every other signal.
5. **Whether the legs were ever bridged. They must not be.** A bridged pair is how each
   party ends up hearing the other's untranslated voice.

### A recording exists that the customer says nobody agreed to

Do not speculate — the evidence is on the row:

```sql
SELECT consent_policy, consent_status, disclosure_played_at, disclosure_language,
       consent_received_at, recording_started_at
FROM voip_calls WHERE id = '…';
```

`recording_started_at` must be **after** `disclosure_played_at`, and `consent_status` must
be `granted` or `not_required`. If it is not, that is a real incident: stop recording for
that organisation, preserve the rows, and escalate.

### Recordings and retention

A phone recording is **not** in our object storage. It is on the carrier's, and
`voip_calls.provider_recording_id` is the only durable handle on it — the URL beside it is
kept for operations and is deliberately never served to a client, because on some carriers
it is publicly fetchable.

`business::retention::sweep_voip_recordings_once` runs alongside the meeting retention
sweep (same `RETENTION_SWEEP_ENABLED` switch) and deletes at the provider **before**
clearing the handle. A failed delete leaves the row untouched so the next pass retries;
that is why a stuck carrier shows as recordings that stay `saved` rather than as recordings
marked `deleted` that still exist.

The window comes from `voip_org_settings.recording_retention_days`. **NULL means keep** —
there is no fallback to the org's general `retention_days`, because deleting a customer's
recordings on a schedule they never set is worse than keeping them one pass too long.

```sql
-- Recordings past their window that the sweep has not managed to delete.
SELECT c.id, c.ended_at, s.recording_retention_days
FROM voip_calls c JOIN voip_org_settings s ON s.org_id = c.org_id
WHERE c.provider_recording_id IS NOT NULL AND c.recording_status = 'saved'
  AND s.recording_retention_days > 0
  AND COALESCE(c.ended_at, c.started_at)
      < now() - make_interval(days => s.recording_retention_days);
```

Individual account deletion does **not** remove these. `voip_calls.user_id` is ON DELETE
SET NULL so an org's billing history survives an employee leaving, and a phone recording is
a multi-party, org-owned artifact — the same scope rule `SafetyService::delete_user`
already applies to cloud meeting recordings. A data-subject request for one goes through
the tenant admin, not the individual's account deletion.

## 4. Rolling out

`VOIP_ROLLOUT_STAGE` walks `disabled → internal → beta → business → ga`, and every step is
reversible by setting the previous value. `VOIP_BETA_ORG_IDS` lists the organisations
admitted during `internal` and `beta`.

An organisation's own `enabled` switch is required at **every** stage, GA included. Being
on the beta list is permission for them to turn it on, never a substitute for having done
so — so widening the stage cannot switch the feature on for anyone by surprise.

## 5. Spend

`VOIP_DAILY_PROVIDER_SPEND_LIMIT` stops new dials once the day's provider cost passes it.
The count includes calls the provider has not rated yet, using their estimate, so an
unreconciled backlog cannot hide a spike.

`VOIP_MAX_DESTINATION_RATE` refuses a destination above a per-minute ceiling **before
dialing**. It is the single most effective anti-toll-fraud control here: premium-rate
international ranges are the classic target, and a ceiling checked before the dial stops
the attack rather than reporting it afterwards.

If you suspect fraud in progress:

1. `VOIP_ROLLOUT_STAGE=disabled` — stops every new dial across the deployment.
2. Disable the Outbound Voice Profile in the Telnyx portal — stops it at the carrier.
3. Then, and only then, investigate:

```sql
SELECT recipient_country, count(*), sum(credits_consumed)
FROM voip_calls
WHERE started_at > now() - interval '24 hours'
GROUP BY 1 ORDER BY 3 DESC;
```

Group by country, never by number. The numbers are in the table; they do not belong in an
incident channel.

## 6. Rollback

The feature is additive: new tables, new routes, one new nav entry. Rolling back the
deployment is enough, and the env switches in §1 are faster.

If a release must be reverted while calls are live, prefer `VOIP_ROLLOUT_STAGE=disabled`
first and let the in-flight calls settle — an abrupt restart is safe (state is in Postgres
and webhooks are idempotent) but leaves holds for the sweep to close, which the customer
sees as a briefly wrong balance.
