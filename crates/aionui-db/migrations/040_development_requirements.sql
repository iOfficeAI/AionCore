CREATE TABLE development_requirement_versions (
    id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    content TEXT NOT NULL,
    change_summary TEXT,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES development_runs(id) ON DELETE CASCADE,
    UNIQUE(run_id, version)
);

CREATE TABLE development_acceptance_criteria (
    id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL,
    requirement_version_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    statement TEXT NOT NULL,
    required INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES development_runs(id) ON DELETE CASCADE,
    FOREIGN KEY(requirement_version_id) REFERENCES development_requirement_versions(id) ON DELETE CASCADE,
    UNIQUE(requirement_version_id, ordinal)
);

CREATE TABLE development_plan_revisions (
    id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    summary TEXT NOT NULL,
    content TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES development_runs(id) ON DELETE CASCADE,
    UNIQUE(run_id, revision)
);

CREATE TABLE development_task_criteria (
    run_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    criterion_id TEXT NOT NULL,
    mapped_at INTEGER NOT NULL,
    PRIMARY KEY(task_id, criterion_id),
    FOREIGN KEY(run_id) REFERENCES development_runs(id) ON DELETE CASCADE,
    FOREIGN KEY(task_id) REFERENCES team_tasks(id) ON DELETE CASCADE,
    FOREIGN KEY(criterion_id) REFERENCES development_acceptance_criteria(id) ON DELETE CASCADE
);

CREATE TABLE development_completion_evidence (
    id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    criterion_id TEXT NOT NULL,
    evidence_type TEXT NOT NULL CHECK (evidence_type IN ('code', 'test', 'no_change')),
    artifact_id TEXT,
    reference TEXT NOT NULL,
    accepted INTEGER NOT NULL DEFAULT 0,
    reviewer_id TEXT,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES development_runs(id) ON DELETE CASCADE,
    FOREIGN KEY(task_id) REFERENCES team_tasks(id) ON DELETE CASCADE,
    FOREIGN KEY(criterion_id) REFERENCES development_acceptance_criteria(id) ON DELETE CASCADE,
    FOREIGN KEY(artifact_id) REFERENCES task_artifacts(id) ON DELETE SET NULL
);

CREATE INDEX idx_development_requirements_run ON development_requirement_versions(run_id, version);
CREATE INDEX idx_development_criteria_run ON development_acceptance_criteria(run_id, requirement_version_id, ordinal);
CREATE INDEX idx_development_plan_run ON development_plan_revisions(run_id, revision);
CREATE INDEX idx_development_evidence_criterion ON development_completion_evidence(run_id, criterion_id, accepted);

CREATE TRIGGER development_requirement_versions_no_update
BEFORE UPDATE ON development_requirement_versions BEGIN SELECT RAISE(ABORT, 'requirement versions are append-only'); END;
CREATE TRIGGER development_requirement_versions_no_delete
BEFORE DELETE ON development_requirement_versions BEGIN SELECT RAISE(ABORT, 'requirement versions are append-only'); END;
CREATE TRIGGER development_plan_revisions_no_update
BEFORE UPDATE ON development_plan_revisions BEGIN SELECT RAISE(ABORT, 'plan revisions are append-only'); END;
CREATE TRIGGER development_plan_revisions_no_delete
BEFORE DELETE ON development_plan_revisions BEGIN SELECT RAISE(ABORT, 'plan revisions are append-only'); END;
CREATE TRIGGER development_completion_evidence_no_update
BEFORE UPDATE ON development_completion_evidence BEGIN SELECT RAISE(ABORT, 'completion evidence is append-only'); END;
CREATE TRIGGER development_completion_evidence_no_delete
BEFORE DELETE ON development_completion_evidence BEGIN SELECT RAISE(ABORT, 'completion evidence is append-only'); END;
