// Full-language phonemizer for Vox Voices — the piece kokoro-js can't do itself.
//
// kokoro-js bundles an ENGLISH-ONLY eSpeak build (via `phonemizer`), so on-device
// synthesis is English-only out of the box. To speak es/fr/it/pt we run the FULL
// eSpeak NG (all language data, ~18 MB WASM) shipped as PACK files and served
// SAME-ORIGIN by the Service Worker at /vox-models/<pack>/<ver>/espeak/* — so it stays
// CSP-clean (`script-src`/`connect-src 'self'`, no blob:) and works offline, exactly
// like the model bytes. We phonemize here, then feed the phonemes straight to Kokoro
// via `generate_from_ids`, bypassing kokoro-js's English phonemizer entirely.
//
// eSpeak NG is GPL-3.0-or-later; it is loaded lazily and only when a non-English voice
// is actually used.

/** Kokoro voice-id first char → eSpeak NG language identifier. English (a/b) never
 *  reaches here — it stays on kokoro-js's own path. */
const ESPEAK_LANG: Record<string, string> = {
  e: 'es',
  f: 'fr-fr',
  i: 'it',
  p: 'pt-br',
  h: 'hi',
};

/** True when this voice needs the full eSpeak path (i.e. it is NOT English). */
export function needsEspeak(voiceId: string): boolean {
  return voiceId[0] in ESPEAK_LANG;
}

/** eSpeak language for a voice id, or null for English / unknown. */
export function espeakLangFor(voiceId: string): string | null {
  return ESPEAK_LANG[voiceId[0]] ?? null;
}

// eSpeak's tie-barred affricates (joined with ZWJ) → Kokoro's precomposed single tokens.
const ZWJ = '‍';
const TIE_MAP: Record<string, string> = {
  [`t${ZWJ}ʃ`]: 'ʧ',
  [`d${ZWJ}ʒ`]: 'ʤ',
  [`t${ZWJ}s`]: 'ʦ',
  [`d${ZWJ}z`]: 'ʣ',
  [`t${ZWJ}ɕ`]: 'ʨ',
  [`d${ZWJ}ʑ`]: 'ʥ',
};

/** Map raw eSpeak IPA onto Kokoro's phoneme inventory. Deliberately language-agnostic —
 *  NO English r→ɹ / x→k rewrites (Kokoro's vocab has r, ɾ, ʁ, x natively). Fixes affricate
 *  ties, drops leftover ZWJ (diphthongs become two valid tokens) and, when a vocab is
 *  supplied, strips glyphs Kokoro doesn't know so they can't become unknown tokens. */
export function normalizeForKokoro(ipa: string, vocab?: Set<string>): string {
  let s = ipa;
  for (const [tie, repl] of Object.entries(TIE_MAP)) s = s.split(tie).join(repl);
  s = s.split(ZWJ).join('').normalize('NFD');
  if (vocab) {
    let out = '';
    for (const ch of s) if (ch === ' ' || vocab.has(ch)) out += ch;
    s = out;
  }
  return s.replace(/\s+/g, ' ').trim();
}

/** Minimal shape of the emscripten module we rely on. */
interface EspeakModule {
  FS: { readFile(path: string, opts: { encoding: 'utf8' }): string };
}
type EspeakFactory = (opts: Record<string, unknown>) => Promise<EspeakModule>;

let factoryPromise: Promise<EspeakFactory> | null = null;
let wasmModulePromise: Promise<WebAssembly.Module> | null = null;

/** Load the eSpeak loader + compile its WASM once, from the SAME-ORIGIN pack URLs. */
async function ready(loaderUrl: string, wasmUrl: string): Promise<{ factory: EspeakFactory; module: WebAssembly.Module }> {
  factoryPromise ??= import(/* @vite-ignore */ loaderUrl).then((m) => m.default as EspeakFactory);
  wasmModulePromise ??= fetch(wasmUrl)
    .then((r) => r.arrayBuffer())
    .then((buf) => WebAssembly.compile(buf));
  const [factory, module] = await Promise.all([factoryPromise, wasmModulePromise]);
  return { factory, module };
}

/** Phonemize `text` in `espeakLang` (e.g. `it`, `fr-fr`) → raw eSpeak IPA. Reuses the
 *  compiled WASM across calls (~100-200 ms each), so only the first call pays compile cost. */
export async function espeakPhonemize(
  text: string,
  espeakLang: string,
  loaderUrl: string,
  wasmUrl: string,
): Promise<string> {
  const { factory, module } = await ready(loaderUrl, wasmUrl);
  // Default run: emscripten invokes main() with these argv, eSpeak writes IPA to out.txt.
  const mod = await factory({
    arguments: ['--phonout', 'out.txt', '--sep= ', '-q', '--ipa=3', '-v', espeakLang, text],
    // Reuse the already-compiled module instead of recompiling 18 MB every call.
    instantiateWasm: (
      imports: WebAssembly.Imports,
      done: (instance: WebAssembly.Instance) => void,
    ) => {
      WebAssembly.instantiate(module, imports).then((instance) => done(instance));
      return {};
    },
  });
  return mod.FS.readFile('out.txt', { encoding: 'utf8' }).replace(/\n/g, ' ').trim();
}
