-- M2-A: users, sessions, audit log.
-- Auth semantics per plan/v1/viewer/rest.md "Superuser / session" + "Auth / PKI"
-- (UserInfo shape, role gates). v1's confusing `role` (free-text job title)
-- vs `user_role` (authz) split is kept but renamed job_title/user_role for
-- clarity. Password hashing and token generation are NOT this layer's job
-- (wave-2 auth service) — this schema only stores the hash/token-hash.

CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT    NOT NULL,
    full_name     TEXT,
    job_title     TEXT,
    user_role     TEXT    NOT NULL DEFAULT 'viewer'
                          CHECK (user_role IN ('viewer', 'manager', 'admin')),
    is_superuser  INTEGER NOT NULL DEFAULT 0,
    enabled       INTEGER NOT NULL DEFAULT 1,
    created_at    TEXT    NOT NULL DEFAULT (datetime('now')),
    last_login    TEXT
);

-- Raw session tokens are NEVER stored; callers pass a SHA-256 hex hash of the
-- bearer token as `token_hash`.
CREATE TABLE sessions (
    token_hash    TEXT    PRIMARY KEY,
    user_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at    INTEGER NOT NULL,
    last_seen_at  INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL
);

CREATE INDEX idx_sessions_user_id ON sessions (user_id);

CREATE TABLE audit_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts         TEXT    NOT NULL DEFAULT (datetime('now')),
    event_type TEXT    NOT NULL,
    username   TEXT,
    user_id    INTEGER,
    ip         TEXT,
    target     TEXT,
    details    TEXT,
    success    INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX idx_audit_log_ts ON audit_log (ts);
CREATE INDEX idx_audit_log_event_type ON audit_log (event_type);
