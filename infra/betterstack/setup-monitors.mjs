// Provisions VoxTranslate's external uptime monitors on **Better Stack** (issue #69,
// point 1) via the Uptime REST API — a stronger alert path than the best-effort
// GitHub cron (`.github/workflows/uptime.yml`): 3-min checks, email + mobile push,
// and a status page. The monitor set lives in `monitors.json` (version-controlled),
// so this is the reproducible source of truth, not click-ops in the dashboard.
//
// Idempotent: matches existing monitors by URL and PATCHes them in place, only
// creating the ones that are missing — so re-running after editing monitors.json
// just reconciles the dashboard to the file. Mirrors the directus/*.mjs setup scripts.
//
//   BETTERSTACK_API_TOKEN=<uptime-api-token> node infra/betterstack/setup-monitors.mjs
//   BETTERSTACK_API_TOKEN=… node infra/betterstack/setup-monitors.mjs --dry-run   # preview only
//
// Get the token in Better Stack → Settings → API tokens (Uptime). The token is a
// secret: pass it via the env var as above — never commit it or paste it anywhere
// that lands in git. See README.md in this folder.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const BASE = 'https://uptime.betterstack.com/api/v2';
const HERE = dirname(fileURLToPath(import.meta.url));

const args = process.argv.slice(2);
const DRY_RUN = args.includes('--dry-run');
const configPath = argValue('--config') || join(HERE, 'monitors.json');

const TOKEN = process.env.BETTERSTACK_API_TOKEN || '';
if (!TOKEN) {
  fail(
    'Set BETTERSTACK_API_TOKEN to your Better Stack Uptime API token.\n' +
      '  Better Stack → Settings → API tokens (Uptime). Then:\n' +
      '  BETTERSTACK_API_TOKEN=<token> node infra/betterstack/setup-monitors.mjs',
  );
}

// ──────────────────────────────────────────────────────────────────────────
// Tiny REST client. Throws on non-2xx with the response body so failures are
// actionable (e.g. a region the free plan rejects).
// ──────────────────────────────────────────────────────────────────────────
async function api(path, { method = 'GET', body } = {}) {
  const res = await fetch(path.startsWith('http') ? path : BASE + path, {
    method,
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      'Content-Type': 'application/json',
    },
    body: body == null ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  const json = text ? JSON.parse(text) : {};
  if (!res.ok) {
    throw new Error(`${method} ${path} → ${res.status}: ${text}`);
  }
  return json;
}

// Compare URLs ignoring a single trailing slash, so "…/app" and "…/app/" match.
const normUrl = (u) => String(u || '').replace(/\/+$/, '');

async function listMonitors() {
  const out = [];
  let url = `${BASE}/monitors?per_page=50`;
  while (url) {
    const page = await api(url);
    for (const m of page.data || []) out.push(m);
    url = page.pagination?.next || null;
  }
  return out;
}

function buildSpecs(config) {
  const defaults = config.defaults || {};
  return (config.monitors || []).map((m) => resolveSecrets({ ...defaults, ...m }));
}

// `request_headers` values may reference a secret as `$VAR` / `${VAR}` so the secret
// is never committed (e.g. the origin-lock header `x-origin-verify: $CF_ORIGIN_SECRET`,
// #111 / spec 0078). Resolve from the environment; fail loudly when a referenced var is
// unset — otherwise we'd send an empty header and the monitor would silently 403 behind
// the origin lock.
function resolveSecrets(spec) {
  if (!Array.isArray(spec.request_headers)) return spec;
  const request_headers = spec.request_headers.map((h) => {
    const ref = /^\$\{?(\w+)\}?$/.exec(String(h.value ?? ''));
    if (!ref) return h;
    const val = process.env[ref[1]];
    if (!val) {
      fail(
        `monitor "${spec.pronounceable_name}" needs request header ${h.name} = $${ref[1]}, ` +
          `but $${ref[1]} is unset.\n  Run with: ${ref[1]}=<value> BETTERSTACK_API_TOKEN=<token> ` +
          `node infra/betterstack/setup-monitors.mjs`,
      );
    }
    return { ...h, value: val };
  });
  return { ...spec, request_headers };
}

// Only flag a PATCH when a managed field actually differs, so re-runs are quiet.
function diffFields(spec, existingAttrs) {
  const changed = [];
  for (const [k, v] of Object.entries(spec)) {
    const cur = existingAttrs[k];
    const a = JSON.stringify(Array.isArray(v) ? v : v);
    const b = JSON.stringify(cur);
    if (a !== b) changed.push(k);
  }
  return changed;
}

async function main() {
  const config = JSON.parse(readFileSync(configPath, 'utf8'));
  const specs = buildSpecs(config);
  console.log(
    `Better Stack: reconciling ${specs.length} monitor(s) from ${configPath}` +
      (DRY_RUN ? ' [DRY RUN — no writes]' : ''),
  );

  const existing = await listMonitors();
  const byUrl = new Map(existing.map((m) => [normUrl(m.attributes?.url), m]));

  let created = 0;
  let updated = 0;
  let unchanged = 0;

  for (const spec of specs) {
    const match = byUrl.get(normUrl(spec.url));
    if (!match) {
      console.log(`  + create  ${spec.pronounceable_name}  (${spec.url})`);
      if (!DRY_RUN) {
        const res = await api('/monitors', { method: 'POST', body: spec });
        console.log(`            → id ${res.data?.id}`);
      }
      created++;
      continue;
    }
    const changed = diffFields(spec, match.attributes || {});
    if (changed.length === 0) {
      console.log(`  = ok      ${spec.pronounceable_name}  (id ${match.id})`);
      unchanged++;
      continue;
    }
    console.log(
      `  ~ update  ${spec.pronounceable_name}  (id ${match.id}) — ${changed.join(', ')}`,
    );
    if (!DRY_RUN) await api(`/monitors/${match.id}`, { method: 'PATCH', body: spec });
    updated++;
  }

  console.log(
    `Done. created=${created} updated=${updated} unchanged=${unchanged}` +
      (DRY_RUN ? ' (dry run)' : ''),
  );
  if (!DRY_RUN && (created || updated)) {
    console.log('View them at https://uptime.betterstack.com/team/monitors');
  }
}

function argValue(flag) {
  const i = args.indexOf(flag);
  return i >= 0 ? args[i + 1] : null;
}

function fail(msg) {
  console.error(`\n✖ ${msg}\n`);
  process.exit(1);
}

main().catch((err) => fail(err.message || String(err)));
