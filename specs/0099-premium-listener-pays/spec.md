# 0099 — Premium = listener-pays (you pay for the quality YOU receive)

| | |
|---|---|
| **Status** | Design — approved model, implementation NOT started |
| **Owner** | micio86dev |
| **Created** | 2026-06-17 |
| **Depends on** | 0093 (engine registry), 0094 (capacity fallback) |

## 1. Problem

Today Premium is **speaker-pays**: when a Premium user speaks, *their* audio is sent
to OpenAI and the **listeners** receive the premium translated voice + subtitles
(confirmed in prod logs: speaker `it`, `targets:["es"]` → the `es` listener gets the
OpenAI output). So the person who PAYS does not experience the benefit — others do.
Users find this inverted; the intuitive model is **listener-pays**: *I pay Premium →
I receive the OpenAI-quality translation of everyone, in my language.*

## 2. Approved model (decision 2026-06-17)

**Listener-pays, per-listener engine.** Each participant's engine choice means "the
quality I want to RECEIVE." When speaker S (lang `src`) talks, for **each listener L**
(lang `tgt`, engine `E`): translate `src→tgt` with **E**, deliver to L, and bill **L**.
Same target language with mixed listeners runs **both** pipelines (a Premium listener →
OpenAI, a Standard listener → Deepgram+Groq) — "ognuno il suo engine". De-dup streams
by `(tgt_lang, engine)`.

## 3. Current architecture (what must change)

- **Engine is per-SPEAKER** (`lib.rs`: `active_engine = engines.resolve(speaker's engine param)`),
  and `active_engine.start_session(speaker_ctx)` translates the speaker's audio to ALL
  targets with that one engine.
- **Billing is per-SPEAKER**: `spawn_meter(billed_user=speaker, rate=active_engine.rate, scale=targets)`.
- Premium engine (`engine/premium.rs`): `start_session` opens one OpenAI session per
  target, fanning the speaker's PCM to each.

## 4. Design

