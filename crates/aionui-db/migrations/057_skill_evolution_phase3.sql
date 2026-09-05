-- Skill Evolution Phase 3: heuristic gate fields + team experience ACL + user settings.
-- Does NOT inject experience into inference prompts. Does NOT vendor wikiskill CLI.

-- Experience Hub ACL
ALTER TABLE experience_articles ADD COLUMN visibility TEXT NOT NULL DEFAULT 'private';
-- team_id already exists on experience_articles (056)

CREATE INDEX IF NOT EXISTS idx_experience_articles_team_vis
    ON experience_articles(team_id, visibility, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_experience_articles_visibility
    ON experience_articles(visibility, updated_at DESC);

-- Proposal gate + optional team binding
ALTER TABLE skill_evolution_proposals ADD COLUMN team_id TEXT;
ALTER TABLE skill_evolution_proposals ADD COLUMN visibility TEXT NOT NULL DEFAULT 'private';
ALTER TABLE skill_evolution_proposals ADD COLUMN gate_mode TEXT NOT NULL DEFAULT 'human_only';
ALTER TABLE skill_evolution_proposals ADD COLUMN gate_score INTEGER;
ALTER TABLE skill_evolution_proposals ADD COLUMN gate_signals TEXT NOT NULL DEFAULT '[]';
ALTER TABLE skill_evolution_proposals ADD COLUMN gate_recommendation TEXT;
ALTER TABLE skill_evolution_proposals ADD COLUMN try_run_ok INTEGER;

CREATE INDEX IF NOT EXISTS idx_sep_team
    ON skill_evolution_proposals(team_id, updated_at DESC);

-- Per-user skill-evolution settings (gate mode + thresholds)
CREATE TABLE IF NOT EXISTS skill_evolution_settings (
    user_id TEXT PRIMARY KEY NOT NULL,
    gate_mode TEXT NOT NULL DEFAULT 'human_only'
        CHECK (gate_mode IN ('human_only', 'heuristic_assist', 'auto_apply_on_pass')),
    assist_threshold INTEGER NOT NULL DEFAULT 70,
    auto_threshold INTEGER NOT NULL DEFAULT 90,
    default_experience_visibility TEXT NOT NULL DEFAULT 'private'
        CHECK (default_experience_visibility IN ('private', 'team', 'owner_editors')),
    updated_at INTEGER NOT NULL
);
