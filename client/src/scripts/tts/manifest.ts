// Voice-pack manifest: the contract between the client and the Supabase-hosted packs.
// The manifest is fetched FIRST; the client compares the offered version against what's
// installed and only downloads/updates when needed. Designed for MULTIPLE packs (add a
// pack / language / engine by editing the manifest — no client change). Everything here
// is pure (no I/O) so it's fully unit-testable.

import type { VoiceInfo } from './types';

export interface ManifestFile {
  /** Relative path under the pack's baseUrl AND the same path the Service Worker
   *  serves at `/vox-models/<packId>/<version>/<path>` — must match the layout the
   *  engine expects (e.g. transformers.js `onnx/…`, `tokenizer.json`). */
  path: string;
  bytes: number;
  /** Lowercase hex SHA-256 of the file's bytes, for integrity verification. */
  sha256: string;
}

export interface ManifestVoice {
  id: string;
  name: string;
  /** BCP-47 tag, e.g. `en-US`. */
  lang: string;
  gender?: string;
}

export interface ManifestPack {
  id: string;
  /** Engine that consumes this pack, e.g. `kokoro`. */
  engine: string;
  /** Dotted numeric version, e.g. `1.0.0`. */
  version: string;
  displayName: string;
  /** Base language codes this pack can synthesize, e.g. `["en"]`. */
  languages: string[];
  totalBytes: number;
  /** Absolute base URL (Supabase public object URL) the files live under. */
  baseUrl: string;
  files: ManifestFile[];
  voices: ManifestVoice[];
}

export interface Manifest {
  schemaVersion: number;
  packs: ManifestPack[];
}

export type InstallState = 'not-installed' | 'up-to-date' | 'update-available';

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null;
}

/** Parse + validate a fetched manifest; throws a descriptive error on a bad shape so
 *  the installer can surface a clean "couldn't be completed" message. */
export function parseManifest(raw: unknown): Manifest {
  if (!isObj(raw)) throw new Error('manifest: not an object');
  const schemaVersion = Number(raw.schemaVersion);
  if (!Number.isFinite(schemaVersion)) throw new Error('manifest: missing schemaVersion');
  if (!Array.isArray(raw.packs)) throw new Error('manifest: packs is not an array');
  const packs = raw.packs.map((p, i) => parsePack(p, i));
  return { schemaVersion, packs };
}

function parsePack(raw: unknown, idx: number): ManifestPack {
  if (!isObj(raw)) throw new Error(`manifest: pack ${idx} not an object`);
  const str = (k: string): string => {
    const v = raw[k];
    if (typeof v !== 'string' || !v) throw new Error(`manifest: pack ${idx} missing "${k}"`);
    return v;
  };
  if (!Array.isArray(raw.files) || raw.files.length === 0)
    throw new Error(`manifest: pack ${idx} has no files`);
  const files: ManifestFile[] = raw.files.map((f, j) => {
    if (!isObj(f)) throw new Error(`manifest: pack ${idx} file ${j} not an object`);
    if (typeof f.path !== 'string' || !f.path)
      throw new Error(`manifest: pack ${idx} file ${j} missing path`);
    if (typeof f.sha256 !== 'string' || !/^[0-9a-f]{64}$/i.test(f.sha256))
      throw new Error(`manifest: pack ${idx} file ${j} bad sha256`);
    return { path: f.path, bytes: Number(f.bytes) || 0, sha256: f.sha256.toLowerCase() };
  });
  const voices: ManifestVoice[] = Array.isArray(raw.voices)
    ? raw.voices.filter(isObj).map((v) => ({
        id: String(v.id ?? ''),
        name: String(v.name ?? v.id ?? ''),
        lang: String(v.lang ?? ''),
        gender: typeof v.gender === 'string' ? v.gender : undefined,
      }))
    : [];
  const languages = Array.isArray(raw.languages) ? raw.languages.map(String) : [];
  return {
    id: str('id'),
    engine: str('engine'),
    version: str('version'),
    displayName: str('displayName'),
    languages,
    totalBytes: Number(raw.totalBytes) || files.reduce((n, f) => n + f.bytes, 0),
    baseUrl: str('baseUrl'),
    files,
    voices,
  };
}

/** Compare dotted numeric versions: -1 if a<b, 0 if equal, 1 if a>b. Missing/NaN
 *  segments count as 0, so `1.2` and `1.2.0` are equal. */
export function compareVersions(a: string, b: string): number {
  const pa = a.split('.');
  const pb = b.split('.');
  const n = Math.max(pa.length, pb.length);
  for (let i = 0; i < n; i++) {
    const x = Number(pa[i]) || 0;
    const y = Number(pb[i]) || 0;
    if (x !== y) return x < y ? -1 : 1;
  }
  return 0;
}

/** Install state for a pack given the currently-installed version (or null). */
export function installState(
  installedVersion: string | null,
  manifestVersion: string,
): InstallState {
  if (!installedVersion) return 'not-installed';
  return compareVersions(manifestVersion, installedVersion) > 0
    ? 'update-available'
    : 'up-to-date';
}

/** The first pack for `engine` (today: `kokoro`), or null. Multi-pack ready. */
export function selectPack(m: Manifest, engine: string): ManifestPack | null {
  return m.packs.find((p) => p.engine === engine) ?? null;
}

/** All packs whose languages include `lang` (base code). */
export function packsForLanguage(m: Manifest, lang: string): ManifestPack[] {
  return m.packs.filter((p) => p.languages.includes(lang));
}

/** Map a pack's voices to provider-agnostic VoiceInfo (Vox = on-device/local). */
export function voiceInfosFromPack(pack: ManifestPack): VoiceInfo[] {
  return pack.voices.map((v) => ({
    id: v.id,
    name: v.name,
    lang: v.lang,
    provider: 'vox',
    local: true,
  }));
}
