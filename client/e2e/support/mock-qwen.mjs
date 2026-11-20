// A stand-in for the Qwen realtime WebSocket, for the Live Connect e2e (spec 0121).
//
// The real server's Standard engine dials `QWEN_REALTIME_ENDPOINT?model=...`, sends one
// `session.update`, then streams `input_audio_buffer.append` frames. This mock speaks exactly
// the slice of that dialect the engine reads (see server/src/engine/realtime_mock.rs, the Rust
// stand-in the server tests use, and server/src/engine/qwen.rs for the parser):
//
//   * session.update  ->  session.updated                      (the "ready" signal)
//   * the FIRST audio append of each upstream session answers, once, with
//       conversation.item.input_audio_transcription.delta   (the speaker's own words)
//       response.audio_transcript.delta                      (the translation)
//       response.done
//       response.audio.delta x 3                             (base64 PCM16 @ 24 kHz, a tone)
//
// The words say who spoke and for whom, so an assertion can tell the two directions apart:
// original `[it speaks] ...`, translation `[it->es] ...`. The languages come from the
// `session.update` the engine sent (`translation.language` = target,
// `input_audio_transcription.language` = source), so the mock invents nothing about routing.
//
// It does not translate, model latency or validate the key. It also serves
// `GET /__stats` so a test can see how many upstream sessions each direction opened.
//
// Pure Node (no dependency): a WebSocket server is ~60 lines of RFC 6455 and this file must
// run from a bare `node`, with nothing installed beyond the client's own dev dependencies.

import { createHash } from 'node:crypto';
import { createServer } from 'node:http';

const GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';

/** `ms` of a 440 Hz tone as base64 PCM16 mono @ 24 kHz, loud enough to be audible and non-silent. */
export function toneBase64(ms = 100, hz = 440, rate = 24000) {
  const n = Math.round((rate * ms) / 1000);
  const buf = Buffer.alloc(n * 2);
  for (let i = 0; i < n; i++) {
    buf.writeInt16LE(Math.round(Math.sin((2 * Math.PI * hz * i) / rate) * 9000), i * 2);
  }
  return buf.toString('base64');
}

/** One server->client text frame (unmasked, FIN). */
function frame(opcode, payload) {
  const body = Buffer.isBuffer(payload) ? payload : Buffer.from(payload);
  const len = body.length;
  let head;
  if (len < 126) head = Buffer.from([0x80 | opcode, len]);
  else if (len < 65536) head = Buffer.from([0x80 | opcode, 126, len >> 8, len & 0xff]);
  else {
    head = Buffer.alloc(10);
    head[0] = 0x80 | opcode;
    head[1] = 127;
    head.writeBigUInt64BE(BigInt(len), 2);
  }
  return Buffer.concat([head, body]);
}

/**
 * Incremental parser of client->server frames. Calls `onMessage(opcode, Buffer)` for each
 * complete message (continuation frames are reassembled), answers pings, honours close.
 */
function parser(socket, onMessage) {
  let buf = Buffer.alloc(0);
  let fragments = [];
  let fragOpcode = 0;
  socket.on('data', (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    for (;;) {
      if (buf.length < 2) return;
      const fin = (buf[0] & 0x80) !== 0;
      const opcode = buf[0] & 0x0f;
      const masked = (buf[1] & 0x80) !== 0;
      let len = buf[1] & 0x7f;
      let off = 2;
      if (len === 126) {
        if (buf.length < 4) return;
        len = buf.readUInt16BE(2);
        off = 4;
      } else if (len === 127) {
        if (buf.length < 10) return;
        len = Number(buf.readBigUInt64BE(2));
        off = 10;
      }
      const need = off + (masked ? 4 : 0) + len;
      if (buf.length < need) return;
      const mask = masked ? buf.subarray(off, off + 4) : null;
      const data = Buffer.from(buf.subarray(off + (masked ? 4 : 0), need));
      buf = buf.subarray(need);
      if (mask) for (let i = 0; i < data.length; i++) data[i] ^= mask[i & 3];

      if (opcode === 0x8) {
        socket.end(frame(0x8, Buffer.alloc(0)));
        return;
      }
      if (opcode === 0x9) {
        socket.write(frame(0xa, data));
        continue;
      }
      if (opcode === 0xa) continue;
      if (opcode === 0x0) {
        fragments.push(data);
        if (fin) {
          onMessage(fragOpcode, Buffer.concat(fragments));
          fragments = [];
        }
      } else if (fin) {
        onMessage(opcode, data);
      } else {
        fragOpcode = opcode;
        fragments = [data];
      }
    }
  });
}

