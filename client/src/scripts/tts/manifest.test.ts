import { describe, expect, it } from 'vitest';

import {
  compareVersions,
  installState,
  type Manifest,
  packsForLanguage,
  parseManifest,
  selectPack,
  voiceInfosFromPack,
} from './manifest';

const SHA = 'a'.repeat(64);

function rawPack(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    id: 'kokoro-en',
    engine: 'kokoro',
    version: '1.0.0',
    displayName: 'Vox Voices — English',
    languages: ['en'],
    totalBytes: 100,
    baseUrl: 'https://x.supabase.co/storage/v1/object/public/voice-packs/kokoro-en/1.0.0/',
    files: [{ path: 'model.onnx', bytes: 100, sha256: SHA }],
    voices: [{ id: 'af_heart', name: 'Heart', lang: 'en-US', gender: 'female' }],
    ...over,
  };
}

describe('parseManifest', () => {
  it('parses a valid manifest', () => {
    const m = parseManifest({ schemaVersion: 1, packs: [rawPack()] });
    expect(m.schemaVersion).toBe(1);
    expect(m.packs).toHaveLength(1);
    expect(m.packs[0].files[0].sha256).toBe(SHA);
    expect(m.packs[0].voices[0].id).toBe('af_heart');
  });

  it('defaults totalBytes to the sum of file bytes when absent', () => {
    const p = rawPack({ totalBytes: undefined, files: [{ path: 'a', bytes: 30, sha256: SHA }] });
    const m = parseManifest({ schemaVersion: 1, packs: [p] });
    expect(m.packs[0].totalBytes).toBe(30);
  });

  it.each([
    ['not an object', 42],
    ['missing packs', { schemaVersion: 1 }],
    ['pack missing id', { schemaVersion: 1, packs: [rawPack({ id: '' })] }],
    ['pack with no files', { schemaVersion: 1, packs: [rawPack({ files: [] })] }],
    ['a non-object file entry', { schemaVersion: 1, packs: [rawPack({ files: [42] })] }],
    [
      'a file missing its path',
      { schemaVersion: 1, packs: [rawPack({ files: [{ path: '', bytes: 1, sha256: SHA }] })] },
    ],
    [
      'bad sha256',
      { schemaVersion: 1, packs: [rawPack({ files: [{ path: 'a', bytes: 1, sha256: 'nope' }] })] },
    ],
  ])('throws on %s', (_label, bad) => {
    expect(() => parseManifest(bad)).toThrow();
  });

  it('tolerates malformed/missing optional fields (voices, languages, bytes)', () => {
    const m = parseManifest({
      schemaVersion: 1,
      packs: [
        rawPack({
          voices: 'not-an-array',
          languages: undefined,
          files: [{ path: 'a', bytes: undefined, sha256: SHA }],
        }),
      ],
    });
    expect(m.packs[0].voices).toEqual([]);
    expect(m.packs[0].languages).toEqual([]);
    expect(m.packs[0].files[0].bytes).toBe(0);
  });

  it('fills voice name/gender defaults and filters non-object voice entries', () => {
    const m = parseManifest({
      schemaVersion: 1,
      packs: [
        rawPack({
          voices: [{ id: 'x' }, null, 'nope', { id: 'y', name: 'Yara', lang: 'it-IT', gender: 5 }],
        }),
      ],
    });
    expect(m.packs[0].voices).toEqual([
      { id: 'x', name: 'x', lang: '', gender: undefined }, // name defaults to id
      { id: 'y', name: 'Yara', lang: 'it-IT', gender: undefined }, // non-string gender dropped
    ]);
  });
});

describe('compareVersions', () => {
  it('orders dotted numeric versions', () => {
    expect(compareVersions('1.0.0', '1.0.1')).toBe(-1);
    expect(compareVersions('1.2.0', '1.1.9')).toBe(1);
    expect(compareVersions('2.0', '2.0.0')).toBe(0);
    expect(compareVersions('1.10.0', '1.9.0')).toBe(1);
  });
});

describe('installState', () => {
  it('reflects installed vs offered version', () => {
    expect(installState(null, '1.0.0')).toBe('not-installed');
    expect(installState('1.0.0', '1.0.0')).toBe('up-to-date');
    expect(installState('1.0.0', '1.1.0')).toBe('update-available');
    expect(installState('1.2.0', '1.1.0')).toBe('up-to-date'); // never "downgrade"
  });
});

describe('pack selection helpers', () => {
  const m: Manifest = parseManifest({
    schemaVersion: 1,
    packs: [rawPack(), rawPack({ id: 'other', engine: 'future', languages: ['es'] })],
  });

  it('selectPack finds by engine', () => {
    expect(selectPack(m, 'kokoro')?.id).toBe('kokoro-en');
    expect(selectPack(m, 'nope')).toBeNull();
  });

  it('packsForLanguage filters by language', () => {
    expect(packsForLanguage(m, 'en').map((p) => p.id)).toEqual(['kokoro-en']);
    expect(packsForLanguage(m, 'es').map((p) => p.id)).toEqual(['other']);
    expect(packsForLanguage(m, 'zz')).toEqual([]);
  });

  it('voiceInfosFromPack maps to provider-agnostic voices', () => {
    const vs = voiceInfosFromPack(m.packs[0]);
    expect(vs).toEqual([
      { id: 'af_heart', name: 'Heart', lang: 'en-US', provider: 'vox', local: true },
    ]);
  });
});
