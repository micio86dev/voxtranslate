// k6 load test — HTTP endpoints (spec 0027).
//
// Hits only endpoints the server answers itself; NONE of these touch Deepgram or
// Groq, so the run costs nothing and measures our server's capacity, not theirs.
//
//   k6 run -e BASE_URL=http://localhost:3001 loadtest/http.js
//
// Boot the server in guest mode first (see loadtest/README.md):
//   DEEPGRAM_API_KEY=dummy GROQ_API_KEY=dummy PORT=3001 cargo run --release

import http from 'k6/http';
import { check, sleep } from 'k6';
import { Rate } from 'k6/metrics';

const BASE = __ENV.BASE_URL || 'http://localhost:3001';
const errors = new Rate('http_errors');

// Endpoints the server resolves locally (no external API calls). In guest mode
// (no DATABASE_URL) the billing endpoint returns 503 — that's a valid response,
// not an error, so we accept it.
const ENDPOINTS = [
  { path: '/health', ok: [200] },
  { path: '/rooms', ok: [200] },
  { path: '/api/ice', ok: [200] },
  { path: '/api/content/i18n', ok: [200] },
  { path: '/api/billing/packages', ok: [200, 503] },
];

export const options = {
  scenarios: {
    http_ramp: {
      executor: 'ramping-vus',
      startVUs: 0,
      stages: [
        { duration: '30s', target: 50 },
        { duration: '1m', target: 200 },
        { duration: '30s', target: 0 },
      ],
    },
  },
  thresholds: {
    http_req_duration: ['p(95)<300', 'p(99)<800'],
    http_errors: ['rate<0.01'],
  },
};

export default function () {
  for (const ep of ENDPOINTS) {
    const res = http.get(`${BASE}${ep.path}`, { tags: { endpoint: ep.path } });
    const ok = check(res, {
      [`${ep.path} status ok`]: (r) => ep.ok.includes(r.status),
    });
    errors.add(!ok, { endpoint: ep.path });
  }
  sleep(1);
}
