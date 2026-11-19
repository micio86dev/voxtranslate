# Live Connect: real-device test protocol

Spec 0121 (S8, S18): "Do not claim that every hardware configuration is supported unless verified."

## Status: NOT RUN

**No real-device run of Live Connect has happened.** Every result in this repository so far comes
from automation against a mock translation provider and a synthetic microphone (see "Simulated vs
verified" below). The results table at the end is empty on purpose. The product owner has to run
this protocol with two real phones and fill it in; until then nothing in the product, the marketing
copy or the support answers may say that any hardware combination, headset, distance or room is
supported.

## What you need

- Two phones on **different** networks where possible (one on Wi-Fi, one on cellular), a modern
  Android phone with Chrome and an iPhone with Safari at minimum; a second pair of the other kind
  if you have it.
- An account with credit (about 5 USD is plenty) on the environment under test, and a way to read
  the server's `/metrics` or its logs.
- Two people who speak different languages (or one person who can speak two, for a first pass), a
  quiet room, a noisy room (a TV, a cafe, a street), a tape measure.
- Bluetooth earbuds/headset (standard SBC/AAC, not a vendor-specific profile), wired earbuds,
  nothing at all (loudspeakers).
- A phone screen recorder or a notebook for the notes columns, and a stopwatch.

## The scenario, every row

1. Host: sign in, open `/live`, pick a language, create. Guest: open the link on the other phone,
   pick a language, accept the notice, grant the microphone.
2. Both phones show the other person online. Each person speaks for about a minute, **at the same
   time for at least 10 seconds** (talk over each other on purpose), then alternately in short
   sentences, then one long sentence (15 s).
3. During the run: mute and unmute one phone; turn on airplane mode for ~10 s on one phone and turn
   it off; background the browser for ~10 s; end from the other phone.
4. After the run: note the owner's balance change, and look the conversation up in the dashboard.

## Rows to run

Setup letters: S = phone loudspeaker, B = Bluetooth headset, W = wired headset. "Headphones" in the
page's settings is ticked only when the person really wears them.

| ID | Phone A | Phone B | Distance between the phones | Room | Notes |
|----|---------|---------|-----------------------------|------|-------|
| M1 | S | S | 0.3 m (adjacent) | quiet | the worst case for echo: the speakers feed the other microphone |
| M2 | S | S | 1 m | quiet | |
| M3 | S | S | 3 m | quiet | |
| M4 | S | S | 1 m | noisy | background speech and music |
| M5 | B | S | 1 m | quiet | one side with a headset, the other on the loudspeaker |
| M6 | S | B | 1 m | quiet | same, roles swapped (which phone hosts matters for billing) |
| M7 | B | B | 1 m | quiet | headphones ticked on both |
| M8 | B | B | 1 m | noisy | |
| M9 | W | S | 1 m | quiet | |
| M10 | S | S | different rooms | quiet | the intended use: nobody hears the other phone acoustically |
| M11 | S | S | different networks (Wi-Fi / cellular) | quiet | reconnection and latency on a mobile network |
| M12 | B | B | different rooms | quiet | the best case; the reference for latency |

Add rows for any real combination customers will use (a desk with a counter speaker, a hotel
reception phone). A row is **verified** only when the whole scenario above was run on it.

## What to record in every row

| Metric (spec S18) | How |
|---|---|
| Speech to translated audio, perceived | stopwatch or screen recording: from the end of a short sentence to the first audible translation, 5 samples, record the median and the worst |
| Server-side latency | the delta of `voxtranslate_live_translation_ms` (`_sum` / `_count`) over the run; excludes the phones and the network, so the perceived number must be larger |
| Audio interruptions | count of audible gaps/stutters in the translated speech per minute |
| Reconnection behaviour | after airplane mode: did the phone return by itself, within how many seconds, did the conversation continue (same session) |
| Duplicate translations | same sentence translated or spoken twice (count) |
| Echo handling | did a phone translate or play back its **own** translated playback or the other phone's loudspeaker (self-translation loop); any runaway or ping-pong between the phones |
| Concurrent speakers | during the 10 s of talking over each other: did both directions keep working, was anything dropped |
| Mute | does the other side see it, does the muted phone stay silent for the engine |
| Billing | owner's balance change versus the minutes spoken; the guest must be charged nothing |
| Lifecycle | the ended screen on each side, with the right wording; the conversation appears in the dashboard |
| Subjective quality | 1 (unusable) to 5 (natural) for each direction, and one line why |

Record the model, OS and browser versions of each phone, and the build/commit under test.

## Pass criteria

Set by the product owner before the run, not after. As a starting point for "usable": both
directions work during overlapping speech, no self-translation loop for at least two minutes, no
unprompted audio feedback, reconnection within 15 s, perceived latency the owner is willing to
accept. A configuration that fails is documented as **not supported** (with the workaround: use a
headset, increase the distance, mute when not speaking), not hidden.

## Results

Nothing has been measured. Do not fill this table from the automated tests.

| ID | Date | Phones, OS, browser | Perceived latency (median / worst) | Server latency (mean) | Interruptions/min | Reconnect | Duplicates | Echo / loop | Overlap OK | Quality A to B (1-5) | Verdict | Notes |
|----|------|---------------------|-----------------------------------|-----------------------|-------------------|-----------|------------|-------------|------------|----------------------|---------|-------|
| M1 | | | | | | | | | | | not run | |
| M2 | | | | | | | | | | | not run | |
| M3 | | | | | | | | | | | not run | |
| M4 | | | | | | | | | | | not run | |
| M5 | | | | | | | | | | | not run | |
| M6 | | | | | | | | | | | not run | |
| M7 | | | | | | | | | | | not run | |
| M8 | | | | | | | | | | | not run | |
| M9 | | | | | | | | | | | not run | |
| M10 | | | | | | | | | | | not run | |
| M11 | | | | | | | | | | | not run | |
| M12 | | | | | | | | | | | not run | |

## Simulated vs verified

| | Simulated or automated | Verified with real devices |
|---|---|---|
| Two directions at once, no turn-taking | yes: server tests and browser e2e with a mock provider | **no** |
| Protocol, lifecycle, billing, isolation, retention | yes (see `docs/live-connect.md`, "Testing strategy") | no |
| UI flows (host, guest, desk, staff) | yes: Chromium against the real server, fake media devices | no: no Safari/iOS, no real touch devices, no real permission prompts |
| Provider quality and latency | no: the provider is a mock in every automated run | **no** |
| Microphone | a synthetic tone, never speech | **no** |
| Loudspeaker, Bluetooth, wired headset | headless audio output only | **no** |
| Echo and self-translation loops | the client guard is unit-tested with fake levels | **no**; the server-side echo ledger was deferred until there are measurements from this table |
| Distance, noisy rooms | not modelled | **no** |
| Mobile networks, reconnection on them | the reconnect logic is tested with fake sockets | **no** |
| Server-side latency | measured, but against an instant mock | **no** |
| Load | a 6-socket smoke run against the mock (`loadtest/README.md`) | **no** |

When this table is filled in, move each configuration it proves from "not verified" to a stated
support level in the product documentation, and open an issue for every row that failed.
