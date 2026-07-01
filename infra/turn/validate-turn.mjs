#!/usr/bin/env node
// Great-Firewall corridor TURN validation (spec: China corridor, PRs #331/#332).
//
// Proves that a `turns://…:443` TLS relay is (a) reachable on 443, (b) able to
// allocate a relayed transport address with the configured credentials — the exact
// thing a China-side browser needs when forced onto relay. Dependency-free (Node
// built-ins only) so it runs anywhere, including a mainland-China VPS, which is the
// only way to confirm the relay survives the GFW (DNS here uses Azure Traffic Manager
// geo-routing, so the PoP differs by vantage).
//
// Usage:
//   TURN_HOST=global.relay.metered.ca TURN_PORT=443 \
//   TURN_USER=... TURN_PASS=... [API_BASE=https://api.voxtranslate.app] \
//   node infra/turn/validate-turn.mjs
//
// Exit code 0 iff the TURN allocation succeeds.

import tls from 'node:tls';
import crypto from 'node:crypto';

const HOST = process.env.TURN_HOST || 'global.relay.metered.ca';
const PORT = Number(process.env.TURN_PORT || 443);
const USER = process.env.TURN_USER || '';
const PASS = process.env.TURN_PASS || '';
const API_BASE = process.env.API_BASE || 'https://api.voxtranslate.app';
const TIMEOUT_MS = Number(process.env.TURN_TIMEOUT_MS || 12000);

const MAGIC = 0x2112a442;
const METHOD_BINDING = 0x0001;
const METHOD_ALLOCATE = 0x0003;
const CLASS_REQUEST = 0x0000;
// STUN message type = (method bits interleaved with class bits); for these methods
// with request/success/error classes the encoding below is sufficient.
const msgType = (method, cls) =>
  ((method & 0x0f80) << 2) | ((method & 0x0070) << 1) | (method & 0x000f) | (cls & 0x0110);

const AT = {
  USERNAME: 0x0006,
  MESSAGE_INTEGRITY: 0x0008,
  ERROR_CODE: 0x0009,
  REALM: 0x0014,
  NONCE: 0x0015,
  XOR_RELAYED_ADDRESS: 0x0016,
  REQUESTED_TRANSPORT: 0x0019,
  SOFTWARE: 0x8022,
};

function encAttr(type, value) {
  const len = value.length;
  const pad = (4 - (len % 4)) % 4;
  const b = Buffer.alloc(4 + len + pad);
  b.writeUInt16BE(type, 0);
  b.writeUInt16BE(len, 2);
  value.copy(b, 4);
  return b;
}

function header(type, tid, bodyLen) {
  const h = Buffer.alloc(20);
  h.writeUInt16BE(type, 0);
  h.writeUInt16BE(bodyLen, 2);
  h.writeUInt32BE(MAGIC, 4);
  tid.copy(h, 8);
  return h;
}

// Long-term credential key = MD5(username ":" realm ":" password) (RFC 5389 §15.4).
const longTermKey = (user, realm, pass) =>
  crypto.createHash('md5').update(`${user}:${realm}:${pass}`).digest();

function withMessageIntegrity(type, tid, attrs, key) {
  const pre = Buffer.concat(attrs);
  // Length field must already count the 24-byte MESSAGE-INTEGRITY attribute.
  const h = header(type, tid, pre.length + 24);
  const mac = crypto.createHmac('sha1', key).update(Buffer.concat([h, pre])).digest();
  return Buffer.concat([h, pre, encAttr(AT.MESSAGE_INTEGRITY, mac)]);
}

function parse(msg) {
  const type = msg.readUInt16BE(0);
  const len = msg.readUInt16BE(2);
  const attrs = {};
  let off = 20;
  const end = 20 + len;
  while (off + 4 <= end) {
    const t = msg.readUInt16BE(off);
    const l = msg.readUInt16BE(off + 2);
    const v = msg.slice(off + 4, off + 4 + l);
    attrs[t] = v;
    off += 4 + l + ((4 - (l % 4)) % 4);
  }
  return { type, attrs };
}

function decodeError(v) {
  if (!v || v.length < 4) return null;
  const code = (v[2] & 0x07) * 100 + v[3];
  return { code, reason: v.slice(4).toString('utf8') };
}

function decodeXorAddr(v) {
  if (!v || v.length < 8) return null;
  const port = v.readUInt16BE(2) ^ (MAGIC >>> 16);
  const ip = [];
  for (let i = 0; i < 4; i++) ip.push(v[4 + i] ^ ((MAGIC >>> (24 - 8 * i)) & 0xff));
  return `${ip.join('.')}:${port}`;
}

function sendRecv(sock, message, label = 'response') {
  return new Promise((resolve, reject) => {
    let buf = Buffer.alloc(0);
    const timer = setTimeout(() => {
      sock.removeListener('data', onData);
      reject(new Error(`timeout waiting for ${label}`));
    }, TIMEOUT_MS);
    const onData = (d) => {
      buf = Buffer.concat([buf, d]);
      if (buf.length >= 20) {
        const total = 20 + buf.readUInt16BE(2);
        if (buf.length >= total) {
          clearTimeout(timer);
          sock.removeListener('data', onData);
          resolve(parse(buf.slice(0, total)));
        }
      }
    };
    sock.on('data', onData);
    sock.write(message, (e) => e && (clearTimeout(timer), reject(e)));
  });
}

