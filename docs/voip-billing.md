# Pricing — translated phone calls (spec 0111)

**Short version: a VoIP minute is priced from its real provider cost, at a configurable
minimum gross margin of 20%, and the floor is a proven invariant rather than a
convention.**

This document exists for the same reason `docs/pricing-standard-qwen.md` and
`docs/pricing-talk-to-anyone.md` do: so the number on an invoice has a written derivation
before anyone is surprised by it.

---

## 1. Margin is not markup

The rest of VoxTranslate prices as `cost × (1 + markup)` (`engine/metadata.rs:68`). The
commercial requirement for VoIP is expressed as a **gross margin** floor. They are the same
statement from two sides:

```
margin = markup / (1 + markup)          markup = margin / (1 - margin)
```

So the house default of **25% markup IS a 20% gross margin**. Nothing was reinvented for
VoIP; a guard was put over the existing convention, expressed in the units the business
actually reasons in.

The distinction matters because getting it backwards is an easy and expensive mistake:

| Price | Cost | Markup | **Gross margin** |
|---|---|---|---|
| 1.20 | 1.00 | 20% | **16.7%** ← below the floor |
| 1.25 | 1.00 | 25% | **20.0%** ← the floor |

## 2. The formula

```
price_per_minute = provider_cost × (1 + safety_buffer) / (1 - min_gross_margin)
```

| Parameter | Env | Default |
|---|---|---|
| `min_gross_margin` | `VOIP_MIN_GROSS_MARGIN_PERCENT` | 20 (⇒ 0.20) |
| `safety_buffer` | `VOIP_COST_SAFETY_BUFFER_PERCENT` | 10 (⇒ 0.10) |

The buffer inflates the *observed* cost before pricing, so the realised margin survives the
provider's invoice coming in above the rate deck. It widens the margin, never narrows it.

Rounding is **away from zero** at six decimal places. Rounding to nearest can land a hair
under the floor, and the floor is the thing the function exists to guarantee.

## 3. What goes into the cost

`ProviderCost` is itemised, not a single number, because the reconcile step has to be able
to explain a margin miss and "it cost more than we thought" is not an explanation.

| Component | What it is |
|---|---|
| `telephony` | Destination carrier rate + the provider's per-minute platform fee, from the rate deck |
| `translation` | STT + translation + TTS **for both directions**, at the chosen tier |
| `media_streaming` | Media forking/streaming, where charged separately |
| `recording` | Call recording, when enabled |
| `storage` | Recording object storage, amortised over its retention period |
| `ancillary` | AI analysis, taxes and surcharges, allocated number rental |

**Both directions.** A translated call runs two translation streams for its whole
duration — the same shape as Talk to Anyone (`docs/pricing-talk-to-anyone.md`), and for the
same reason: closing the idle direction would lose the first clause of every turn. Pricing
one direction would undercharge every single call.

## 4. Worked example

An Italian user calls a Chinese mobile on the Standard tier.

```
telephony     (rate deck, +8613…)            0.0250 /min
translation   (Standard 0.0045 × 2 way)      0.0090 /min
media stream                                 0.0000 /min
                                             ----------
provider cost                                0.0340 /min

× (1 + 0.10) buffer                          0.0374
÷ (1 - 0.20) margin                          0.0468 /min  ← quoted to the user

realised margin against the observed cost:  (0.0468 - 0.0340) / 0.0468 = 27.4%
realised margin if the invoice comes in 10% high (0.0374):        20.1%
```

The buffer is doing exactly its job in that last line.

## 5. Credits, holds and settlement

Credits are unchanged: **1 credit = $0.01**, in `organizations.credits_balance`. There is
no second currency.

1. **Quote.** Rate deck lookup (longest prefix, freshness-checked) → `ProviderCost` →
   price/minute. Shown before dialing along with the available balance.
2. **Hold.** `ceil(price × estimated_minutes)` credits are **deducted from the pool at
   dial time**, with a `voip_hold` ledger row. This is what makes concurrent dials safe:
   the money is already gone, so the second dial simply finds a smaller balance. A
   check-then-act balance test is racy no matter how carefully it is written.
3. **Meter.** Per-second accrual through `CreditAccumulator`, `Decimal` throughout.
4. **Settle.** On hangup: refund the unused part (`voip_hold_release`), or charge the
   overrun (`voip_overage`). One transaction. Idempotent — a redelivered hangup cannot
   refund twice.
5. **Reconcile.** When the provider's authoritative cost is available, record
   `actual_provider_cost_usd` and the realised `gross_margin`. A realised margin below the
   floor raises an alarm; it is never quietly absorbed.

### Accounting identities

Asserted in code (`Settlement::is_coherent`) and in tests, for every reservation:

```
released = held - min(actual, held)
settled + shortfall = actual
net pool movement = -settled
```

`shortfall` is the part of a call the pool could not cover **after** the call had already
happened. It is recorded rather than forgiven: a call cannot be un-made, and a silently
swallowed shortfall is a loss nobody is measuring.

## 6. Why VoIP rounds its tail up

The meeting meter (`CreditAccumulator`) floors to whole credits and deliberately gives away
the sub-cent remainder — "too small to charge". VoIP does the opposite and rounds its
final tail **up**.

That is a deliberate divergence, not an inconsistency. On a five-cent call, forgiving
$0.0099 gives away a fifth of the revenue and lands the margin near zero. The tail is at
most one cent, it is on the customer's side of a bill they agreed a per-minute rate for,
and it is the difference between the margin invariant holding and being approximately
true. There is a test that fails if it is changed back.

## 7. Fail-closed pricing

A destination with **no rate**, or a rate older than `VOIP_RATE_MAX_AGE_SECS` (default 24h),
is **refused** — `rate_unavailable` — not dialed at a guessed price.

That is a real availability cost, taken on purpose. The alternative is discovering the
price on the invoice. A rate-deck sync that produces nothing therefore stops every call
immediately, which is loud, rather than mispricing them quietly, which is not.

## 8. Spend controls

| Control | Env | Default |
|---|---|---|
| Refuse a destination above this provider cost/min | `VOIP_MAX_DESTINATION_RATE` | $1.00 |
| Stop dialing past this daily provider spend | `VOIP_DAILY_PROVIDER_SPEND_LIMIT` | $50 |
| Max call duration | `VOIP_MAX_CALL_DURATION_MINUTES` | 60 |
| Concurrent calls, global / org / user | `VOIP_MAX_CONCURRENT_CALLS_*` | 50 / 10 / 2 |

`VOIP_MAX_DESTINATION_RATE` is the single most effective anti-toll-fraud control there is:
premium-rate international ranges are the classic target, and a ceiling checked **before**
dialing stops the attack rather than reporting it.

## 9. Inherited caveat

`docs/pricing-standard-qwen.md` marks the underlying Qwen per-minute price as
**unconfirmed**. VoIP inherits that uncertainty on the translation component and doubles
the exposure to it, because it runs two streams. Confirm `QWEN_COST_PER_MINUTE` in the
Model Studio console for the deployed region before charging anyone for a phone call.