### 4.1 Engine resolution → per (target_lang, engine)
Room state must expose, per participant, `(lang, engine)`. When S speaks, compute the
set of distinct **`(tgt_lang, engine)`** pairs across the OTHER participants (excluding
S's own lang). For each pair, run the matching engine on S's audio `src→tgt`:
- `engine = premium` → an OpenAI session (`src→tgt`); deliver to that lang's Premium listeners.
- `engine = standard` → Deepgram+Groq `src→tgt`; deliver to that lang's Standard listeners.

Delivery must be **engine-aware**: today `broadcast_to_lang(room, lang, msg)` hits everyone
of a lang. New: deliver a translation to listeners of `(lang, engine)` only — add the
engine dimension to the peer/recipient filter.

### 4.2 Premium engine
`PremiumEngine` becomes driven by **listener demand**: it opens an OpenAI session for
each `(tgt_lang)` that has ≥1 Premium listener (not "all targets"). Capacity permits are
acquired per such session (unchanged mechanism). Capacity fallback (0094): if no permit,
that target's Premium listeners fall back to the Standard stream for `tgt`.

### 4.3 Billing (the hard part — correctness-critical)
Flip from speaker-metering to **listener-metering**. A Premium listener is billed for the
time they RECEIVE premium translation, i.e. while any other participant is speaking and
being translated into the listener's language via OpenAI. Concretely: meter per
**(listener, source-speaker)** active-translation time at the **listener's** engine rate;
charge the listener's balance. Guests can't be billed → guests can't choose Premium-receive
(or receive only Standard). Pre-join credit check moves to the listener.

### 4.4 Pricing
Reword from "per target language (speaker)" to **per source you listen to** /
per-minute-of-premium-listening. Update the engine descriptions (i18n) accordingly.

### 4.5 UI
Engine picker copy: "Standard / Premium — the quality of translations **you receive**."
The `engineDesc*` i18n strings (spec 0236) need rewording in all 8 languages.

## 5. Rollout & test plan (do NOT ship untested)
1. Implement behind the existing engine registry; keep speaker-pays code path until the
   listener path is proven, to allow a clean cutover.
2. Unit-test the `(tgt_lang, engine)` routing resolver (pure).
3. Integration-test: 1:1 it↔es with listener engines {std, premium} in each combination →
   assert each listener receives the right engine's output; assert billing hits the
   **listener**, not the speaker.
4. Capacity fallback test (Premium listener, no permit → Standard).
5. Manual prod-preview verification of audio/subtitles + a billing dry-run before enabling.

## 6. Risks
- **Billing correctness on a live paid product** — highest risk; needs careful metering +
  tests + a dry run.
- **OpenAI cost/load doubles** in mixed rooms (per-listener-engine) — acceptable per the
  approved model, but watch the capacity cap (`OPENAI_REALTIME_MAX_SESSIONS`).
- Larger surface: core WS loop + engines + billing + pricing + UI — multi-step, reviewed.

## 7. Status note
This is a substantial re-architecture of the real-time + billing path. It will be built
incrementally with tests and deployed only once the routing + billing are proven —
**not** a hot-patch.

## 8. Implementation progress (2026-06-17)

Decisions locked with the owner: model = listener-pays (approved); **audio = surgical**
— all-Standard rooms keep Opus/WebM (spec 0043 intact); the speaker switches to PCM16 and
Standard uses Deepgram `linear16` **only when ≥1 Premium listener is present**, confining
the ~12× STT-bandwidth cost to rooms where someone already pays Premium. Rollout behind an
OFF-by-default config flag (`LISTENER_PAYS`); live path stays speaker-pays until the dry-run.

**DONE + committed + tested (branch `feat/premium-listener-pays`):**
- Foundation (`c4cc707`): `Peer.engine` (the quality a peer RECEIVES; guests pinned to
  default), `Peer.speaking`; pure `routes_for_speaker` + live `translation_routes`
  ((tgt_lang, engine) routes — same lang/mixed engines ⇒ both); `broadcast_to_lang_engine`
  (delivery filtered by lang+engine); `set_speaking`/`active_source_count` (listener-meter
  primitive). 7 unit tests.
- Billing flip primitive (`8601cae`): `MeterScope::{Speaker,Listener}`; Listener scope bills
  the listener at their engine rate × `active_source_count`. Live path still Speaker scope.
- Core-loop scaffolding (`7e43e43`): `Config.listener_pays` (env `LISTENER_PAYS`, OFF by
  default, `env_flag` helper, all test fixtures updated); `rooms.target_langs_for_engine`
  (engine-filtered targets — the listener-pays analog of `get_room_languages`). +1 test.
- Engine threading (`cfcfcef`): `SessionDeps{listener_pays,pcm_input}`; `deepgram::AudioFormat`
  {WebmOpus, Pcm16} → `open_deepgram_ws`/`detect_language` take a format (Pcm16 = Deepgram
  `linear16`); Standard `process_transcripts` and Premium `SessionReader` self-filter targets
  (`target_langs_for_engine`) + delivery (`broadcast_to_engine_or_peer` / `broadcast_to_lang_engine`)
  when `listener_pays`. `rooms.broadcast_to_engine_or_peer` (+test).
- Core WS loop (`3ec3da2`): `Start` runs Premium (premium-listener langs) + Standard (always)
  on one captured stream fanned to `audio_feeds`; surgical `pcm_input = any premium listener`;
  capacity fallback → Standard serves everyone; `set_speaking` on Start/Stop. Billed listeners
  metered the WHOLE connection at their receive-rate (`spawn_listener_meter` at JOIN,
  `MeterScope::Listener` reads live lang); guests keep the per-Start cap. Exhaust → drop the
  listener to free Standard (`set_peer_engine`) + targeted `EngineDowngraded`; billing stops.

- Capture-format signalling (`abfde12`): `ServerMessage::CaptureFormat{pcm}` pushed per peer
  (pcm iff a Premium listener other than them is present); `rooms.has_engine_listener` /
  `peer_engines`; `notify_capture_formats` on join/leave/exhaust. Core-loop `pcm_input` now
  uses the SAME room-level condition, so server + client always agree on the wire format.
- Client (`b63f277`): `capture_format` handler → `applyCaptureFormat` swaps PcmCapture↔
  AudioCapture; `serverCaptureFormat` flags listener-pays mode; `engine_downgraded` made
  mode-aware (self-downgrade = receive-engine change, no capture swap). astro check clean,
  251 client tests green. Subtitle/audio rendering needs no change (server routes per-engine).

**✅ FUNCTIONALLY COMPLETE END-TO-END (steps 1–6), all gated behind `LISTENER_PAYS` (OFF in
prod). Live speaker-pays path byte-identical. 184 lib + 251 client tests green.**

**REMAINING — launch prep, do WITH the owner dry-run:**
7. **i18n + pricing copy** (8 langs): the engine descriptions are already mode-neutral; the one
   speaker-pays string is `engineCostPerLanguage` ("per translation language"). To reframe it as
   "per source you listen to" the client must know the mode — so **expose `listener_pays` to the
   client** (e.g. on `/api/engines` or `/api/config`) and switch the picker copy + cost line on
   it. Deferred so neither mode shows wrong copy before the flag flips.
8. **Validation + dry-run**: the pure routing/billing logic is unit-tested (resolver,
   active_source_count, meter scope); a full WS integration test needs engine mocks + live
   Deepgram/OpenAI. The real gate is a **billing dry-run with real services** before flipping
   `LISTENER_PAYS` on. Resolve the two documented items then: capacity-fallback premium billing;
   a hard cap for a Standard listener who exhausts.

## 9. Re-base onto main + Gemini (2026-06-18) — IN PROGRESS

The branch was 13 commits behind main and predated **Gemini** (spec 0100). Re-based on
`feat/premium-listener-pays`:

**DONE + committed + pushed (flag OFF = prod byte-identical, 200 lib tests green):**
- `27c9189` Merge main: reconcile (#262), premium.rs↔pro.rs rename (**premium=Gemini,
  pro=OpenAI**; ids frozen: `OPENAI_ID="premium"`, `GEMINI_ID="gemini_live_translate"`),
  Gemini as the Premium tier, the translated-voice reconnect fixes (#264), gpt-oss docs.
  Kept the branch's engine-agnostic primitives (rooms routing, `MeterScope::Listener`,
  `LISTENER_PAYS` flag, `CaptureFormat`, deepgram `linear16`). lib.rs/engines resolved to
  main's structure; listener-pays re-applied on top.
- `9b78fe4` Listener-pays routing re-applied to ALL 3 engines (was 2, pre-Gemini):
  pro.rs (OpenAI/`OPENAI_ID`) + premium.rs (Gemini/`GEMINI_ID`) compute targets via
  `target_langs_for_engine(room, speaker, ENGINE_ID)` and deliver via
  `SessionReader::deliver` → `broadcast_to_lang_engine(.., ENGINE_ID, ..)` when
  `deps.listener_pays`. Integrated with the live reconcile (start_session AND reconcile tick).

**REMAINING — the lib.rs N-engine core loop (the billing-critical heart; do focused + tested):**
The branch's `lib.rs` core loop was DISCARDED in the merge (took main's reconcile lib.rs)
and must be re-applied **generalized from 2 engines to N**. Blueprint = pre-merge tip
`e5f11d5:server/src/lib.rs` (`notify_capture_formats`, `spawn_listener_meter`, the
`if state.config.listener_pays` Start block, the `audio_feeds` fan-out, `set_speaking`,
join-time `receive_engine` + listener meter). Generalization rules:
- "PCM-input engines" = registry engines with `capabilities.translated_audio` (OpenAI +
  Gemini), NOT hardcoded `PREMIUM_ID`. `pcm = any(has_engine_listener(room, peer, eid))`
  over those ids — same set in `notify_capture_formats` so client+server agree.
