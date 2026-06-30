# Changelog

All notable changes to the VoxTranslate server are documented here. The version
is the umbrella release version (anchored on this crate's `Cargo.toml`). This log
starts at 1.11.4; for earlier history see the `vX.Y.Z` git tags.

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
