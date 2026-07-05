// Full-eSpeak phonemizer glue. The pure IPA-normalization helpers are tested
// directly; espeakPhonemize is exercised against a stub emscripten loader module
// (imported via a data: URL — same dynamic-import path as production) and a real,
// minimal WebAssembly module served by a mocked fetch, so the compile-once /
// instantiate-per-call memoisation runs for real.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { espeakLangFor, needsEspeak, normalizeForKokoro } from './espeak-phonemizer';

const ZWJ = '‍';

describe('needsEspeak / espeakLangFor', () => {
  it('maps non-English voice prefixes to eSpeak languages', () => {
    expect(needsEspeak('if_sara')).toBe(true);
    expect(espeakLangFor('if_sara')).toBe('it');
    expect(espeakLangFor('ef_dora')).toBe('es');
    expect(espeakLangFor('ff_siwis')).toBe('fr-fr');
    expect(espeakLangFor('pf_dora')).toBe('pt-br');
    expect(espeakLangFor('hf_alpha')).toBe('hi');
  });

  it('keeps English (a/b) voices on the built-in path', () => {
    expect(needsEspeak('af_heart')).toBe(false);
    expect(needsEspeak('bm_george')).toBe(false);
    expect(espeakLangFor('af_heart')).toBeNull();
    expect(espeakLangFor('zz_unknown')).toBeNull();
  });
});

describe('normalizeForKokoro', () => {
  it('rewrites tie-barred affricates to precomposed tokens', () => {
    expect(normalizeForKokoro(`t${ZWJ}ʃa`)).toBe('ʧa');
    expect(normalizeForKokoro(`d${ZWJ}ʒ d${ZWJ}z t${ZWJ}s`)).toBe('ʤ ʣ ʦ');
    expect(normalizeForKokoro(`t${ZWJ}ɕ d${ZWJ}ʑ`)).toBe('ʨ ʥ');
  });

  it('drops leftover ZWJ so diphthongs become two plain tokens', () => {
    expect(normalizeForKokoro(`a${ZWJ}ʊ`)).toBe('aʊ');
  });

  it('filters glyphs outside the supplied vocab (spaces always survive)', () => {
    const vocab = new Set(['a', 'b']);
    expect(normalizeForKokoro('axb byz', vocab)).toBe('ab b');
  });

  it('keeps everything when no vocab is supplied and collapses whitespace', () => {
    expect(normalizeForKokoro('  ʁa   x\t k  ')).toBe('ʁa x k');
  });

  it('applies NFD normalization (composed → base + combining)', () => {
    // 'é' decomposes to 'e' + U+0301; a vocab knowing only the base keeps 'e'.
    expect(normalizeForKokoro('é', new Set(['e']))).toBe('e');
  });
});

describe('espeakPhonemize', () => {
  // Smallest valid wasm module: magic + version. WebAssembly.compile accepts it.
  const WASM_BYTES = new Uint8Array([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);
  const G = globalThis as Record<string, unknown>;

  // Stub emscripten loader (default-exports a factory), served as a data: URL so the
  // module's `import(loaderUrl)` follows its real code path. It records the opts it
  // received and drives instantiateWasm to completion like emscripten does.
  const LOADER_SRC = `
    export default async function factory(opts) {
      globalThis.__espeakTestCalls.push(opts);
      const instance = await new Promise((resolve) => {
        const ret = opts.instantiateWasm({}, resolve);
        globalThis.__espeakTestSyncReturn = ret;
      });
      globalThis.__espeakTestInstances.push(instance);
      return { FS: { readFile: (path, o) => globalThis.__espeakTestOutput } };
    }`;
  const loaderUrl = `data:text/javascript;base64,${Buffer.from(LOADER_SRC).toString('base64')}`;
  const wasmUrl = 'https://app.test/vox-models/pack/1.0.0/espeak/espeak-ng.wasm';

  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    G.__espeakTestCalls = [];
    G.__espeakTestInstances = [];
    G.__espeakTestOutput = ' ˈtʃaʊ \n ˈbɛlla \n';
    fetchMock = vi.fn(async () => new Response(WASM_BYTES.slice()));
    vi.stubGlobal('fetch', fetchMock);
    vi.resetModules(); // fresh module → clears the factory/wasm memoisation
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    delete G.__espeakTestCalls;
    delete G.__espeakTestInstances;
    delete G.__espeakTestOutput;
    delete G.__espeakTestSyncReturn;
  });

  it('runs eSpeak with the right argv and returns single-line IPA', async () => {
    const { espeakPhonemize } = await import('./espeak-phonemizer');
    const out = await espeakPhonemize('ciao bella', 'it', loaderUrl, wasmUrl);

    expect(out).toBe('ˈtʃaʊ   ˈbɛlla'); // \n → space, trimmed
    const calls = G.__espeakTestCalls as { arguments: string[] }[];
    expect(calls).toHaveLength(1);
    expect(calls[0].arguments).toEqual([
      '--phonout', 'out.txt', '--sep= ', '-q', '--ipa=3', '-v', 'it', 'ciao bella',
    ]);
    // instantiateWasm returned {} synchronously and delivered a real instance async.
    expect(G.__espeakTestSyncReturn).toEqual({});
    expect((G.__espeakTestInstances as unknown[])[0]).toBeInstanceOf(WebAssembly.Instance);
  });

  it('fetches + compiles the wasm once, reusing it across calls', async () => {
    const { espeakPhonemize } = await import('./espeak-phonemizer');
    await espeakPhonemize('uno', 'it', loaderUrl, wasmUrl);
    G.__espeakTestOutput = 'olá\n';
    const second = await espeakPhonemize('olá', 'pt-br', loaderUrl, wasmUrl);

    expect(second).toBe('olá');
    expect(fetchMock).toHaveBeenCalledTimes(1); // compiled module memoised
    const calls = G.__espeakTestCalls as { arguments: string[] }[];
    expect(calls).toHaveLength(2); // but eSpeak itself runs per call
    expect(calls[1].arguments).toContain('pt-br');
    expect((G.__espeakTestInstances as unknown[])[1]).toBeInstanceOf(WebAssembly.Instance);
  });

  it('propagates a wasm fetch failure to the caller', async () => {
    fetchMock.mockRejectedValue(new Error('offline'));
    const { espeakPhonemize } = await import('./espeak-phonemizer');
    await expect(espeakPhonemize('ciao', 'it', loaderUrl, wasmUrl)).rejects.toThrow('offline');
  });
});
