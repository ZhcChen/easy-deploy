ALTER TABLE apps
ADD COLUMN environment TEXT NOT NULL DEFAULT 'test' CHECK (environment IN ('production', 'test'));

CREATE INDEX IF NOT EXISTS idx_apps_environment ON apps(environment);
