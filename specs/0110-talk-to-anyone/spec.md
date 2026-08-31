# 0110 — Talk to Anyone (face-to-face on one device)

| | |
|---|---|
| **Status** | 🚧 Implemented, pending live verification |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-08-27 |
| **Shipped** | — |
| **Version** | — |
| **Depends on** | [0093](../0093-premium-translation-engine/spec.md), [0043](../0043-low-latency-capture/spec.md), [0102](../0102-language-first-ux/spec.md) |

## 1. Context & Problem

Every translation surface VoxTranslate has today assumes **two devices**: a room of
symmetric peers, each declaring one `lang`, each with their own microphone. Travel is the
opposite shape — two people, one phone on a café table, no earbuds, no room code, no
second account.

The whole value of this mode is **automatic direction detection**. A "my turn / their
turn" button would make it slower than pointing at a menu. So the product question is
narrow: given one microphone carrying two languages, decide per utterance which language
was spoken, and speak only the other one aloud.

**Decisive finding: no provider will tell us.** Language identification exists on none of
the live paths, and none of them accept a candidate list:

| Tier | Provider | Source-language control |
|---|---|---|
| Standard | Qwen LiveTranslate | ONE optional `input_audio_transcription.language` string; omitted ⇒ the model self-detects, but never reports what it decided |
| Pro | OpenAI gpt-realtime-translate | no source field at all (`openai.rs:107`) |
| Premium | Gemini Live Translate | `translationConfig` carries only the target (`gemini.rs:75`) |
| Enhanced | Cartesia Ink-2 | one `language` param; omitted ⇒ multilingual, no reported result |

Deepgram's `detect_language` survives only on the REST/batch path, which no live tier
uses. So detection is **ours**, and it runs on the ORIGINAL transcript that every engine
already returns — which is why one implementation covers Standard, Pro and Premium
without touching any of them.

## 2. Goals / Non-Goals

**Goals**
- Pick the other person's language, press Start, put the phone down, talk. No turn
  buttons, no room code, no second device.
- One implementation across every server-side tier, keyed on `EngineCapabilities` and
  never on an engine id.
- Never speak a language that was not configured: a third language is silence, not a
  guess.
- No feedback loop, with **event-driven** microphone gating — never a fixed delay after a
  response.
- Reuse the room, the engines, the meter and the picker as they are. No new translation
  architecture, no new billing mechanism.

**Non-Goals**
- No support for the **Enhanced** tier (client-direct): the browser talks to Cartesia
  itself, so the server never sees the frames it would have to gate. It resolves to
  Standard with a visible note. Its selling point — cloning each REMOTE peer's voice — is
  meaningless when both people share one microphone.
- No guests. Like the extension (spec: `extension.rs`), this is billed and needs an
  account.
- No transcript persistence. A face-to-face conversation is treated like an extension
  session, not like a recorded call.
- No simultaneous speech. One microphone produces one mixed stream; one utterance wins.

## 3. Requirements

- **R1 — One choice.** As a signed-in user whose account language is Italian, I open
  `/talk`, choose Spanish, and press Start.
  - *Given* my account carries `users.language`, *then* my own language is shown, not
    asked for.
  - *Given* the picker, *then* it offers every language at least one server-side tier can
    speak, minus my own, searchable, each with flag **and** name.
  - *Given* more than one tier can output it, *then* tier cards appear with the cheapest
    pre-selected, and **client-direct tiers are never listed**.

- **R2 — Automatic direction.** *Given* a conversation configured `it ↔ es`, *when*
  Italian is spoken, *then* the Spanish translation is spoken aloud and the Italian echo
  is discarded; *when* Spanish is spoken, the reverse — with no user action between turns.
  - *Given* a short reply the classifier can only lean towards, *then* it is spoken when
    that lean AGREES with the direction the conversation last settled on — and stays
    silent when it contradicts it, or when the model named a third language, or when the
    text was too short to ask at all. A prior confirms a lean; it never replaces one.
  - *Given* a direction that only survived because the prior agreed, *then* it does NOT
    become the next sentence's prior. Only evidence anchors the conversation, so one weak
    read cannot ratchet a whole conversation the wrong way.
  - *Given* the conversation is stopped or runs out of credit, *then* the prior is
    forgotten: after a pause the next speaker is anyone's guess.

