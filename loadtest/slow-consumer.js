// k6 load test — bounded hot-path channels under heavy fan-out (spec 0065, #123).
//
// Goal: confirm the server's per-peer outbound channel (`out_tx`, now bounded to
// OUT_CHANNEL_CAP) keeps memory FLAT under sustained broadcast pressure, and that
// idle receivers don't make it grow. Each room has a few "talkers" flooding
// chat/emoji/whiteboard (pure relay/broadcast — lang=en so NO Groq, no `start`/
// audio so NO Deepgram) and several "leechers" that join and only listen.
//
//   k6 run -e WS_URL=ws://localhost:3001 -e ROOMS=20 loadtest/slow-consumer.js
//
// While it runs, watch the server's resident memory — it should plateau, not
// climb:
//   while true; do ps -o rss= -p $(pgrep -f voxtranslate-server) | \
//     awk '{printf "RSS %.1f MB\n", $1/1024}'; sleep 2; done
//
// What this DOES vs DOES NOT validate
// -----------------------------------
// k6's WS client always drains the TCP socket, so it cannot reproduce a *true*
// stalled reader (one whose kernel receive window fills and back-pressures the
// server's `pump_to_ws`). This scenario therefore validates the BOUNDED-under-
// load + no-leak property, not the close-on-stall path. The close-on-stall path
// (overflow → `Notify` → clean teardown) is covered by the Rust unit test
// `rooms::tests::peertx_overflow_keeps_peer_and_signals_close`. To reproduce a
// real TCP stall, use a raw client that pauses its socket after joining — see
// loadtest/README.md ("Reproducing a true slow consumer").

import ws from 'k6/ws';
import { check } from 'k6';
import { Counter } from 'k6/metrics';

const WS_BASE = __ENV.WS_URL || 'ws://localhost:3001';
const ROOMS = Number(__ENV.ROOMS || 20);
const HOLD_SEC = Number(__ENV.HOLD_SEC || 30);
// Of every TALKER_EVERY peers in a room, one is a talker; the rest leech.
const TALKER_EVERY = Number(__ENV.TALKER_EVERY || 4);

const joined = new Counter('ws_room_joined');
const received = new Counter('ws_app_messages');
const sessionErrors = new Counter('ws_session_errors');

export const options = {
  scenarios: {
    fanout: {
      executor: 'ramping-vus',
      startVUs: 0,
      stages: [
        { duration: '30s', target: 120 },
        { duration: '2m', target: 400 }, // sustained pressure — memory must plateau
        { duration: '30s', target: 0 },
      ],
    },
  },
  thresholds: {
    ws_connecting: ['p(95)<500'],
    ws_session_errors: ['count<1'],
  },
};

export default function () {
  const room = `load-${__VU % ROOMS}`;
  const id = `vu${__VU}-it${__ITER}`;
  const url = `${WS_BASE}/ws?room=${room}&lang=en&id=${id}&public=false`;
  const isTalker = __VU % TALKER_EVERY === 0;

  const res = ws.connect(url, {}, (socket) => {
    socket.on('open', () => {
      joined.add(1);
      if (isTalker) {
        // Flood the room: chat (relay), emoji (broadcast), whiteboard (broadcast
        // + stored op-log snapshot). This is the fan-out that fills peers' out_tx.
        socket.setInterval(() => {
          socket.send(JSON.stringify({ type: 'chat', text: `msg from ${id}` }));
        }, 500);
        socket.setInterval(() => {
          socket.send(JSON.stringify({ type: 'emoji', emoji: '🔥' }));
        }, 700);
        socket.setInterval(() => {
          socket.send(
            JSON.stringify({
              type: 'whiteboard',
              op: { kind: 'draw', x: __ITER % 100, y: __VU % 100, color: '#09f' },
            }),
          );
        }, 600);
      }
      // Everyone leaves after HOLD_SEC (leechers just listened the whole time).
      socket.setTimeout(() => socket.close(), HOLD_SEC * 1000);
    });

    socket.on('message', () => received.add(1));
    socket.on('error', (e) => {
      if (e && e.error && !`${e.error}`.includes('close')) sessionErrors.add(1);
    });
  });

  check(res, { 'ws handshake (101)': (r) => r && r.status === 101 });
}
