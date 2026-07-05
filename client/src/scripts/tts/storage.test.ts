// IndexedDB wrapper, exercised against a hand-rolled in-memory fake of the exact
// IDB surface the module uses (open/upgradeneeded, transactions with oncomplete/
// onabort, request onsuccess/onerror, put/get/getAll/getAllKeys/delete). No
// fake-indexeddb package — the fake fires its callbacks on microtasks, mirroring
// the async contract the real API provides.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  DB_NAME,
  DB_VERSION,
  IdbPackStorage,
  packStorage,
  STORE_BENCH,
  STORE_FILES,
  STORE_META,
  type BenchRecord,
  type InstallMeta,
} from './storage';

// ---------------------------------------------------------------------------
// Minimal in-memory IndexedDB fake
// ---------------------------------------------------------------------------

type Handler = (() => void) | null;

interface StoredRecord {
  [k: string]: unknown;
}

class FakeRequest<T> {
  onsuccess: Handler = null;
  onerror: Handler = null;
  result!: T;
  error: Error | null = null;
}

class FakeTransaction {
  oncomplete: Handler = null;
  onabort: Handler = null;
  onerror: Handler = null;
  error: Error | null = null;
  private pending = 0;
  private done = false;

  constructor(private fail: Error | null) {}

  /** A request was issued in this transaction. */
  track(): void {
    this.pending++;
  }

  /** A request settled; auto-commit once no requests remain (like real IDB, which
   *  keeps a transaction alive while its callbacks issue follow-up requests). */
  settleOne(): void {
    this.pending--;
    queueMicrotask(() => this.maybeComplete());
  }

  private maybeComplete(): void {
    if (this.done || this.pending > 0) return;
    this.done = true;
    if (this.fail) {
      this.error = this.fail;
      this.onabort?.();
    } else {
      this.oncomplete?.();
    }
  }
}

class FakeObjectStore {
  constructor(
    private data: Map<IDBValidKey, StoredRecord>,
    private keyPath: string,
    public transaction: FakeTransaction,
    private failRequests: Error | null,
  ) {}

  private request<T>(compute: () => T): FakeRequest<T> {
    const r = new FakeRequest<T>();
    this.transaction.track();
    queueMicrotask(() => {
      if (this.failRequests) {
        r.error = this.failRequests;
        r.onerror?.();
      } else {
        r.result = compute();
        r.onsuccess?.();
      }
      this.transaction.settleOne();
    });
    return r;
  }

  put(value: StoredRecord): FakeRequest<IDBValidKey> {
    return this.request(() => {
      const key = value[this.keyPath] as IDBValidKey;
      this.data.set(key, value);
      return key;
    });
  }

  get(key: IDBValidKey): FakeRequest<StoredRecord | undefined> {
    return this.request(() => this.data.get(key));
  }

  getAll(): FakeRequest<StoredRecord[]> {
    return this.request(() => [...this.data.values()]);
  }

  getAllKeys(): FakeRequest<IDBValidKey[]> {
    return this.request(() => [...this.data.keys()]);
  }

  delete(key: IDBValidKey): FakeRequest<undefined> {
    return this.request(() => {
      this.data.delete(key);
      return undefined;
    });
  }
}

class FakeDatabase {
  stores = new Map<string, Map<IDBValidKey, StoredRecord>>();
  keyPaths = new Map<string, string>();
  /** Aborts the NEXT transaction (covers the tx onabort/onerror reject path). */
  failNextTransaction: Error | null = null;
  /** Fails every request in the NEXT transaction (covers req onerror). */
  failNextRequests: Error | null = null;

  objectStoreNames = {
    contains: (name: string): boolean => this.stores.has(name),
  };

  createObjectStore(name: string, opts: { keyPath: string }): void {
    this.stores.set(name, new Map());
    this.keyPaths.set(name, opts.keyPath);
  }

  transaction(name: string, _mode: IDBTransactionMode): { objectStore(n: string): FakeObjectStore } {
    const txFail = this.failNextTransaction;
    this.failNextTransaction = null;
    const reqFail = this.failNextRequests;
    this.failNextRequests = null;
    const tx = new FakeTransaction(txFail);
    return {
      objectStore: (n: string): FakeObjectStore => {
        const data = this.stores.get(n);
        const keyPath = this.keyPaths.get(n);
        if (!data || !keyPath) throw new Error(`no such store: ${n}`);
        return new FakeObjectStore(data, keyPath, tx, reqFail);
      },
    };
  }
}

