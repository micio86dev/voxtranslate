// Build the Standard-vs-Premium analytics dashboards in Directus (spec 0097 / #241).
//
// Like setup-backoffice.mjs, dashboards/panels live in Directus's own tables and
// are created via its API — the browser UI is org-blocked. Panels read ONLY the
// precomputed aggregate tables (plan_usage_daily, user_usage_stats) filled by the
// server roll-up, never the raw session_usage_events store.
//
// Run AFTER the analytics migrations (012) have been applied (i.e. after the
// server deploy creates the tables):
//   DIRECTUS_URL=https://directus-production-ad16.up.railway.app \
//     DIRECTUS_ADMIN_EMAIL=… DIRECTUS_ADMIN_PASSWORD=…   # or DIRECTUS_TOKEN=<static admin token>
//   node directus/setup-analytics-dashboards.mjs
//
// Idempotent: re-running PATCHes existing collections and replaces the dashboards'
// panels.

const fail = (m) => {
  console.error(m);
  process.exit(1);
};
const trunc = (e) => String(e).slice(0, 160);

const base = (process.env.DIRECTUS_URL || '').replace(/\/$/, '');
if (!base) fail('Set DIRECTUS_URL to your Directus base URL.');
let TOKEN = '';

async function api(path, { method = 'GET', body } = {}) {
  const res = await fetch(base + path, {
    method,
    headers: {
      'Content-Type': 'application/json',
      ...(TOKEN ? { Authorization: `Bearer ${TOKEN}` } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) throw new Error(`${method} ${path} → ${res.status} ${await res.text()}`);
  return res.status === 204 ? null : res.json();
}

async function getToken() {
  if (process.env.DIRECTUS_TOKEN) return process.env.DIRECTUS_TOKEN;
  const email = process.env.DIRECTUS_ADMIN_EMAIL;
  const password = process.env.DIRECTUS_ADMIN_PASSWORD;
  if (!email || !password) fail('Set DIRECTUS_TOKEN or DIRECTUS_ADMIN_EMAIL + DIRECTUS_ADMIN_PASSWORD.');
  const r = await api('/auth/login', { method: 'POST', body: { email, password } });
  return r.data.access_token;
}

/** Register an existing DB table as a Directus collection (no CREATE TABLE). */
async function ensureCollection(name, cfg) {
  const meta = { icon: cfg.icon, color: cfg.color || null, note: cfg.note || null, hidden: false };
  try {
    await api(`/collections/${name}`, { method: 'PATCH', body: { meta } });
    return 'updated';
  } catch (ePatch) {
    try {
      await api('/collections', { method: 'POST', body: { collection: name, schema: null, meta } });
      return 'registered';
    } catch (ePost) {
      console.warn(`  ! ${name}: ${trunc(ePatch)} | ${trunc(ePost)}`);
      return 'failed';
    }
  }
}

async function ensureDashboard(name, icon, note) {
  const found = (
    await api(`/dashboards?filter[name][_eq]=${encodeURIComponent(name)}&fields=id&limit=1`)
  ).data;
  if (found.length) {
    const id = found[0].id;
    const existing = (await api(`/panels?filter[dashboard][_eq]=${id}&fields=id&limit=-1`)).data;
    for (const p of existing) await api(`/panels/${p.id}`, { method: 'DELETE' });
    await api(`/dashboards/${id}`, { method: 'PATCH', body: { icon, note } });
    return id;
  }
  return (await api('/dashboards', { method: 'POST', body: { name, icon, note } })).data.id;
}

const panels = [];
const COL_X = [1, 7, 13, 19];
const STD = { plan: { _eq: 'standard' } };
const PREM = { plan: { _eq: 'premium' } };

function metric(dash, idx, baseY, p) {
  panels.push({
    dashboard: dash, name: p.name, note: p.note || null, icon: p.icon || 'analytics',
    color: p.color || null, show_header: true, type: 'metric',
    position_x: COL_X[idx % 4], position_y: baseY + Math.floor(idx / 4) * 6, width: 6, height: 6,
    options: {
      collection: p.collection, field: p.field || 'id', function: p.fn || 'count',
      sortField: null, filter: p.filter || {}, prefix: p.prefix || '', suffix: p.suffix || '',
      abbreviate: true, conditionalFormatting: [],
    },
  });
}
function timeseries(dash, o) {
  panels.push({
    dashboard: dash, name: o.name, note: o.note || null, icon: 'show_chart', color: o.color || null,
    show_header: true, type: 'time-series',
    position_x: o.x ?? 1, position_y: o.y, width: o.w ?? 24, height: o.h ?? 11,
    options: {
      collection: o.collection, dateField: o.dateField, valueField: o.field || 'id',
      function: o.fn || 'count', precision: o.precision || 'day', range: o.range || '12 weeks',
      filter: o.filter || {}, color: o.color || null, fillHoles: true, showXAxis: true, showYAxis: true,
    },
  });
}
function barchart(dash, o) {
  panels.push({
    dashboard: dash, name: o.name, note: o.note || null, icon: 'bar_chart', color: o.color || null,
    show_header: true, type: 'bar-chart',
    position_x: o.x ?? 1, position_y: o.y, width: o.w ?? 24, height: o.h ?? 12,
    options: {
      collection: o.collection, xAxis: o.xField, yAxis: o.field || 'id', function: o.fn || 'count',
      filter: o.filter || {}, horizontal: o.horizontal ?? true, color: o.color || null,
      showDataLabel: true, sortDirection: 'desc',
    },
  });
}
function list(dash, o) {
  panels.push({
    dashboard: dash, name: o.name, note: o.note || null, icon: 'leaderboard', color: null,
    show_header: true, type: 'list',
    position_x: o.x ?? 1, position_y: o.y, width: o.w ?? 24, height: o.h ?? 12,
    options: { collection: o.collection, limit: o.limit || 10, sortField: o.sortField, sortDirection: o.sortDirection || 'desc', filter: o.filter || {} },
  });
}

const GREEN = '#2ECDA7';
const PURPLE = '#6644FF';
const BLUE = '#3399FF';

// Dashboard 1 — Plan Distribution (Standard vs Premium).
function buildPlanDistribution(d) {
  let i = 0;
  metric(d, i++, 1, { name: 'Standard minutes', icon: 'graphic_eq', color: BLUE, fn: 'sum', field: 'total_minutes', collection: 'plan_usage_daily', filter: STD });
  metric(d, i++, 1, { name: 'Premium minutes', icon: 'auto_awesome', color: PURPLE, fn: 'sum', field: 'total_minutes', collection: 'plan_usage_daily', filter: PREM });
  metric(d, i++, 1, { name: 'Standard sessions', icon: 'videocam', color: BLUE, fn: 'sum', field: 'total_sessions', collection: 'plan_usage_daily', filter: STD });
  metric(d, i++, 1, { name: 'Premium sessions', icon: 'videocam', color: PURPLE, fn: 'sum', field: 'total_sessions', collection: 'plan_usage_daily', filter: PREM });
  barchart(d, { y: 7, h: 12, name: 'Minutes by plan', collection: 'plan_usage_daily', xField: 'plan', fn: 'sum', field: 'total_minutes', color: PURPLE });
}

// Dashboard 2 — Revenue Overview (cost/revenue per plan over time).
function buildRevenue(d) {
  let i = 0;
  metric(d, i++, 1, { name: 'Standard cost (¢)', icon: 'payments', color: BLUE, fn: 'sum', field: 'total_cost_cents', collection: 'plan_usage_daily', filter: STD, note: 'From event cost_cents — wire per-event cost to populate' });
  metric(d, i++, 1, { name: 'Premium cost (¢)', icon: 'payments', color: PURPLE, fn: 'sum', field: 'total_cost_cents', collection: 'plan_usage_daily', filter: PREM });
  metric(d, i++, 1, { name: 'Stripe revenue', icon: 'attach_money', color: GREEN, prefix: '$', fn: 'sum', field: 'amount', collection: 'credit_transactions', filter: { kind: { _eq: 'purchase' } }, note: 'Authoritative revenue (all plans)' });
  metric(d, i++, 1, { name: 'Credits spent', icon: 'mic', prefix: '$', fn: 'sum', field: 'amount', collection: 'credit_transactions', filter: { kind: { _eq: 'usage' } } });
  timeseries(d, { y: 7, name: 'Premium cost / day (¢)', color: PURPLE, collection: 'plan_usage_daily', dateField: 'day', fn: 'sum', field: 'total_cost_cents', filter: PREM });
}

// Dashboard 3 — Usage Trends (daily/weekly/monthly).
function buildUsageTrends(d) {
  timeseries(d, { x: 1, y: 1, w: 12, name: 'Minutes / day', color: PURPLE, collection: 'plan_usage_daily', dateField: 'day', fn: 'sum', field: 'total_minutes' });
  timeseries(d, { x: 13, y: 1, w: 12, name: 'Sessions / day', color: BLUE, collection: 'plan_usage_daily', dateField: 'day', fn: 'sum', field: 'total_sessions' });
  timeseries(d, { x: 1, y: 12, w: 12, name: 'Premium minutes / day', color: PURPLE, collection: 'plan_usage_daily', dateField: 'day', fn: 'sum', field: 'total_minutes', filter: PREM });
  timeseries(d, { x: 13, y: 12, w: 12, name: 'Standard minutes / day', color: BLUE, collection: 'plan_usage_daily', dateField: 'day', fn: 'sum', field: 'total_minutes', filter: STD });
}

// Dashboard 4 — Feature Breakdown (raw event store, grouped by feature).
function buildFeatureBreakdown(d) {
  barchart(d, { x: 1, y: 1, w: 24, h: 14, name: 'Events by feature', collection: 'session_usage_events', xField: 'feature', fn: 'count', color: PURPLE, note: 'translation / screen_share / session (extend as more events are wired)' });
  barchart(d, { x: 1, y: 15, w: 24, h: 12, name: 'Events by type & plan', collection: 'session_usage_events', xField: 'event_type', fn: 'count', color: BLUE });
}

// Dashboard 5 — User Intelligence (top / power users).
function buildUserIntelligence(d) {
  let i = 0;
  metric(d, i++, 1, { name: 'Active users', icon: 'group', color: GREEN, collection: 'user_usage_stats' });
  metric(d, i++, 1, { name: 'Total minutes', icon: 'timer', color: PURPLE, fn: 'sum', field: 'total_minutes', collection: 'user_usage_stats' });
  metric(d, i++, 1, { name: 'Premium minutes', icon: 'auto_awesome', color: PURPLE, fn: 'sum', field: 'premium_minutes', collection: 'user_usage_stats' });
  metric(d, i++, 1, { name: 'Standard minutes', icon: 'graphic_eq', color: BLUE, fn: 'sum', field: 'standard_minutes', collection: 'user_usage_stats' });
  list(d, { x: 1, y: 7, w: 12, h: 14, name: 'Top users by minutes', collection: 'user_usage_stats', sortField: 'total_minutes', note: 'Power users / cost-heavy' });
  list(d, { x: 13, y: 7, w: 12, h: 14, name: 'Recently active', collection: 'user_usage_stats', sortField: 'last_active_at' });
}

async function main() {
  TOKEN = await getToken();
  console.log(`→ ${base}`);

  console.log('Registering aggregate collections…');
  for (const [name, cfg] of [
    ['plan_usage_daily', { icon: 'donut_large', color: '#6644FF', note: 'Daily Standard-vs-Premium aggregates (#241)' }],
    ['user_usage_stats', { icon: 'leaderboard', color: '#2ECDA7', note: 'Per-user usage rollup (#241)' }],
    ['session_usage_events', { icon: 'bolt', color: '#FFA439', note: 'Raw analytics events (append-only) — use aggregates for dashboards' }],
  ]) {
    console.log(`  ${name}: ${await ensureCollection(name, cfg)}`);
  }

  const dashes = [
    ['📊 Plan Distribution', 'donut_large', 'Standard vs Premium usage', buildPlanDistribution],
    ['💰 Revenue Overview', 'payments', 'Cost / revenue per plan', buildRevenue],
    ['📈 Usage Trends', 'show_chart', 'Daily usage over time', buildUsageTrends],
    ['🧩 Feature Breakdown', 'bar_chart', 'Usage by feature / event', buildFeatureBreakdown],
    ['🧠 User Intelligence', 'leaderboard', 'Top + power users', buildUserIntelligence],
  ];
  for (const [name, icon, note, build] of dashes) {
    const id = await ensureDashboard(name, icon, note);
    build(id);
    console.log(`  dashboard: ${name}`);
  }

  console.log(`Creating ${panels.length} panels…`);
  for (const p of panels) {
    try {
      await api('/panels', { method: 'POST', body: p });
    } catch (e) {
      console.warn(`  ! panel "${p.name}": ${trunc(e)}`);
    }
  }
  console.log('✓ Analytics dashboards ready.');
}

main().catch((e) => fail(String(e)));
