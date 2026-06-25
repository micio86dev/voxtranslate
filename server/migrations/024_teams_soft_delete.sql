-- Soft-delete for teams (data safety for B2B customers): deleting a team now sets
-- archived_at instead of removing the row, mirroring projects. The row (and its
-- team_members) stays in the DB so an accidental delete is recoverable.
ALTER TABLE teams ADD COLUMN IF NOT EXISTS archived_at TIMESTAMPTZ;
