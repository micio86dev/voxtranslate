// Friends list + friend requests (send / accept / reject / cancel / remove) and
// inviting an existing friend into the current call. Auth/session plumbing lives in
// auth.ts; this module only talks JSON to the `/api/friends` endpoints. Every call
// fails soft (returns an empty/false result) so the UI never throws on a network blip.

import { authHeaders, HTTP_BASE } from './auth';

export interface Friend {
  id: string;
  name: string;
  email: string;
  avatar_url?: string | null;
}

export interface FriendRequests {
  /** Requests others sent me, awaiting my accept/reject. */
  incoming: Friend[];
  /** Requests I sent that are still pending (I can cancel them). */
  outgoing: Friend[];
}

export async function fetchFriends(): Promise<Friend[]> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/friends`, { headers: authHeaders() });
    if (!res.ok) return [];
    return (await res.json()) as Friend[];
  } catch {
    return [];
  }
}

export async function fetchFriendRequests(): Promise<FriendRequests> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/friends/requests`, { headers: authHeaders() });
    if (!res.ok) return { incoming: [], outgoing: [] };
    return (await res.json()) as FriendRequests;
  } catch {
    return { incoming: [], outgoing: [] };
  }
}

/**
 * Send a friend request, either by `email` (typed in the panel) or `userId` (the
 * "add friend" button next to a logged-in call participant). Returns `null` on
 * success, or a short server message / status code on failure so the caller can map
 * it to a localized error.
 */
export async function sendFriendRequest(by: { email?: string; userId?: string }): Promise<string | null> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/friends/request`, {
      method: 'POST',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify({ email: by.email, user_id: by.userId }),
    });
    if (res.ok) return null;
    return (await res.text().catch(() => '')) || String(res.status);
  } catch {
    return 'network';
  }
}

export async function acceptFriend(userId: string): Promise<boolean> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/friends/${encodeURIComponent(userId)}/accept`, {
      method: 'POST',
      headers: authHeaders(),
    });
    return res.ok;
  } catch {
    return false;
  }
}

/** Reject a request, cancel one I sent, or unfriend — the server treats all three
 *  as deleting the single relationship row (see friends.rs::remove). */
export async function removeFriend(userId: string): Promise<boolean> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/friends/${encodeURIComponent(userId)}`, {
      method: 'DELETE',
      headers: authHeaders(),
    });
    return res.ok;
  } catch {
    return false;
  }
}

export async function inviteFriendToCall(userId: string, room: string): Promise<boolean> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/friends/${encodeURIComponent(userId)}/invite`, {
      method: 'POST',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify({ room }),
    });
    return res.ok;
  } catch {
    return false;
  }
}
