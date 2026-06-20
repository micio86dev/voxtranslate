#!/usr/bin/env node
// One-off: merge translated onboarding strings into every locale file.
//
// Translators write per-locale partials to client/.onb-i18n/<code>.json containing ONLY the
// new onb* keys. This script validates each partial (exact key set, non-empty, placeholders
// preserved) and inserts the keys into client/src/scripts/i18n/<code>.json via minimal string
// insertion (no reformatting of existing content). Idempotent: a locale that already has the
// keys is skipped. en is the source and is never touched.
import fs from 'node:fs';
import path from 'node:path';
import url from 'node:url';

const here = path.dirname(url.fileURLToPath(import.meta.url));
const I18N_DIR = path.join(here, '..', 'src', 'scripts', 'i18n');
const TMP_DIR = path.join(here, '..', '.onb-i18n');

const en = JSON.parse(fs.readFileSync(path.join(I18N_DIR, 'en.json'), 'utf8'));
const ONB_KEYS = Object.keys(en).filter((k) => k.startsWith('onb'));
const ph = (s) => [...new Set(String(s).match(/\{[a-zA-Z]+\}/g) ?? [])].sort().join(',');
const enPh = Object.fromEntries(ONB_KEYS.map((k) => [k, ph(en[k])]));

const locales = fs
  .readdirSync(I18N_DIR)
  .filter((f) => f.endsWith('.json') && f !== 'en.json')
  .map((f) => f.replace(/\.json$/, ''));

const report = { merged: [], skipped: [], missing: [], invalid: [] };

for (const code of locales) {
  const tmpPath = path.join(TMP_DIR, `${code}.json`);
  const locPath = path.join(I18N_DIR, `${code}.json`);
  const locText = fs.readFileSync(locPath, 'utf8');
  const locObj = JSON.parse(locText);

  if (ONB_KEYS.every((k) => k in locObj)) {
    report.skipped.push(code);
    continue;
  }
  if (!fs.existsSync(tmpPath)) {
    report.missing.push(code);
    continue;
  }

  let tr;
  try {
    tr = JSON.parse(fs.readFileSync(tmpPath, 'utf8'));
  } catch (e) {
    report.invalid.push(`${code}: bad JSON (${e.message})`);
    continue;
  }

  const problems = [];
  const trKeys = Object.keys(tr);
  const missingKeys = ONB_KEYS.filter((k) => !(k in tr));
  const extraKeys = trKeys.filter((k) => !ONB_KEYS.includes(k));
  if (missingKeys.length) problems.push(`missing ${missingKeys.join(',')}`);
  if (extraKeys.length) problems.push(`extra ${extraKeys.join(',')}`);
  for (const k of ONB_KEYS) {
    if (!(k in tr)) continue;
    if (typeof tr[k] !== 'string' || tr[k].trim() === '') problems.push(`empty ${k}`);
    else if (ph(tr[k]) !== enPh[k]) problems.push(`placeholder ${k} (${ph(tr[k])} != ${enPh[k]})`);
  }
  if (problems.length) {
    report.invalid.push(`${code}: ${problems.join('; ')}`);
    continue;
  }

  const block = ONB_KEYS.map((k) => `  ${JSON.stringify(k)}: ${JSON.stringify(tr[k])}`).join(',\n');
  const idx = locText.lastIndexOf('}');
  const head = locText.slice(0, idx).replace(/\s*$/, '');
  const out = `${head},\n${block}\n}\n`;
  fs.writeFileSync(locPath, out);
  report.merged.push(code);
}

console.log(`merged:  ${report.merged.length} (${report.merged.join(', ') || '-'})`);
console.log(`skipped: ${report.skipped.length} (already had keys)`);
if (report.missing.length) console.log(`MISSING partials: ${report.missing.join(', ')}`);
if (report.invalid.length) {
  console.log('INVALID partials:');
  for (const line of report.invalid) console.log(`  - ${line}`);
}
const incomplete = report.missing.length + report.invalid.length;
console.log(incomplete ? `\n❌ ${incomplete} locale(s) still need work.` : `\n✅ all ${locales.length} locales have onboarding keys.`);
process.exit(incomplete ? 1 : 0);
