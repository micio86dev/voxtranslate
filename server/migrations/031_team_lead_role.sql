-- Team-lead role (spec: admin "team & project insights" assistant). A team member
-- can be a 'lead': leads are the people who can run the insights assistant scoped to
-- their teams' members + projects (org owners see everything). Reuses team_members
-- rather than a separate table — a lead IS a member with elevated standing.
ALTER TABLE team_members
    ADD COLUMN IF NOT EXISTS role TEXT NOT NULL DEFAULT 'member'
    CHECK (role IN ('lead', 'member'));

-- "Which teams does this user lead?" is the hot lookup for insights eligibility/scope.
CREATE INDEX IF NOT EXISTS idx_team_members_user_role
    ON team_members (user_id, role);
