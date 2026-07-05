# Vox Voices (local neural TTS)

High-quality, on-device text-to-speech for translated speech playback, branded **Vox Voices**
(engine: Kokoro, via kokoro-js + transformers.js + ONNX Runtime Web). Browser SpeechSynthesis
remains the **permanent fallback** — if Vox Voices is unavailable, unsupported for a language,
slow, or failing, playback behaves exactly as before.

The word "Kokoro" is never shown to users. User-facing branding is always **Vox Voices**.

## Architecture

Provider-based. The app talks only to `TTSManager`; engines sit behind `TTSProvider`.
All code is under `client/src/scripts/tts/` (+ the UI in `client/src/scripts/audio-settings.ts`):

| Module | Role |
|---|---|
| `manager.ts` | `TTSManager` — utterance queue (no-cut, drop-oldest), per-line provider routing (pref × capability × benchmark × health), silent fallback. The app's `speak()`/`unlockTts()`/`stopTts()` delegate here. |
| `providers/browser.ts` | `BrowserSpeechProvider` — the original SpeechSynthesis path, verbatim. |
| `providers/kokoro.ts` | `KokoroProvider` — thin; lazy-loads the engine on first synth. |
| `kokoro-engine.ts` | Heavy chunk (kokoro-js/transformers/ORT); loads same-origin, synth → Float32 @ 24 kHz. |
| `playback.ts` | `PlaybackService` — plays a whole utterance via `AudioBufferSourceNode`. |
| `storage.ts` | IndexedDB store for model files + install/benchmark metadata. |
| `manifest.ts` / `installer.ts` | Manifest parse + versioning; streamed download, SHA-256 verify, install/update/remove. |
| `benchmark.ts` / `health.ts` | One-time device benchmark; runtime session-only degrade. |
| `preferences.ts` | Server-synced engine/Vox-voice pref + device-local browser-voice pref. |
| `register.ts` | Boot wiring + Vox provider lifecycle (activate on install, deactivate on remove). |

Engine chunk is **lazy** (dynamic `import()`), verified absent from the boot entry — zero
startup regression.

## CSP / offline model loading

Everything loads **same-origin**, so no CSP change is needed (`script-src 'self' 'wasm-unsafe-eval'`,
`worker-src 'self'`, `connect-src 'self' *.supabase.co`):

- Download pack files from Supabase (`*.supabase.co`, already allowed) → store in IndexedDB.
- The **Service Worker** (`client/public/sw.js`) serves them at `/vox-models/<pack>/<ver>/<path>`.
- transformers.js is pointed at `/vox-models/...` (`env.localModelPath`); ORT wasm at
  `/vox-models/<pack>/<ver>/ort/` (`env.backends.onnx.wasm.wasmPaths`).
- kokoro-js hardcodes a HuggingFace URL for voices but checks a `kokoro-voices` Cache Storage
  first, so the engine **pre-seeds that cache** from the verified IndexedDB bytes → no network.

## Language coverage

kokoro-js reliably synthesizes **English only** (American + British voices). This is enforced by
capability, not hardcoded: `KokoroProvider.supports(lang)` derives from the installed pack's
`languages`. For any language Vox can't serve, the manager transparently uses Browser Voice.
Adding a language/provider later requires only a new pack in the manifest — no app-logic change.

## Configuration

Set the build-time env var (Astro `PUBLIC_*`) to enable the feature:

```
PUBLIC_VOX_MANIFEST_URL=https://<ref>.supabase.co/storage/v1/object/public/voice-packs/manifest.json
```

With it unset (the default) the whole feature is **dormant**: no install UI, Browser Voice only.

## Building & uploading the pack (ops)