- **R3 — Only the two configured languages.** *Given* German is spoken into an `it ↔ es`
  conversation, *then* nothing is spoken and nothing is captioned. Same for an utterance
  too short or too ambiguous to attribute: the status line reads *Listening…*, never an
  error, and no confidence value is ever shown.

- **R4 — No feedback loop.** *Given* translated speech is audible, *then* microphone
  frames are held; *when* it stops, they resume **on that event**, with no timer.
  - *Given* a person talks over the translation for longer than `BARGE_IN_MS`, *then*
    queued audio is dropped and the microphone reopens immediately.

- **R5 — Partials without duplicates.** *Given* a stream of interim transcripts, *then* a
  revision replaces rather than appends, a repeated final speaks once, and translated
  audio for an unresolved utterance is held (bounded) rather than played on a guess.
  - *Given* both sessions finalize the same sentence over independent sockets, *then*
    each side's final is parked on its own slot and the verdict picks the winner — the
    order they arrive in can never decide which copy survives.
  - *Given* a verdict arrives for a parked sentence, *then* the utterance is closed out
    whether or not anything is released: a sentence whose only parked copy was the echo
    is still over, and a direction must never outlive the sentence it was resolved for.
  - *Given* the losing session's copy of an already-forwarded sentence lands afterwards,
    *then* it is swallowed rather than treated as a new utterance.
  - *Given* both sessions tag their audio with the same source peer id while each keeps
    its own `seq`, *then* the client orders playback per `(speaker, language)`. The
    speaker is not the stream here, and one shared counter let the monotonic-seq guard
    drop every chunk from whichever session had fallen behind — subtitles with no voice.

- **R6 — Honest billing.** *Given* both directions are held open, *then* the meter bills
  two translation streams for the whole conversation, using the existing
  `usage::billable_streams` path with no new mechanism.

- **R7 — Clean teardown.** *Given* I press End (or leave the page), *then* the microphone
  tracks, socket, playback graph, level monitor, wake lock, meter and room peers are all
  released, and the usage session is finalized.

## 4. Design & Architecture

### The room shape

`server/src/extension.rs` already proved that an asymmetric surface can be expressed in
the symmetric room model by registering more than one peer. Talk to Anyone is that with a
second listener:

```text
  source peer    id = "<sid>-src"     lang = "auto"    owns the engine session
  listener USER  id = "<sid>"         lang = <user>    the signed-in user's language
  listener OTHER id = "<sid>-other"   lang = <other>   the person opposite them
```

A `lang = "auto"` speaker is translated into EVERY room language
(`engine/standard.rs:23-30`), so the engine opens one upstream session per side, keeps
both alive through `reconcile_langs`, and the meter counts two streams — all without a
line of engine code. **No engine was modified by this feature.**

Each utterance therefore comes back twice: once translated, and once as a source→source
echo. The echo is both useless and the first turn of the feedback loop, so the handler
gates it.

### Components / files

- `server/src/talk/mod.rs` — the `/ws/talk` handler: language validation, auth, the
  three-peer private room, the meter, and the frame gate. Owns the two listener channels
  so which language a frame belongs to is known from the channel it arrived on — audio
  frames are never parsed.
- `server/src/talk/direction.rs` — `Direction`, `script_hint`, `Resolver`. Three stages,
  cheapest first, and each may abstain: a length floor, a pure Unicode-script vote, then
  one Groq JSON call constrained to the two candidates.
- `server/src/talk/utterance.rs` — partial/revision/duplicate handling and the bounded
  per-language audio hold buffer.
- `server/src/protocol.rs` — `ServerMessage::TalkDirection { spoken, target }`.
- `server/src/metrics.rs` — `voxtranslate_talk_direction_ms`,
  `voxtranslate_talk_direction_total{outcome}`.
- `client/src/pages/talk.astro` — standalone route (never loads `app.ts`).
- `client/src/scripts/talk/{session-machine,conversation,view,page,self-audio-guard}.ts`.
- `client/src/scripts/wake-lock.ts` — the app's first use of the API.
- `client/src/scripts/pcm-capture.ts` — `setGated()`; `pcm-playback.ts` +
  `pcm-playback-worklet.js` — `{ playing }` edges.

