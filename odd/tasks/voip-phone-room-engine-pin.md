# voip-phone-room-engine-pin

## Objective
Fix the live production bug for good: on a web-app-initiated VoIP call the phone party
hears translated audio, but the web caller hears NOTHING when the phone party speaks.

## Problem
Hotfix 1.60.4 (`7dd9e420`, `pcmPlayback.unlock()` in the dial submit handler) did not fix
it — the AudioContext was never the cause. Production Railway logs for the call of
2026-09-21 19:47 UTC (room `ph-123ffc5403f14f58a6c978b9eb42bacb`, placed AFTER 1.60.4 was
live) show the real mechanism:

```
19:47:43 warn  a client-direct engine cannot serve a phone leg; substituting the default engine
               requested_engine="cartesia" substituted_engine="standard"   (POST /voip/calls)
19:47:45 info  peer joined  room=ph-… name="Alessandro Micelli" lang="it" peers=2 engine="cartesia"
19:47:46 info  standard: start_session speaker=<web peer> source="it" targets=["es"]      ← web→phone OK
19:47:52 info  standard: start_session speaker="phone-…" source="es" targets="[]"        ← phone→web: nothing
19:47:52 info  standard: reconcile tick … listener_pays=true want="[]" active="[]"
```

- The web caller's browser joins the phone room with `session.engine = request.engine_id
  || selectedEngine` (`app.ts` `enterPhoneCall`). The dial request never carries
  `engine_id`, so the peer joins on the user's SELECTED engine — here `cartesia`
  (Enhanced, client-direct).
- The server refuses a client-direct engine for the telephone (`resolve_for_phone`, the
  2026-09-16 incident fix) and runs the phone leg on `standard`.
- The phone leg's speaker session runs under listener-pays: its target languages are
  the languages of listeners who chose THAT engine. The only listener chose `cartesia`
  → empty target set → the phone party's speech is never translated → silence.
- The opposite direction works because the web speaker's session targets the phone
  peer (`lang=es`, `engine=standard`).

Every phone call this user placed since 2026-09-19 hits the substitution (the warn fires
on every `/voip/quote` and `/voip/calls`), so the bug reproduces 100% for anyone whose
selected engine is client-direct, and the same shape would hit a Pro/Premium phone leg
joined by a browser on Standard.

## Fix
Two layers, one rule: **a room with a telephone in it runs on the telephone's engine.**

1. Client (`app.ts` dial submit): send `engine_id: phoneQuote.engine_id` — the quote
   already carries the server-resolved (substituted) engine — so `enterPhoneCall` joins
   the room on the engine the phone leg actually uses. Pure helper + unit test in
   `phone-call.ts`.
2. Server (`voip/session.rs` `run_leg`): the phone leg opens exactly ONE engine session,
   so it must run with `listener_pays: false` — the same "no premium engine running →
   serve EVERYONE" branch the ordinary speaker path takes in `lib.rs` (`any_premium_ok`).
   `run_leg` currently forwards the raw global `LISTENER_PAYS` flag, which in
   `standard.rs` only does two things: scope targets to `(lang, Standard)` listeners and
   deliver via `broadcast_to_lang_engine`. Neither makes sense for a telephone. Factor
   the deps builder so a unit test asserts it against a `listener_pays = true` config.
   Defence in depth: covers stale clients and a spoofed `?engine=`.

## Constraints
- Hotfix off `main` (`hotfix/1.60.5`), patch bump, merge `main` first then `develop`.
- No new i18n keys (no new user-facing strings).
- TDD strict: RED first. Runners: client `npx vitest run <file>`; server `cargo test -p
  voxtranslate-server <name>`.
- Route: delegated writer (2+ non-trivial files across client and server).

## Tasks
- [x] 1. Root cause confirmed from production logs (above); hotfix branch created.
- [x] 2. Client: pure helper `phoneRoomEngine(quote)` in `phone-call.ts` (RED→GREEN),
      dial submit sends `engine_id` from the quote; `enterPhoneCall` therefore joins on it.
- [x] 3. Server: `run_leg` opens its engine session with `listener_pays: false`
      (RED→GREEN unit test on the factored `phone_session_deps` builder).
- [x] 4. Checks: `npx vitest run` (client), `npx tsc --noEmit`, `cargo test -p
      voxtranslate-server --lib voip::session`, `cargo clippy` clean, `cargo fmt --check`
      clean. (Unscoped `cargo test -p voxtranslate-server` hits this machine's pre-existing
      rust-lld crash on unrelated integration binaries — see evidence below; not caused by
      this change.)
- [ ] 5. Work-unit commit(s) on `hotfix/1.60.5`, native review per RDD.
- [ ] 6. PR → `main` (no version file is bumped on hotfixes; the tag is the version), CI green, merge, tag, back-merge `develop`,
      prune branch.
- [ ] 7. Ask the user to re-test one real call and confirm the phone party is audible.

## Verification evidence
- `npx vitest run src/scripts/phone-call.test.ts`: RED confirmed
  (`phoneRoomEngine is not a function`), then GREEN — 13/13 passed after adding the helper.
- `npx vitest run` (full client suite): 103 files, 2076 tests passed.
- `npx tsc --noEmit`: clean, no output.
- `cargo fmt --check` (server): clean, no output.
- Server RED test `phone_leg_deps_ignore_the_global_listener_pays_flag` written against
  the OLD behaviour (forwarding `state.config.listener_pays`) first: RED confirmed
  (`assertion failed: !deps.listener_pays`). Fix applied (`listener_pays: false`).
- `cargo test -p voxtranslate-server --lib voip::session -- --nocapture`: GREEN — 23/23
  passed (DB-gated ones skip with "no DATABASE_URL", as documented), including
  `phone_leg_deps_ignore_the_global_listener_pays_flag`.
- `cargo clippy -p voxtranslate-server --all-targets -- -D warnings`: clean, no warnings.
- `cargo fmt --check`: clean, no output.
- Note: the unscoped `cargo test -p voxtranslate-server` (no `--lib`) additionally builds
  every integration test binary, and on this machine that hit a pre-existing rust-lld
  segfault (`WordLiteralSection::getLiteral4Offset`) while linking several UNRELATED
  binaries (`invoices`, `friends`, `business_projects`, `user_preferences_api`,
  `business_audit`, `help_assistant_integration`) — a known local linker fragility with
  this crate's large typst/PDF dependency stack on this volume (see `server/.cargo/
  config.toml`'s own comments on `ld`/`lld` size-limit failures here). None of those
  binaries touch `voip::session`; `--lib` scoping (above) is the correct, unaffected way
  to run this change's tests and it is fully green.
