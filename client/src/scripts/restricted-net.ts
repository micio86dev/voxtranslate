// Great-Firewall (GFW) restricted-network detection via reachability probes.
//
// We deliberately do NOT trust geo-IP: participants inside mainland China routinely
// run a VPN that falsifies their apparent country, and many "China" IPs are proxies.
// Instead we PROBE the exact dependencies that break behind the GFW — a canary that
// is reliably reachable outside China and reliably reset/poisoned inside it, plus the
// Cartesia endpoint the Enhanced tier connects to directly. If they're unreachable on
// a short timeout we treat the network as restricted and harden the call: request the
// `turns://…:443` TLS relay from `/api/ice?restricted=1` and force
// `iceTransportPolicy: 'relay'` (and, in a follow-up, gate the client-direct Enhanced
// tier to server-proxied Standard).
//
// This tests REACHABILITY, not claimed location, so it's robust to spoofing: a client
// on a working VPN reaches the probes and gets the normal path (correct — the tunnel
// makes the GFW irrelevant); a China client without one fails the probes and gets the
// hardened path (also correct). Detection FAILS OPEN — any error resolves to `false`
// (normal behavior) so it can never block a call for a non-China user.

/** Endpoints whose reachability we probe. `google.com/generate_204` is a connectivity
 *  canary reliably blocked by the GFW (unlike gstatic, which is often reachable inside
 *  China → false negatives); `api.cartesia.ai` is the Enhanced tier's direct dependency.
 *  BOTH hosts MUST be in the CSP `connect-src` (`client/vercel.json`) — otherwise the
 *  browser blocks the fetch before it leaves, which is indistinguishable from a network
 *  failure and would flag EVERY user as restricted. `const` so they're tuned in one place. */
export const RESTRICTED_PROBE_URLS = [
  'https://www.google.com/generate_204',
  'https://api.cartesia.ai/',
] as const;

/** How long to wait for a probe before calling the endpoint unreachable. Short: a
 *  real GFW reset/timeout settles well within this, and we don't want to delay join
 *  for open networks (where the probes answer in well under this). */
export const PROBE_TIMEOUT_MS = 2500;

/**
 * Pure decision core (no I/O, unit-tested): given each probe's reachability, decide
 * whether the network is restricted. Restricted ⇔ EVERY probe failed. Requiring ALL
 * to fail is conservative — one reachable endpoint means the network is open, so a
 * single provider outage (e.g. Cartesia down) never trips the whole hardening path and
 * mislabels a non-China user. An empty list ⇒ not restricted (fail open).
 */
export function isRestrictedVerdict(reachable: boolean[]): boolean {
  if (reachable.length === 0) return false;
  return reachable.every((r) => !r);
}

/** Fire one no-cors probe. Resolves `true` if the host answered at all — an opaque
 *  response (or even an HTTP error) is fine, we only care that the TCP/TLS connection
 *  wasn't reset. `no-cors` is essential: it prevents a CORS rejection from masquerading
 *  as a network failure. Resolves `false` on network error / timeout. Never throws. */
async function probe(url: string, timeoutMs: number): Promise<boolean> {
  try {
    await fetch(url, {
      mode: 'no-cors',
      cache: 'no-store',
      signal: AbortSignal.timeout(timeoutMs),
    });
    return true;
  } catch {
    return false;
  }
}

async function runProbes(): Promise<boolean> {
  // Fail open if the probe mechanism isn't available (e.g. an older browser without
  // AbortSignal.timeout, or a stripped environment without fetch): run the normal path
  // rather than force every such client onto relay + the Enhanced gate. A genuine GFW
  // network still trips — there fetch exists and the probe endpoints get reset.
  if (typeof fetch !== 'function') return false;
  if (typeof AbortSignal === 'undefined' || typeof AbortSignal.timeout !== 'function') return false;
  try {
    const results = await Promise.all(RESTRICTED_PROBE_URLS.map((u) => probe(u, PROBE_TIMEOUT_MS)));
    return isRestrictedVerdict(results);
  } catch {
    return false; // fail open — never block a call on a detection error
  }
}

/** In-flight/settled probe for this page load. Cached as a Promise so an earlier
 *  caller (pre-join warm-up) and the call path share one probe, and repeated calls
 *  are free. Per-page (not persisted) so a network change on reload is re-detected. */
let inflight: Promise<boolean> | null = null;

/**
 * Whether the current network looks GFW-restricted. Probes once per page load; later
 * calls reuse the result. Safe to call eagerly (e.g. at pre-join) to warm the verdict
 * before it's needed at call start.
 */
export function isRestrictedNetwork(): Promise<boolean> {
  if (!inflight) inflight = runProbes();
  return inflight;
}

/** Drop the cached verdict so the next call re-probes. For tests and any future
 *  "network changed, re-check" flow. */
export function resetRestrictedNetwork(): void {
  inflight = null;
}
