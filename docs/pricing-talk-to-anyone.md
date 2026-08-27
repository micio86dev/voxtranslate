# Pricing — Talk to Anyone (spec 0110)

**Short version: a Talk to Anyone minute costs twice a one-language minute on the same
tier, because two translation streams are open the whole time.**

No billing mechanism was added or changed for this feature. The number falls out of the
existing meter; this document exists so the reason is written down before anyone is
surprised by an invoice.

## How it is counted

The meter is unchanged (`server/src/usage.rs`):

```
charge per tick = rate_per_second × interval × billable_streams
```

- `rate_per_second` = `EngineMetadata::user_rate_per_second()` = `cost × (1 + markup) / 60`.
- `billable_streams(targets, speaker_lang, scale_per_language)` returns the number of
  distinct target languages, excluding the speaker's own.

A Talk to Anyone session registers a source pseudo-peer with `lang = "auto"` and **two**
listener peers (spec 0110 §4). `"auto"` matches neither target, so nothing is filtered
out and the count is **2**.

That is not an accounting quirk — it is what is actually happening upstream. Two realtime
sessions are open for the whole conversation, one per direction, because a face-to-face
conversation changes speaker every few seconds and closing the idle direction would lose
the first clause of every turn.

## The numbers

At the current Standard defaults (`QWEN_COST_PER_MINUTE = 0.0036`, 25% markup):

| | Per stream | Talk to Anyone (2 streams) |
|---|---|---|
| Standard (Qwen) | $0.0045/min | **$0.0090/min** |
| Premium (Gemini) | $0.0345/min | **$0.0690/min** |
| Pro (OpenAI, placeholder rate) | $0.4500/min | **$0.9000/min** |

A ten-minute conversation on Standard is **$0.09**.

For comparison, a 1:1 call between two devices costs one stream per speaker, and each
speaker pays their own — so a Talk to Anyone minute costs the same as a two-language 1:1
call minute, with one account paying for both halves. Which is exactly right: one person
opened the app, and both directions are being translated for them.

## What the user is told

The setup screen carries `talkCostNote`:

> Both languages stay ready the whole time, so a conversation is charged as two
> translation streams.

The tier cards show the same per-minute rate and the same "per translation language"
note the call picker uses (`cost_scales_per_language`), so the doubling is derivable from
the UI without doing arithmetic in your head.

## Metering boundaries

- The meter starts on the first `start` control frame, **not** on connection. Opening the
  setup screen and never pressing Start costs nothing.
- It is cancelled on `stop`, on credit exhaustion, and on teardown. Billing never
  outlives the audio.
- A signed-out visitor cannot start a session at all — this is billed, with no guest tier,
  the same rule the Chrome extension follows.

## Inherited caveat

`docs/pricing-standard-qwen.md` marks the underlying Qwen per-token prices as
**unconfirmed** ("read it in the Model Studio console for your region and set
`QWEN_COST_PER_MINUTE` accordingly before launch"). Talk to Anyone doubles the exposure to
that uncertainty, so confirm the rate before promoting this mode.

## If two streams ever becomes too expensive

The economy option, deliberately not implemented: let the direction resolver update the
source peer's live language, so `billable_streams` filters one side out and the engine's
reconcile tick closes the idle session. That halves the cost and costs ~1 s of reconcile
plus a connect on every speaker change — which, in a real back-and-forth, means losing the
first clause of most turns. It is a real trade, not a free win, and it should only be
taken with usage data in hand.