async function run() {
  console.log(`# TURN validation → turns:${HOST}:${PORT}?transport=tcp\n`);
  if (!USER || !PASS) {
    console.error('FAIL: set TURN_USER and TURN_PASS (the relay-scoped account).');
    process.exit(2);
  }

  const sock = await new Promise((resolve, reject) => {
    const s = tls.connect({ host: HOST, port: PORT, servername: HOST, timeout: TIMEOUT_MS }, () =>
      resolve(s),
    );
    s.once('timeout', () => reject(new Error('TLS timeout')));
    s.once('error', reject);
  });
  console.log(`✓ TLS on :${PORT}  (${sock.getProtocol()}, peer ${sock.remoteAddress})`);

  const soft = encAttr(AT.SOFTWARE, Buffer.from('voxtranslate-turn-check'));

  // 1) STUN Binding — cheap reachability probe. Non-fatal: some TURN stacks answer
  // only TURN methods on the TLS port, so a miss here isn't a failure.
  try {
    const bindTid = crypto.randomBytes(12);
    const bind = await sendRecv(
      sock,
      Buffer.concat([header(msgType(METHOD_BINDING, CLASS_REQUEST), bindTid, soft.length), soft]),
      'STUN Binding',
    );
    console.log(`✓ STUN Binding response (type 0x${bind.type.toString(16).padStart(4, '0')})`);
  } catch (e) {
    console.log(`· STUN Binding skipped (${e.message}) — proceeding to Allocate`);
  }

  // 2) TURN Allocate, unauthenticated → expect 401 with REALM + NONCE.
  const reqTransport = encAttr(AT.REQUESTED_TRANSPORT, Buffer.from([17, 0, 0, 0])); // UDP
  const a1Tid = crypto.randomBytes(12);
  const a1Body = Buffer.concat([reqTransport, soft]);
  const a1 = await sendRecv(
    sock,
    Buffer.concat([header(msgType(METHOD_ALLOCATE, CLASS_REQUEST), a1Tid, a1Body.length), a1Body]),
    'Allocate challenge',
  );
  const err = decodeError(a1.attrs[AT.ERROR_CODE]);
  const realm = a1.attrs[AT.REALM];
  const nonce = a1.attrs[AT.NONCE];
  if (!err || err.code !== 401 || !realm || !nonce) {
    console.error('FAIL: expected 401 with REALM+NONCE, got', err, 'realm?', !!realm, 'nonce?', !!nonce);
    sock.end();
    process.exit(1);
  }
  console.log(`✓ Allocate challenge: 401, realm="${realm.toString('utf8')}"`);

  // 3) Authenticated Allocate with MESSAGE-INTEGRITY → expect success + XOR-RELAYED-ADDRESS.
  const key = longTermKey(USER, realm.toString('utf8'), PASS);
  const a2Tid = crypto.randomBytes(12);
  const a2 = await sendRecv(
    sock,
    withMessageIntegrity(
      msgType(METHOD_ALLOCATE, CLASS_REQUEST),
      a2Tid,
      [reqTransport, encAttr(AT.USERNAME, Buffer.from(USER)), encAttr(AT.REALM, realm), encAttr(AT.NONCE, nonce)],
      key,
    ),
    'Allocate success',
  );
  const relayed = decodeXorAddr(a2.attrs[AT.XOR_RELAYED_ADDRESS]);
  sock.end();
  if (!relayed) {
    console.error('FAIL: no relayed address; error =', decodeError(a2.attrs[AT.ERROR_CODE]));
    process.exit(1);
  }
  console.log(`✓ ALLOCATION SUCCESS — relayed transport address ${relayed}`);

  // App-reachability + whether prod already serves the turns:443 restricted profile.
  // From a China vantage this is the decisive extra signal: if api.voxtranslate.app
  // itself is unreachable, signaling never connects and the relay above is moot.
  try {
    const ctrl = AbortSignal.timeout(TIMEOUT_MS);
    const res = await fetch(`${API_BASE}/api/ice?restricted=1`, { signal: ctrl });
    const body = await res.json();
    const urls = (body.iceServers || []).flatMap((e) => e.urls || []);
    const has443 = urls.some((u) => /^turns:.*:443/.test(u));
    console.log(`✓ ${API_BASE} reachable (HTTP ${res.status})`);
    console.log(
      has443
        ? '✓ /api/ice?restricted=1 already serves a turns:443 relay (profile is live)'
        : '· /api/ice?restricted=1 does NOT yet serve turns:443 — set Railway TURN_TLS_* and deploy #331',
    );
  } catch (e) {
    console.log(`· ${API_BASE} NOT reachable (${e.message}) — if from China, this is the blocker to fix first`);
  }

  console.log('\nRESULT: the turns:443 relay accepts the credentials and allocates a relay.');
  console.log('        (Run this from a mainland-China vantage to confirm GFW survival.)');
}

run().catch((e) => {
  console.error('FAIL:', e.message);
  process.exit(1);
});