- Start (listener-pays): run **each** translated_audio engine a cross-language listener chose
  (`translation_routes`), push its feed; then Standard ALWAYS with
  `listener_pays = any_premium_started` (true → serve only Standard listeners; false → serve
  everyone = capacity fallback). Fan captured audio to all `audio_feeds`; `set_speaking(true)`.
- Bill the LISTENER from join (`spawn_listener_meter`, `MeterScope::Listener` at the peer's
  receive-engine rate); guests keep the per-Start speaking cap.
- Then step 7 (client i18n/pricing copy) + step 8 (dry-run). Known-deferred edge cases (§8):
  capacity-fallback premium billing; Standard-listener exhaust cap.

**✅ DONE (2026-06-18, committed + pushed): the lib.rs N-engine core loop is re-applied.**
`notify_capture_formats`/`spawn_listener_meter` + the Start/binary/Stop/exhaust/PeerLeft
listener-pays branches, generalized to N engines (capability-driven, not hardcoded ids).
Flag OFF = prod byte-identical; fmt+clippy clean; 200 lib + all integration + 256 client
tests pass. **Verified end-to-end locally** (`LISTENER_PAYS=1`, guest-premium bypass): a
speaker on Standard whose LISTENER chose Gemini → the listener received **Gemini**-quality
translation (subtitles + voice) and the speaker was told to capture PCM. The listener's
choice drove the quality — listener-pays confirmed.

**Refinement (MVP carry-over from the original branch, validate/decide at the dry-run):** the
**engine SET** for a speaker is chosen at their `Start` (which engines run = the listeners
present then). LANGUAGES reconcile live (the per-engine reconcile from #262), but a listener
who joins / changes their receive-engine *mid-speech* isn't picked up until the speaker
re-Starts (mute→unmute). Fine for the dry-run (engines chosen pre-join). A full engine-set
reconcile (timer in the WS loop, start/stop engines + restart Standard when the premium set
flips its `listener_pays` flag) is the follow-up — share the structure with the language
reconcile.
