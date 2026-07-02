// VoicePackInstaller — turns a manifest entry into an installed, verified, offline
// voice pack. Streams each file (granular progress + cancellation), verifies its
// SHA-256 BEFORE storing (never persists corrupt bytes), and writes the install
// record only once every file is in place. A failed/cancelled install (or a failed
// UPDATE) cleans up its own partial bytes and leaves any working install intact — the
// app stays fully functional either way.

import {
  type Manifest,
  type ManifestPack,
  type InstallState,
  installState,
  parseManifest,
} from './manifest';
import type { InstallMeta, PackStorage } from './storage';

export interface InstallProgress {
  /** 0-based index of the file currently downloading. */
  fileIndex: number;
  fileCount: number;
  /** Cumulative bytes received across all files. */
  receivedBytes: number;
  /** Expected total (pack.totalBytes). */
  totalBytes: number;
  /** 0..1 (clamped). */
  fraction: number;
}

export interface InstallOptions {
  onProgress?: (p: InstallProgress) => void;
  signal?: AbortSignal;
}

export interface PackStatus {
  pack: ManifestPack;
  state: InstallState;
  installedVersion: string | null;
}

/** True when an error is (or wraps) a user cancellation, so callers can stay silent
 *  rather than showing a failure message. */
export function isCancel(err: unknown): boolean {
  return err instanceof Error && (err.name === 'AbortError' || err.name === 'CancelledError');
}

const CONTENT_TYPES: Record<string, string> = {
  onnx: 'application/octet-stream',
  bin: 'application/octet-stream',
  json: 'application/json',
  txt: 'text/plain',
  wasm: 'application/wasm',
  mjs: 'text/javascript',
  js: 'text/javascript',
};

function contentTypeFor(path: string): string {
  const ext = path.split('.').pop()?.toLowerCase() ?? '';
  return CONTENT_TYPES[ext] ?? 'application/octet-stream';
}

/** Lowercase hex SHA-256 of a buffer (Web Crypto — available in secure contexts). */
export async function sha256Hex(buf: ArrayBuffer): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', buf);
  const bytes = new Uint8Array(digest);
  let hex = '';
  for (const b of bytes) hex += b.toString(16).padStart(2, '0');
  return hex;
}

export class VoicePackInstaller {
  constructor(
    private storage: PackStorage,
    private manifestUrl: string,
    private fetchImpl: typeof fetch = (...a: Parameters<typeof fetch>) => fetch(...a),
  ) {}

  /** Fetch + parse the manifest. Empty manifest URL → null (feature dormant). */
  async fetchManifest(signal?: AbortSignal): Promise<Manifest | null> {
    if (!this.manifestUrl) return null;
    const res = await this.fetchImpl(this.manifestUrl, { signal, cache: 'no-cache' });
    if (!res.ok) throw new Error(`manifest fetch failed: ${res.status}`);
    return parseManifest(await res.json());
  }

  /** Per-pack install state vs what's already stored (drives the settings UI). */
  async checkStatus(signal?: AbortSignal): Promise<PackStatus[]> {
    const manifest = await this.fetchManifest(signal);
    if (!manifest) return [];
    const out: PackStatus[] = [];
    for (const pack of manifest.packs) {
      const meta = await this.storage.getMeta(pack.id);
      const installedVersion = meta?.version ?? null;
      out.push({ pack, installedVersion, state: installState(installedVersion, pack.version) });
    }
    return out;
  }

  /** Download + verify + store every file for `pack`, then write the install record.
   *  Preserves any currently-installed version until the new one fully lands. */
  async install(pack: ManifestPack, opts: InstallOptions = {}): Promise<InstallMeta> {
    const { onProgress, signal } = opts;
    const prev = await this.storage.getMeta(pack.id);
    const total = pack.totalBytes || pack.files.reduce((n, f) => n + f.bytes, 0);
    let received = 0;

    try {
      for (let i = 0; i < pack.files.length; i++) {
        const file = pack.files[i];
        const url = pack.baseUrl.replace(/\/?$/, '/') + file.path;
        const buf = await this.download(url, signal, (delta) => {
          received += delta;
          onProgress?.({
            fileIndex: i,
            fileCount: pack.files.length,
            receivedBytes: received,
            totalBytes: total,
            fraction: total > 0 ? Math.min(1, received / total) : 0,
          });
        });

        const actual = await sha256Hex(buf);
        if (actual !== file.sha256.toLowerCase())
          throw new Error(`integrity check failed for ${file.path}`);

        const blob = new Blob([buf], { type: contentTypeFor(file.path) });
        await this.storage.putFile(
          this.storage.fileKey(pack.id, pack.version, file.path),
          blob,
        );
      }
    } catch (err) {
      // Roll back ONLY this version's partial bytes — keep any working install.
      await this.storage.deleteFiles(pack.id, pack.version).catch(() => {});
      throw err;
    }

    const meta: InstallMeta = {
      packId: pack.id,
      version: pack.version,
      engine: pack.engine,
      languages: pack.languages,
      voices: pack.voices,
      files: pack.files.map((f) => ({ path: f.path, bytes: f.bytes, sha256: f.sha256 })),
      totalBytes: total,
      installedAt: Date.now(),
    };
    await this.storage.putMeta(meta);

    // Successful update: drop the superseded version's bytes (and its stale benchmark).
    if (prev && prev.version !== pack.version) {
      await this.storage.deleteFiles(pack.id, prev.version).catch(() => {});
      await this.storage.deleteBench(pack.id).catch(() => {});
    }
    return meta;
  }

  /** Re-hash every stored file against the recorded manifest — detects corruption or a
   *  partially-evicted pack. Returns false on the first mismatch/missing file. */
  async verifyIntegrity(packId: string): Promise<boolean> {
    const meta = await this.storage.getMeta(packId);
    if (!meta) return false;
    for (const f of meta.files) {
      const blob = await this.storage.getFile(this.storage.fileKey(packId, meta.version, f.path));
      if (!blob) return false;
      const actual = await sha256Hex(await blob.arrayBuffer());
      if (actual !== f.sha256.toLowerCase()) return false;
    }
    return true;
  }

  /** Uninstall: delete every stored file, the install record, and the benchmark. */
  async remove(packId: string): Promise<void> {
    await this.storage.deleteFiles(packId);
    await this.storage.deleteMeta(packId);
    await this.storage.deleteBench(packId);
  }

  /** Stream one file, reporting byte deltas; resolves to the full buffer. Aborts
   *  cleanly on `signal`. Falls back to a non-streamed read where ReadableStream is
   *  unavailable (progress then jumps per-file instead of per-chunk). */
  private async download(
    url: string,
    signal: AbortSignal | undefined,
    onDelta: (bytes: number) => void,
  ): Promise<ArrayBuffer> {
    const res = await this.fetchImpl(url, { signal, cache: 'no-cache' });
    if (!res.ok) throw new Error(`download failed (${res.status}) for ${url}`);
    const reader = res.body?.getReader();
    if (!reader) {
      const buf = await res.arrayBuffer();
      onDelta(buf.byteLength);
      return buf;
    }
    const chunks: Uint8Array[] = [];
    let size = 0;
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      if (value) {
        chunks.push(value);
        size += value.length;
        onDelta(value.length);
      }
    }
    const out = new Uint8Array(size);
    let off = 0;
    for (const c of chunks) {
      out.set(c, off);
      off += c.length;
    }
    return out.buffer;
  }
}
