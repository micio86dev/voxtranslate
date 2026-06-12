import { describe, it, expect, vi, beforeAll } from 'vitest';

// api.ts/chat.ts import auth.ts, which reads `location` at module load — stub
// node globals, then dynamic-import after the stubs are in place (static imports
// hoist above the stubs, so they must be lazy). Same pattern as auth.test.ts.
vi.stubGlobal('location', { protocol: 'http:', host: 'localhost:4321' });
vi.stubGlobal('localStorage', {
  getItem: () => null,
  setItem: () => {},
  removeItem: () => {},
  clear: () => {},
});

let checkUploadFile: typeof import('./api').checkUploadFile;
let UPLOAD_EXTS: typeof import('./api').UPLOAD_EXTS;
let UPLOAD_MAX_BYTES: number;
let formatSize: typeof import('./chat').formatSize;

beforeAll(async () => {
  const api = await import('./api');
  checkUploadFile = api.checkUploadFile;
  UPLOAD_EXTS = api.UPLOAD_EXTS;
  UPLOAD_MAX_BYTES = api.UPLOAD_MAX_BYTES;
  formatSize = (await import('./chat')).formatSize;
});

/** A minimal File-like value; `checkUploadFile` only reads `name` + `size`. */
function file(name: string, size: number): File {
  return { name, size } as unknown as File;
}

describe('checkUploadFile (spec 0018 R5 client pre-check)', () => {
  it('accepts every supported extension at a normal size', () => {
    for (const ext of UPLOAD_EXTS) {
      expect(checkUploadFile(file(`memo.${ext}`, 1024))).toBeNull();
    }
    // Case-insensitive on the extension.
    expect(checkUploadFile(file('MEMO.MP3', 2048))).toBeNull();
  });

  it('rejects unsupported types', () => {
    expect(checkUploadFile(file('virus.exe', 1024))).toBe('type');
    expect(checkUploadFile(file('photo.png', 1024))).toBe('type');
    expect(checkUploadFile(file('noextension', 1024))).toBe('type');
  });

  it('rejects oversized and empty files', () => {
    expect(checkUploadFile(file('big.wav', UPLOAD_MAX_BYTES + 1))).toBe('size');
    expect(checkUploadFile(file('empty.txt', 0))).toBe('size');
    // Exactly at the cap is allowed.
    expect(checkUploadFile(file('edge.pdf', UPLOAD_MAX_BYTES))).toBeNull();
  });
});

describe('formatSize', () => {
  it('formats bytes, KB and MB', () => {
    expect(formatSize(512)).toBe('512 B');
    expect(formatSize(2048)).toBe('2 KB');
    expect(formatSize(3.4 * 1024 * 1024)).toBe('3.4 MB');
  });
});
