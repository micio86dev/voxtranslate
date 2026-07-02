#!/usr/bin/env node
// Build a Vox Voices pack manifest from a local directory of model files.
//
// Usage:
//   node scripts/build-vox-pack.mjs <srcDir> <baseUrl> [version] [packId]
//
// <srcDir>  a directory laid out exactly as the engine expects (mirrors the HF repo
//           onnx-community/Kokoro-82M-v1.0-ONNX plus a self-hosted ORT runtime):
//             config.json
//             tokenizer.json
//             tokenizer_config.json
//             onnx/model_quantized.onnx          (dtype q8)
//             voices/<voice>.bin                 (one per voice)
//             ort/ort-wasm-simd-threaded.jsep.wasm
//             ort/ort-wasm-simd-threaded.jsep.mjs
// <baseUrl> the public base URL the files will live under on Supabase Storage, e.g.
//           https://<ref>.supabase.co/storage/v1/object/public/voice-packs/kokoro-en/1.0.0/
//
// Emits <srcDir>/manifest-pack.json (a single pack). Combine packs into the top-level
// manifest.json ({ schemaVersion: 1, packs: [...] }) that PUBLIC_VOX_MANIFEST_URL points to.
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

const [, , srcDir, baseUrl, version = '1.0.0', packId = 'kokoro-en'] = process.argv;
if (!srcDir || !baseUrl) {
  console.error('usage: build-vox-pack.mjs <srcDir> <baseUrl> [version] [packId]');
  process.exit(1);
}

/** Recursively list files as paths relative to root (POSIX separators). */
function walk(root, rel = '') {
  const out = [];
  for (const name of fs.readdirSync(path.join(root, rel))) {
    const r = rel ? `${rel}/${name}` : name;
    const stat = fs.statSync(path.join(root, r));
    if (stat.isDirectory()) out.push(...walk(root, r));
    else if (name !== 'manifest-pack.json') out.push(r);
  }
  return out;
}

function sha256(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

/** Kokoro voice-id prefix → BCP-47 lang. First char = language/region, second = gender.
 *  (a=American, b=British English; e=Spanish, f=French, i=Italian, p=BR Portuguese, h=Hindi.) */
const LANG_BY_PREFIX = {
  a: 'en-US',
  b: 'en-GB',
  e: 'es-ES',
  f: 'fr-FR',
  i: 'it-IT',
  p: 'pt-BR',
  h: 'hi-IN',
};

/** Kokoro voice metadata from its id, e.g. `if_sara` → Italian female "Sara". */
function voiceMeta(id) {
  const lang = LANG_BY_PREFIX[id[0]] ?? 'en-US';
  const gender = id[1] === 'f' ? 'female' : id[1] === 'm' ? 'male' : undefined;
  const name = id.split('_')[1]?.replace(/^./, (c) => c.toUpperCase()) ?? id;
  return { id, name, lang, gender };
}

const files = walk(srcDir).map((rel) => {
  const abs = path.join(srcDir, rel);
  return { path: rel, bytes: fs.statSync(abs).size, sha256: sha256(abs) };
});

const voices = files
  .filter((f) => f.path.startsWith('voices/') && f.path.endsWith('.bin'))
  .map((f) => voiceMeta(path.basename(f.path, '.bin')));

// Supported languages = the unique base codes of the bundled voices (data-driven, so
// adding a voice for a new language automatically un-gates it in the app).
const languages = [...new Set(voices.map((v) => v.lang.split('-')[0]))];

const pack = {
  id: packId,
  engine: 'kokoro',
  version,
  displayName: 'Vox Voices',
  languages,
  totalBytes: files.reduce((n, f) => n + f.bytes, 0),
  baseUrl: baseUrl.replace(/\/?$/, '/'),
  files,
  voices,
};

const outPath = path.join(srcDir, 'manifest-pack.json');
fs.writeFileSync(outPath, `${JSON.stringify(pack, null, 2)}\n`);
console.log(`wrote ${outPath}`);
console.log(`  ${files.length} files, ${voices.length} voices, ${(pack.totalBytes / 1048576).toFixed(1)} MB`);
console.log('Wrap it as { "schemaVersion": 1, "packs": [<this>] } in manifest.json at PUBLIC_VOX_MANIFEST_URL.');
