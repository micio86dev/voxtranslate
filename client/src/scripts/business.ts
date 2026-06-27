//! VoxTranslate for Business — thin client helpers for the call app (spec 0106).
//!
//! These let a signed-in business user associate a room with their org/project
//! before a call, and upload the cloud recording afterwards. Everything is
//! best-effort and guarded by the caller so the consumer flow is never affected.

import { authHeaders, HTTP_BASE } from "./auth";

/** Where the standalone Business dashboard lives (separate origin). */
export const DASHBOARD_URL = "https://dashboard.voxtranslate.app";

export interface BusinessOrg {
  id: string;
  name: string;
  role: string;
  /** 'business' | 'enterprise'. */
  plan: string;
  /** 'none' | 'active' | 'past_due' | 'canceled' — gates cloud recording. */
  subscription_status: string;
}

/** Cloud recording is a paid feature — only orgs with an active subscription. */
export function canCloudRecord(org: BusinessOrg | undefined): boolean {
  return org?.subscription_status === "active";
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
    const res = await fetch(
      `${HTTP_BASE}/api/business/organizations/${orgId}/projects`,
      {
        headers: authHeaders(),
      },
    );
    if (!res.ok) return [];
    return (await res.json()) as BusinessProject[];
  } catch {
    return [];
  }
}

/** Bind a room to an org/project + recording intent before the call starts. */
export async function bindRoom(
  room: string,
  body: {
    org_id: string;
    project_id?: string | null;
    cloud_recording_enabled?: boolean;
  },
): Promise<boolean> {
  try {
    const res = await fetch(
      `${HTTP_BASE}/api/rooms/${encodeURIComponent(room)}/business`,
      {
        method: "PATCH",
        headers: { ...authHeaders(), "Content-Type": "application/json" },
        body: JSON.stringify(body),
      },
    );
    return res.ok;
  } catch {
    return false;
  }
}

/** Upload a finished cloud recording for a business call. Returns success.
 *
 * Direct-to-storage: the server hands back a one-shot signed URL, the browser PUTs
 * the (potentially ~1 GB) video straight to storage, then `complete` records the
 * path + charges credits. The recording never flows through our server. */
export async function uploadRecording(
  sessionId: string,
  blob: Blob,
  durationSeconds: number,
): Promise<boolean> {
  try {
    const base = `${HTTP_BASE}/api/business/rooms/${encodeURIComponent(sessionId)}/recording`;
    // 1) Ask for a signed upload URL + the object path to report back.
    const presign = await fetch(`${base}/upload-url`, {
      method: "POST",
      headers: authHeaders(),
    });
    if (!presign.ok) return false;
    const { upload_url, object_path } = (await presign.json()) as {
      upload_url: string;
      object_path: string;
    };

    // 2) PUT the video straight to storage (no auth header — the URL is signed).
    const put = await fetch(upload_url, {
      method: "PUT",
      headers: { "Content-Type": "video/webm", "x-upsert": "true" },
      body: blob,
    });
    if (!put.ok) return false;

    // 3) Tell the server it landed → record path, charge credits, transcribe.
    const done = await fetch(`${base}/complete`, {
      method: "POST",
      headers: { ...authHeaders(), "Content-Type": "application/json" },
      body: JSON.stringify({ object_path, duration_seconds: durationSeconds }),
    });
    return done.ok;
  } catch {
    return false;
  }
}

/** The org/project a room is already bound to (e.g. a scheduled meeting created in
 *  the dashboard), so the pre-join UI can pre-select it instead of clobbering the
 *  project on connect. Returns null when unbound or the caller isn't a member. */
export async function getRoomBinding(
  room: string,
): Promise<{ org_id: string; project_id: string | null } | null> {
  try {
    const res = await fetch(
      `${HTTP_BASE}/api/rooms/${encodeURIComponent(room)}/business`,
      {
        headers: authHeaders(),
      },
    );
    if (!res.ok) return null;
    const data = (await res.json()) as {
      org_id: string | null;
      project_id: string | null;
    };
    // The server now returns 200 with a null org_id for an unbound room (instead of
    // a 404 that logged a console error on every standard call) — treat it as null.
    return data.org_id ? { org_id: data.org_id, project_id: data.project_id } : null;
  } catch {
    return null;
  }
}
