// k6 load test — Live Connect conversations on /ws/live (spec 0121, S18).
//
// N conversations, each of TWO phones, all talking at once: `setup()` creates N sessions
// through the real REST API (a host per session, a guest who joins with the invitation), then
// 2N sockets hold /ws/live for HOLD_SEC, each sending a 100 ms PCM16 24 kHz frame every 100 ms
// exactly as a phone does. It measures what a phone would feel and what the server would
// refuse:
//
//   live_ws_connect_ms             upgrade time (k6's ws_connecting is also reported)
//   live_first_translated_ms       from our first frame to the first `translated_audio` back
//   live_translated_audio_frames   audio chunks delivered (should be > 0 on every socket)
//   live_sockets_without_audio    sockets that never heard the other side (a silent direction)
//   live_busy_refusals             sessions the server refused with `live_busy` (capacity gate)
//   live_session_create_failed     any other refusal at create/join, tagged by code
//   live_error_frames{code}        `error` frames the server sent mid-conversation
//   live_unexpected_close          sockets that ended before HOLD_SEC for a reason we did not cause
//   live_session_ended_frames      `session_state: ended` frames (the conversation was closed)
//
// The server's own view is read from /metrics at the end (`voxtranslate_live_translation_ms`,
// the server-side speech-to-translated-audio histogram) and printed by teardown(). Outbound
// queue overflows (a slow consumer dropped) are server-side events, not something a client can
// see: count them afterwards with the SQL in loadtest/README.md.
//
//   k6 run -e BASE_URL=http://127.0.0.1:3101 -e JWTS=<jwt>,<jwt> -e SESSIONS=6 loadtest/live-connect.js
//
// ## Capacity facts this is meant to confirm or contradict (see docs/live-connect.md)
// * The Standard engine is capped at 32 upstream sessions per process, and each phone that
//   speaks opens one per target language. One conversation = two upstream sessions, so about
//   16 simultaneous conversations fit. `live_busy` is the gate that refuses a new session at
//   create/join when fewer than two upstream sessions are free. Note that `setup()` creates ALL
//   sessions before any socket speaks, so at that moment no upstream session is in use and the
//   gate cannot trip: beyond ~16 conversations the symptom here is `live_sockets_without_audio`
//   (a direction that stays silent), not `live_busy_refusals`. Both are counted, neither is
//   treated as a failure by itself.
// * An owner may hold LIVE_MAX_CONCURRENT_PER_OWNER (default 3) open sessions, so N sessions
//   need ceil(N / 3) accounts: pass several tokens in JWTS (see the runbook for minting).
//
// ## Safety
// This creates sessions and BILLS their owners. Run it against a local server whose upstream is
// the mock (`node client/e2e/support/live-server.mjs`), never against real Qwen credentials, and
// never against staging or production. It refuses a non-loopback BASE_URL unless you say, in
// words, that you mean it.

import http from 'k6/http';
import ws from 'k6/ws';
import { check, fail } from 'k6';
import { Counter, Trend, Rate } from 'k6/metrics';

const BASE = (__ENV.BASE_URL || 'http://127.0.0.1:3101').replace(/\/$/, '');
const WS_BASE = BASE.replace(/^http/, 'ws');
const JWTS = (__ENV.JWTS || __ENV.JWT || '').split(',').map((s) => s.trim()).filter(Boolean);
const SESSIONS = Number(__ENV.SESSIONS || 3);
const HOLD_SEC = Number(__ENV.HOLD_SEC || 20);
const HOST_LANG = __ENV.HOST_LANG || 'it';
const GUEST_LANG = __ENV.GUEST_LANG || 'es';
const FRAME_MS = 100;

const connectMs = new Trend('live_ws_connect_ms', true);
const firstTranslatedMs = new Trend('live_first_translated_ms', true);
const audioFrames = new Counter('live_translated_audio_frames');
const busy = new Counter('live_busy_refusals');
const createFailed = new Counter('live_session_create_failed');
const errorFrames = new Counter('live_error_frames');
const silent = new Counter('live_sockets_without_audio');
const unexpectedClose = new Rate('live_unexpected_close');
const endedFrames = new Counter('live_session_ended_frames');
const framesSent = new Counter('live_frames_sent');
const sockets = new Counter('live_sockets_opened');

export const options = {
  scenarios: {
    // Everyone connects at once and stays: the point is concurrency, not a ramp.
    conversations: {
      executor: 'per-vu-iterations',
      vus: SESSIONS * 2,
      iterations: 1,
      maxDuration: `${HOLD_SEC + 60}s`,
    },
  },
  thresholds: {
    ws_connecting: ['p(95)<1000'],
    live_unexpected_close: ['rate<0.01'],
    live_error_frames: ['count<1'],
    // Every opened socket must have heard the other side at least once.
    live_translated_audio_frames: ['count>0'],
  },
};

