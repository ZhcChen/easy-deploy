ALTER TABLE api_tokens
ADD COLUMN app_scope_json TEXT NOT NULL DEFAULT '[]';

ALTER TABLE api_tokens
ADD COLUMN unit_scope_json TEXT NOT NULL DEFAULT '[]';

ALTER TABLE api_tokens
ADD COLUMN expires_at TEXT;

ALTER TABLE api_tokens
ADD COLUMN rate_limit_per_minute INTEGER NOT NULL DEFAULT 60
CHECK (rate_limit_per_minute BETWEEN 1 AND 10000);

ALTER TABLE api_tokens
ADD COLUMN max_concurrent_requests INTEGER NOT NULL DEFAULT 2
CHECK (max_concurrent_requests BETWEEN 1 AND 100);

ALTER TABLE api_tokens
ADD COLUMN rate_window_started_at TEXT NOT NULL
DEFAULT '';

ALTER TABLE api_tokens
ADD COLUMN rate_window_count INTEGER NOT NULL DEFAULT 0
CHECK (rate_window_count >= 0);

ALTER TABLE api_tokens
ADD COLUMN active_request_count INTEGER NOT NULL DEFAULT 0
CHECK (active_request_count >= 0);

CREATE INDEX IF NOT EXISTS idx_api_tokens_expiry_status
ON api_tokens(status, expires_at);
