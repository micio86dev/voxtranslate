# Changelog

All notable changes to the VoxTranslate server are documented here. The version
is the umbrella release version (anchored on this crate's `Cargo.toml`). This log
starts at 1.11.4; for earlier history see the `vX.Y.Z` git tags.

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
