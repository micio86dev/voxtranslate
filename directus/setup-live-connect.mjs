// Provisions the Live Connect (spec 0121) administration in Directus via the REST API:
//
//   1. a "Live Connect" folder with the live_* collections registered, readable
//      display templates, relations, and every sensitive column hidden; plus
//      `live_desk_usage_daily`, a read-only rollup TABLE (credits per desk per day, metadata
//      only) that stands in for `live_usage_ticks`, whose composite key Directus cannot
//      manage (Directus ignores views too, so the rollup is a table, see migration 068);
//   2. two roles, created here (nothing else in this repo creates roles):
//        "Live Connect Support"  read-only, METADATA ONLY (no names, no transcripts, no QR)
//        "Live Connect Privacy"  Support + the one Flow button that reads a transcript;
//   3. the Flow buttons, each calling the VoxTranslate server (`/api/admin/live/*`)
//      with the shared secret: Revoke desk, Regenerate desk, End session, Reconcile
//      session, and (Privacy only) View transcript;
//   4. an Insights dashboard that reads metadata only (including credits per desk).
//
// Directus stays a thin admin UI: it never writes the live tables. Every action goes
// through the server, which audits it (`admin_audit`). Customer-facing workflows
// (desks, QR download, session history, usage) stay in the B2B dashboard.
//
//   DIRECTUS_URL=https://<your-directus> \
//     DIRECTUS_ADMIN_EMAIL=… DIRECTUS_ADMIN_PASSWORD=…   # or DIRECTUS_TOKEN=<static admin token>
//     node directus/setup-live-connect.mjs [--dry-run] [--print-transcript-filter]
//                                          [--transcript-preview] [--audit-access] [--help]
//
//   --dry-run                  print every call this script WOULD make and change nothing;
//                              needs no Directus, no URL and no credentials.
//   --print-transcript-filter  print the permission filters that keep Live Connect
//                              conversations out of EXISTING roles (see README), then exit.
//   --audit-access             read-only: list every NON-admin policy that can read the
//                              collections now holding live conversations WITHOUT the filter
//                              above, then exit 1 if there is one (0 if none). Changes nothing.
//                              A normal run does the same audit at its end, as a warning only.
//   --transcript-preview       OPT IN. The View transcript button then also puts a short excerpt
//                              (3 utterances x 120 characters) into the operator's notification.
//                              That copy is stored in directus_notifications AND e-mailed in
//                              clear text when Directus mail is configured; it is NOT covered by
//                              erasure or retention. DEFAULT: no excerpt (counts only).
//   --no-transcript-preview    accepted for backward compatibility; the default already is "no
//                              excerpt". If both flags are given this one wins.
//   --help                     print this text, then exit.
//
// Privacy by construction: every Flow runs with accountability "activity", so Directus logs THAT
// a Flow ran (directus_activity: who, when) but never stores the operation outputs (the
// conversation) in directus_revisions. Runs made by an earlier version of this script
// (accountability "all") left such revisions behind: a re-run deletes them through the API when
// Directus allows it, otherwise prints the SQL an operator must run.
//
// Idempotent: re-running updates in place (collection metadata, fields, policies,
// permissions, flows and their operations by key, the dashboard's panels by name). Requires
// Directus 11 (policies). Requires (Directus env): FLOWS_ENV_ALLOW_LIST includes
// ADMIN_API_SECRET and VOX_API_URL.
// Caveat: a Flow that is RUNNING while this script re-runs can fail that one run (its
// operations are updated while it executes); re-run during a quiet moment.
// Run setup-backoffice.mjs first so organizations / projects /
// organization_credits_transactions are already registered collections.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const args = new Set(process.argv.slice(2));
const DRY = args.has('--dry-run');
const AUDIT_ONLY = args.has('--audit-access');
const KNOWN_FLAGS = ['--dry-run', '--print-transcript-filter', '--transcript-preview', '--no-transcript-preview', '--audit-access', '--help'];
for (const a of args) if (!KNOWN_FLAGS.includes(a)) console.warn(`! unknown flag ${a} ignored (see --help).`);
if (args.has('--help')) {
  const here = readFileSync(fileURLToPath(import.meta.url), 'utf8');
  console.log(here.split('\n').filter((l) => l.startsWith('//')).map((l) => l.slice(3)).join('\n'));
  process.exit(0);
}
// How many utterances the transcript button previews in the operator's notification. DEFAULT 0:
// the excerpt would be copied into directus_notifications and e-mailed, outside erase/retention.
const PREVIEW_LINES = args.has('--transcript-preview') && !args.has('--no-transcript-preview') ? 3 : 0;

// ──────────────────────────────────────────────────────────────────────────
// What keeps a conversation out of an EXISTING role. `transcript_events`,
// `session_participants`, `transcripts`, `call_sessions` (and everything else that
// hangs off a call session) already hold Live Connect rows, because a live
// conversation reuses the meeting tables. A role that must not read live
// conversations needs this filter on each of them. A NULL `kind` is an ordinary
// meeting, so `_neq` alone would hide it (SQL `NULL <> 'x'` is not true).
// ──────────────────────────────────────────────────────────────────────────
const NOT_LIVE = {
  _or: [{ kind: { _null: true } }, { kind: { _neq: 'live_connect' } }],
};
const TRANSCRIPT_FILTERS = {
  call_sessions: NOT_LIVE,
  ...Object.fromEntries(
    [
      'transcript_events',
      'session_participants',
      'transcripts',
      'session_reports',
      'session_sentiments',
      'session_emails',
      'transcript_bookmarks',
      'chat_files',
    ].map((c) => [c, { session_id: NOT_LIVE }]),
  ),
};

if (args.has('--print-transcript-filter')) {
  console.log(JSON.stringify(TRANSCRIPT_FILTERS, null, 2));
  process.exit(0);
}

// ──────────────────────────────────────────────────────────────────────────
// Tiny REST client. In --dry-run nothing leaves the process: reads answer "nothing
// there yet" and writes are printed.
// ──────────────────────────────────────────────────────────────────────────
const base = (process.env.DIRECTUS_URL || '').replace(/\/$/, '');
if (!DRY && !base) fail('Set DIRECTUS_URL to your Directus base URL (or pass --dry-run).');

