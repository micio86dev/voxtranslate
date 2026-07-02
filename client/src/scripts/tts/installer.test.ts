import { beforeEach, describe, expect, it, vi } from 'vitest';

import { type ManifestPack, parseManifest } from './manifest';
import { isCancel, sha256Hex, VoicePackInstaller } from './installer';
import type { BenchRecord, InstallMeta, PackStorage } from './storage';

/** In-memory PackStorage for tests (no IndexedDB in Node). */
class FakeStorage implements PackStorage {
  files = new Map<string, Blob>();
  metas = new Map<string, InstallMeta>();
  benches = new Map<string, BenchRecord>();
  fileKey(p: string, v: string, path: string): string {
    return `${p}/${v}/${path}`;
  }
  async putFile(k: string, b: Blob): Promise<void> {
    this.files.set(k, b);
  }
  async getFile(k: string): Promise<Blob | undefined> {
    return this.files.get(k);
  }
  async deleteFiles(p: string, v?: string): Promise<void> {
    const pre = v ? `${p}/${v}/` : `${p}/`;
    for (const k of [...this.files.keys()]) if (k.startsWith(pre)) this.files.delete(k);
  }
  async putMeta(m: InstallMeta): Promise<void> {
    this.metas.set(m.packId, m);
  }
  async getMeta(p: string): Promise<InstallMeta | undefined> {
    return this.metas.get(p);
  }
  async listMeta(): Promise<InstallMeta[]> {
    return [...this.metas.values()];
  }
  async deleteMeta(p: string): Promise<void> {
    this.metas.delete(p);
  }
  async putBench(r: BenchRecord): Promise<void> {
    this.benches.set(r.packId, r);
  }
  async getBench(p: string): Promise<BenchRecord | undefined> {
    return this.benches.get(p);
  }
  async deleteBench(p: string): Promise<void> {
    this.benches.delete(p);
  }
  async estimateUsage(): Promise<number | undefined> {
    return undefined;
  }
}

const BASE = 'https://x.test/packs/kokoro-en/';
const bytesFor = (path: string): Uint8Array =>
  new TextEncoder().encode(`bytes-of-${path}`);

async function pack(version = '1.0.0', paths = ['model.onnx', 'config.json']): Promise<ManifestPack> {
  const files = [];
  for (const p of paths) {
    const b = bytesFor(p);
    files.push({ path: p, bytes: b.byteLength, sha256: await sha256Hex(b.buffer as ArrayBuffer) });
  }
  const total = files.reduce((n, f) => n + f.bytes, 0);
  return parseManifest({
    schemaVersion: 1,
    packs: [
      {
        id: 'kokoro-en',
        engine: 'kokoro',
        version,
        displayName: 'Vox',
        languages: ['en'],
        totalBytes: total,
        baseUrl: BASE,
        files,
        voices: [{ id: 'af_heart', name: 'Heart', lang: 'en-US' }],
      },
    ],
  }).packs[0];
}

/** fetch mock: file URLs → streamed bytes; unknown → 404. */
function fetchFor(overrides: Record<string, Uint8Array> = {}): typeof fetch {
  return vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    const path = url.startsWith(BASE) ? url.slice(BASE.length) : '';
    const bytes = overrides[path] ?? (path ? bytesFor(path) : null);
    if (!bytes) return new Response('', { status: 404 });
    return new Response(bytes);
  }) as unknown as typeof fetch;
}

describe('sha256Hex', () => {
  it('produces a 64-char lowercase hex digest', async () => {
    const hex = await sha256Hex(new TextEncoder().encode('abc').buffer as ArrayBuffer);
    expect(hex).toMatch(/^[0-9a-f]{64}$/);
  });
});