interface FakeOpenRequest {
  onupgradeneeded: Handler;
  onsuccess: Handler;
  onerror: Handler;
  result: FakeDatabase;
  error: Error | null;
}

class FakeIdbFactory {
  db = new FakeDatabase();
  failOpen: Error | null = null;
  openCalls: { name: string; version: number | undefined }[] = [];

  open(name: string, version?: number): FakeOpenRequest {
    this.openCalls.push({ name, version });
    const req: FakeOpenRequest = {
      onupgradeneeded: null,
      onsuccess: null,
      onerror: null,
      result: this.db,
      error: null,
    };
    queueMicrotask(() => {
      if (this.failOpen) {
        req.error = this.failOpen;
        req.onerror?.();
        return;
      }
      // Real IDB fires upgradeneeded only on a version bump; always firing it lets
      // the tests cover both the create-store and already-exists branches.
      req.onupgradeneeded?.();
      req.onsuccess?.();
    });
    return req;
  }
}

// ---------------------------------------------------------------------------

const meta = (packId: string, version = '1.0.0'): InstallMeta => ({
  packId,
  version,
  engine: 'kokoro',
  languages: ['en'],
  voices: [{ id: 'af_heart', name: 'Heart', lang: 'en-US' }],
  files: [{ path: 'model.onnx', bytes: 3, sha256: 'a'.repeat(64) }],
  totalBytes: 3,
  installedAt: 42,
});

const bench = (packId: string): BenchRecord => ({
  packId,
  version: '1.0.0',
  ranAt: 7,
  result: { engine: 'vox', initMs: 1, firstAudioMs: 2, avgSynthMs: 3, webgpu: false, passed: true },
});