let TOKEN = '';
let planned = 0;
async function api(path, { method = 'GET', body } = {}) {
  if (DRY) {
    if (method === 'GET') return { data: [] };
    planned++;
    console.log(`  [dry-run] ${method} ${path}${body === undefined ? '' : ' ' + compact(body)}`);
    return { data: { id: `<dry-run:${path.split('?')[0].split('/')[1] || 'id'}>` } };
  }
  const res = await fetch(base + path, {
    method,
    headers: {
      'Content-Type': 'application/json',
      ...(TOKEN ? { Authorization: `Bearer ${TOKEN}` } : {}),
    },
    body: body == null ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  if (!res.ok) throw new Error(`${method} ${path} → ${res.status}: ${text.slice(0, 300)}`);
  return text ? JSON.parse(text) : {};
}

async function getToken() {
  if (process.env.DIRECTUS_TOKEN) return process.env.DIRECTUS_TOKEN;
  const email = process.env.DIRECTUS_ADMIN_EMAIL;
  const password = process.env.DIRECTUS_ADMIN_PASSWORD;
  if (!email || !password)
    fail('Provide DIRECTUS_TOKEN, or DIRECTUS_ADMIN_EMAIL + DIRECTUS_ADMIN_PASSWORD.');
  const r = await api('/auth/login', { method: 'POST', body: { email, password } });
  return r.data.access_token;
}

const q = encodeURIComponent;
async function find(path, filterField, value) {
  const r = await api(`${path}?filter[${filterField}][_eq]=${q(value)}&limit=1&fields=*`);
  return r.data?.[0] || null;
}

// ──────────────────────────────────────────────────────────────────────────
// 1. Data model
// ──────────────────────────────────────────────────────────────────────────
const FOLDER = {
  name: 'live_connect',
  icon: 'forum',
  color: '#2ECDA7',
  note: 'Live Connect (spec 0121): metadata of two-phone conversations. Written ONLY by the server; actions are Flow buttons.',
  translations: [{ language: 'en-US', translation: 'Live Connect' }],
};

const STATUS_SESSION = [
  { text: 'Waiting', value: 'waiting', color: '#FFA439' },
  { text: 'Active', value: 'active', color: '#2ECDA7' },
  { text: 'Ended', value: 'ended', color: '#A2B5CD' },
  { text: 'Expired', value: 'expired', color: '#E35169' },
];
const STATUS_DESK = [
  { text: 'Active', value: 'active', color: '#2ECDA7' },
  { text: 'Revoked', value: 'revoked', color: '#E35169' },
];
const ROLE_PARTICIPANT = [
  { text: 'Host', value: 'host', color: '#3399FF' },
  { text: 'Staff', value: 'staff', color: '#3399FF' },
  { text: 'Guest', value: 'guest', color: '#6644FF' },
];
const PAYER = [
  { text: 'User', value: 'user', color: '#3399FF' },
  { text: 'Org', value: 'org', color: '#00C4B4' },
];
const labels = (choices) => ({
  interface: 'select-dropdown',
  options: { choices },
  display: 'labels',
  display_options: { choices, showAsDot: false },
});
const m2o = (template) => ({
  interface: 'select-dropdown-m2o',
  special: ['m2o'],
  options: { template },
  display: 'related-values',
  display_options: { template },
});
const HIDDEN_HASH =
  'Hidden on purpose: a SHA-256 fingerprint of a credential. No use to an operator; never exposed to Support.';
const PERSONAL =
  'PERSONAL DATA (a person\'s name). Admin-only: excluded from the Live Connect Support and Privacy roles; erased with the conversation.';

// Collections. `live_usage_ticks` is NOT here: its primary key is composite
// (session_id, peer_id, tick_seq) and Directus ignores tables without a single
// primary key column ("doesn't have a primary key column and will be ignored").
// Per-session billing is checked by the "Reconcile session" Flow; per-desk totals come
// from the `live_desk_usage_daily` table below.
const COLLECTIONS = {
  live_desks: {
    icon: 'storefront',
    display: '{{name}} · {{location_label}} ({{status}})',
    note: 'Permanent QR entry points of a business. The QR itself is derived from a server secret and never stored.',
    sort: 1,
    fields: {
      id: {},
      org_id: m2o('{{name}}'),
      project_id: m2o('{{name}}'),
      name: {},
      location_label: {},
      default_lang: {},
      languages: {},
      public_id: { hidden: true, note: 'Part of the printed QR URL: hidden so the QR is never reconstructable here.' },
      token_version: { note: 'Bumped by Regenerate: every earlier printed QR stops verifying.' },
      status: labels(STATUS_DESK),
      created_by: { hidden: true },
      created_at: {},
      revoked_at: {},
    },
  },
  live_sessions: {
    icon: 'forum',
    display: '{{status}} · {{end_reason}} · {{created_at}}',
    note: 'One row per conversation: lifecycle, owner, project and desk. NO content here.',
    sort: 2,
    fields: {
      id: {},
      call_session_id: { hidden: true, note: 'Points at the meeting-side row that holds what was said (conversation content). Not for Support.' },
      owner_user_id: { note: 'The account that owns (and pays for) a personal conversation. Shown as an id on purpose: user names are not exposed to Support.' },
      org_id: m2o('{{name}}'),
      project_id: m2o('{{name}}'),
      desk_id: m2o('{{name}}'),
      status: labels(STATUS_SESSION),
      invite_hash: { hidden: true, note: HIDDEN_HASH },
      host_lang: {},
      save_transcript: {},
      engine_id: {},
      max_guests: { note: 'Seats for guests on the one invitation (1-3).' },
      use_voice_clone: { note: 'The host opted in to speak with their cloned voice for this conversation (Enhanced tier only). A flag, never the voice itself.' },
      created_at: {},
      expires_at: {},
      started_at: {},
      ended_at: {},
      end_reason: { note: 'ended_by_host / ended_by_guest / ended_by_staff / ended_by_admin / abandoned / expired / max_duration / desk_revoked / desk_regenerated / …' },
    },
    // Reverse (one-to-many) views so an operator reads a session in one place.
    aliases: [
      { field: 'participants', relation: ['live_participants', 'session_id'], template: '{{role}} · {{lang}}', note: 'Who was on the call (no names).' },
      { field: 'events', relation: ['live_session_events', 'session_id'], template: '{{kind}} · {{ts}}', note: 'Bounded diagnostics timeline (kinds only).' },
    ],
  },
  live_participants: {
    icon: 'groups',
    display: '{{role}} · {{lang}}',
    note: 'The two seats of a conversation. Names are personal data and are NOT visible to Support.',
    sort: 3,
    fields: {
      peer_id: {},
      session_id: m2o('{{status}} · {{created_at}}'),
      role: labels(ROLE_PARTICIPANT),
      user_id: { hidden: true },
      display_name: { note: PERSONAL },
      lang: {},
      token_hash: { hidden: true, note: HIDDEN_HASH },
      consent_at: {},
      joined_at: {},
      left_at: {},
    },
  },
  // A rolled-up TABLE (migration 068): one row per (desk, day) with the sum of the
  // organisation-paid credits and the number of conversations, refreshed hourly by the server
  // from the `live_desk_usage` view. It exists because `live_usage_ticks` has a composite
  // primary key, and a view cannot stand in for it: Directus 11 only introspects BASE TABLEs
  // (verified on 11.17.4), so a view is invisible to it. Metadata only: ids, a day, two counts.
  live_desk_usage_daily: {
    icon: 'bar_chart',
    display: '{{desk_id.name}} · {{day}} · {{credits}} credits',
    note: 'Credits per service desk per day (UTC), organisation-paid conversations only. Read-only rollup, refreshed hourly: no names, no text.',
    sort: 5,
    fields: {
      id: { hidden: true, note: 'Surrogate key: "<desk id>:<day>".' },
      org_id: m2o('{{name}}'),
      desk_id: m2o('{{name}}'),
      day: {},
      sessions: { note: 'Conversations with at least one metered tick that day.' },
      credits: { note: 'Organisation credits debited that day for this desk.' },
      refreshed_at: { note: 'When this row was last recomputed.' },
    },
    // `desk_id` has no foreign key (a deleted desk keeps its history); this relation lives in
    // Directus only, so a desk shows as its name and not as a uuid. `org_id` is a real FK.
    relations: [{ field: 'desk_id', related: 'live_desks' }],
  },
  live_session_events: {
    icon: 'timeline',
    display: '{{kind}} · {{ts}}',
    note: 'Bounded diagnostics (join, leave, reconnect, admin_ended, …). Never tokens or transcript text.',
    sort: 4,
    fields: {
      id: {},
      session_id: m2o('{{status}} · {{created_at}}'),
      ts: {},
      kind: {},
      detail: { note: 'Free-form diagnostic JSON: admin-only, excluded from Support.' },
    },
  },
};

// Columns each role may read. EVERYTHING not listed is invisible to the role (an
// explicit allow-list: a column added later is hidden until someone lists it).
const SUPPORT_FIELDS = {
  live_desks: ['id', 'org_id', 'project_id', 'name', 'location_label', 'default_lang', 'languages', 'token_version', 'status', 'created_at', 'revoked_at'],
  live_sessions: ['id', 'owner_user_id', 'org_id', 'project_id', 'desk_id', 'status', 'host_lang', 'save_transcript', 'engine_id', 'max_guests', 'use_voice_clone', 'created_at', 'expires_at', 'started_at', 'ended_at', 'end_reason', 'participants', 'events'],
  // No display_name, no token_hash, no user_id.
  live_participants: ['peer_id', 'session_id', 'role', 'lang', 'consent_at', 'joined_at', 'left_at'],
  // No `detail`.
  live_session_events: ['id', 'session_id', 'ts', 'kind'],
  // Metadata only: no name, no text.
  live_desk_usage_daily: ['id', 'org_id', 'desk_id', 'day', 'sessions', 'credits', 'refreshed_at'],
};

async function ensureFolder() {
  const meta = { icon: FOLDER.icon, color: FOLDER.color, note: FOLDER.note, collapse: 'open', translations: FOLDER.translations };
  try {
    await api('/collections', { method: 'POST', body: { collection: FOLDER.name, schema: null, meta } });
    return 'created';
  } catch {
    await api(`/collections/${FOLDER.name}`, { method: 'PATCH', body: { meta } });
    return 'updated';
  }
}

async function ensureCollection(name, cfg) {
  const meta = {
    icon: cfg.icon,
    note: cfg.note,
    display_template: cfg.display,
    group: FOLDER.name,
    sort: cfg.sort,
    hidden: false,
  };
  // PATCH first: it also registers an as-yet-unmanaged table (verified on Directus 11).
  try {
    await api(`/collections/${name}`, { method: 'PATCH', body: { meta } });
    return 'updated';
  } catch (ePatch) {
    try {
      await api('/collections', { method: 'POST', body: { collection: name, schema: null, meta } });
      return 'registered';
    } catch (ePost) {
      throw new Error(`${name}: ${trunc(ePatch)} | ${trunc(ePost)}`);
    }
  }
}

// Field metadata: read-only in the app (only the server writes these tables), hidden
// where sensitive, with a readable interface/display for statuses and relations.
async function ensureFields(collection, fields) {
  for (const [field, spec] of Object.entries(fields)) {
    const meta = { readonly: true, ...spec };
    await api(`/fields/${collection}/${field}`, { method: 'PATCH', body: { meta } });
  }
}

async function ensureAliases(collection, aliases = []) {
  for (const a of aliases) {
    const meta = {
      special: ['o2m'],
      interface: 'list-o2m',
      options: { template: a.template, enableCreate: false, enableSelect: false },
      display: 'related-values',
      display_options: { template: a.template },
      readonly: true,
      note: a.note,
    };
    const have = (await api(`/fields/${collection}/${a.field}`).catch(() => null))?.data;
    if (have && have.field) {
      await api(`/fields/${collection}/${a.field}`, { method: 'PATCH', body: { meta } });
    } else {
      await api(`/fields/${collection}`, { method: 'POST', body: { field: a.field, type: 'alias', schema: null, meta } });
    }
    // The foreign key already exists in Postgres; this only gives it the reverse name.
    // (Directus needs collection/field/related_collection at the top level here.)
    const [many, field] = a.relation;
    await api(`/relations/${many}/${field}`, {
      method: 'PATCH',
      body: { collection: many, field, related_collection: collection, meta: { one_field: a.field } },
    });
  }
}

// Relations Directus knows about but the database does not (a column without a foreign key):
// `schema: null` creates the relation in Directus only, so a desk shows as its name and not
// as a uuid. Safe to re-run: an existing relation is updated, never duplicated.
async function ensureMetaRelations(collection, relations = []) {
  for (const r of relations) {
    const have = (await api(`/relations/${collection}/${r.field}`).catch(() => null))?.data;
    const meta = { many_collection: collection, many_field: r.field, one_collection: r.related };
    if (have && have.field) {
      await api(`/relations/${collection}/${r.field}`, { method: 'PATCH', body: { meta } });
    } else {
      await api('/relations', {
        method: 'POST',
        body: { collection, field: r.field, related_collection: r.related, schema: null, meta },
      });
    }
  }
}

// ──────────────────────────────────────────────────────────────────────────
// 2. Roles and policies (Directus 11: permissions hang off policies)
// ──────────────────────────────────────────────────────────────────────────
const SUPPORT = 'Live Connect Support';
const PRIVACY = 'Live Connect Privacy';
const SUPPORT_POLICY = 'Live Connect Support (metadata)';
const PRIVACY_POLICY = 'Live Connect Privacy (transcript button)';
const FLOW_PREFIX = 'Live Connect - ';
const DASHBOARD = 'Live Connect - Operations';
const TRANSCRIPT_FLOW = `${FLOW_PREFIX}View transcript`;

const readPerm = (collection, fields, permissions = {}) => ({
  collection,
  action: 'read',
  fields,
  permissions,
  validation: null,
  presets: null,
});

function supportPermissions() {
  const perms = Object.entries(SUPPORT_FIELDS).map(([c, f]) => readPerm(c, f));
  // Names only, for display of the relations above.
  perms.push(readPerm('organizations', ['id', 'name']));
  perms.push(readPerm('projects', ['id', 'name']));
  // Live Connect usage in the org ledger, amounts only (no description, no session link).
  perms.push(readPerm('organization_credits_transactions', ['id', 'org_id', 'amount', 'type', 'created_at'], { type: { _eq: 'live_connect' } }));
  // Insights + buttons: only the Live Connect ones, and never the transcript button.
  perms.push(readPerm('directus_dashboards', ['*'], { name: { _starts_with: FLOW_PREFIX } }));
  perms.push(readPerm('directus_panels', ['*'], { dashboard: { name: { _starts_with: FLOW_PREFIX } } }));
  perms.push(
    readPerm('directus_flows', ['*'], {
      _and: [
        { trigger: { _eq: 'manual' } },
        { name: { _starts_with: FLOW_PREFIX } },
        { name: { _neq: TRANSCRIPT_FLOW } },
      ],
    }),
  );
  return perms;
}

function privacyPermissions() {
  // Adds exactly one thing: seeing the transcript button.
  return [readPerm('directus_flows', ['*'], { _and: [{ trigger: { _eq: 'manual' } }, { name: { _eq: TRANSCRIPT_FLOW } }] })];
}

async function ensurePolicy(name, description, icon) {
  const body = { name, icon, description, admin_access: false, app_access: true, enforce_tfa: false };
  const have = await find('/policies', 'name', name);
  if (have) {
    await api(`/policies/${have.id}`, { method: 'PATCH', body });
    return have.id;
  }
  return (await api('/policies', { method: 'POST', body })).data.id;
}

async function ensureRole(name, description, icon) {
  const body = { name, icon, description };
  const have = await find('/roles', 'name', name);
  if (have) {
    await api(`/roles/${have.id}`, { method: 'PATCH', body });
    return have.id;
  }
  return (await api('/roles', { method: 'POST', body })).data.id;
}

async function ensureAccess(roleId, policyId) {
  const r = await api(`/access?filter[role][_eq]=${roleId}&filter[policy][_eq]=${policyId}&limit=1`);
  if (r.data?.length) return 'linked';
  await api('/access', { method: 'POST', body: { role: roleId, policy: policyId } });
  return 'linked now';
}

const norm = (p) =>
  JSON.stringify({
    collection: p.collection,
    action: p.action,
    fields: [...(p.fields || [])].sort(),
    permissions: p.permissions || {},
  });

// Replace the policy's permissions with exactly `desired` (only when they differ).
async function setPermissions(policyId, desired) {
  const have = (await api(`/permissions?filter[policy][_eq]=${policyId}&limit=-1&fields=*`)).data || [];
  const same =
    have.length === desired.length &&
    JSON.stringify(have.map(norm).sort()) === JSON.stringify(desired.map(norm).sort());
  if (same) return 'unchanged';
  for (const p of have) await api(`/permissions/${p.id}`, { method: 'DELETE' });
  await api('/permissions', { method: 'POST', body: desired.map((p) => ({ ...p, policy: policyId })) });
  return `set (${desired.length})`;
}

// ──────────────────────────────────────────────────────────────────────────
// 3. Flows: manual buttons that call the server
// ──────────────────────────────────────────────────────────────────────────
const reasonField = (label, hint) => ({
  field: 'reason',
  type: 'text',
  name: label,
  meta: { field: 'reason', type: 'text', interface: 'input-multiline', required: true, width: 'full', note: hint },
});

// Runs inside Directus's sandbox (isolated-vm). Only plain JS: no imports.
// `data.$trigger.body.keys` is the selected row(s); `data.actor_lookup` the item-read.
// One step does the checks AND builds the request, because Directus allows each
// operation to be the failure target of only ONE predecessor.
const PREPARE = (idField, withReason, privacyRoleId) => `
module.exports = async function (data) {
  const keys = (data.$trigger && data.$trigger.body && data.$trigger.body.keys) || [];
  if (keys.length !== 1) throw new Error('Select exactly one row: this action applies to a single desk/session.');
  ${privacyRoleId
    ? `const acc = data.$accountability || {};
  const roles = acc.roles || (acc.role ? [acc.role] : []);
  if (!acc.admin && !roles.includes(${JSON.stringify(privacyRoleId)})) throw new Error('Only the Live Connect Privacy role may read a conversation.');`
    : ''}
  const user = Array.isArray(data.actor_lookup) ? data.actor_lookup[0] : data.actor_lookup;
  const actor = user && user.email;
  if (!actor) throw new Error('Your Directus account has no email: the audit trail needs one.');
  const body = { ${idField}: String(keys[0]), actor: actor };
  ${withReason ? `body.reason = String((data.$trigger.body && data.$trigger.body.reason) || '').trim();` : ''}
  return { body: JSON.stringify(body) };
};`;

// Turns the server's answer (or its refusal) into a short message for the operator.
const FORMAT = (kind) => `
module.exports = async function (data) {
  const KIND = ${JSON.stringify(kind)};
  const out = (s, m) => ({ subject: s, message: m });
  // A step BEFORE the request failed (selection, role, missing e-mail): nothing was sent.
  if (data.request === undefined) {
    const e = data.$last;
    return out('Live Connect: not done', String((e && e.message) || 'The action did not run.'));
  }
  const r = data.request || {};
  if (!r.data || typeof r.data !== 'object') {
    return out('Live Connect: not done', 'The server did not answer as expected' + (r.status ? ' (HTTP ' + r.status + ')' : '') + (r.message ? ': ' + r.message : '') + '.');
  }
  const res = r.data;
  if (res.code || res.message && !res.status) {
    return out('Live Connect: refused', String(res.code || 'error') + (res.message ? ' - ' + res.message : ''));
  }
  if (KIND === 'desk-revoke') return out('Desk revoked', 'Status: ' + res.status + '. Waiting customers closed: ' + res.waiting_closed + '. Audited.');
  if (KIND === 'desk-regenerate') return out('Desk regenerated', 'Token version is now ' + res.token_version + '; waiting customers closed: ' + res.waiting_closed + '. The new QR is NOT shown here by design: the organisation downloads it from its dashboard. Audited.');
  if (KIND === 'session-end') return out('Session ended', 'End reason: ' + res.end_reason + '. Finalisation started. Audited.');
  if (KIND === 'session-reconcile') {
    const t = res.ticks || {}, l = res.ledger || {};
    const lines = [
      'Verdict: ' + String(res.status).toUpperCase() + ' (payer: ' + res.payer + ')',
      'Ticks: ' + t.count + ' (org credits ' + t.org_credits + ', user USD ' + t.user_usd + ')',
      'Ledger: org entries ' + l.org_entries + ' (credits ' + l.org_credits + '), usage USD ' + l.user_usd,
      'Ticks without a debit: ' + res.missing_ticks + ' / debits without a tick: ' + res.duplicate_ticks,
    ].concat((res.notes || []).map(function (n) { return 'Note: ' + n; }));
    return out('Reconciliation: ' + res.status, lines.join('\\n'));
  }
  if (KIND === 'session-transcript') {
    const us = res.utterances || [];
    const clip = function (s) { s = String(s || ''); return s.length > 120 ? s.slice(0, 120) + '...' : s; };
    const N = ${PREVIEW_LINES};
    const preview = us.slice(0, N).map(function (u) { return '- [' + u.original_lang + '] ' + clip(u.original); });
    return out(
      'Transcript read (' + us.length + ' utterances)',
      'This access was AUDITED with your name and your reason.' +
        (N > 0 ? '\\n' + preview.join('\\n') + (us.length > N ? '\\n... ' + (us.length - N) + ' more not shown.' : '') +
          '\\nThis excerpt is a COPY: it is stored in this notification and sent by e-mail if Directus mail is on, and it is NOT covered by erasure or retention. Delete this notification when you are done.' : '\\nNo excerpt is shown, stored in this notification, e-mailed or kept in the Flow run history: only the counts above.')
    );
  }
  return out('Done', JSON.stringify(res));
};`;

function flowSpecs(privacyRoleId) {
  return [
    {
      name: `${FLOW_PREFIX}Revoke desk`,
      icon: 'block',
      color: '#E35169',
      collection: 'live_desks',
      description: 'Switch a desk\'s QR off and close everyone waiting at it (server: /api/admin/live/desk-revoke).',
      confirm: 'Revokes the desk of the selected row: its printed QR stops working immediately and customers still waiting are closed. Audited with your name and reason.',
      fields: [reasonField('Reason', 'Why is this desk being revoked?')],
      endpoint: '/api/admin/live/desk-revoke',
      idField: 'desk_id',
      kind: 'desk-revoke',
    },
    {
      name: `${FLOW_PREFIX}Regenerate desk`,
      icon: 'autorenew',
      color: '#FFA439',
      collection: 'live_desks',
      description: 'Invalidate every printed QR of a desk and put it back in service (server: /api/admin/live/desk-regenerate). The new QR is never shown here.',
      confirm: 'Replaces the desk\'s QR: every earlier print stops working. You will NOT see the new QR: the organisation downloads it from its own dashboard. Audited.',
      fields: [reasonField('Reason', 'Why is the QR being replaced?')],
      endpoint: '/api/admin/live/desk-regenerate',
      idField: 'desk_id',
      kind: 'desk-regenerate',
    },
    {
      name: `${FLOW_PREFIX}End session`,
      icon: 'call_end',
      color: '#E35169',
      collection: 'live_sessions',
      description: 'End an open conversation as the platform (server: /api/admin/live/session-end).',
      confirm: 'Ends the selected conversation now (end reason ended_by_admin). The two phones are told within seconds. Audited with your name and reason.',
      fields: [reasonField('Reason', 'Why is this conversation being ended?')],
      endpoint: '/api/admin/live/session-end',
      idField: 'session_id',
      kind: 'session-end',
    },
    {
      name: `${FLOW_PREFIX}Reconcile session`,
      icon: 'fact_check',
      color: '#3399FF',
      collection: 'live_sessions',
      description: 'Read-only billing check: metered ticks vs the ledger (server: /api/admin/live/session-reconcile). Changes nothing.',
      confirm: 'Compares what the metering recorded with what was debited for the selected conversation. Read-only.',
      fields: [],
      endpoint: '/api/admin/live/session-reconcile',
      idField: 'session_id',
      kind: 'session-reconcile',
    },
    {
      name: TRANSCRIPT_FLOW,
      icon: 'visibility',
      color: '#6644FF',
      collection: 'live_sessions',
      description: 'PRIVILEGED: read what was said in one conversation (server: /api/admin/live/session-transcript). Requires the Privacy role, a real reason, and is audited BEFORE anything is shown.',
      confirm: 'You are about to read the CONTENT of a conversation. Your name, the session and your reason are written to the audit log first. If the audit cannot be written, nothing is shown.',
      fields: [reasonField('Reason (min. 10 characters)', 'Be specific: e.g. "customer ticket #123 asked for a copy".')],
      endpoint: '/api/admin/live/session-transcript',
      idField: 'session_id',
      kind: 'session-transcript',
      privacyRoleId,
    },
  ];
}

// Operation chain for one flow. Each op carries its successors by KEY; ids are
// resolved after creation.
//   actor_lookup → prepare(check + build) → request ─ok→ format → notify
//                         └ reject → format_prep_fail → notify_prep_fail
//                                          └ reject → format_fail → notify_fail
function operationsFor(spec) {
  const withReason = spec.fields.length > 0;
  const notify = (key, from) => ({
    key,
    name: 'Tell the operator',
    type: 'notification',
    options: {
      recipient: ['{{$accountability.user}}'],
      subject: `{{${from}.subject}}`,
      message: `{{${from}.message}}`,
      permissions: '$full',
    },
  });
  return [
    {
      key: 'actor_lookup',
      name: 'Who is acting',
      type: 'item-read',
      options: { collection: 'directus_users', key: ['{{$accountability.user}}'], query: { fields: ['email'] }, permissions: '$full', emitEvents: false },
      resolve: 'prepare',
    },
    { key: 'prepare', name: 'Check and build the request', type: 'exec', options: { code: PREPARE(spec.idField, withReason, spec.privacyRoleId) }, resolve: 'request', reject: 'format_prep_fail' },
    {
      key: 'request',
      name: 'Call the VoxTranslate server',
      type: 'request',
      options: {
        method: 'POST',
        url: `{{$env.VOX_API_URL}}${spec.endpoint}`,
        headers: [
          { header: 'Content-Type', value: 'application/json' },
          { header: 'X-Admin-Secret', value: '{{$env.ADMIN_API_SECRET}}' },
        ],
        body: '{{prepare.body}}',
      },
      resolve: 'format',
      reject: 'format_fail',
    },
    { key: 'format', name: 'Format the answer', type: 'exec', options: { code: FORMAT(spec.kind) }, resolve: 'notify' },
    notify('notify', 'format'),
    { key: 'format_fail', name: 'Format the refusal', type: 'exec', options: { code: FORMAT(spec.kind) }, resolve: 'notify_fail' },
    notify('notify_fail', 'format_fail'),
    { key: 'format_prep_fail', name: 'Format the check failure', type: 'exec', options: { code: FORMAT(spec.kind) }, resolve: 'notify_prep_fail' },
    notify('notify_prep_fail', 'format_prep_fail'),
  ];
}

function triggerOptions(spec) {
  return {
    collections: [spec.collection],
    location: 'item',
    requireSelection: true,
    requireConfirmation: true,
    confirmationDescription: spec.confirm,
    async: false,
    fields: spec.fields,
  };
}

// "activity": Directus records THAT the Flow ran (directus_activity: who, when, which Flow) but
// not what the operations produced. "all" also writes every operation's output into
// directus_revisions, which for the transcript Flow is the whole conversation, readable by
// any Directus administrator without an audit row, and untouched by erasure.
const FLOW_ACCOUNTABILITY = 'activity';

// Updates a Flow IN PLACE: operations are matched by their stable `key`, so a re-run never
// deletes and recreates the chain that a running Flow is executing; operations that no longer
// exist in this script are removed last.
async function ensureFlow(spec) {
  const body = {
    name: spec.name,
    icon: spec.icon,
    color: spec.color,
    description: spec.description,
    status: 'active',
    trigger: 'manual',
    accountability: FLOW_ACCOUNTABILITY,
    options: triggerOptions(spec),
  };
  let flow = await find('/flows', 'name', spec.name);
  let verb;
  if (flow) {
    await api(`/flows/${flow.id}`, { method: 'PATCH', body });
    verb = 'updated';
  } else {
    flow = (await api('/flows', { method: 'POST', body })).data;
    verb = 'created';
  }
  const ops = operationsFor(spec);
  const existing = (await api(`/operations?filter[flow][_eq]=${flow.id}&limit=-1&fields=id,key`)).data || [];
  const byKey = new Map(existing.map((o) => [o.key, o.id]));
  const ids = {};
  let x = 19;
  for (const op of ops) {
    const fields = { name: op.name, type: op.type, position_x: x, position_y: op.key.includes('fail') ? 19 : 1, options: op.options };
    if (byKey.has(op.key)) {
      ids[op.key] = byKey.get(op.key);
      await api(`/operations/${ids[op.key]}`, { method: 'PATCH', body: fields });
    } else {
      ids[op.key] = (await api('/operations', { method: 'POST', body: { flow: flow.id, key: op.key, ...fields } })).data.id;
    }
    if (!op.key.includes('fail')) x += 18;
  }
  for (const op of ops) {
    const patch = { resolve: op.resolve ? ids[op.resolve] : null, reject: op.reject ? ids[op.reject] : null };
    await api(`/operations/${ids[op.key]}`, { method: 'PATCH', body: patch });
  }
  await api(`/flows/${flow.id}`, { method: 'PATCH', body: { operation: ids[ops[0].key] } });
  // Obsolete operations (a key this script no longer has): cut their links, then delete.
  const keep = new Set(Object.values(ids));
  const stale = existing.filter((o) => !keep.has(o.id));
  for (const o of stale) await api(`/operations/${o.id}`, { method: 'PATCH', body: { resolve: null, reject: null } });
  for (const o of stale) await api(`/operations/${o.id}`, { method: 'DELETE' });
  return { verb, id: flow.id };
}

// Runs recorded by an earlier version of this script (accountability "all") stored every
// operation's output in directus_revisions. Count them, delete them through the API when
// Directus lets us, otherwise say exactly what the operator has to run.
// Only the RUN revisions: editing a Flow also writes revisions, but those hold configuration, not
// conversation text, and are kept.
const REVISION_FILTER = (flowIds) =>
  `filter[collection][_eq]=directus_flows&filter[item][_in]=${flowIds.map(q).join(',')}&filter[activity][action][_eq]=run`;
const REVISION_SQL = `DELETE FROM directus_revisions
 WHERE activity IN (
   SELECT id FROM directus_activity
    WHERE action = 'run' AND collection = 'directus_flows'
      AND item IN (SELECT id::text FROM directus_flows WHERE name LIKE '${FLOW_PREFIX}%'));`;

// Notifications written by a --transcript-preview run (or by the old default) hold a copy of
// utterances. They belong to operators, so this only COUNTS them and says how to remove them.
async function countExcerptNotifications() {
  if (DRY) return 0;
  const r = await api(
    `/notifications?filter[subject][_starts_with]=${q('Transcript read (')}&filter[message][_contains]=${q('\n- [')}&aggregate[count]=id`,
  );
  return Number(r.data?.[0]?.count?.id ?? r.data?.[0]?.count ?? 0);
}

async function purgeFlowRevisions(flowIds) {
  if (DRY) {
    console.log('  [dry-run] GET /revisions (count of old Flow-run revisions) and DELETE them when the API allows it');
    return { found: 0, left: 0 };
  }
  const count = async () => {
    const r = await api(`/revisions?${REVISION_FILTER(flowIds)}&aggregate[count]=id`);
    return Number(r.data?.[0]?.count?.id ?? r.data?.[0]?.count ?? 0);
  };
  const found = await count();
  if (found === 0) return { found, left: 0 };
  try {
    await api(`/revisions?${REVISION_FILTER(flowIds)}`, { method: 'DELETE' });
  } catch {
    /* the API does not expose deleting revisions: reported below */
  }
  return { found, left: await count() };
}

// ──────────────────────────────────────────────────────────────────────────
// 4. Insights: metadata only (panels run with the VIEWER's permissions, so a
//    Support user's panels can only ever read what Support can read)
// ──────────────────────────────────────────────────────────────────────────
const panels = [];
const COL_X = [1, 7, 13, 19];
function metric(dash, idx, baseY, p) {
  panels.push({
    dashboard: dash, name: p.name, note: p.note || null, icon: p.icon || 'analytics', color: p.color || null,
    show_header: true, type: 'metric', position_x: COL_X[idx % 4], position_y: baseY + Math.floor(idx / 4) * 6, width: 6, height: 6,
    options: { collection: p.collection, field: p.field || 'id', function: p.fn || 'count', sortField: null, filter: p.filter || {}, prefix: p.prefix || '', suffix: p.suffix || '', abbreviate: true, conditionalFormatting: [] },
  });
}
function timeseries(dash, { x = 1, y, w = 24, h = 11, name, note, color, collection, dateField, fn = 'count', field = 'id', filter, precision = 'day', range = '8 weeks' }) {
  panels.push({
    dashboard: dash, name, note: note || null, icon: 'show_chart', color: color || null, show_header: true, type: 'time-series',
    position_x: x, position_y: y, width: w, height: h,
    options: { collection, dateField, valueField: field, function: fn, precision, range, filter: filter || {}, color: color || null, fillHoles: true, showXAxis: true, showYAxis: true },
  });
}
function list(dash, { x = 1, y, w = 12, h = 12, name, note, collection, sortField = 'created_at', sortDirection = 'desc', limit = 8, filter, template }) {
  panels.push({
    dashboard: dash, name, note: note || null, icon: 'list', color: null, show_header: true, type: 'list',
    position_x: x, position_y: y, width: w, height: h,
    options: { collection, limit, sortField, sortDirection, filter: filter || {}, ...(template ? { displayTemplate: template } : {}) },
  });
}
function barchart(dash, { x = 1, y, w = 12, h = 14, name, note, color, collection, xField, fn = 'count', field = 'id', filter, horizontal = true, displayField }) {
  panels.push({
    dashboard: dash, name, note: note || null, icon: 'bar_chart', color: color || null, show_header: true, type: 'bar-chart',
    position_x: x, position_y: y, width: w, height: h,
    options: { collection, xAxis: xField, yAxis: field, function: fn, filter: filter || {}, horizontal, color: color || null, showDataLabel: true, sortDirection: 'desc', ...(displayField ? { xAxisDisplayField: displayField } : {}) },
  });
}

async function ensureDashboard(name, icon, note) {
  const found = (await api(`/dashboards?filter[name][_eq]=${q(name)}&fields=id&limit=1`)).data;
  if (found?.length) {
    await api(`/dashboards/${found[0].id}`, { method: 'PATCH', body: { icon, note } });
    return found[0].id;
  }
  return (await api('/dashboards', { method: 'POST', body: { name, icon, note } })).data.id;
}

// Panels are matched by NAME (unique within this dashboard) and updated in place; a panel this
// script no longer defines is removed.
async function syncPanels(dash) {
  const have = (await api(`/panels?filter[dashboard][_eq]=${dash}&fields=id,name&limit=-1`)).data || [];
  const byName = new Map(have.map((p) => [p.name, p.id]));
  let ok = 0;
  for (const p of panels) {
    try {
      if (byName.has(p.name)) await api(`/panels/${byName.get(p.name)}`, { method: 'PATCH', body: p });
      else await api('/panels', { method: 'POST', body: p });
      ok++;
    } catch (e) {
      console.warn(`  ! panel "${p.name}": ${trunc(e)}`);
    }
  }
  const wanted = new Set(panels.map((p) => p.name));
  for (const p of have) if (!wanted.has(p.name)) await api(`/panels/${p.id}`, { method: 'DELETE' });
  return ok;
}

const DAYS7 = '$NOW(-7 days)';
const DAYS30 = '$NOW(-30 days)';
const FAILED = { _or: [{ end_reason: { _in: ['abandoned', 'expired', 'ended_by_billing', 'max_duration'] } }, { status: { _eq: 'expired' } }] };

function buildInsights(d) {
  let i = 0;
  metric(d, i++, 1, { name: 'Open sessions now', icon: 'sensors', color: '#2ECDA7', collection: 'live_sessions', filter: { status: { _in: ['waiting', 'active'] } }, note: 'waiting + active' });
  metric(d, i++, 1, { name: 'Active conversations', icon: 'forum', color: '#2ECDA7', collection: 'live_sessions', filter: { status: { _eq: 'active' } } });
  metric(d, i++, 1, { name: 'Sessions (7d)', icon: 'history', collection: 'live_sessions', filter: { created_at: { _gte: DAYS7 } } });
  metric(d, i++, 1, { name: 'Expired while waiting (7d)', icon: 'hourglass_empty', color: '#FFA439', collection: 'live_sessions', filter: { _and: [{ created_at: { _gte: DAYS7 } }, { status: { _eq: 'expired' } }, { started_at: { _null: true } }] }, note: 'Nobody answered in time. Compare with "Sessions (7d)" for the waiting-expired rate.' });
  metric(d, i++, 1, { name: 'Failed / abandoned (7d)', icon: 'error', color: '#E35169', collection: 'live_sessions', filter: { _and: [{ created_at: { _gte: DAYS7 } }, FAILED] } });
  metric(d, i++, 1, { name: 'Ended by an operator (7d)', icon: 'admin_panel_settings', collection: 'live_sessions', filter: { _and: [{ created_at: { _gte: DAYS7 } }, { end_reason: { _eq: 'ended_by_admin' } }] } });
  metric(d, i++, 1, { name: 'Desks revoked', icon: 'block', color: '#E35169', collection: 'live_desks', filter: { status: { _eq: 'revoked' } } });
  metric(d, i++, 1, { name: 'Org credits spent (7d)', icon: 'toll', color: '#00C4B4', collection: 'organization_credits_transactions', fn: 'sum', field: 'amount', filter: { _and: [{ type: { _eq: 'live_connect' } }, { created_at: { _gte: DAYS7 } }] }, note: 'Negative: credits debited from organisation wallets for Live Connect.' });
  const tsY = 1 + Math.ceil(i / 4) * 6;
  timeseries(d, { x: 1, y: tsY, w: 12, name: 'Sessions / day', color: '#3399FF', collection: 'live_sessions', dateField: 'created_at' });
  timeseries(d, { x: 13, y: tsY, w: 12, name: 'Failed / abandoned / day', color: '#E35169', collection: 'live_sessions', dateField: 'created_at', filter: FAILED });
  const mid = tsY + 11;
  barchart(d, { x: 1, y: mid, w: 12, name: 'Sessions by end reason', color: '#6644FF', collection: 'live_sessions', xField: 'end_reason', filter: { end_reason: { _nnull: true } }, note: 'Why conversations ended' });
  barchart(d, { x: 13, y: mid, w: 12, name: 'Sessions by desk', color: '#00C4B4', collection: 'live_sessions', xField: 'desk_id', filter: { desk_id: { _nnull: true } }, note: 'Busiest desks (count). Credits per desk are in the two panels below.' });
  timeseries(d, { x: 1, y: mid + 14, w: 24, h: 10, name: 'Org credits spent / day', color: '#00C4B4', collection: 'organization_credits_transactions', dateField: 'created_at', fn: 'sum', field: 'amount', filter: { type: { _eq: 'live_connect' } } });
  // Credits per desk, from the live_desk_usage_daily table (see migration 068). The bar-chart panel has
  // no top-N option, so the chart shows every desk and the list below it is the top 10.
  const deskY = mid + 24;
  barchart(d, { x: 1, y: deskY, w: 12, name: 'Credits by desk (30d)', color: '#00C4B4', collection: 'live_desk_usage_daily', xField: 'desk_id', fn: 'sum', field: 'credits', filter: { day: { _gte: DAYS30 } }, displayField: 'name', note: 'Organisation credits debited per service desk over the last 30 days (UTC days).' });
  list(d, { x: 13, y: deskY, w: 12, h: 14, name: 'Top 10 desk-days by credits (30d)', collection: 'live_desk_usage_daily', sortField: 'credits', sortDirection: 'desc', limit: 10, filter: { day: { _gte: DAYS30 } }, template: '{{desk_id.name}} · {{day}} · {{credits}} credits · {{sessions}} conv.', note: 'The ten busiest (desk, day) pairs of the last 30 days.' });
  const listY = deskY + 14;
  list(d, { x: 1, y: listY, w: 12, name: 'Failed / abandoned sessions', collection: 'live_sessions', filter: FAILED, note: 'Newest first. Select one → Reconcile session / End session.' });
  list(d, { x: 13, y: listY, w: 12, name: 'Recent sessions', collection: 'live_sessions' });
}

// ──────────────────────────────────────────────────────────────────────────
// 5. Access audit (READ-ONLY). Live conversations live in the meeting tables, so any EXISTING
//    role that can read them can read live conversations. This script never edits a role: it
//    lists the policies that lack the live-exclusion filter so a human fixes them.
// ──────────────────────────────────────────────────────────────────────────
const canon = (v) => {
  if (Array.isArray(v)) return v.map(canon).sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b)));
  if (v && typeof v === 'object') return Object.fromEntries(Object.keys(v).sort().map((k) => [k, canon(v[k])]));
  return v;
};
const same = (a, b) => JSON.stringify(canon(a)) === JSON.stringify(canon(b));
const NEQ_ONLY = { kind: { _neq: 'live_connect' } }; // stricter than NOT_LIVE (also hides NULL kind): still safe
// Does `filter` (a permission's item rule) contain one of the accepted exclusions, possibly
// inside an `_and`? Anything we cannot prove counts as "not guarded".
function guards(filter, accepted) {
  if (!filter || typeof filter !== 'object') return false;
  if (accepted.some((a) => same(filter, a))) return true;
  return Array.isArray(filter._and) && filter._and.some((f) => guards(f, accepted));
}
const ACCEPTED = Object.fromEntries(
  Object.keys(TRANSCRIPT_FILTERS).map((c) => [
    c,
    c === 'call_sessions' ? [NOT_LIVE, NEQ_ONLY] : [{ session_id: NOT_LIVE }, { session_id: NEQ_ONLY }],
  ]),
);

