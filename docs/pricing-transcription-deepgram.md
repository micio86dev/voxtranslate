# Batch transcription pricing

How `DEEPGRAM_COST_PER_MINUTE` (default **$0.0043**) is derived, and why the rate
it replaced was losing money on every hour transcribed.

## Where the number comes from

Batch transcription is the only thing still running on Deepgram: `/v1/listen`
with `model=nova-2` and `detect_language=true` (`server/src/deepgram.rs`).

The authoritative figure is the org's own **Deepgram console → Billing → Spend**,
which reports the *rate applied* for the models actually called — not the public
list price, which may differ by contract, volume, or model generation. As of
2026-09-07 that read:

| Model | Rate applied |
|---|---|
| Nova / Nova-2 (pre-recorded) | **$0.258 / hour** = $0.0043 / min |

For reference, the public list at the same date (Nova-2 is legacy and no longer
listed): Nova-3 monolingual $0.0043/min, Nova-3 multilingual $0.0052/min,
Whisper Large $0.0048/min. A move to Nova-3 with language detection would land on
the multilingual row.

## What the customer pays

Same rule as every metered rate in the product — `cost × (1 + markup)`, charged
against the org credit pool at **100 credits = $1**:

```
$0.0043/min × 60 = $0.258/hour × 1.25 = $0.3225/hour → 33 credits/hour
```

Rounded **up**: a transcription is one discrete job, not a stream with a next
tick to carry a remainder into.

| Audio | Cost to us | Charged | Credits |
|---|---|---|---|
| 30 min | $0.129 | $0.161 | 17 |
| 1 hour | $0.258 | $0.3225 | 33 |
| 10 hours | $2.58 | $3.23 | 330 |

## What it replaced, and how it hid

The rate was a flat `5` credits an hour — **$0.05** — hardcoded, with no cost
recorded anywhere behind it. Against $0.258 of Deepgram bill that recovered under
a fifth of what the hour cost, losing about **$0.21 per hour transcribed**.

It did not need Deepgram's price list to be spotted. The giveaway was internal:

| With 1,000 credits (the Business monthly allowance) you could buy |
|---|
| 27 minutes of voice assistant |
| 44 minutes of help assistant |
| 16 hours of recording |
| **200 hours of transcription** |

Recording an hour cost 60 credits. Transcribing that same hour cost 5 — twelve
times less, for the service with an external per-minute bill attached. Every
other rate in the product is derived from a documented cost; these flat ones were
not, which is exactly why this one drifted so far without anything failing.

## Still unanchored

Three sibling rates remain flat numbers with no cost basis written down:

| Rate | Charged | Cost basis |
|---|---|---|
| Recording | 1 credit/min | none recorded |
| Transcript translation | 2 credits / 1,000 words | none recorded |
| Insight query | 3 credits | none recorded |

Translation and insights run on Groq (`openai/gpt-oss-20b`), which is cheap enough
that a wide margin is plausible — but *plausible* is what the transcription rate
looked like too. Each should be given the same treatment: measure the real cost,
put it in an env var, derive the rate.

## Env

| Variable | Default | Meaning |
|---|---|---|
| `DEEPGRAM_COST_PER_MINUTE` | `0.0043` | Raw per-minute cost, from the Deepgram console. |
| `DEEPGRAM_MARKUP_PERCENT` | `25` | Markup as a percentage, same convention as the engine rates. |

Both are read at boot. Changing the rate is a variable change, not a release.
