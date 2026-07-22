CREATE TABLE development_run_roles (
    run_id      TEXT NOT NULL,
    slot_id     TEXT NOT NULL,
    role        TEXT NOT NULL CHECK (role IN ('implementer', 'tester', 'reviewer', 'integrator')),
    assigned_at INTEGER NOT NULL,
    PRIMARY KEY(run_id, slot_id, role),
    FOREIGN KEY(run_id) REFERENCES development_runs(id) ON DELETE CASCADE
);

CREATE INDEX idx_development_run_roles_run_role ON development_run_roles(run_id, role, slot_id);
