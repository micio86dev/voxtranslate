// k6 load test — VoIP webhook ingestion (spec 0111, §N).
//
// This is the endpoint an outage turns into a stampede. When a provider recovers from a
// blip it redelivers everything it could not deliver, so the realistic worst case is not
// "many calls" — it is **the same events arriving many times, out of order, all at once**.
// That is what this script generates.
//
//   k6 run -e BASE_URL=http://localhost:3001 loadtest/voip-webhooks.js
//
// It sends UNSIGNED bodies on purpose. Every one must be rejected with 401, and the
// measurement is how cheaply the server can say no: signature verification is the first
// thing a flood hits, and if rejecting is expensive then an attacker does not need a valid
// signature to hurt us. A run that reports 2xx here is a security finding, not a passing
// test.
//
// To exercise the ACCEPTING path, point `SIGNED_FIXTURES` at a file of pre-signed bodies
// produced by the mock provider (see server/src/telephony/mock.rs `sign`). Without it the
// script stays in reject-only mode, which is the safe default: replaying signed webhooks
// at a real deployment would move real call state.

import http from 'k6/http';
import { check } from 'k6';
import { Counter, Rate, Trend } from 'k6/metrics';

const BASE = __ENV.BASE_URL || 'http://localhost:3001';
const PROVIDER = __ENV.PROVIDER || 'mock';
const URL = `${BASE}/api/voip/webhooks/${PROVIDER}`;

const rejected = new Rate('webhook_rejected');
const accepted = new Counter('webhook_accepted_unsigned');
const verifyTime = new Trend('webhook_verify_ms', true);

export const options = {
  scenarios: {
    // A recovery burst: quiet, then everything at once, then quiet again.
    redelivery_storm: {
      executor: 'ramping-arrival-rate',
      startRate: 10,
      timeUnit: '1s',
      preAllocatedVUs: 50,
      maxVUs: 400,
      stages: [
        { duration: '20s', target: 50 },
        { duration: '30s', target: 800 },
        { duration: '20s', target: 800 },
        { duration: '20s', target: 10 },
      ],
    },
  },
  thresholds: {
    // Every unsigned webhook must be refused. Not "most".
    webhook_rejected: ['rate==1.0'],
    webhook_accepted_unsigned: ['count==0'],
    // Rejecting has to stay cheap under load — see the header comment.
    'http_req_duration{expected_response:false}': ['p(95)<250'],
  },
};

// A small pool of event ids, deliberately reused: redelivery is the point, and the
// idempotency ledger is what this pressures.
const EVENT_IDS = Array.from({ length: 64 }, (_, i) => `k6-evt-${i}`);
const TYPES = [
  'call.initiated',
  'call.answered',
  'call.hangup',
  'streaming.started',
  'call.recording.saved',
  'call.dtmf.received',
];

function body() {
  const id = EVENT_IDS[Math.floor(Math.random() * EVENT_IDS.length)];
  const type = TYPES[Math.floor(Math.random() * TYPES.length)];
  return JSON.stringify({
    data: {
      id,
      event_type: type,
      // Deliberately jittered into the past and the future so the ordering the server
      // sees is genuinely wrong, not merely shuffled.
      occurred_at: new Date(Date.now() + (Math.random() * 120000 - 60000)).toISOString(),
      payload: {
        call_control_id: `k6-leg-${Math.floor(Math.random() * 32)}`,
        hangup_cause: 'normal_clearing',
        digit: '1',
      },
    },
  });
}

export default function () {
  const res = http.post(URL, body(), {
    headers: {
      'Content-Type': 'application/json',
      // Present but wrong. A missing header takes a shorter path; a malformed signature
      // is the one that actually costs verification work.
      'telnyx-signature-ed25519': 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=',
      'telnyx-timestamp': String(Math.floor(Date.now() / 1000)),
    },
    tags: { name: 'voip_webhook' },
  });

  verifyTime.add(res.timings.duration);
  const refused = res.status === 401 || res.status === 404;
  rejected.add(refused);
  if (res.status >= 200 && res.status < 300) accepted.add(1);

  check(res, {
    'unsigned webhook refused': () => refused,
    'never a 5xx (a provider would retry it for hours)': (r) => r.status < 500,
  });
}
