# Changelog

All notable changes to the VoxTranslate server are documented here. The version
is the umbrella release version (anchored on this crate's `Cargo.toml`). This log
starts at 1.11.4; for earlier history see the `vX.Y.Z` git tags.

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