async function auditAccess() {
  const collections = Object.keys(TRANSCRIPT_FILTERS);
  const policies = (await api('/policies?limit=-1&fields=id,name,admin_access')).data || [];
  const access = (await api('/access?limit=-1&fields=policy,role,user')).data || [];
  const attached = new Set(access.map((a) => (typeof a.policy === 'object' ? a.policy?.id : a.policy)));
  const perms =
    (
      await api(
        `/permissions?filter[action][_eq]=read&filter[collection][_in]=${collections.map(q).join(',')}&limit=-1&fields=id,policy,collection,fields,permissions`,
      )
    ).data || [];
  const byPolicy = new Map(policies.map((p) => [p.id, p]));
  const findings = [];
  for (const perm of perms) {
    const policy = byPolicy.get(typeof perm.policy === 'object' ? perm.policy?.id : perm.policy);
    if (!policy || policy.admin_access) continue; // administrators can read everything by design
    if (!perm.fields || perm.fields.length === 0) continue; // a read permission with no field reads nothing
    if (guards(perm.permissions, ACCEPTED[perm.collection])) continue;
    findings.push({ policy: policy.name, collection: perm.collection, attached: attached.has(policy.id) });
  }
  findings.sort((a, b) => a.policy.localeCompare(b.policy) || a.collection.localeCompare(b.collection));
  const admins = policies.filter((p) => p.admin_access).length;
  return { findings, admins, total: policies.length };
}