### Direction resolution

```text
  original transcript ─┬─ under MIN_RESOLVE_CHARS ──────────────► Unknown (hold)
                       ├─ script_hint: ≥4 chars belonging to
                       │   exactly ONE of the two candidates ────► decided, 0 ms, 0 cost
                       └─ else: one Groq JSON call, 2 candidates ► decided | "other"
```

`script_hint` counts only characters that belong to exactly one candidate. A script both
languages share — Han in a `ja ↔ zh` conversation — is skipped entirely rather than
credited to either side. That is the difference between "I cannot tell" and a confident
wrong answer.

The verdict floor (`MIN_CONFIDENCE = 0.6`), the `"other"` escape hatch, and a nonsense
confidence all resolve to `Unknown`, which means **hold**, never "translate anyway".

### Key decisions

- *Both directions warm, gate the output* (vs. opening one session and switching on a
  flip) — a conversation alternates every few seconds, and a reconcile-plus-connect on
  every turn change would lose the first clause of each one. The cost is honest: two
  interpreter channels are open, so two are billed.
- *Detection from the transcript, not the audio* — the only signal every tier shares, so
  one implementation covers Standard, Pro and Premium and future engines get it free.
- *Direction latched per utterance* — a mid-sentence flip would split one thought across
  two voices.
- *Gate, don't stop, the capture* — `stop()` closes the AudioContext and reloads the
  worklet, costing hundreds of milliseconds and the head of the next utterance.
- *Playback edges from the worklet* — only the worklet knows when audio is actually
  leaving the graph. A timer would either clip the next sentence or leave the microphone
  deaf. It distinguishes end-of-utterance from a mid-word underrun by reusing the file's
  existing tail timeout.
- *Two orthogonal state axes* — phase (session) and activity (utterance). Folding the
  brief's eleven states into one enum produces `PAUSED_WHILE_SPEAKING` and, shortly
  after, state spaghetti.
- *Enhanced falls back rather than being refused* — a dead Start button teaches the user
  nothing; a note explains it.

### Data model / protocol

No migrations, no new tables, no transcript rows. One new `ServerMessage` variant
(`talk_direction`), which is a UI signal only — the gating it reports has already
happened server-side.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S1 | Two-candidate direction resolver (script + Groq), pure and unit-tested | `server/src/talk/direction.rs` |
| S2 | Utterance state: partials, revisions, duplicate finals, bounded audio hold | `server/src/talk/utterance.rs` |
| S3 | `/ws/talk`: three-peer room, meter, frame gate, teardown | `server/src/talk/mod.rs`, `lib.rs`, `protocol.rs`, `metrics.rs` |
| S4 | Capture gate + playback edges (mirrored to the extension worklet) | `pcm-capture.ts`, `pcm-playback.ts`, `pcm-playback-worklet.js` |
| S5 | Session machine, conversation engine, self-audio guard, wake lock | `client/src/scripts/talk/`, `wake-lock.ts` |
| S6 | Page, view, picker, tier cards, home CTA | `talk.astro`, `talk/view.ts`, `talk/page.ts`, `index.astro` |
| S7 | i18n across all 83 locales | `i18n/*.json`, `scripts/merge-talk-i18n.mjs` |

## 6. Testing & Verification

- **Unit (server):** `direction.rs` — a third language is never translated (R3), the
  confidence floor abstains, regional variants collapse to one side, disjoint scripts
  decide for free, a SHARED script is never evidence (`ja ↔ zh`), two Latin languages
  abstain. `utterance.rs` — audio is held then only the winner plays (R2), the echo
  direction never reaches the speakers (R4), the hold buffer is bounded and drops the
  oldest, a duplicate final does not speak twice (R5), an unresolved utterance says
  nothing and resets. `talk/mod.rs` — language validation matches the `/ws` rule, the
  original transcript is read from both subtitle shapes.
