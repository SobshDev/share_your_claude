CREATE TABLE person (
 id TEXT PRIMARY KEY, name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 100),
 created_at TEXT NOT NULL
);
CREATE TABLE api_key (
 id TEXT PRIMARY KEY, person_id TEXT NOT NULL REFERENCES person(id), label TEXT NOT NULL,
 prefix TEXT NOT NULL, secret_hash BLOB NOT NULL UNIQUE,
 created_at TEXT NOT NULL, last_used_at TEXT, revoked_at TEXT
);
CREATE TABLE model (
 id TEXT PRIMARY KEY, display_name TEXT NOT NULL, model_group TEXT NOT NULL,
 enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0,1)), reviewed_at TEXT
);
CREATE TABLE model_alias (
 alias TEXT PRIMARY KEY, model_id TEXT NOT NULL REFERENCES model(id)
);
CREATE TABLE key_model_grant (
 key_id TEXT NOT NULL REFERENCES api_key(id), model_id TEXT NOT NULL REFERENCES model(id),
 PRIMARY KEY(key_id,model_id)
);
CREATE TABLE claude_credential (
 id INTEGER PRIMARY KEY CHECK(id = 1), encrypted_tokens BLOB NOT NULL,
 expires_at INTEGER NOT NULL, generation INTEGER NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('connected','needs_reauth'))
);
CREATE TABLE request_usage (
 id TEXT PRIMARY KEY, key_id TEXT NOT NULL REFERENCES api_key(id), endpoint TEXT NOT NULL,
 requested_model TEXT NOT NULL, resolved_model TEXT, response_model TEXT, upstream_request_id TEXT,
 started_at TEXT NOT NULL, finished_at TEXT,
 outcome TEXT NOT NULL CHECK(outcome IN ('in_progress','completed','denied','upstream_error','interrupted')),
 http_status INTEGER, usage_state TEXT NOT NULL DEFAULT 'unknown'
 CHECK(usage_state IN ('complete','partial','unknown','not_applicable')),
 input_tokens INTEGER CHECK(input_tokens >= 0), cache_read_tokens INTEGER CHECK(cache_read_tokens >= 0),
 cache_write_tokens INTEGER CHECK(cache_write_tokens >= 0), output_tokens INTEGER CHECK(output_tokens >= 0),
 raw_usage TEXT
);
CREATE INDEX usage_key_time ON request_usage(key_id,started_at);
CREATE INDEX usage_model_time ON request_usage(resolved_model,started_at);
CREATE TABLE admin_session (
 token_hash BLOB PRIMARY KEY, csrf_token TEXT NOT NULL, expires_at INTEGER NOT NULL
);
-- Candidates, not unverified entitlement claims. Owner refreshes and reviews the catalog after login.
INSERT INTO model VALUES
 ('claude-fable-5-1','Claude Fable 5.1','fable-5.1',0,NULL);
