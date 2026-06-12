// k6 load test — WebSocket signaling / room fan-out (spec 0027).
//
// Each VU joins a room over /ws and behaves like a real (silent) participant:
// it chats, reacts, toggles mute/hand, then leaves. VUs are spread across `ROOMS`
// rooms and ALL use lang=en, so chat broadcasts to peers WITHOUT translation —
// no Groq. No `start`/audio frames are ever sent — no Deepgram. So this measures
// the server's room + relay + broadcast machinery only.
//
//   k6 run -e WS_URL=ws://localhost:3001 -e ROOMS=20 loadtest/signaling.js
//
// (To also exercise translation, use mixed langs — but that hits Groq; mock it or
//  accept the cost. See loadtest/README.md.)

import ws from 'k6/ws';
import { check } from 'k6';
import { Counter } from 'k6/metrics';

const WS_BASE = __ENV.WS_URL || 'ws://localhost:3001';
const ROOMS = Number(__ENV.ROOMS || 20); // VUs per room ≈ peak VUs / ROOMS
const HOLD_SEC = Number(__ENV.HOLD_SEC || 20); // how long each peer stays in the call

const joined = new Counter('ws_room_joined');
const received = new Counter('ws_app_messages');
const sessionErrors = new Counter('ws_session_errors');

export const options = {
  scenarios: {
    signaling: {
      executor: 'ramping-vus',
      startVUs: 0,
      stages: [
        { duration: '30s', target: 100 },
        { duration: '1m', target: 300 },
        { duration: '30s', target: 0 },
      ],
    },
  },
  thresholds: {
    ws_connecting: ['p(95)<500'], // time to the WS handshake
    ws_session_errors: ['count<1'],
  },
};

export default function () {
  const room = `load-${__VU % ROOMS}`; // peers sharing a room → real fan-out
  const id = `vu${__VU}-it${__ITER}`; // unique peer id within the room
  const url = `${WS_BASE}/ws?room=${room}&lang=en&id=${id}&public=false`;

  const res = ws.connect(url, {}, (socket) => {
    socket.on('open', () => {
      joined.add(1);
      // Chat every 3s (same-lang → relay, no translation).
      socket.setInterval(() => {
        socket.send(JSON.stringify({ type: 'chat', text: `hi from ${id}` }));
      }, 3000);
      // Emoji reaction every 5s (broadcast, no translation).
      socket.setInterval(() => {
        socket.send(JSON.stringify({ type: 'emoji', emoji: '👍' }));
      }, 5000);
      // Toggle mute every 7s (relayed presence update).
      let muted = false;
      socket.setInterval(() => {
        muted = !muted;
        socket.send(JSON.stringify({ type: 'mute_audio', muted }));
      }, 7000);
      // Leave after HOLD_SEC.
      socket.setTimeout(() => socket.close(), HOLD_SEC * 1000);
    });

    socket.on('message', () => received.add(1));
    socket.on('error', (e) => {
      // k6 reports a normal close as an error on some versions; ignore those.
      if (e && e.error && !`${e.error}`.includes('close')) sessionErrors.add(1);
    });
  });

  check(res, { 'ws handshake (101)': (r) => r && r.status === 101 });
}