- **Integration (server, `tests/integration.rs`):** identical / `auto` / malformed
  language pairs are refused at the handshake; an unauthenticated session is refused with
  `invalid_token` and leaves no room behind (R7).
- **Unit (client):** the full session-machine transition table including every illegal
  transition and the dead-session guard; `SelfAudioGuard` gates on the playback edge with
  no timer, ignores a brief peak, requires continuous energy, and never gates in `open`
  mode (R4); `PcmCapture.setGated` drops frames without tearing the graph down; wake lock
  acquires, re-acquires on `visibilitychange`, and no-ops where unsupported; the
  conversation renders a full exchange, bounds its reconnects, and releases everything on
  end (R7); the view keeps "Listening…" while undecided (R3).
- **e2e (`client/e2e/talk.spec.ts`):** the setup is one choice, a client-direct tier is
  never offered (R1), a signed-out visitor is refused before any microphone prompt, the
  home hero reaches it, it fits a 360 px portrait screen, and axe reports no violations.
- **Manual:** the live pipeline needs a real Qwen key, a microphone and two people, so
  R2/R4 latency and echo behaviour are verified by hand — see the checklist below.

### Manual QA checklist

1. Italian ↔ Spanish · 2. Italian ↔ English · 3. English ↔ Spanish
4. Both speakers alternating quickly · 5. Speaker pauses mid-sentence
6. User speaks while TTS is playing (barge-in) · 7. Loud room · 8. Quiet room
9. Phone volume high · 10. Phone volume low · 11. Microphone next to the speaker
12. Bluetooth speaker connected · 13. Bluetooth microphone connected
14. Microphone permission denied · 15. Network temporarily disconnected
16. Credits run out mid-conversation · 17. App backgrounded · 18. Phone locked
19. Session runs 10+ minutes · 20. Rapid repeated conversations

Items 11–13 and 17–18 are the ones that cannot be automated and matter most: echo
cancellation quality varies by device class, and iOS suspends the AudioContext when
backgrounded.

## 7. Deployment & Operations

- **Env:** none added. Uses the existing `DASHSCOPE_API_KEY` / `GROQ_API_KEY`, and the
  configured `GROQ_TRANSLATION_MODEL` for the direction classifier.
- **Cost:** two translation streams per conversation-minute. See
  [`docs/pricing-talk-to-anyone.md`](../../docs/pricing-talk-to-anyone.md).
- **Metrics to watch:** `voxtranslate_talk_direction_total{outcome="unknown"}` — a rising
  share means people are hearing deliberate silence, which no other metric shows.
- Rollout as usual: server via CI `railway up`, client autodeploys.

## 8. Risks / Open Items

- Two people speaking **simultaneously** produce one mixed stream; one utterance wins.
  A physical limit of one microphone, not a bug.
- Direction resolution needs roughly a clause. Very short utterances stay `Unknown` and
  are not spoken — deliberate (R3), but it is the behaviour most likely to read as
  "it didn't work".
- Close language pairs (`es/pt`, `hr/sr`) may need a second clause before the classifier
  commits.
- Barge-in quality tracks the device's echo cancellation. On a Bluetooth speaker with
  poor AEC the gate holds until playback ends.
- iOS Safari suspends the AudioContext when backgrounded; the session pauses rather than
  continuing. No claim of background operation is made.
- The Qwen per-token prices underlying the Standard rate are marked **unconfirmed** in
  `docs/pricing-standard-qwen.md`. This mode doubles the exposure to that uncertainty.
- `SelfAudioGuard` has an `open` (full-duplex) mode that is implemented but not enabled;
  turning it on needs per-device AEC measurement first.

## 9. References

- Files: `server/src/talk/`, `server/src/protocol.rs`, `server/src/metrics.rs`,
  `client/src/pages/talk.astro`, `client/src/scripts/talk/`,
  `client/src/scripts/wake-lock.ts`
- Tests: `server/src/talk/*` (inline), `server/tests/integration.rs`,
  `client/src/scripts/talk/*.test.ts`, `client/e2e/talk.spec.ts`
- Prior art in-repo: `server/src/extension.rs` (the multi-peer room trick),
  `voxtranslate-chrome-extension/src/state/session-machine.ts` (the pure reducer)