describe('VoicePackInstaller.install', () => {
  let store: FakeStorage;
  beforeEach(() => {
    store = new FakeStorage();
  });

  it('downloads, verifies, stores every file and writes the install record', async () => {
    const p = await pack();
    const progress: number[] = [];
    const inst = new VoicePackInstaller(store, 'https://x.test/manifest.json', fetchFor());
    const meta = await inst.install(p, { onProgress: (pr) => progress.push(pr.fraction) });

    expect(store.files.has('kokoro-en/1.0.0/model.onnx')).toBe(true);
    expect(store.files.has('kokoro-en/1.0.0/config.json')).toBe(true);
    expect(meta.version).toBe('1.0.0');
    expect(store.metas.get('kokoro-en')?.version).toBe('1.0.0');
    expect(progress.at(-1)).toBe(1); // reaches 100%
  });

  it('rejects and cleans up partial bytes on an integrity mismatch (never persists corrupt)', async () => {
    const p = await pack();
    // Serve tampered bytes for the second file so its sha256 won't match.
    const inst = new VoicePackInstaller(
      store,
      'm',
      fetchFor({ 'config.json': new TextEncoder().encode('TAMPERED') }),
    );
    await expect(inst.install(p)).rejects.toThrow(/integrity/i);
    expect(store.metas.size).toBe(0);
    // The already-stored first file for THIS version was rolled back.
    expect([...store.files.keys()].filter((k) => k.startsWith('kokoro-en/1.0.0/'))).toEqual([]);
  });

  it('preserves the working version if an UPDATE fails midway', async () => {
    const v1 = await pack('1.0.0');
    const inst1 = new VoicePackInstaller(store, 'm', fetchFor());
    await inst1.install(v1);

    const v2 = await pack('2.0.0');
    const inst2 = new VoicePackInstaller(
      store,
      'm',
      fetchFor({ 'config.json': new TextEncoder().encode('TAMPERED') }),
    );
    await expect(inst2.install(v2)).rejects.toThrow();
    // Old version + meta still intact.
    expect(store.metas.get('kokoro-en')?.version).toBe('1.0.0');
    expect(store.files.has('kokoro-en/1.0.0/model.onnx')).toBe(true);
    expect(store.files.has('kokoro-en/2.0.0/model.onnx')).toBe(false);
  });

  it('on a successful update, drops the superseded version and its stale benchmark', async () => {
    const v1 = await pack('1.0.0');
    const inst = new VoicePackInstaller(store, 'm', fetchFor());
    await inst.install(v1);
    store.benches.set('kokoro-en', {
      packId: 'kokoro-en',
      version: '1.0.0',
      ranAt: 1,
      result: { engine: 'vox', initMs: 1, firstAudioMs: 1, avgSynthMs: 1, webgpu: false, passed: true },
    });

    const v2 = await pack('2.0.0');
    await new VoicePackInstaller(store, 'm', fetchFor()).install(v2);
    expect(store.metas.get('kokoro-en')?.version).toBe('2.0.0');
    expect(store.files.has('kokoro-en/1.0.0/model.onnx')).toBe(false); // superseded, dropped
    expect(store.benches.has('kokoro-en')).toBe(false); // stale bench cleared
  });

  it('supports the non-streaming fetch fallback (no res.body)', async () => {
    const p = await pack('1.0.0', ['model.onnx']);
    const fallbackFetch = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const path = url.slice(BASE.length);
      const bytes = bytesFor(path);
      return { ok: true, status: 200, arrayBuffer: async () => bytes.buffer } as Response;
    }) as unknown as typeof fetch;
    const meta = await new VoicePackInstaller(store, 'm', fallbackFetch).install(p);
    expect(meta.version).toBe('1.0.0');
    expect(store.files.has('kokoro-en/1.0.0/model.onnx')).toBe(true);
  });
});

describe('VoicePackInstaller integrity / remove / status', () => {
  let store: FakeStorage;
  beforeEach(() => {
    store = new FakeStorage();
  });

  it('verifyIntegrity passes for a good install and fails for a missing/tampered file', async () => {
    const p = await pack('1.0.0', ['model.onnx']);
    const inst = new VoicePackInstaller(store, 'm', fetchFor());
    await inst.install(p);
    expect(await inst.verifyIntegrity('kokoro-en')).toBe(true);

    store.files.set('kokoro-en/1.0.0/model.onnx', new Blob([new TextEncoder().encode('corrupt')]));
    expect(await inst.verifyIntegrity('kokoro-en')).toBe(false);

    store.files.delete('kokoro-en/1.0.0/model.onnx');
    expect(await inst.verifyIntegrity('kokoro-en')).toBe(false);
    expect(await inst.verifyIntegrity('unknown-pack')).toBe(false);
  });

  it('remove deletes files, meta and benchmark', async () => {
    const p = await pack('1.0.0', ['model.onnx']);
    const inst = new VoicePackInstaller(store, 'm', fetchFor());
    await inst.install(p);
    await inst.remove('kokoro-en');
    expect(store.files.size).toBe(0);
    expect(store.metas.size).toBe(0);
    expect(store.benches.size).toBe(0);
  });

  it('checkStatus reports per-pack install state', async () => {
    const p = await pack('2.0.0', ['model.onnx']);
    const manifestJson = JSON.stringify({
      schemaVersion: 1,
      packs: [
        {
          id: p.id,
          engine: p.engine,
          version: p.version,
          displayName: p.displayName,
          languages: p.languages,
          totalBytes: p.totalBytes,
          baseUrl: p.baseUrl,
          files: p.files,
          voices: p.voices,
        },
      ],
    });
    const mfetch = vi.fn(async () => new Response(manifestJson)) as unknown as typeof fetch;
    const inst = new VoicePackInstaller(store, 'https://x.test/manifest.json', mfetch);

    let status = await inst.checkStatus();
    expect(status[0].state).toBe('not-installed');

    store.metas.set('kokoro-en', {
      packId: 'kokoro-en',
      version: '1.0.0',
      engine: 'kokoro',
      languages: ['en'],
      voices: [],
      files: [],
      totalBytes: 0,
      installedAt: 1,
    });
    status = await inst.checkStatus();
    expect(status[0].state).toBe('update-available');
    expect(status[0].installedVersion).toBe('1.0.0');
  });
});

describe('isCancel', () => {
  it('recognises aborts', () => {
    const e = new Error('x');
    e.name = 'AbortError';
    expect(isCancel(e)).toBe(true);
    expect(isCancel(new Error('nope'))).toBe(false);
    expect(isCancel('str')).toBe(false);
  });
});

describe('empty manifest URL keeps the feature dormant', () => {
  it('fetchManifest returns null and checkStatus is empty', async () => {
    const inst = new VoicePackInstaller(new FakeStorage(), '', fetchFor());
    expect(await inst.fetchManifest()).toBeNull();
    expect(await inst.checkStatus()).toEqual([]);
  });
});
