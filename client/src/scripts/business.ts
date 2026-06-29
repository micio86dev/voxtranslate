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

/** A project voice note in the project's data history (spec: B2B project voice notes). */
export interface ProjectVoiceMessage {
  id: string;
  session_id: string;
  transcript_id: string | null;
  created_by_name: string;
  file_name: string;
  content_type: string;
  size_bytes: number;
  duration_seconds: number | null;
  source_language: string;
  word_count: number | null;
  translated: boolean;
  created_at: string;
}

/** Outcome of a project voice-message upload (mirrors the chat-upload result). */
export interface VoiceMessageResult {
  ok: boolean;
  status: number;
  /** Why the transcript wasn't translated, when applicable: 'credits' (org out of
   *  credits — note saved untranslated), 'error' (Groq failed, refunded). */
  translateBlocked?: "credits" | "error";
}

/**
 * Upload a recorded voice note onto a project (no call). XHR (not fetch) so we can
 * report progress. The server transcribes + translates + persists into the
 * project's data history; the response carries the translate-blocked reason.
 */
export function uploadProjectVoiceMessage(
  orgId: string,
  projectId: string,
  file: File,
  durationSeconds: number | null,
  onProgress?: (fraction: number) => void,
): Promise<VoiceMessageResult> {
  return new Promise((resolve) => {
    const form = new FormData();
    form.append("file", file, file.name);
    if (durationSeconds != null) form.append("duration_seconds", String(Math.round(durationSeconds)));

    const xhr = new XMLHttpRequest();
    xhr.open(
      "POST",
      `${HTTP_BASE}/api/business/organizations/${encodeURIComponent(orgId)}/projects/${encodeURIComponent(projectId)}/voice-messages`,
    );
    for (const [k, v] of Object.entries(authHeaders())) xhr.setRequestHeader(k, v);
    if (onProgress && xhr.upload) {
      xhr.upload.onprogress = (e) => {
        if (e.lengthComputable) onProgress(e.loaded / e.total);
      };
    }
    xhr.onload = () => {
      const ok = xhr.status >= 200 && xhr.status < 300;
      let translateBlocked: "credits" | "error" | undefined;
      try {
        const b = JSON.parse(xhr.responseText)?.translate_blocked;
        if (b === "credits" || b === "error") translateBlocked = b;
      } catch {
        /* non-JSON body — ignore */
      }
      resolve({ ok, status: xhr.status, translateBlocked });
    };
    xhr.onerror = () => resolve({ ok: false, status: 0 });
    xhr.onabort = () => resolve({ ok: false, status: 0 });
    xhr.send(form);
  });
}

/** A project's voice notes, newest first (empty on error). */
export async function listProjectVoiceMessages(
  orgId: string,
  projectId: string,
): Promise<ProjectVoiceMessage[]> {
  try {
    const res = await fetch(
      `${HTTP_BASE}/api/business/organizations/${orgId}/projects/${projectId}/voice-messages`,
      { headers: authHeaders() },
    );
    if (!res.ok) return [];
    const data = (await res.json()) as { voice_messages?: ProjectVoiceMessage[] };
    return data.voice_messages ?? [];
  } catch {
    return [];
  }
}

/** A short-lived signed playback URL for a project voice note (null on error). */
export async function voiceMessageAudioUrl(
  orgId: string,
  projectId: string,
  voiceMessageId: string,
): Promise<string | null> {
  try {
    const res = await fetch(
      `${HTTP_BASE}/api/business/organizations/${orgId}/projects/${projectId}/voice-messages/${voiceMessageId}/audio-url`,
      { headers: authHeaders() },
    );
    if (!res.ok) return null;
    return ((await res.json()) as { url?: string }).url ?? null;
  } catch {
    return null;
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
