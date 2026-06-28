// REST helpers for notification preferences + the in-app center. Auth plumbing lives
// in auth.ts; this only talks JSON to /api/notifications*. Fails soft on any error.

import { authHeaders, HTTP_BASE } from './auth';

export interface PrefRow {
  type: string;
  channel: string;
  enabled: boolean;
}
export interface Prefs {
  preferences: PrefRow[];
  quiet_hours_start: number | null;
  quiet_hours_end: number | null;
  timezone: string;
}
export interface InAppNotification {
  id: string;
  type: string;
  title: string;
  body: string;
  data: Record<string, unknown>;
  read_at: string | null;
  created_at: string;
}

/** Notification event types (must match the server's TYPES), friends first. */
export const NOTIF_EVENTS = [
  'friend_request',
  'friend_accepted',
  'call_invite',
  'friend_active',
  'meeting_invited',
  'meeting_reminder',
  'meeting_updated',
  'meeting_cancelled',
] as const;
export const NOTIF_CHANNELS = ['push', 'email', 'in_app'] as const;

export async function fetchPreferences(): Promise<Prefs | null> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/notifications/preferences`, { headers: authHeaders() });
    if (!res.ok) return null;
    return (await res.json()) as Prefs;
  } catch {
    return null;
  }
}

export async function savePreferences(preferences: PrefRow[]): Promise<boolean> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/notifications/preferences`, {
      method: 'PATCH',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify({ preferences }),
    });
    return res.ok;
  } catch {
    return false;
  }
}

export async function fetchUnread(limit = 20): Promise<InAppNotification[]> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/notifications?unread=true&limit=${limit}`, {
      headers: authHeaders(),
    });
    if (!res.ok) return [];
    const data = (await res.json()) as { notifications?: InAppNotification[] };
    return data.notifications ?? [];
  } catch {
    return [];
  }
}

export async function markRead(id: string): Promise<boolean> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/notifications/${encodeURIComponent(id)}/read`, {
      method: 'POST',
      headers: authHeaders(),
    });
    return res.ok;
  } catch {
    return false;
  }
}
