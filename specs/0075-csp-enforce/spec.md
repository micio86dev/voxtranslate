# 0075 — Enforce the full Content-Security-Policy

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-16 |
| **Shipped** | 2026-06-16 |
| **Version** | — |
| **Commits** | `d437f49` (#175) |
| **Depends on** | [0028](../0028-security-hardening/spec.md), [0029](../0029-security-followups/spec.md), [0017](../0017-virtual-background/spec.md) |

## 1. Context & Problem

The app ships a **minimal** enforcing CSP — `frame-ancestors 'none'; object-src
'none'; base-uri 'self'` (`client/vercel.json`, spec 0028). A **full** policy that
also constrains `script-src`/`connect-src`/`img-src`/… was deliberately deferred in
0028 §6 and 0029 §2 because it *"needs testing on a Vercel preview so it doesn't
break GSI / WS / Stripe"* — a wrong allowlist silently breaks the app (a blocked
`connect-src` kills the WS/REST calls; a blocked `script-src` kills Google Sign-In).

As a staging step (#117 / #121, commit `f02d2d7`) the full policy was added as
**`Content-Security-Policy-Report-Only`** — it reports violations but enforces
nothing. This spec does the audit the earlier specs asked for and **promotes the
report-only policy to enforcing**, closing the long-standing `SECURITY.md`
follow-up "CSP is minimal".

The audit is the substance of the change, not the one-line edit. The blast radius of
getting it wrong is a silently broken production app, so every external origin the
client actually touches at runtime was enumerated from the code before flipping the
header.

## 2. Goals / Non-Goals

**Goals**
- Replace the minimal enforcing CSP with a **full enforcing CSP** that allows
  exactly the origins the client uses and nothing else.
- **Zero functional regression**: Google Sign-In, the Railway WS + REST API,
  Supabase Storage images, and the MediaPipe virtual-background all keep working.
- Keep the policy as a single, readable header; drop the now-redundant Report-Only.

**Non-Goals**
- Removing `script-src 'unsafe-inline'` (Astro emits inline hydration/SW-registration
  scripts; dropping it needs CSP **nonces/hashes** — a separate Astro-config change,
  tracked as the next tightening in `SECURITY.md`).
- A violation-report **collector** endpoint (`report-uri`/`report-to`) — the staged
  Report-Only never had one either; revisit if we want telemetry on blocks.
- Server-side CSP. The Railway origin serves JSON/WS, not HTML documents, so CSP is
  moot there; the document is served by Vercel, whose header this is.

## 3. Requirements

- **R1 — Full policy enforced.** The `/(.*)` response carries one
  `Content-Security-Policy` header with `default-src 'self'` and explicit
  per-directive allowlists; no `Content-Security-Policy-Report-Only` remains.
  - *Given* any page load, *when* the browser parses the response, *then* it enforces
    `default-src 'self'` and blocks any origin not on an allowlist.
- **R2 — Auth unaffected.** Google Identity Services keeps loading and posting.
  - *Given* the enforcing policy, *when* a user clicks Google Sign-In, *then*
    `accounts.google.com` script + frame load and the popup completes (COOP already
    set to `same-origin-allow-popups`).
- **R3 — Realtime unaffected.** The WS + REST calls to the Railway origin succeed.
  - *Given* the policy, *when* the client opens `wss://…railway.app/ws` and fetches
    `https://…railway.app/…`, *then* `connect-src` allows both schemes for that host.
- **R4 — Virtual background unaffected.** Background blur keeps working.
  - *Given* the policy, *when* a user enables blur, *then* the MediaPipe UMD script,
    its WASM/model fetches from `cdn.jsdelivr.net`, and WASM instantiation all
    succeed (`script-src`/`connect-src` allow the CDN; `'wasm-unsafe-eval'` allows
    WebAssembly). If blocked anyway, the call still works (graceful degradation,
    spec 0017) — blur is the only thing that can fail.

## 4. Design & Architecture

- **Components / files:** `client/vercel.json` — the only enforcement point (Vercel
  serves the document; `headers[].source: "/(.*)"`).
- **Source audit (the verification that 0028/0029 required):**

  | Directive | Allowlist | Why (code evidence) |
  |-----------|-----------|---------------------|
  | `script-src` | `'self' 'unsafe-inline' 'wasm-unsafe-eval' accounts.google.com cdn.jsdelivr.net` | Astro inline scripts + SW registration (`Base.astro`); GSI client; MediaPipe UMD `<script>` + WASM (`virtual-background.ts:11,48`) |
  | `connect-src` | `'self' https+wss://…railway.app accounts.google.com *.supabase.co cdn.jsdelivr.net` | WS+REST to Railway (`app.ts:43` `PUBLIC_WS_HOST`); OAuth; Supabase; MediaPipe asset fetches |
  | `img-src` | `'self' data: blob: *.googleusercontent.com *.gstatic.com *.supabase.co` | Google avatars; Supabase blobs; canvas/blob previews |
  | `frame-src` | `accounts.google.com` | GSI popup/iframe only |
  | `style-src` | `'self' 'unsafe-inline' accounts.google.com` | Astro/GSI inline styles |
  | `font-src` | `'self' data:` | No web fonts loaded (verified: no `fonts.g*`) |
  | `media-src` | `'self' blob:` | Recorded-blob playback; WebRTC `srcObject` is not CSP-governed |
  | `worker-src` | `'self'` | `/sw.js` only; no blob/remote workers (verified) |
  | `object-src`/`base-uri`/`frame-ancestors`/`form-action` | `'none'`/`'self'`/`'none'`/`'self'` | carried over from the minimal policy |

- **Key decisions:**
  - *Stripe needs no entry* — Checkout is **server-side hosted** (`auth.ts:284`
    returns a hosted URL; a top-level redirect, not an embedded `js.stripe.com`
    iframe), so neither `script-src`/`frame-src`/`connect-src` is involved.
  - *`'wasm-unsafe-eval'`, not `'unsafe-eval'`* — MediaPipe needs WebAssembly
    instantiation, which the narrow `'wasm-unsafe-eval'` permits without opening JS
    `eval`. If reports ever show MediaPipe needs more, the fallback is `'unsafe-eval'`
    or self-hosting the model; blur degrades gracefully meanwhile.
  - *Single enforcing header, drop Report-Only* — the report-only policy was the
    staging copy; once enforced it is redundant.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Audit every external origin the client loads/connects to (script/connect/img/frame/font/media/worker) | `client/src/**` |
| S1 | Promote the report-only policy to enforcing, adding the audited MediaPipe (`cdn.jsdelivr.net` + `'wasm-unsafe-eval'`) entries; remove the report-only header | `client/vercel.json` |
| S2 | Mark the `SECURITY.md` follow-up done; record the audit | `SECURITY.md`, this spec |

## 6. Testing & Verification

- **Static audit (done):** enumerated `https?://`/`wss?://` references and
  resource-loading APIs (`new Worker`, `Worklet`, `WebAssembly`, `eval`, remote
  `import()`, `createObjectURL`) across `client/src` — the only runtime external
  origins are GSI, Supabase, the Railway origin, Google avatar hosts, and the
  MediaPipe CDN; all are covered.
- **Preview verification (the 0028/0029 gate):** on a Vercel preview, with the
  enforcing header, confirm **zero CSP violations** in the console across: load,
  Google Sign-In, create/join a room (WS + REST), send a chat image, and enable
  background blur. Any violation ⇒ tighten the offending directive before merge.
- **Rollback:** revert `client/vercel.json`; Vercel redeploys the previous header
  within a deploy cycle. Graceful blur degradation bounds worst-case impact.

## 7. Deployment & Operations

- No env vars, no migration. Vercel applies the header on the next client deploy
  (auto on `main`). The Railway server is untouched.
- If `PUBLIC_WS_HOST` ever changes to a non-Railway host (e.g. a custom API domain),
  `connect-src` must be updated in lockstep or all realtime calls break.

## 8. Risks / Open Items

- **MediaPipe eval surface:** `'wasm-unsafe-eval'` assumed sufficient; graceful
  degradation is the safety net, fallback documented above.
- **`'unsafe-inline'` on `script-src`/`style-src`** remains — the next hardening step
  is Astro CSP nonces/hashes (tracked in `SECURITY.md`).
- No violation-report collector, so post-deploy blocks surface only as console
  warnings during the preview check, not as telemetry.

## 9. References

- Predecessors: specs [0028](../0028-security-hardening/spec.md) §6,
  [0029](../0029-security-followups/spec.md) §2; staging commit `f02d2d7` (#117/#121).
- Audit master: issue #114 §5 ("CSP completa" → P2).
- Files: `client/vercel.json`, `client/src/scripts/virtual-background.ts`,
  `client/src/scripts/{app,auth}.ts`, `client/src/layouts/Base.astro`, `SECURITY.md`.
- External: [MDN CSP](https://developer.mozilla.org/docs/Web/HTTP/Headers/Content-Security-Policy),
  [`'wasm-unsafe-eval'`](https://developer.mozilla.org/docs/Web/HTTP/Headers/Content-Security-Policy/script-src).
