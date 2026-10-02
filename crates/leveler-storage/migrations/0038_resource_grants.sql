-- Additive resource approvals. No legacy permission rule conversion.
CREATE TABLE resource_grants (
    project_identity TEXT NOT NULL,
    session_identity TEXT NOT NULL,
    scope TEXT NOT NULL CHECK (scope IN ('session', 'project')),
    schema_version INTEGER NOT NULL,
    binding_json TEXT NOT NULL,
    CHECK ((scope = 'session' AND length(session_identity) > 0)
        OR (scope = 'project' AND session_identity = '')),
    UNIQUE (project_identity, session_identity, scope, binding_json)
);
CREATE INDEX resource_grants_lookup ON resource_grants(project_identity, session_identity);
