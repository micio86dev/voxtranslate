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

There are deliberately **no destination, organisation or phone-number labels** on any of
these. A metric label is exported to whoever scrapes us, and a country on
`calls_failed_total` would put a customer's calling pattern into a third-party system. The
per-call detail lives in `voip_calls`, behind authorisation.

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

`webhook::settle_finished_calls` runs over exactly this set and is idempotent. If rows
persist after it has run, the calls are not terminal — look for a missing hangup webhook.

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
