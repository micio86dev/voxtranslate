// Live Connect — the "Desks" entry in the account menu (spec 0121, B2B desks).
//
// Hidden by default and revealed only for a member of an organisation that has at least one
// desk: everybody else would find a menu item that leads to an empty list. `app.ts` calls this
// once the account bar is up; keeping the lookup here is what keeps that hook a single line.

import { myDesks, type ApiResult, type StaffDesk } from './api';

const ENTRY_ID = 'desks-btn';

export async function revealDesksEntry(
  fetchDesks: () => Promise<ApiResult<{ desks: StaffDesk[] }>> = () => myDesks(),
  doc: Document = document,
): Promise<boolean> {
  const entry = doc.getElementById(ENTRY_ID);
  if (!entry) return false;
  // The home screen is entered many times per visit; once shown there is nothing to learn.
  if (!entry.classList.contains('hidden')) return true;
  try {
    const res = await fetchDesks();
    if (!res.ok || res.data.desks.length === 0) return false;
    entry.classList.remove('hidden');
    return true;
  } catch {
    // The menu item is a shortcut; the page itself is reachable by URL.
    return false;
  }
}
