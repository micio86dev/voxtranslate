// Persistent voice-pack storage (IndexedDB). Holds the downloaded model FILES plus
// the install + benchmark METADATA, so packs survive browser restarts, aren't
// re-downloaded, and can be integrity-checked, versioned, and removed cleanly.
//
// The Service Worker (client/public/sw.js) reads the SAME database to serve model
// bytes same-origin at `/vox-models/<packId>/<version>/<path>`. If you change the DB
// name / store names / key format here, update sw.js to match — the contract is:
//   DB   "vox-voices" (v1)
//   store "files"  keyPath "key"   { key: "<packId>/<version>/<path>", blob: Blob }
//   store "meta"   keyPath "packId"
//   store "bench"  keyPath "packId"

import type { BenchmarkResult } from './types';

export const DB_NAME = 'vox-voices';
export const DB_VERSION = 1;
export const STORE_FILES = 'files';
export const STORE_META = 'meta';
export const STORE_BENCH = 'bench';

export interface InstalledFile {
  path: string;
  bytes: number;
  sha256: string;
}

export interface InstallMeta {
  packId: string;
  version: string;
  engine: string;
  languages: string[];
  voices: { id: string; name: string; lang: string; gender?: string }[];
  files: InstalledFile[];
  totalBytes: number;
  installedAt: number;
}

export interface BenchRecord {
  packId: string;
  version: string;
  ranAt: number;
  result: BenchmarkResult;
}

/** The storage surface the installer/benchmark/provider depend on. Injectable so the
 *  installer can be unit-tested against an in-memory fake (no IndexedDB in Node). */
export interface PackStorage {
  fileKey(packId: string, version: string, path: string): string;
  putFile(key: string, blob: Blob): Promise<void>;
  getFile(key: string): Promise<Blob | undefined>;
  /** Delete stored files for a pack. With `version`, only that version's files
   *  (so a failed UPDATE never destroys the working installed version); without it,
   *  every version (full uninstall). */
  deleteFiles(packId: string, version?: string): Promise<void>;
  putMeta(meta: InstallMeta): Promise<void>;
  getMeta(packId: string): Promise<InstallMeta | undefined>;
  listMeta(): Promise<InstallMeta[]>;
  deleteMeta(packId: string): Promise<void>;
  putBench(rec: BenchRecord): Promise<void>;
  getBench(packId: string): Promise<BenchRecord | undefined>;
  deleteBench(packId: string): Promise<void>;
  /** Estimated bytes used by our store, when the browser exposes the API. */
  estimateUsage(): Promise<number | undefined>;
}

function req<T>(r: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

function tx(store: IDBObjectStore): Promise<void> {
  return new Promise((resolve, reject) => {
    store.transaction.oncomplete = () => resolve();
    store.transaction.onabort = store.transaction.onerror = () =>
      reject(store.transaction.error);
  });
}

export class IdbPackStorage implements PackStorage {
  private db: Promise<IDBDatabase> | null = null;

  private open(): Promise<IDBDatabase> {
    if (this.db) return this.db;
    this.db = new Promise((resolve, reject) => {
      const open = indexedDB.open(DB_NAME, DB_VERSION);
      open.onupgradeneeded = () => {
        const db = open.result;
        if (!db.objectStoreNames.contains(STORE_FILES))
          db.createObjectStore(STORE_FILES, { keyPath: 'key' });
        if (!db.objectStoreNames.contains(STORE_META))
          db.createObjectStore(STORE_META, { keyPath: 'packId' });
        if (!db.objectStoreNames.contains(STORE_BENCH))
          db.createObjectStore(STORE_BENCH, { keyPath: 'packId' });
      };
      open.onsuccess = () => resolve(open.result);
      open.onerror = () => reject(open.error);
    });
    return this.db;
  }

  fileKey(packId: string, version: string, path: string): string {
    return `${packId}/${version}/${path}`;
  }

  async putFile(key: string, blob: Blob): Promise<void> {
    const db = await this.open();
    const store = db.transaction(STORE_FILES, 'readwrite').objectStore(STORE_FILES);
    store.put({ key, blob });
    await tx(store);
  }

  async getFile(key: string): Promise<Blob | undefined> {
    const db = await this.open();
    const store = db.transaction(STORE_FILES, 'readonly').objectStore(STORE_FILES);
    const rec = await req<{ key: string; blob: Blob } | undefined>(store.get(key));
    return rec?.blob;
  }

  async deleteFiles(packId: string, version?: string): Promise<void> {
    const db = await this.open();
    const store = db.transaction(STORE_FILES, 'readwrite').objectStore(STORE_FILES);
    // Keys are "<packId>/<version>/<path>" — scope the prefix to a version when given.
    const prefix = version ? `${packId}/${version}/` : `${packId}/`;
    const keys = await req<IDBValidKey[]>(store.getAllKeys());
    for (const k of keys) if (typeof k === 'string' && k.startsWith(prefix)) store.delete(k);
    await tx(store);
  }

  async putMeta(meta: InstallMeta): Promise<void> {
    const db = await this.open();
    const store = db.transaction(STORE_META, 'readwrite').objectStore(STORE_META);
    store.put(meta);
    await tx(store);
  }

  async getMeta(packId: string): Promise<InstallMeta | undefined> {
    const db = await this.open();
    const store = db.transaction(STORE_META, 'readonly').objectStore(STORE_META);
    return req<InstallMeta | undefined>(store.get(packId));
  }

  async listMeta(): Promise<InstallMeta[]> {
    const db = await this.open();
    const store = db.transaction(STORE_META, 'readonly').objectStore(STORE_META);
    return req<InstallMeta[]>(store.getAll());
  }

  async deleteMeta(packId: string): Promise<void> {
    const db = await this.open();
    const store = db.transaction(STORE_META, 'readwrite').objectStore(STORE_META);
    store.delete(packId);
    await tx(store);
  }

  async putBench(rec: BenchRecord): Promise<void> {
    const db = await this.open();
    const store = db.transaction(STORE_BENCH, 'readwrite').objectStore(STORE_BENCH);
    store.put(rec);
    await tx(store);
  }

  async getBench(packId: string): Promise<BenchRecord | undefined> {
    const db = await this.open();
    const store = db.transaction(STORE_BENCH, 'readonly').objectStore(STORE_BENCH);
    return req<BenchRecord | undefined>(store.get(packId));
  }

  async deleteBench(packId: string): Promise<void> {
    const db = await this.open();
    const store = db.transaction(STORE_BENCH, 'readwrite').objectStore(STORE_BENCH);
    store.delete(packId);
    await tx(store);
  }

  async estimateUsage(): Promise<number | undefined> {
    try {
      const est = await navigator.storage?.estimate?.();
      return est?.usage;
    } catch {
      return undefined;
    }
  }
}

/** Shared instance (browser only). */
export const packStorage: PackStorage = new IdbPackStorage();
