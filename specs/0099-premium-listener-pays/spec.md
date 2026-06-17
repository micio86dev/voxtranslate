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

**REMAINING (next sessions, in order):**
1. **Config flag** `LISTENER_PAYS` (Config + env), default false.
2. **Standard engine `linear16` mode**: per-session format flag in `engine/standard.rs` +
   `deepgram.rs` streaming params (`encoding=linear16&sample_rate&channels` vs `container=webm`).
3. **Core WS loop** (`lib.rs`, behind flag): on `Start`, resolve `translation_routes`; run
   each demanded engine on the speaker's audio (one captured stream fanned to all sessions),
   premium opening sessions only for langs with ≥1 premium listener; capacity fallback per
   premium target → that lang's premium listeners drop to the Standard stream; deliver via
   `broadcast_to_lang_engine`. Set `speaking` true/false on Start/Stop.
4. **Listener meter wiring**: spawn the `MeterScope::Listener` meter at JOIN (not on Start)
   for billed users; guests pinned to Standard + the existing time cap.
5. **Format-switch signalling**: server tracks "room has ≥1 premium listener"; tells speakers
   to capture PCM16 vs Opus (reuse the engine-downgrade swap path); recompute on join/leave/
   engine change.
6. **Client**: react to the format signal; engine picker = "quality you RECEIVE".
7. **i18n + pricing copy** (8 langs): `engineDesc*` reworded; pricing "per source you listen to".
8. **Tests + dry-run**: 1:1 it↔es each listener-engine combo (right engine output + listener
   billed, not speaker); capacity-fallback; coverage ≥85%; **owner billing dry-run before
   flipping `LISTENER_PAYS` on in prod.**
