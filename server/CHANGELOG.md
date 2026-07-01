# Changelog

All notable changes to the VoxTranslate server are documented here. The version
is the umbrella release version (anchored on this crate's `Cargo.toml`). This log
starts at 1.11.4; for earlier history see the `vX.Y.Z` git tags.

## [1.12.1] — 2026-07-01

### Fixed
- **GFW probe flagged every user as restricted (regression from 1.12.0).** The
  China reachability probe (`restricted-net.ts`) fetches two hosts, but neither was in
  the client CSP `connect-src`, so the browser blocked both fetches — indistinguishable
  from a network failure — and every prod user (incl. non-China) was treated as
  restricted: the Enhanced tier was hidden and `iceTransportPolicy` was forced to
  `relay`. Added `https://www.google.com` + `https://api.cartesia.ai` to `connect-src`,
  switched the canary from `gstatic` (often reachable inside China) to
  `www.google.com/generate_204`, and added a CSP test guarding the probe hosts.

## [1.12.0] — 2026-07-01

### Added
- **China / Great-Firewall corridor: survivable TURN + Enhanced tier gate.**
  Additive and scoped to detected China-side clients — non-China behavior is
  byte-for-byte unchanged.
  - Restricted TURN profile (`TURN_TLS_*`): `GET /api/ice?restricted=1` returns a
    `turns://…:443` TLS-on-443 relay (managed Asia PoP), parsed independently of the
    default relay's Cloudflare mode (which offers no `:443` endpoint). Off unless
    configured; without the flag the response is unchanged. (#331)
  - Client reachability probe (`restricted-net.ts`) — reachability, not geo-IP, so
    it's robust to VPN/geo-spoofing — that requests the `:443` profile and forces
    `iceTransportPolicy: 'relay'` for GFW-restricted peers, whose direct UDP
    candidates the firewall resets. Fails open. (#331)
  - The client-direct Enhanced (Cartesia) tier — the browser connects straight to a
    GFW-blocked domain — is gated to server-proxied Standard for restricted clients,
    reusing the existing degradation path; new `engineRestrictedNetwork` string
    translated across all 84 locales. (#332)
  - Ops runbook + dependency-free TURN Allocate validator under `infra/turn/`. (#331)

## [1.11.9] — 2026-06-30

### Added
- **Durable RLS lockdown for new public tables.** Supabase re-flagged
  `rls_disabled_in_public` on staging + prod: the sqlx migrations already lock
  every app table, but new Directus collections (and any table created after the
  last manual lockdown) land RLS-off. New `infra/supabase/rls-lockdown-cron.sql`
  installs `public.enforce_public_rls()` and a nightly `pg_cron` job that enables
  RLS on any RLS-off public table and revokes `anon`/`authenticated` — idempotent,
  and locks down immediately on first run. A secret-guarded
  `POST /api/admin/rls/enforce` endpoint plus a Directus `collections.create` Flow
  (`directus/setup-rls-lockdown-flow.mjs`) lock a freshly-created collection within
  seconds rather than waiting for the nightly job.

## [1.11.8] — 2026-06-30

### Fixed
- **Diarized transcripts showed "Speaker 1/2/…" instead of real names.** The
  post-call transcription labelled each utterance by Deepgram's integer
  voice-cluster index and stored that, so the dashboard transcript view and the
  TXT/PDF exports never showed who actually spoke — even though the names are
  captured live in `transcript_events`. Each diarized cluster is now attributed
  to a real participant by matching its utterances against the realtime
  transcript (shared content words, reinforced by timestamp overlap) and taking
  the cluster's top-voted name; a cluster with no confident evidence keeps its
  "Speaker N" placeholder. Applied both in the transcription job (new transcripts
  store real names — search embeddings benefit too) and on the fly when reading/
  exporting, so existing transcripts get real names with no data migration.
  Pre-existing cached translations keep their baked-in labels until re-translated.

## [1.11.7] — 2026-06-30

Follow-ups to the 1.11.6 recording/transcript release.

### Fixed
- **Self-tile flicker in cloud recordings.** `selfRecordingStream()` (and the
  whiteboard source) build a fresh `MediaStream` wrapper around the same track on
  every `syncRoster` tick (~1×/s). The compositor re-bound `srcObject` and replayed
  the `<video>` each time, reloading it (`videoWidth` → 0 for a frame → placeholder)
  so the recorded self tile flickered like the camera was toggling off/on; the audio
  mixer was re-wired on the same cadence. Both now re-bind only when the underlying
  track actually changes.
- **Transcript-status column showed "—" for calls with a live transcript.** The
  call-history query reported only the recording-derived `transcript_status`, so a
  call whose transcript opens fine via the realtime fallback still listed as "—". It
  now reports `live` when there's no recording transcript but realtime speech events
  exist (companion dashboard label in `voxtranslate-dashboard` 0.8.3).

## [1.11.6] — 2026-06-30

Bug-fix release for the B2B dashboard's call recordings and transcripts (reported
from production: calls visible in History but with no transcript/recording, and an
empty project detail).

### Fixed
- **Recorded business calls were saved locally instead of uploaded to the cloud.**
  Ending a recorded call via hang-up nulled `activeSessionId` during teardown
  *before* the async `stopRecording()` resolved, so the cloud-upload gate failed
  and the recording fell back to a local download — only manual mid-call stops
  uploaded. `stopRecording` now captures the session id synchronously and the stop
  is initiated before `activeSessionId` is cleared.
- **Dashboard showed "no transcript" for calls that were never cloud-recorded.**
  The transcript endpoint only read the recording-derived `transcripts` row. It now
  falls back to reconstructing the transcript from the realtime `transcript_events`
  captured during the call (the same one shown in the call app) and returns
  `source: "live"` so the dashboard hides the recording-only tools.

### Dashboard (companion release `voxtranslate-dashboard` 0.8.2)
- Project detail now lists the project's calls (was a hardcoded placeholder).
- Renders `source: "live"` transcripts; "Enable push" CTA hidden when push is
  already enabled.

## [1.11.5] — 2026-06-30

Bug-fix release for three production issues reported from a live call.

### Fixed
- **Cloud recording `403 — not enabled for this call`.** `recording/upload-url`
  read only the `call_sessions.cloud_recording_enabled` snapshot, which
  `session_started` writes once (`ON CONFLICT DO NOTHING`) and never updates while
  the room stays occupied (the `session_id` is stable). A call that materialized
  before recording was enabled was therefore permanently un-recordable. It now
  authorizes on the frozen flag **OR** the room's current `room_business_bindings`
  entry; the paid-feature gate is unchanged (only `bind` sets it, behind an active
  subscription). Regression test added.

### Client (umbrella release)
- Gate joining a call on a WebRTC-support check: in-app browsers / restricted
  WebViews and insecure contexts that don't expose `RTCPeerConnection` now get a
  translated "open in Chrome/Safari" message on pre-join instead of an uncaught
  `RTCPeerConnection is not a constructor` crash (all 84 locales).
- Service worker no longer resolves `respondWith` to `undefined` on a network
  failure + cache miss (it threw "Failed to convert value to 'Response'" on flaky
  connections); cache bumped v2 → v3.

## [1.11.4] — 2026-06-30

Test-coverage release — **no runtime behaviour change** (the release binary is
identical; only test code, a `#[cfg(test)]` block, and the version string moved).

### Tests
- Added hermetic DB integration suites: `config_more`, `location`,
  `business_audit`, `business_members`, `business_projects`, and `admin`,
  plus unit tests for `business::meetings::build_rrule`.
- Coverage moved 68.1% → 69.6% lines (config.rs 73→98%, business/audit 37→99%,
  business/projects 82→99%, location 43→88%; admin endpoints now covered).

### CI
- Ratcheted the enforced line-coverage floor 68 → 69.
- Reclaim the llvm-cov target dir after the coverage gate so the client
  `npm ci`/build has disk headroom on the GitHub runner.
