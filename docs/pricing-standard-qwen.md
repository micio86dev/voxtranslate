# Standard tier pricing — Qwen LiveTranslate Realtime

How `QWEN_COST_PER_MINUTE` (default **$0.0036**) is derived, and — importantly — the room
size at which the new Standard stops being cheaper than the old one.

> The **token rates** below are authoritative: Alibaba publishes them as billing units for
> `qwen3.5-livetranslate-flash-realtime`. The **price per token** is the working figure
> from the omni-flash-realtime tariff and is NOT confirmed for the livetranslate family —
> read it in the Model Studio console for your region and set `QWEN_COST_PER_MINUTE`
> accordingly before launch.

## Billing units (published)

| Item | Rate |
| --- | --- |
| Audio input | 7 tokens/second |
| Audio output | 12.5 tokens/second |
| Token input | $0.55 / 1M *(to confirm)* |
| Token output | $4.50 / 1M *(to confirm)* |

Audio output is billed at **1.8× the token rate of input**, and output tokens cost ~8×
what input tokens cost. So the spoken translation dominates: it is ~94% of the bill.

## Cost of one target-language session, per minute of speech

A "session" is one WebSocket translating one speaker into **one** target language. A
speaker in a room with three other languages runs three of them.

| Component | Tokens/min | Cost/min |
| --- | --- | --- |
| Audio input (the speaker) | 420 | $0.000231 |
| Audio output (the spoken translation) | 750 | $0.003375 |
| **Total** | | **~$0.0036** |

At the default 25% markup the user rate is **$0.0045/min per target language**.

## Versus the previous Standard (Deepgram + Groq) — read this before shipping

The old pipeline billed a **flat** $0.0080/min however many languages the room needed: one
Deepgram stream served every listener and Groq translation was rounding error. Qwen bills
**per language session**, so the comparison depends on room composition — and it inverts:

| Distinct target languages | Old (flat) | New (per language) | Delta |
| --- | --- | --- | --- |
| 1 | $0.0080 | $0.0036 | **−55%** |
| 2 | $0.0080 | $0.0072 | **−10%** |
| 3 | $0.0080 | $0.0108 | **+35%** ⚠️ |

**A 4-person room with three distinct languages costs ~35% more than it used to.** That is
the tier's worst case today, since Standard's `max_room_size` is 4.

This is a real trade, not a regression to paper over: the room that costs more is also the
one that now gets a translated *voice* per language instead of the browser's
`SpeechSynthesis`. But it must be a decision, not a surprise. The options, if the 3-language
case matters:

1. **Accept it** — 1:1 calls (the common case) get 55% cheaper, and quality rises everywhere.
2. **Cap concurrency** — lower `QWEN_REALTIME_MAX_SESSIONS`; Standard degrades the language
   set rather than rejecting speakers (see `engine/standard.rs`), so cost is bounded but
   some listeners fall back to reading captions.
3. **Text-only for the 3rd+ language** — run `modalities: ["text"]` past a threshold. Output
   text tokens are a fraction of audio tokens. Not implemented; would need a policy hook in
   `spawn_lang_session`.

An earlier revision of this document put the break-even beyond any reachable room size. It
assumed audio output was billed at the same ~427 tokens/min as input; the published rate is
750. The corrected number is above.

## Position in the tier ladder

| Tier | Engine | Cost/min (per language) | Markup | User rate |
| --- | --- | --- | --- | --- |
| Standard | Qwen LiveTranslate Realtime | $0.0036 | 25% | $0.0045 |
| Premium | Gemini 3.5 Live Translate | $0.023 | 50% | $0.0345 |
| Pro | OpenAI GPT-Realtime-Translate | $0.30 (placeholder) | 50% | $0.45 |

`engine::standard::tests::standard_stays_cheaper_than_the_premium_tiers` asserts the
Standard user rate stays below Premium's, so a careless `QWEN_COST_PER_MINUTE` bump fails
CI rather than silently inverting the ladder.

## Knobs

| Variable | Default | Notes |
| --- | --- | --- |
| `QWEN_COST_PER_MINUTE` | `0.0036` | Raw server cost per language-minute. |
| `QWEN_COST_MARKUP_PERCENT` | `25` | Falls back to `ENGINE_DEFAULT_MARKUP_PERCENT`. |
| `QWEN_REALTIME_MAX_SESSIONS` | `32` | Process-wide concurrent-session cap, and the lever in option 2 above. |
| `QWEN_REALTIME_MODEL` | `qwen3.5-livetranslate-flash-realtime` | Also selects the wire dialect — see `engine::qwen::QwenDialect`. |

## Region availability (not a latency question)

Realtime models are **not offered in every Model Studio region**. Verified against the live
`/compatible-mode/v1/models` catalogue:

| Region | Endpoint | Realtime models |
| --- | --- | --- |
| China (Beijing) | `dashscope.aliyuncs.com` | ✅ 25, incl. livetranslate + omni |
| Singapore (International) | `dashscope-intl.aliyuncs.com` | ✅ per docs |
| US (Virginia) | `{workspace}.us-east-1.maas.aliyuncs.com` | ❌ **none** — 89 models, batch ASR + text MT only |

A key issued in a region without realtime authenticates perfectly and then fails at
`session.update` with *"Access to model denied"*. Check the catalogue, not just the key:

```text
cargo run -p voxtranslate-server --bin qwen-catalogue            # primary route
cargo run -p voxtranslate-server --bin qwen-catalogue -- --fallback
```

It queries that region's `/compatible-mode/v1/models` and requires BOTH entries we dial —
the translate model (`QWEN_REALTIME_MODEL`) and the realtime ASR model (`QWEN_ASR_MODEL`),
which backs the original-language transcript in calls and is the *only* model the webinar
path uses. A region carrying just the first gives translated audio with no original
captions and no webinars at all. Exit code 0 = usable, 1 = missing a model, 2 = the check
could not run.