/**
 * Start the mock on `port` (0 = any free port). Resolves to
 * `{ port, url, stats(), stop() }`; `url` is what to put in `QWEN_REALTIME_ENDPOINT`.
 */
export function startMockQwen({ port = 0, host = '127.0.0.1', chunks = 3 } = {}) {
  /** @type {{ id: number, source: string|null, target: string|null, appended: number, answered: boolean, open: boolean }[]} */
  const sessions = [];
  const audio = toneBase64();

  const server = createServer((req, res) => {
    if (req.url?.startsWith('/__stats')) {
      res.setHeader('content-type', 'application/json');
      res.end(JSON.stringify({ sessions }));
      return;
    }
    res.statusCode = 426;
    res.end('websocket only');
  });

  server.on('upgrade', (req, socket) => {
    const key = req.headers['sec-websocket-key'];
    if (!key) return socket.destroy();
    socket.write(
      'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n' +
        `Sec-WebSocket-Accept: ${createHash('sha1').update(key + GUID).digest('base64')}\r\n\r\n`,
    );
    const me = { id: sessions.length + 1, source: null, target: null, appended: 0, answered: false, open: true };
    sessions.push(me);
    const send = (obj) => socket.writable && socket.write(frame(0x1, JSON.stringify(obj)));

    parser(socket, (opcode, data) => {
      if (opcode !== 0x1) return;
      let msg;
      try {
        msg = JSON.parse(data.toString('utf8'));
      } catch {
        return;
      }
      if (msg.type === 'session.update') {
        const s = msg.session ?? {};
        me.source = s.input_audio_transcription?.language ?? 'auto';
        // LiveTranslate carries the target as a field; Omni only in the prompt (not modelled).
        me.target = s.translation?.language ?? 'unknown';
        send({ type: 'session.updated', session: {} });
        return;
      }
      if (msg.type !== 'input_audio_buffer.append') return;
      me.appended += 1;
      if (me.answered) return;
      me.answered = true;
      const who = `${me.source}->${me.target}`;
      send({ type: 'session.updated' });
      send({
        type: 'conversation.item.input_audio_transcription.delta',
        delta: `[${me.source} speaks] hello from the mock`,
      });
      send({ type: 'response.audio_transcript.delta', delta: `[${who}] hello from the mock` });
      send({ type: 'response.done' });
      for (let i = 0; i < chunks; i++) send({ type: 'response.audio.delta', delta: audio });
    });
    socket.on('close', () => {
      me.open = false;
    });
    socket.on('error', () => socket.destroy());
  });

  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, host, () => {
      const actual = server.address().port;
      resolve({
        port: actual,
        url: `ws://${host}:${actual}/api-ws/v1/realtime`,
        stats: () => ({ sessions: sessions.map((s) => ({ ...s })) }),
        stop: () =>
          new Promise((done) => {
            server.closeAllConnections?.();
            server.close(() => done());
          }),
      });
    });
  });
}

// `node mock-qwen.mjs [port]` runs it on its own, for poking at by hand.
if (import.meta.url === `file://${process.argv[1]}`) {
  const m = await startMockQwen({ port: Number(process.argv[2] ?? process.env.MOCK_QWEN_PORT ?? 3102) });
  console.log(`mock-qwen listening on ${m.url}`);
}
