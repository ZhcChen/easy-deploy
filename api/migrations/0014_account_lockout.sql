ALTER TABLE admin_accounts
ADD COLUMN failed_login_attempts INTEGER NOT NULL DEFAULT 0;

ALTER TABLE admin_accounts
ADD COLUMN locked_at TEXT;

ALTER TABLE admin_accounts
ADD COLUMN locked_reason TEXT NOT NULL DEFAULT '';

CREATE INDEX IF NOT EXISTS idx_admin_accounts_locked_at ON admin_accounts(locked_at);