function printAudit({ findings, admins, total }) {
  console.log(`▸ Access audit (read-only): ${total} policies, ${admins} with admin access (always able to read live conversations: keep that group small)`);
  if (findings.length === 0) {
    console.log('  ✓ no non-admin policy can read live conversations unfiltered.');
    return;
  }
  const w = Math.max(6, ...findings.map((f) => f.policy.length));
  console.log('');
  console.log('  !!! WARNING: these policies can read LIVE CONVERSATIONS (S15: not exposed by default) !!!');
  console.log(`  ${'POLICY'.padEnd(w)}  ${'COLLECTION'.padEnd(22)}  ATTACHED TO A ROLE/USER`);
  for (const f of findings) console.log(`  ${f.policy.padEnd(w)}  ${f.collection.padEnd(22)}  ${f.attached ? 'yes' : 'no (inert today)'}`);
  console.log('');
  console.log('  Remedy: add the live-exclusion filter to each row above (this script never edits');
  console.log('  existing roles). The exact JSON per collection:');
  console.log('    node directus/setup-live-connect.mjs --print-transcript-filter');
}

// ──────────────────────────────────────────────────────────────────────────
// Run
// ──────────────────────────────────────────────────────────────────────────
async function requireDirectus11() {
  const info = (await api('/server/info')).data;
  const version = info?.version;
  if (!version) {
    console.log('  ! could not read the Directus version (needs an admin token); assuming 11.');
    return;
  }
  if (Number.parseInt(String(version).split('.')[0], 10) < 11) {
    fail(`Directus ${version} found: this script needs Directus 11 (policies).`);
  }
  console.log(`  Directus ${version}`);
}

