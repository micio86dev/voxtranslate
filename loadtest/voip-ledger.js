// k6 load test — credit-ledger contention on one organisation (spec 0111, R9/§N).
//
// **This script is the only one here that creates call rows and moves credits, so it
// refuses to run without an explicit opt-in.** A load test that can bill a customer is
// not a load test.
//
//   k6 run -e BASE_URL=… -e JWT=… -e ORG_ID=… -e ALLOW_DIALING=true loadtest/voip-ledger.js
//
// Run it ONLY against a staging deployment with `VOIP_PROVIDER=mock`, on a throwaway
// organisation, with a small balance. The mock provider places no telephone call and
// contacts no carrier — but the credit ledger it drives is the real one, which is the
// whole point.
//
// ## What it measures
//
// The correctness of the reservation under contention is already proven, deterministically,
// by `two_concurrent_holds_cannot_both_take_the_last_credits` in
// `server/src/voip/reservation.rs`. A load test cannot prove a race is absent; it can only
// fail to trigger one, and treating that as evidence is how races reach production.
//
// So this measures the two things a unit test cannot: **throughput** of `SELECT … FOR
// UPDATE` on one hot organisation row, and whether the ledger stays *coherent* while
// contended. The invariant checked here is the one that matters commercially — the pool
// must never go negative, and it must never be possible for more concurrent calls to start
// than the balance can cover.

import http from 'k6/http';
import { check, fail } from 'k6';
import { Counter, Rate, Trend } from 'k6/metrics';

const BASE = __ENV.BASE_URL || 'http://localhost:3001';
const JWT = __ENV.JWT || '';
const ORG = __ENV.ORG_ID || '';
const DEST = __ENV.DESTINATION || '+393201234567';

const dialTime = new Trend('voip_dial_ms', true);
const held = new Counter('voip_holds_taken');
const refusedForCredits = new Counter('voip_refused_insufficient');
const serverErrors = new Rate('voip_server_errors');

export function setup() {
  if (__ENV.ALLOW_DIALING !== 'true') {
    fail(
      'refusing to run: this script creates calls and moves credits. Re-run with ' +
        'ALLOW_DIALING=true against a staging deployment using VOIP_PROVIDER=mock.',
    );
  }
  if (!JWT || !ORG) {
    fail('JWT and ORG_ID are required.');
  }
  return {};
}

export const options = {
  scenarios: {
    // Everyone at once, on one organisation. The point is the contention, not the ramp.
    hot_org: {
      executor: 'constant-vus',
      vus: Number(__ENV.VUS || 40),
      duration: __ENV.DURATION || '45s',
    },
  },
  thresholds: {
    voip_server_errors: ['rate==0.0'],
    // Contention must not turn into a stall. `FOR UPDATE` on one row serialises by design;
    // this is the budget for that serialisation.
    voip_dial_ms: ['p(95)<1500'],
  },
};

export default function () {
  const res = http.post(
    `${BASE}/api/business/organizations/${ORG}/voip/calls`,
    JSON.stringify({
      destination: DEST,
      source_language: 'en',
      target_language: 'it',
      transcribe: false,
      estimated_minutes: 1,
    }),
    {
      headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${JWT}` },
      tags: { name: 'voip_dial' },
    },
  );

  dialTime.add(res.timings.duration);
  serverErrors.add(res.status >= 500);

  if (res.status === 201) {
    held.add(1);
  } else if (res.status === 402) {
    // The expected steady state once the balance is exhausted: refusals, not overspend.
    const body = res.json();
    if (body && body.error === 'insufficient_credits') refusedForCredits.add(1);
  }

  check(res, {
    'never a server error under contention': (r) => r.status < 500,
    'answered with either a call or a refusal': (r) => r.status === 201 || r.status === 402,
  });
}

export function teardown() {
  // Deliberately does not clean up. The rows it created ARE the evidence: after a run,
  // check on the staging database that
  //
  //   SELECT credits_balance FROM organizations WHERE id = '<ORG_ID>';       -- never < 0
  //   SELECT sum(amount) FROM organization_credits_transactions
  //     WHERE org_id = '<ORG_ID>';                                           -- matches the drop
  //   SELECT count(*) FROM voip_credit_reservations
  //     WHERE org_id = '<ORG_ID>' AND state = 'held';                        -- only live calls
  //
  // A script that tidied up would destroy exactly what has to be inspected.
}