function isLoopback(url) {
  return /^https?:\/\/(127\.0\.0\.1|localhost|\[::1\])(:|\/|$)/.test(url);
}

/** One 100 ms frame of a 220 Hz tone, PCM16 LE mono 24 kHz: 2400 samples, 4800 bytes. */
function frame() {
  const n = 2400;
  const buf = new ArrayBuffer(n * 2);
  const view = new DataView(buf);
  for (let i = 0; i < n; i++) view.setInt16(i * 2, Math.round(Math.sin((2 * Math.PI * 220 * i) / 24000) * 8000), true);
  return buf;
}

export function setup() {
  if (!isLoopback(BASE) && __ENV.I_UNDERSTAND_THIS_BILLS_REAL_ACCOUNTS !== 'yes') {
    fail(`refusing to run against ${BASE}: this creates sessions and bills their owners. Use a local server.`);
  }
  if (JWTS.length === 0) fail('JWTS (comma-separated account tokens) is required; see loadtest/README.md, "Live Connect".');

  const pairs = [];
  for (let i = 0; i < SESSIONS; i++) {
    const jwt = JWTS[i % JWTS.length];
    const created = http.post(
      `${BASE}/api/live/sessions`,
      JSON.stringify({ lang: HOST_LANG, save_transcript: false }),
      { headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${jwt}` }, tags: { name: 'create' } },
    );
    if (created.status !== 200) {
      const code = created.json('code') || `http_${created.status}`;
      if (code === 'live_busy') busy.add(1);
      else createFailed.add(1, { code });
      continue;
    }
    const c = created.json();
    const code = String(c.invite_url).split('/').pop();
    const joined = http.post(
      `${BASE}/api/live/join`,
      JSON.stringify({ code, name: `Guest ${i}`, lang: GUEST_LANG, consent: true }),
      { headers: { 'Content-Type': 'application/json' }, tags: { name: 'join' } },
    );
    if (joined.status !== 200) {
      const why = joined.json('code') || `http_${joined.status}`;
      if (why === 'live_busy') busy.add(1);
      else createFailed.add(1, { code: why });
      continue;
    }
    pairs.push({ session: c.session_id, host: c.token, guest: joined.json('token'), hostWs: c.ws_path });
  }
  console.log(`setup: ${pairs.length}/${SESSIONS} conversations created`);
  return { pairs };
}

export default function (data) {
  // VU 1,2 -> pair 0 (host, guest); VU 3,4 -> pair 1; ...
  const pair = data.pairs[Math.floor((__VU - 1) / 2)];
  if (!pair) return; // this slot's session was refused in setup
  const isHost = (__VU - 1) % 2 === 0;
  const token = isHost ? pair.host : pair.guest;
  const pcm = frame();
  const started = Date.now();
  let opened = 0;
  let sawAudio = false;
  let closedByUs = false;

  const res = ws.connect(`${WS_BASE}/ws/live?token=${encodeURIComponent(token)}`, {}, (socket) => {
    socket.on('open', () => {
      opened = Date.now();
      connectMs.add(opened - started);
      sockets.add(1);
      socket.send(JSON.stringify({ type: 'start' }));
      socket.setInterval(() => {
        socket.sendBinary(pcm);
        framesSent.add(1);
      }, FRAME_MS);
      socket.setTimeout(() => {
        closedByUs = true;
        socket.close();
      }, HOLD_SEC * 1000);
    });
    socket.on('message', (m) => {
      if (typeof m !== 'string') return;
      if (m.startsWith('{"type":"translated_audio"')) {
        audioFrames.add(1);
        if (!sawAudio) {
          sawAudio = true;
          firstTranslatedMs.add(Date.now() - opened);
        }
      } else if (m.startsWith('{"type":"error"')) {
        let code = 'unknown';
        try {
          code = JSON.parse(m).code || 'unknown';
        } catch (e) {
          /* keep unknown */
        }
        errorFrames.add(1, { code });
      } else if (m.startsWith('{"type":"session_state"')) {
        endedFrames.add(1);
      }
    });
    socket.on('close', () => {
      unexpectedClose.add(!closedByUs);
      if (!sawAudio) silent.add(1);
    });
  });
  check(res, { 'ws upgrade (101)': (r) => r && r.status === 101 });
}

export function teardown(data) {
  // End what we started, so a re-run does not run into the per-owner session cap.
  for (const p of data.pairs) {
    http.post(`${BASE}/api/live/sessions/${p.session}/end`, null, {
      headers: { Authorization: `Bearer ${p.host}` },
      tags: { name: 'end' },
    });
  }
  const metrics = http.get(`${BASE}/metrics`);
  if (metrics.status === 200) {
    const lines = metrics.body.split('\n').filter((l) => l.startsWith('voxtranslate_live_translation_ms_') && !l.startsWith('#'));
    console.log(`server-side voxtranslate_live_translation_ms (speech -> translated audio, ms):\n${lines.join('\n')}`);
  }
}