describe('IdbPackStorage', () => {
  let factory: FakeIdbFactory;
  let storage: IdbPackStorage;

  beforeEach(() => {
    factory = new FakeIdbFactory();
    vi.stubGlobal('indexedDB', factory as unknown as IDBFactory);
    storage = new IdbPackStorage();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('opens the versioned DB once and creates the three stores on upgrade', async () => {
    await storage.putMeta(meta('p1'));
    await storage.getMeta('p1'); // second op → memoised open, no second open() call

    expect(factory.openCalls).toEqual([{ name: DB_NAME, version: DB_VERSION }]);
    expect(factory.db.stores.has(STORE_FILES)).toBe(true);
    expect(factory.db.stores.has(STORE_META)).toBe(true);
    expect(factory.db.stores.has(STORE_BENCH)).toBe(true);
  });

  it('skips createObjectStore when the stores already exist (upgrade re-entry)', async () => {
    await storage.putMeta(meta('p1'));
    // A NEW instance against the same backend: upgradeneeded fires again but every
    // contains() check is true, so the existing data must survive untouched.
    const again = new IdbPackStorage();
    expect((await again.getMeta('p1'))?.packId).toBe('p1');
  });

  it('fileKey formats <packId>/<version>/<path>', () => {
    expect(storage.fileKey('kokoro-en', '1.0.0', 'onnx/model.onnx')).toBe(
      'kokoro-en/1.0.0/onnx/model.onnx',
    );
  });

  it('putFile/getFile round-trips a blob; a missing key resolves undefined', async () => {
    const blob = new Blob(['hello'], { type: 'text/plain' });
    const key = storage.fileKey('p1', '1.0.0', 'a.txt');
    await storage.putFile(key, blob);

    const got = await storage.getFile(key);
    expect(got).toBe(blob);
    expect(await storage.getFile('p1/1.0.0/missing')).toBeUndefined();
  });

  it('deleteFiles scoped to a version removes ONLY that version', async () => {
    const b = new Blob(['x']);
    await storage.putFile('p1/1.0.0/a', b);
    await storage.putFile('p1/1.0.0/b', b);
    await storage.putFile('p1/2.0.0/a', b);
    await storage.putFile('other/1.0.0/a', b);

    await storage.deleteFiles('p1', '1.0.0');
    expect(await storage.getFile('p1/1.0.0/a')).toBeUndefined();
    expect(await storage.getFile('p1/1.0.0/b')).toBeUndefined();
    expect(await storage.getFile('p1/2.0.0/a')).toBe(b); // other version intact
    expect(await storage.getFile('other/1.0.0/a')).toBe(b); // other pack intact
  });

  it('deleteFiles without a version removes every version of the pack', async () => {
    const b = new Blob(['x']);
    await storage.putFile('p1/1.0.0/a', b);
    await storage.putFile('p1/2.0.0/a', b);
    await storage.putFile('other/1.0.0/a', b);
    // A non-string key sneaks in (the typeof guard must skip it, not crash).
    factory.db.stores.get(STORE_FILES)?.set(123, { key: 123 });

    await storage.deleteFiles('p1');
    expect(await storage.getFile('p1/1.0.0/a')).toBeUndefined();
    expect(await storage.getFile('p1/2.0.0/a')).toBeUndefined();
    expect(await storage.getFile('other/1.0.0/a')).toBe(b);
    expect(factory.db.stores.get(STORE_FILES)?.has(123)).toBe(true);
  });

  it('putMeta/getMeta/listMeta/deleteMeta round-trip install records', async () => {
    expect(await storage.listMeta()).toEqual([]);
    await storage.putMeta(meta('p1'));
    await storage.putMeta(meta('p2', '2.0.0'));

    expect((await storage.getMeta('p1'))?.version).toBe('1.0.0');
    expect((await storage.listMeta()).map((m) => m.packId).sort()).toEqual(['p1', 'p2']);
    expect(await storage.getMeta('nope')).toBeUndefined();

    await storage.deleteMeta('p1');
    expect(await storage.getMeta('p1')).toBeUndefined();
    expect(await storage.listMeta()).toHaveLength(1);
  });

  it('putBench/getBench/deleteBench round-trip benchmark records', async () => {
    await storage.putBench(bench('p1'));
    expect((await storage.getBench('p1'))?.result.passed).toBe(true);
    expect(await storage.getBench('nope')).toBeUndefined();

    await storage.deleteBench('p1');
    expect(await storage.getBench('p1')).toBeUndefined();
  });

  it('rejects every operation when the DB fails to open (memoised failure)', async () => {
    factory.failOpen = new Error('quota blocked');
    await expect(storage.putFile('k', new Blob(['x']))).rejects.toThrow('quota blocked');
    // The failed open promise is memoised — later ops reject too, without reopening.
    await expect(storage.getMeta('p1')).rejects.toThrow('quota blocked');
    expect(factory.openCalls).toHaveLength(1);
  });

  it('rejects a write when its transaction aborts', async () => {
    await storage.putMeta(meta('p1')); // open + create stores first
    factory.db.failNextTransaction = new Error('tx aborted');
    await expect(storage.putMeta(meta('p2'))).rejects.toThrow('tx aborted');
  });

  it('rejects a read when its request errors', async () => {
    await storage.putMeta(meta('p1'));
    factory.db.failNextRequests = new Error('read failed');
    await expect(storage.getMeta('p1')).rejects.toThrow('read failed');
    factory.db.failNextRequests = new Error('read failed');
    await expect(storage.listMeta()).rejects.toThrow('read failed');
  });

  it('rejects deleteFiles when listing keys errors', async () => {
    await storage.putFile('p1/1.0.0/a', new Blob(['x']));
    factory.db.failNextRequests = new Error('keys failed');
    await expect(storage.deleteFiles('p1')).rejects.toThrow('keys failed');
  });

  describe('estimateUsage', () => {
    it('returns the estimate when the browser exposes the API', async () => {
      vi.stubGlobal('navigator', {
        storage: { estimate: async () => ({ usage: 1234, quota: 99999 }) },
      });
      expect(await storage.estimateUsage()).toBe(1234);
    });

    it('returns undefined when the API is missing', async () => {
      vi.stubGlobal('navigator', {});
      expect(await storage.estimateUsage()).toBeUndefined();
    });

    it('returns undefined when estimate() throws', async () => {
      vi.stubGlobal('navigator', {
        storage: {
          estimate: async () => {
            throw new Error('blocked');
          },
        },
      });
      expect(await storage.estimateUsage()).toBeUndefined();
    });
  });
});

describe('packStorage shared instance', () => {
  it('is an IdbPackStorage', () => {
    expect(packStorage).toBeInstanceOf(IdbPackStorage);
  });
});