async function main() {
  if (DRY) {
    console.log('DRY RUN: nothing is sent anywhere; reads are assumed empty, so everything shows as "create".\n');
  } else {
    TOKEN = await getToken();
    console.log('▸ Server');
    await requireDirectus11();
  }

  if (AUDIT_ONLY) {
    if (DRY) {
      console.log('▸ Access audit: would read /policies, /access and /permissions (read-only) and exit 1 if a non-admin policy reads live conversations unfiltered.');
      return;
    }
    const result = await auditAccess();
    printAudit(result);
    process.exit(result.findings.length ? 1 : 0);
  }

  console.log(
    PREVIEW_LINES > 0
      ? '▸ Transcript preview: ON (--transcript-preview). The View transcript button copies 3 x 120 characters into the operator\'s notification, which is stored in directus_notifications and e-mailed in clear text when Directus mail is configured; erasure and retention do NOT reach those copies.'
      : '▸ Transcript preview: OFF (default). The View transcript button reports counts only; no conversation text reaches a notification or an e-mail.',
  );

  console.log('▸ Folder');
  console.log(`  • ${FOLDER.name}: ${await ensureFolder()}`);

  console.log('▸ Collections');
  for (const [name, cfg] of Object.entries(COLLECTIONS)) {
    console.log(`  • ${name}: ${await ensureCollection(name, cfg)}`);
  }
  console.log('  • live_usage_ticks: skipped (composite primary key; Directus cannot manage it). Per-desk credits: the live_desk_usage_daily table above.');

  console.log('▸ Fields (read-only, sensitive columns hidden) and relations');
  for (const [name, cfg] of Object.entries(COLLECTIONS)) {
    await ensureFields(name, cfg.fields);
    await ensureAliases(name, cfg.aliases);
    await ensureMetaRelations(name, cfg.relations);
    console.log(`  • ${name}: ${Object.keys(cfg.fields).length} fields${cfg.aliases ? `, ${cfg.aliases.length} reverse relations` : ''}`);
  }

  console.log('▸ Roles and policies');
  const supportPolicy = await ensurePolicy(SUPPORT_POLICY, 'Read-only metadata of Live Connect: statuses, languages, durations, end reasons, credits. No names, no transcripts, no QR.', 'support_agent');
  const privacyPolicy = await ensurePolicy(PRIVACY_POLICY, 'Adds the "View transcript" button (reason mandatory, audited by the server).', 'policy');
  const supportRole = await ensureRole(SUPPORT, 'Live Connect support: read-only metadata.', 'support_agent');
  const privacyRole = await ensureRole(PRIVACY, 'Live Connect support + the audited transcript button.', 'policy');
  console.log(`  • ${SUPPORT_POLICY}: permissions ${await setPermissions(supportPolicy, supportPermissions())}`);
  console.log(`  • ${PRIVACY_POLICY}: permissions ${await setPermissions(privacyPolicy, privacyPermissions())}`);
  console.log(`  • ${SUPPORT} → ${SUPPORT_POLICY}: ${await ensureAccess(supportRole, supportPolicy)}`);
  console.log(`  • ${PRIVACY} → ${SUPPORT_POLICY}: ${await ensureAccess(privacyRole, supportPolicy)}`);
  console.log(`  • ${PRIVACY} → ${PRIVACY_POLICY}: ${await ensureAccess(privacyRole, privacyPolicy)}`);

  console.log(`▸ Flows (accountability "${FLOW_ACCOUNTABILITY}": runs are logged, their content is not)`);
  const flowIds = [];
  for (const spec of flowSpecs(privacyRole)) {
    const { verb, id } = await ensureFlow(spec);
    flowIds.push(id);
    console.log(`  • ${spec.name}: ${verb}`);
  }
  const rev = await purgeFlowRevisions(flowIds);
  if (rev.found === 0) {
    console.log('  • old Flow-run revisions: none');
  } else if (rev.left === 0) {
    console.log(`  • old Flow-run revisions: ${rev.found} deleted through the API (they held operation outputs, i.e. conversation text)`);
  } else {
    console.log(`  ! ${rev.left} old Flow-run revision(s) still hold operation outputs (conversation text) and the Directus API cannot delete revisions.`);
    console.log('    They are readable by any Directus administrator and survive erasure. Run this ONCE against the Directus database yourself:');
    for (const l of REVISION_SQL.split('\n')) console.log('      ' + l);
  }

  const excerpts = await countExcerptNotifications();
  if (excerpts > 0) {
    console.log(`  ! ${excerpts} notification(s) "Transcript read (...)" still hold an excerpt of a conversation (copies are also in the e-mail that was sent, which cannot be recalled).`);
    console.log('    Delete them (Directus: Notifications, or as admin: DELETE /notifications?filter[subject][_starts_with]=Transcript read ( ).');
  }

  console.log('▸ Insights');
  const dash = await ensureDashboard(DASHBOARD, 'forum', 'Live Connect operations. Metadata only: statuses, end reasons, counts, credits.');
  buildInsights(dash);
  const ok = DRY ? (await Promise.all(panels.map((p) => api('/panels', { method: 'POST', body: p }))), panels.length) : await syncPanels(dash);
  console.log(`  • ${ok}/${panels.length} panels`);

  let audit = null;
  if (DRY) {
    console.log('▸ Access audit: would read /policies, /access and /permissions (read-only) after provisioning.');
  } else {
    audit = await auditAccess();
    printAudit(audit);
  }

  if (DRY) {
    console.log(`\n✓ Dry run complete: ${planned} write calls planned, none sent.`);
  } else {
    console.log('\n✓ Live Connect administration provisioned.');
    console.log('  Create operators in Directus (Users) and give them ONE of the two roles;');
    console.log('  this script creates no users. Existing roles are untouched: see the README for');
    console.log('  the permission filter that keeps live conversations out of them');
    console.log('  (node directus/setup-live-connect.mjs --print-transcript-filter).');
    if (audit?.findings.length) {
      console.log(`\n!!! ${audit.findings.length} access finding(s) above are still OPEN: provisioning is done, but live conversations remain readable by those policies. Re-check with --audit-access (exits 1 until fixed).`);
    }
  }
}

function compact(v) {
  const s = JSON.stringify(v);
  return s.length > 220 ? s.slice(0, 220) + `… (${s.length} chars)` : s;
}
function trunc(e) {
  return String(e && e.message ? e.message : e).replace(/\s+/g, ' ').slice(0, 200);
}
function fail(msg) {
  console.error('✗ ' + msg);
  process.exit(1);
}

main().catch((e) => fail(trunc(e)));
