//! VoxTranslate for Business — thin client helpers for the call app (spec 0106).
//!
//! These let a signed-in business user associate a room with their org/project
//! before a call, and upload the cloud recording afterwards. Everything is
//! best-effort and guarded by the caller so the consumer flow is never affected.

import { authHeaders, HTTP_BASE } from './auth';

/** Where the standalone Business dashboard lives (separate origin). */
export const DASHBOARD_URL = 'https://dashboard.voxtranslate.app';

export interface BusinessOrg {
  id: string;
  name: string;
  role: string;
}

export interface BusinessProject {
  id: string;
  name: string;
}

/** The caller's organizations (empty for non-business users / on error). */
export async function listMyOrgs(): Promise<BusinessOrg[]> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/business/organizations`, {
      headers: authHeaders(),
    });
    if (!res.ok) return [];
    return (await res.json()) as BusinessOrg[];
  } catch {
    return [];
  }
}

/** Active projects in an org. */
export async function listProjects(orgId: string): Promise<BusinessProject[]> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/business/organizations/${orgId}/projects`, {
      headers: authHeaders(),
    });
    if (!res.ok) return [];
    return (await res.json()) as BusinessProject[];
  } catch {
    return [];
  }
}

/** Bind a room to an org/project + recording intent before the call starts. */
export async function bindRoom(
  room: string,
  body: { org_id: string; project_id?: string | null; cloud_recording_enabled?: boolean },
): Promise<boolean> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/rooms/${encodeURIComponent(room)}/business`, {
      method: 'PATCH',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    return res.ok;
  } catch {
    return false;
  }
}

/** Upload a finished cloud recording for a business call. Returns success. */
export function uploadRecording(
  sessionId: string,
  blob: Blob,
  durationSeconds: number,
): Promise<boolean> {
  return new Promise((resolve) => {
    const form = new FormData();
    form.append('duration_seconds', String(durationSeconds));
    form.append('file', blob, 'recording.webm');
    const xhr = new XMLHttpRequest();
    xhr.open(
      'POST',
      `${HTTP_BASE}/api/business/rooms/${encodeURIComponent(sessionId)}/recording/complete`,
    );
    for (const [k, v] of Object.entries(authHeaders())) xhr.setRequestHeader(k, v);
    xhr.onload = () => resolve(xhr.status >= 200 && xhr.status < 300);
    xhr.onerror = () => resolve(false);
    xhr.send(form);
  });
}
