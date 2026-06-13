-- Acquisition source (marketing attribution). One nullable column on `users`,
-- captured ONLY at first login (first-touch): the `?source` / `utm_source` /
-- `ref` query param the visitor arrived with, persisted client-side across the
-- Google sign-in so the server can stamp it on the new account. Returning logins
-- never overwrite it — the value records where the user *originally* came from,
-- which is what campaign reporting needs. NULL = organic / pre-attribution users.
ALTER TABLE users
    ADD COLUMN IF NOT EXISTS source TEXT;

-- Group-by source for the "Acquisition" KPI panel (Directus Insights).
CREATE INDEX IF NOT EXISTS idx_users_source ON users (source) WHERE source IS NOT NULL;