1. Assemble a source dir mirroring `onnx-community/Kokoro-82M-v1.0-ONNX`. Ship **both**
   model dtypes so the engine can pick the right backend per device (see the ⚠️ note below):
   - `onnx/model.onnx` — **fp32**, used on the **WebGPU** path (GPU-resident and correct).
   - `onnx/model_quantized.onnx` — **q8**, used on the **wasm/CPU** path (fast + correct).

   Plus `voices/<voice>.bin` and a self-hosted ORT runtime under `ort/`
   (`ort-wasm-simd-threaded.jsep.wasm` + `.mjs`, from `onnxruntime-web/dist`).

   > ⚠️ **Do NOT ship only `model_fp16.onnx`.** Kokoro's iSTFT vocoder is garbled by
   > onnxruntime-web's **fp16-on-WebGPU** path (robotic ~half-second noise / distorted
   > speech on Apple-Silicon Metal — kokoro-js's README: *"if using webgpu, we recommend
   > dtype=fp32"*). `kokoro-engine.ts` therefore **never** runs fp16 on WebGPU: it uses
   > fp32 on WebGPU (only if the pack ships `model.onnx`) and otherwise falls back to the
   > wasm path with q8/fp16. A pack that ships fp16 only will run on wasm (correct but
   > slower) and will usually fail the benchmark → Browser Voice.
2. Generate the pack manifest with hashes:
   ```
   node client/scripts/build-vox-pack.mjs <srcDir> \
     https://<ref>.supabase.co/storage/v1/object/public/voice-packs/kokoro-en/1.0.0/ 1.0.0 kokoro-en
   ```
3. Wrap it as `{ "schemaVersion": 1, "packs": [<pack>] }` in `manifest.json`.
4. Upload the files + `manifest.json` to a **public-read** Supabase Storage bucket at the paths in
   `baseUrl` (files) and at the `PUBLIC_VOX_MANIFEST_URL` (manifest).
5. To ship an update, bump the pack `version`, re-upload under the new version path, and update the
   manifest — clients detect it and prompt "Update now" (preferences + selected voice preserved).

## Server persistence

Migration `033_users_tts_prefs.sql` adds `users.tts_engine_pref` + `users.tts_voice_id`, surfaced
in `UserProfile` and written via `POST /api/user/tts-prefs`. The engine preference + Vox voice sync
across devices; the browser voice stays device-local (voiceURIs aren't portable).

## Developer mode

Append `?dev=1` (sticky via localStorage `vox_dev_mode`) to reveal raw benchmark/engine metrics in
the Audio Settings modal. Hidden from normal users.

## QA checklist

- **Regression (feature off):** with no manifest URL, translated speech + timer voice behave exactly
  as today; Audio Settings still opens with the Browser Voice picker + engine selector.
- **Install:** progress %, downloaded/total bytes, cancel keeps the app functional; corrupt bytes
  auto-delete; friendly retry on failure.
- **Benchmark:** runs after install/update and on demand; friendly verdict only (numbers behind dev mode).
- **Playback:** join with an **English** target + a foreign speaker → hear a Vox voice; switch engine
  to Browser; force a Kokoro throw → single fallback toast, uninterrupted playback.
- **Offline:** reload with no network → installed pack still synthesizes.
- **Remove:** deletes assets + benchmark + metadata and switches to Browser Voice.
- **a11y:** modal keyboard nav + focus trap + ARIA on the voice radiogroups/progress.

## Known follow-ups

- ⚠️ **The currently hosted pack (`voices.voxtranslate.app`, kokoro-en 1.0.0) ships only
  `onnx/model_fp16.onnx`** → on WebGPU devices (e.g. Apple-Silicon Macs) Vox produced
  distorted/robotic audio. The client fix (device-aware dtype, never webgpu+fp16) is in, but
  to get the *fast* WebGPU path back the pack must be **re-uploaded with `onnx/model.onnx`
  (fp32) added** (and ideally `onnx/model_quantized.onnx` q8 for the wasm path). Until then,
  clients run the fp16 model on wasm (correct but slower).
- Upload the pack + manifest to Supabase and run the manual browser QA above (needs the hosted pack).
- i18n: the 40 UI strings are fully translated for 24 locales; **59 locales use English fallback**
  pending a native pass (see `scratchpad/vox-i18n.mjs` for the merge tooling/table).
