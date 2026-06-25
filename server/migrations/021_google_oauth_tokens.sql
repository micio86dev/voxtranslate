-- Google OAuth tokens (spec: scheduled meetings, Phase 1a).
--
-- The login flow upgrades from a GSI ID-token (authentication only) to an OAuth
-- authorization-code flow that also yields a refresh token, so the server can call
-- the Google Calendar API on the user's behalf. One row per user; the refresh token
-- is encrypted at rest (AEAD, see server/src/google_oauth.rs). The short-lived access
-- token + expiry are cached so we only hit Google's token endpoint when it expires.
CREATE TABLE IF NOT EXISTS google_oauth_tokens (
    user_id                 UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    -- AEAD ciphertext, base64(nonce || ct). NULL once revoked / never granted.
    refresh_token_encrypted TEXT,
    access_token            TEXT,
    expires_at              TIMESTAMPTZ,
    scopes                  TEXT NOT NULL DEFAULT '',
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE google_oauth_tokens ENABLE ROW LEVEL SECURITY;

-- Re-apply the guarded REVOKE (anon/authenticated only exist on Supabase, not
-- local/CI Postgres) — same pattern as migration 016.
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM anon;
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM authenticated;
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM authenticated;
  END IF;
END $$;
