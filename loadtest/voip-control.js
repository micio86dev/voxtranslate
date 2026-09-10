// k6 load test — VoIP control plane (spec 0111, §N).
//
// Measures the part of a translated phone call that VoxTranslate actually owns: the
// authenticated API a dialer talks to. Quoting is the hot path — the dashboard fires one
// on every pause in typing — and it does the whole policy gate plus a rate-deck lookup,
// so it is both the most frequent request and the most expensive one.
//
//   k6 run -e BASE_URL=http://localhost:3001 -e JWT=… -e ORG_ID=… loadtest/voip-control.js
//
// **No call is ever placed.** The script only quotes and reads. Dialing would spend money
// even against a mock provider's ledger, and a load test that can bill the customer is not
// a load test. Point it at a staging deployment running `VOIP_PROVIDER=mock`.
//
// Without a JWT and ORG_ID it degrades to measuring the auth rejection path, which is
// still worth knowing and costs nothing to run.

import http from 'k6/http';
import { check } from 'k6';
import { Rate, Trend } from 'k6/metrics';

const BASE = __ENV.BASE_URL || 'http://localhost:3001';
const JWT = __ENV.JWT || '';
const ORG = __ENV.ORG_ID || '';
const authed = !!(JWT && ORG);

const errors = new Rate('voip_errors');
const quoteTime = new Trend('voip_quote_ms', true);
const historyTime = new Trend('voip_history_ms', true);

export const options = {
  scenarios: {
    control_ramp: {
      executor: 'ramping-vus',
      startVUs: 0,
      stages: [
        { duration: '30s', target: 25 },
        { duration: '1m', target: 100 },
        { duration: '30s', target: 0 },
      ],
    },
  },
  thresholds: {
    voip_errors: ['rate<0.01'],
    // Spec 0111 §"PERFORMANCE TARGETS": call-control API p95 < 500 ms excluding provider
    // network. Quoting never touches the provider, so this threshold is honest.
    voip_quote_ms: ['p(95)<500'],
    voip_history_ms: ['p(95)<300'],
  },
};

// Spread across countries and prefix lengths so the longest-prefix rate lookup is
// exercised rather than one cached row being hit over and over.
const DESTINATIONS = [
  '+393201234567',
  '+8613800138000',
  '+4915112345678',
  '+33612345678',
  '+12125551234',
  '+442071838750',
  '+34600123456',
  '+81312345678',
];

function headers() {
  const h = { 'Content-Type': 'application/json' };
  if (JWT) h.Authorization = `Bearer ${JWT}`;
  return h;
}

export default function () {
  const dest = DESTINATIONS[Math.floor(Math.random() * DESTINATIONS.length)];

  const quote = http.post(
    `${BASE}/api/business/organizations/${ORG}/voip/quote`,
    JSON.stringify({ destination: dest, transcribe: true }),
    { headers: headers(), tags: { name: 'voip_quote' } },
  );
  quoteTime.add(quote.timings.duration);

  // 402 is a legitimate answer — it is the policy gate refusing, which is exactly the work
  // we are trying to measure. 401/403 are the expected answers without credentials.
  const quoteOk = authed
    ? [200, 400, 402, 404].includes(quote.status)
    : [401, 403, 404].includes(quote.status);
  errors.add(!quoteOk);
  check(quote, {
    'quote answered without a server error': (r) => r.status < 500,
  });

  const history = http.get(`${BASE}/api/business/organizations/${ORG}/voip/calls?limit=20`, {
    headers: headers(),
    tags: { name: 'voip_history' },
  });
  historyTime.add(history.timings.duration);
  const historyOk = authed
    ? [200, 404].includes(history.status)
    : [401, 403, 404].includes(history.status);
  errors.add(!historyOk);
  check(history, {
    'history answered without a server error': (r) => r.status < 500,
  });
}
