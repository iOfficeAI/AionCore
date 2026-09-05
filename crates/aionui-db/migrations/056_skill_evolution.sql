-- Skill Evolution MVP (WorkMate-native WikiSkill concepts).
-- Experience Hub (wiki layer) + skill evolution proposals (gate objects).
-- Does NOT vendor community wikiskill CLI; does NOT bundle KnowHub.

CREATE TABLE IF NOT EXISTS experience_articles (
    id TEXT PRIMARY KEY NOT NULL,
    owner_user_id TEXT NOT NULL,
    assistant_id TEXT,
    team_id TEXT,
    kind TEXT NOT NULL DEFAULT 'pattern'
        CHECK (kind IN ('pattern', 'index', 'skill_impact', 'rejected_note', 'general')),
    title TEXT NOT NULL,
    body_md TEXT NOT NULL DEFAULT '',
    source_conversation_ids TEXT NOT NULL DEFAULT '[]',
    tags TEXT NOT NULL DEFAULT '[]',
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'archived')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_experience_articles_owner
    ON experience_articles(owner_user_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_experience_articles_assistant
    ON experience_articles(assistant_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS skill_evolution_proposals (
    id TEXT PRIMARY KEY NOT NULL,
    owner_user_id TEXT NOT NULL,
    assistant_id TEXT,
    conversation_id TEXT,
    status TEXT NOT NULL DEFAULT 'draft'
        CHECK (status IN (
            'draft', 'pending_review', 'approved',
            'rejected', 'applied', 'rolled_back'
        )),
    title TEXT NOT NULL,
    experience_summary TEXT NOT NULL DEFAULT '',
    experience_article_ids TEXT NOT NULL DEFAULT '[]',
    action TEXT NOT NULL DEFAULT 'create'
        CHECK (action IN ('create', 'patch')),
    target_skill_key TEXT,
    draft_skill_md TEXT NOT NULL DEFAULT '',
    draft_diff_summary TEXT,
    reviewer_user_id TEXT,
    review_comment TEXT,
    reviewed_at INTEGER,
    applied_skill_key TEXT,
    applied_skill_version TEXT,
    previous_skill_md TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sep_owner_status
    ON skill_evolution_proposals(owner_user_id, status, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sep_assistant
    ON skill_evolution_proposals(assistant_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sep_conversation
    ON skill_evolution_proposals(conversation_id);
