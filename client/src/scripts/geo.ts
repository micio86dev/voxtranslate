// Optional, opt-in location sharing (spec: superadmin user map). Shows a one-time
// banner to signed-in users explaining it helps us improve the service; on accept we
// ask the browser for GPS and POST the coordinates to /api/user/location. Entirely
// optional — "Not now" just hides it, and a prior choice is remembered so we never
// nag. Never blocks any core flow.

import { authHeaders, HTTP_BASE, isLoggedIn } from "./auth";

const KEY = "vox.geo"; // 'granted' | 'dismissed'

function getChoice(): string | null {
  try {
    return localStorage.getItem(KEY);
  } catch {
    return null;
  }
}
function setChoice(v: string): void {
  try {
    localStorage.setItem(KEY, v);
  } catch {
    /* storage blocked — fall through, we just may ask again next session */
  }
}

async function sendLocation(pos: GeolocationPosition): Promise<void> {
  try {
    await fetch(`${HTTP_BASE}/api/user/location`, {
      method: "POST",
      headers: { ...authHeaders(), "Content-Type": "application/json" },
      body: JSON.stringify({
        latitude: pos.coords.latitude,
        longitude: pos.coords.longitude,
        accuracy: pos.coords.accuracy,
      }),
    });
  } catch {
    /* best-effort; nothing to surface to the user */
  }
}

/**
 * Wire the location opt-in banner. Shows it once for signed-in users who haven't
 * chosen yet and whose browser supports geolocation. Idempotent — listeners attach
 * once. No-ops when the markup is absent or the user is a guest.
 */
export function setupGeoOptIn(): void {
  const banner = document.getElementById("geo-banner");
  if (!banner) return;
  if (!isLoggedIn() || !("geolocation" in navigator) || getChoice()) {
    banner.classList.add("hidden");
    return;
  }
  banner.classList.remove("hidden");

  const hide = () => banner.classList.add("hidden");

  document.getElementById("geo-dismiss")?.addEventListener("click", () => {
    setChoice("dismissed");
    hide();
  });

  document.getElementById("geo-enable")?.addEventListener("click", () => {
    hide();
    // Clicking is the user gesture the browser needs before its native prompt.
    navigator.geolocation.getCurrentPosition(
      (pos) => {
        setChoice("granted");
        void sendLocation(pos);
      },
      () => {
        // Denied at the OS level (or unavailable): treat as a soft dismiss so we
        // don't immediately re-ask, but the user can still enable it later in privacy.
        setChoice("dismissed");
      },
      { enableHighAccuracy: false, timeout: 10000, maximumAge: 600000 },
    );
  });
}
