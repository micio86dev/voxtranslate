-- Friends (spec: persistent friends list + friend requests).
--
-- One symmetric row per relationship between two users, keyed on the ORDERED pair
-- (user_low < user_high) so A↔B and B↔A are the same row, and the primary key
-- prevents duplicate / reciprocal requests regardless of who asks first.
--   status       = 'pending' until the recipient accepts, then 'accepted'.
--   requested_by = who sent the request (drives the incoming-vs-outgoing display).
-- Reject / cancel-request / unfriend all just DELETE the row; accept flips status.
CREATE TABLE IF NOT EXISTS friendships (
    user_low     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    user_high    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    status       TEXT NOT NULL DEFAULT 'pending',   -- pending | accepted
    requested_by UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    accepted_at  TIMESTAMPTZ,
    PRIMARY KEY (user_low, user_high),
    CHECK (user_low < user_high)
);
-- The PK already indexes user_low; add the mirror so "where am I the high side?"
-- lookups stay fast when listing a user's friends/requests.
CREATE INDEX IF NOT EXISTS idx_friendships_high ON friendships (user_high);

-- Mirror the project's Supabase hardening: RLS on, and direct grants revoked from
-- the anon/authenticated roles (the server connects as the owner and bypasses RLS).
ALTER TABLE friendships ENABLE ROW LEVEL SECURITY;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON friendships FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON friendships FROM authenticated;
  END IF;
END $$;
