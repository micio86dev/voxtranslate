-- Optional, opt-in user location (spec: superadmin user map). The frontend asks for
-- GPS consent ("help us improve the service"); on accept we store the coordinates
-- here. Plain columns only — the PostGIS geometry column used by the Directus map
-- layout is added best-effort at startup (see `location::ensure_geometry`), so a DB
-- image without PostGIS can never block boot.
ALTER TABLE users
    ADD COLUMN IF NOT EXISTS location_consent     BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS latitude             DOUBLE PRECISION,
    ADD COLUMN IF NOT EXISTS longitude            DOUBLE PRECISION,
    ADD COLUMN IF NOT EXISTS location_updated_at  TIMESTAMPTZ;
